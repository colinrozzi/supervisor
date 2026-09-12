//! # supervisor — the reconciler actor (v0)
//!
//! Holds a roster (from init config) and reconciles reality to it: spawn each
//! declared service + watch it; when one terminates, respawn it (rate-limited)
//! — edge-triggered on `handle-actor-event(event-type == "terminated")`.
//!
//! Per-service `record` (opt-in flight-recorder): watch the child's FULL chain and
//! POST every event to a sink URL via the http-client handler. (File sink arrives
//! when theater ports the filesystem handler.)
//!
//! Composes theater primitives directly (post-#206 @ c197d707): `runtime.spawn`,
//! `lifecycle.monitor`/`monitor-filtered`, `http-client.request`, `timer.now`,
//! `self.log`. No supervisor handler, no store.
//!
//! (Level-triggered timer reconcile + respawn-only-on-Failed + live roster mutation
//! are the next steps; see docs/DESIGN.md.)

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};
use serde::Deserialize;
use theater_guest::State;

packr_guest::setup_guest!();

// Rate limiter default: at most N restarts within M ms, else block that service.
const DEFAULT_MAX: u32 = 5;
const DEFAULT_WINDOW_MS: u64 = 60_000;

// ---- interface metadata (must mirror theater:simple/* exactly for the hash) --
pack_types! {
    variant spawn-failure {
        bad-manifest(string),
        wasm-fetch(string),
        handler-registry(string),
        wasm-invalid(string),
        interface-mismatch(string),
        missing-interface(string),
        missing-metadata(string),
        init-failed(string),
        child-failed(string),
        child-stopped(string),
        timeout(string),
        internal(string),
    }
    variant runtime-error {
        permission-denied(string),
        runtime-unavailable,
        actor-not-found(string),
        invalid-argument(string),
        spawn-failed(spawn-failure),
        internal(string),
    }
    record http-header {
        name: string,
        value: string,
    }
    record http-request {
        method: string,
        url: string,
        headers: list<http-header>,
        body: option<list<u8>>,
    }
    record http-response {
        status: u16,
        headers: list<http-header>,
        body: option<list<u8>>,
    }
    variant filesystem-error {
        not-found(string),
        permission-denied(string),
        already-exists(string),
        not-a-directory(string),
        is-a-directory(string),
        invalid-path(string),
        io-error(string),
    }
    imports {
        theater:simple/self {
            log: func(msg: string),
        }
        theater:simple/runtime {
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, runtime-error>,
            stop-actor: func(id: string) -> result<_, runtime-error>,
        }
        theater:simple/tcp {
            listen: func(address: string) -> result<string, string>,
            activate: func(connection-id: string) -> result<_, string>,
            receive: func(connection-id: string, max-bytes: u32) -> result<list<u8>, string>,
            send: func(connection-id: string, data: list<u8>) -> result<u64, string>,
            close: func(connection-id: string) -> result<_, string>,
        }
        theater:simple/lifecycle {
            monitor: func(subject: string) -> result<_, string>,
            monitor-filtered: func(subject: string, filter: value) -> result<_, string>,
        }
        theater:simple/http-client {
            request: func(req: http-request) -> result<http-response, string>,
        }
        theater:simple/filesystem {
            append-file: func(path: string, content: list<u8>) -> result<_, filesystem-error>,
        }
        theater:simple/timer {
            now: func() -> u64,
        }
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:simple/actor.get-state: func() -> value,
        theater:simple/lifecycle-handlers.handle-actor-event: func(subject: string, event-type: string, data: list<u8>) -> result<_, string>,
        theater:simple/tcp-client.handle-connection: func(connection-id: string) -> result<_, string>,
        theater:simple/tcp-client.on-close: func(connection-id: string, reason: string) -> result<_, string>,
    }
}

#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/timer", name = "now")]
fn timer_now() -> u64;

// Post-#206: monitor(subject) delivers the FULL CHAIN (Pattern::any); monitor-filtered
// takes an arbitrary Pattern. A recording service uses the full-chain monitor (record
// every event); a plain service uses monitor-filtered(terminations()) — woken only on
// a child's terminal event — via the theater-guest `terminations()` preset.
#[import(module = "theater:simple/lifecycle", name = "monitor")]
fn monitor(subject: String) -> Result<(), String>;
#[import(module = "theater:simple/lifecycle", name = "monitor-filtered")]
fn monitor_filtered(subject: String, filter: Value) -> Result<(), String>;

use theater_guest::filters::terminations;

// tcp handler (inbound control surface). receive() blocks until data/EOF, so the server
// reads a whole request synchronously inside handle-connection — no on-data needed.
#[import(module = "theater:simple/tcp", name = "listen")]
fn tcp_listen(address: String) -> Result<String, String>;
#[import(module = "theater:simple/tcp", name = "activate")]
fn tcp_activate(conn: String) -> Result<(), String>;
#[import(module = "theater:simple/tcp", name = "receive")]
fn tcp_receive(conn: String, max_bytes: u32) -> Result<Vec<u8>, String>;
#[import(module = "theater:simple/tcp", name = "send")]
fn tcp_send(conn: String, data: Vec<u8>) -> Result<u64, String>;
#[import(module = "theater:simple/tcp", name = "close")]
fn tcp_close(conn: String) -> Result<(), String>;

// runtime.stop-actor -> result<_, runtime-error>; raw + parse like spawn.
#[import(module = "theater:simple/runtime", name = "stop-actor")]
fn runtime_stop_actor_raw(id: String) -> Value;
fn runtime_stop_actor(id: &str) -> Result<(), String> {
    match runtime_stop_actor_raw(id.to_string()) {
        Value::Result { value: Ok(_), .. } => Ok(()),
        Value::Result { value: Err(e), .. } => Err(error_case(*e)),
        Value::Variant { tag: 0, .. } => Ok(()),
        Value::Variant { tag: 1, payload, .. } => {
            Err(payload.into_iter().next().map(error_case).unwrap_or_else(|| String::from("unknown")))
        }
        _ => Err(String::from("stop-actor: unexpected result")),
    }
}

// http-client.request(req: http-request) -> result<http-response, string>. Records cross
// the boundary as `Value` (a Value::Record), so import raw and hand-build the request.
#[import(module = "theater:simple/http-client", name = "request")]
fn http_request_raw(req: Value) -> Value;

// filesystem.append-file(path, content) -> result<_, filesystem-error>. Append (create
// if absent), sandboxed to the handler's configured root; the file-sink path is relative
// to that root. Import raw + parse the result (Ok unit | Err filesystem-error).
#[import(module = "theater:simple/filesystem", name = "append-file")]
fn append_file_raw(path: String, content: Vec<u8>) -> Value;

/// Append bytes to a sandboxed file; returns () or the filesystem-error case name.
fn fs_append(path: &str, content: Vec<u8>) -> Result<(), String> {
    match append_file_raw(String::from(path), content) {
        Value::Result { value: Ok(_), .. } => Ok(()),
        Value::Result { value: Err(e), .. } => Err(error_detail(*e)),
        Value::Variant { tag: 0, .. } => Ok(()),
        Value::Variant { tag: 1, payload, .. } => {
            Err(payload.into_iter().next().map(error_detail).unwrap_or_else(|| String::from("unknown")))
        }
        _ => Err(String::from("filesystem: unexpected result")),
    }
}

/// Lowercase hex of raw bytes (no_std, no dep) — how a chain event's `data` is carried
/// in the recorded JSON so the sink gets a faithful, inspectable copy.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

/// Minimal JSON string escaping (quotes + backslashes + control) for the small,
/// mostly-safe fields we emit (handles, ids, event types).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// POST `body` as application/json to `url` via the http-client handler.
/// Returns the response status, or an error string.
fn http_post_json(url: &str, body: Vec<u8>) -> Result<u16, String> {
    let header = Value::Record {
        type_name: String::from("http-header"),
        fields: vec![
            (String::from("name"), Value::String(String::from("content-type"))),
            (String::from("value"), Value::String(String::from("application/json"))),
        ],
    };
    let req = Value::Record {
        type_name: String::from("http-request"),
        fields: vec![
            (String::from("method"), Value::String(String::from("POST"))),
            (String::from("url"), Value::String(String::from(url))),
            (
                String::from("headers"),
                Value::List { elem_type: ValueType::Record(String::from("http-header")), items: vec![header] },
            ),
            (
                String::from("body"),
                Value::Option {
                    inner_type: ValueType::List(Box::new(ValueType::U8)),
                    value: Some(Box::new(Value::List {
                        elem_type: ValueType::U8,
                        items: body.into_iter().map(Value::U8).collect(),
                    })),
                },
            ),
        ],
    };
    // response is result<http-response, string>; http-response.status is the u16 we want.
    let status_of = |resp: Value| -> u16 {
        if let Value::Record { fields, .. } = resp {
            for (k, v) in fields {
                if k == "status" {
                    return match v {
                        Value::U16(s) => s,
                        Value::U32(s) => s as u16,
                        _ => 0,
                    };
                }
            }
        }
        0
    };
    match http_request_raw(req) {
        Value::Result { value: Ok(resp), .. } => Ok(status_of(*resp)),
        Value::Result { value: Err(e), .. } => Err(error_case(*e)),
        Value::Variant { tag: 0, payload, .. } => Ok(payload.into_iter().next().map(status_of).unwrap_or(0)),
        Value::Variant { tag: 1, payload, .. } => {
            Err(payload.into_iter().next().map(error_case).unwrap_or_else(|| String::from("unknown")))
        }
        _ => Err(String::from("http-client: unexpected result")),
    }
}

// runtime.spawn returns result<string, runtime-error>; import raw + parse the
// Value. A `result<T,E>` reaches the guest either as packr-native `Value::Result`
// (value: Ok/Err) or as a tagged `Value::Variant` (tag 0 = Ok, tag 1 = Err),
// depending on host/guest ABI vintage — accept both so we're skew-proof.
#[import(module = "theater:simple/runtime", name = "spawn")]
fn runtime_spawn_raw(manifest: String, init_state: Option<Value>, wasm_bytes: Option<Vec<u8>>) -> Value;

/// A `runtime-error` variant Value rendered as its case name (for the log).
fn error_case(v: Value) -> String {
    match v {
        Value::Variant { case_name, .. } => case_name,
        Value::String(s) => s,
        _ => String::from("unknown"),
    }
}

/// Like `error_case` but includes the payload detail string, e.g. "permission-denied: <why>".
fn error_detail(v: Value) -> String {
    match v {
        Value::Variant { case_name, payload, .. } => match payload.into_iter().next() {
            Some(Value::String(s)) => format!("{}: {}", case_name, s),
            _ => case_name,
        },
        Value::String(s) => s,
        _ => String::from("unknown"),
    }
}

fn runtime_spawn(manifest: &str) -> Result<String, String> {
    match runtime_spawn_raw(manifest.to_string(), None, None) {
        // packr-native result
        Value::Result { value: Ok(inner), .. } => match *inner {
            Value::String(id) => Ok(id),
            _ => Err(String::from("spawn: unexpected ok payload")),
        },
        Value::Result { value: Err(inner), .. } => Err(error_case(*inner)),
        // tagged-variant result (other ABI vintage)
        Value::Variant { tag: 0, payload, .. } => match payload.into_iter().next() {
            Some(Value::String(id)) => Ok(id),
            _ => Err(String::from("spawn: unexpected ok payload")),
        },
        Value::Variant { tag: 1, payload, .. } => {
            Err(payload.into_iter().next().map(error_case).unwrap_or_else(|| String::from("unknown")))
        }
        // bare string id (defensive)
        Value::String(id) => Ok(id),
        _ => Err(String::from("spawn: unexpected result")),
    }
}

// ---- roster config (init-state JSON), parsed with serde --------------------
#[derive(Deserialize)]
struct Config {
    services: Vec<ServiceCfg>,
    /// Opt-in control surface: listen on this TCP port for live roster edits.
    #[serde(default)]
    control: Option<ControlCfg>,
}
#[derive(Deserialize)]
struct ControlCfg {
    port: u16,
}
#[derive(Deserialize)]
struct ServiceCfg {
    handle: String,
    manifest: String,
    #[serde(default)]
    max: Option<u32>,
    #[serde(default)]
    window_ms: Option<u64>,
    /// Opt-in flight-recorder: watch this service's FULL chain and write every event
    /// to a sink — `{"kind":"http","url":…}` (POST) or `{"kind":"file","path":…}` (append).
    #[serde(default)]
    record: Option<RecordCfg>,
    /// Keep the last N chain events in memory (queryable via the `chain` control op)
    /// without a sink. Implies a full-chain watch.
    #[serde(default)]
    keep_chain: Option<bool>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum RecordCfg {
    Http { url: String },
    File { path: String },
}
impl RecordCfg {
    fn into_sink(self) -> Sink {
        match self {
            RecordCfg::Http { url } => Sink::Http(url),
            RecordCfg::File { path } => Sink::File(path),
        }
    }
}

// ---- state (in-module cell; a chain projection) ----------------------------
// v0/experiment flat struct: spec (handle/manifest/max/window) + status
// (current_id/restarts/blocked) together. The clean spec/status split is the
// design ideal; this proves the loop.
#[derive(Clone, GraphValue, State)]
struct SupervisorState {
    services: Vec<Svc>,
}
/// Where a recording service's chain events go.
#[derive(Clone, GraphValue)]
enum Sink {
    Http(String), // POST each event to this URL (http-client)
    File(String), // append each event to this sandboxed path (filesystem)
}
/// One recorded chain event kept in memory for the `chain` control op.
#[derive(Clone, GraphValue)]
struct ChainRec {
    seq: u64,
    event_type: String,
    data_hex: String,
}
const CHAIN_BUF_MAX: usize = 200;

#[derive(Clone, GraphValue)]
struct Svc {
    handle: String,
    manifest: String,
    max: u32,
    window_ms: u64,
    current_id: String,
    restarts: Vec<u64>,
    blocked: bool,
    /// Flight-recorder sink (None = not recording) + a per-service event counter.
    record: Option<Sink>,
    rec_seq: u64,
    /// Keep the full chain in memory (queryable) — implies a full-chain watch.
    keep_chain: bool,
    /// Capped ring of recent chain events (when record or keep_chain is on).
    chain_buf: Vec<ChainRec>,
}
impl Svc {
    /// Whether this service is watched on its full chain (vs terminations-only).
    fn full_chain(&self) -> bool {
        self.record.is_some() || self.keep_chain
    }
}

// ---- helpers ---------------------------------------------------------------
fn ok_unit() -> Value {
    let unit = Value::Tuple(vec![]);
    Value::Result { ok_type: unit.infer_type(), err_type: ValueType::String, value: Ok(Box::new(unit)) }
}
fn err_result(msg: &str) -> Value {
    Value::Result { ok_type: ValueType::Tuple(vec![]), err_type: ValueType::String, value: Err(Box::new(Value::String(String::from(msg)))) }
}

/// Extract a config string from the init Value — accepts a bare String, or the
/// `option<list<u8>>` / `list<u8>` byte shapes, unwrapping any tuple.
fn config_string(v: Value) -> Option<String> {
    let v = match v {
        Value::Tuple(mut items) if !items.is_empty() => items.remove(0),
        other => other,
    };
    match v {
        Value::String(s) => Some(s),
        Value::Option { value: Some(inner), .. } => config_string(*inner),
        Value::List { items, .. } => {
            let bytes: Vec<u8> = items.into_iter().filter_map(|x| if let Value::U8(b) = x { Some(b) } else { None }).collect();
            String::from_utf8(bytes).ok()
        }
        _ => None,
    }
}

/// spawn a service's manifest + watch it; returns the new child id.
/// `full_chain` true → watch the FULL chain (record/keep every event); false → terminations only.
fn spawn_and_watch(handle: &str, manifest: &str, full_chain: bool) -> Result<String, String> {
    let id = runtime_spawn(manifest)?;
    let sub = if full_chain { monitor(id.clone()) } else { monitor_filtered(id.clone(), terminations()) };
    if let Err(e) = sub {
        log(format!("[supervisor] {} watch({}) failed: {}", handle, id, e));
    }
    log(format!(
        "[supervisor] spawned {} as {}{}",
        handle, id, if full_chain { " (full-chain watch)" } else { "" }
    ));
    Ok(id)
}

/// Spawn + watch a declared service and build its Svc (spec + fresh status).
fn build_svc(s: ServiceCfg) -> Result<Svc, String> {
    let record = s.record.map(RecordCfg::into_sink);
    let keep_chain = s.keep_chain.unwrap_or(false);
    let full_chain = record.is_some() || keep_chain;
    let current_id = spawn_and_watch(&s.handle, &s.manifest, full_chain)?;
    Ok(Svc {
        handle: s.handle,
        manifest: s.manifest,
        max: s.max.unwrap_or(DEFAULT_MAX),
        window_ms: s.window_ms.unwrap_or(DEFAULT_WINDOW_MS),
        current_id,
        restarts: Vec::new(),
        blocked: false,
        record,
        rec_seq: 0,
        keep_chain,
        chain_buf: Vec::new(),
    })
}

// ---- exports ---------------------------------------------------------------
#[export(name = "theater:simple/actor.init")]
fn init(config: Value) -> Value {
    log(String::from("[supervisor] init — reconciling roster"));
    let raw = match config_string(config) {
        Some(s) if !s.is_empty() => s,
        _ => return err_result("supervisor: init config must be a JSON roster string"),
    };
    let cfg: Config = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return err_result(&format!("supervisor: bad roster JSON: {}", e)),
    };

    let mut services: Vec<Svc> = Vec::new();
    for s in cfg.services {
        let handle = s.handle.clone();
        match build_svc(s) {
            Ok(svc) => services.push(svc),
            // A hard spawn failure at init is fatal — the operator needs to know.
            Err(e) => return err_result(&format!("supervisor: spawn {} failed: {}", handle, e)),
        }
    }
    log(format!("[supervisor] roster up: {} service(s)", services.len()));
    SupervisorState::set(SupervisorState { services });

    // Opt-in control surface: listen for live roster edits (JSON over TCP).
    if let Some(control) = cfg.control {
        let addr = format!("127.0.0.1:{}", control.port);
        match tcp_listen(addr.clone()) {
            Ok(_listener) => log(format!("[supervisor] control surface listening on {}", addr)),
            Err(e) => log(format!("[supervisor] control listen on {} failed: {}", addr, e)),
        }
    }
    ok_unit()
}

#[export(name = "theater:simple/lifecycle-handlers.handle-actor-event")]
fn handle_actor_event(input: Value) -> Value {
    // Host passes Tuple[subject, event-type, data].
    let (subject, event_type, data) = match &input {
        Value::Tuple(items) if items.len() >= 2 => {
            let s = if let Value::String(s) = &items[0] { s.clone() } else { String::from("?") };
            let e = if let Value::String(e) = &items[1] { e.clone() } else { String::from("?") };
            let d = match items.get(2) {
                Some(Value::List { items, .. }) => {
                    items.iter().filter_map(|x| if let Value::U8(b) = x { Some(*b) } else { None }).collect()
                }
                _ => Vec::new(),
            };
            (s, e, d)
        }
        _ => return ok_unit(),
    };

    // Full-chain services (record and/or keep_chain) get every event. Under a short
    // borrow: bump seq, push into the in-memory ring (for the `chain` op), and grab the
    // sink (if any) to write outside the borrow. (Plain services are terminations-only
    // and never reach here for non-terminals.)
    let data_hex = hex(&data);
    let sink = SupervisorState::with_mut(|st| {
        let s = st.services.iter_mut().find(|s| s.current_id == subject && s.full_chain())?;
        let seq = s.rec_seq;
        s.rec_seq += 1;
        s.chain_buf.push(ChainRec { seq, event_type: event_type.clone(), data_hex: data_hex.clone() });
        if s.chain_buf.len() > CHAIN_BUF_MAX {
            s.chain_buf.remove(0);
        }
        s.record.clone().map(|sink| (sink, s.handle.clone(), seq))
    });
    if let Some((sink, handle, seq)) = sink {
        let line = format!(
            "{{\"handle\":\"{}\",\"child\":\"{}\",\"type\":\"{}\",\"seq\":{},\"data_hex\":\"{}\"}}",
            json_escape(&handle), json_escape(&subject), json_escape(&event_type), seq, data_hex
        );
        let outcome = match &sink {
            Sink::Http(url) => http_post_json(url, line.into_bytes()).map(|status| format!("http {}", status)),
            Sink::File(path) => {
                // one JSON object per line (JSONL) — append the line + newline.
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                fs_append(path, bytes).map(|_| format!("file {}", path))
            }
        };
        match outcome {
            Ok(where_) => log(format!("[supervisor] recorded {} seq={} type={} -> {}", handle, seq, event_type, where_)),
            Err(e) => log(format!("[supervisor] record write failed ({} seq={}): {}", handle, seq, e)),
        }
    }

    // Only terminations drive the reconcile. exp1 respawns on ANY terminated; gating
    // on TerminationCause::Failed is #9 (needs pack-dev's panic-trap fix to emit Failed).
    if event_type != "terminated" {
        return ok_unit();
    }

    let now = timer_now();
    SupervisorState::with_mut(|st| {
        let idx = match st.services.iter().position(|s| s.current_id == subject) {
            Some(i) => i,
            None => {
                log(format!("[supervisor] terminated event for unknown id {} — ignoring", subject));
                return;
            }
        };
        // Rate limit: trim to window, count.
        let svc = &mut st.services[idx];
        let window = svc.window_ms;
        svc.restarts.retain(|t| now.saturating_sub(*t) <= window);
        let recent = svc.restarts.len() as u32;
        log(format!(
            "[supervisor] {} ({}) terminated — recent_restarts={} max={}",
            svc.handle, subject, recent, svc.max
        ));
        if svc.blocked {
            log(format!("[supervisor] {} already blocked — not respawning", svc.handle));
            return;
        }
        if recent >= svc.max {
            svc.blocked = true;
            log(format!(
                "[supervisor] crash-loop on {} ({} in {}ms) — BLOCKED, not respawning",
                svc.handle, recent + 1, window
            ));
            return;
        }
        // Reconcile: desired-but-now-absent -> respawn (watch mode carries across incarnations).
        let handle = svc.handle.clone();
        let manifest = svc.manifest.clone();
        let full_chain = svc.full_chain();
        match spawn_and_watch(&handle, &manifest, full_chain) {
            Ok(new_id) => {
                let svc = &mut st.services[idx];
                svc.current_id = new_id;
                svc.restarts.push(now);
            }
            Err(e) => log(format!("[supervisor] respawn {} failed: {}", handle, e)),
        }
    });
    ok_unit()
}

// ---- control surface (JSON over TCP) ---------------------------------------

#[export(name = "theater:simple/tcp-client.handle-connection")]
fn handle_connection(input: Value) -> Value {
    let conn = match &input {
        Value::Tuple(items) if !items.is_empty() => match &items[0] {
            Value::String(c) => c.clone(),
            _ => return ok_unit(),
        },
        Value::String(c) => c.clone(),
        _ => return ok_unit(),
    };
    if let Err(e) = tcp_activate(conn.clone()) {
        log(format!("[supervisor] control activate failed: {}", e));
        return ok_unit();
    }
    // Read a newline-terminated JSON request (receive blocks until data/EOF).
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match tcp_receive(conn.clone(), 4096) {
            Ok(chunk) if chunk.is_empty() => break, // EOF
            Ok(chunk) => {
                buf.extend_from_slice(&chunk);
                if buf.contains(&b'\n') || buf.len() > 65536 {
                    break;
                }
            }
            Err(e) => {
                log(format!("[supervisor] control receive failed: {}", e));
                let _ = tcp_close(conn);
                return ok_unit();
            }
        }
    }
    let line: &[u8] = buf.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let mut reply = handle_op(line).into_bytes();
    reply.push(b'\n');
    let _ = tcp_send(conn.clone(), reply);
    let _ = tcp_close(conn);
    ok_unit()
}

#[export(name = "theater:simple/tcp-client.on-close")]
fn on_close(_input: Value) -> Value {
    ok_unit()
}

fn err_reply(msg: &str) -> String {
    format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(msg))
}
fn ok_msg(msg: &str) -> String {
    format!("{{\"ok\":true,\"message\":\"{}\"}}", json_escape(msg))
}
fn svc_json(s: &Svc) -> String {
    format!(
        "{{\"handle\":\"{}\",\"manifest\":\"{}\",\"max\":{},\"window_ms\":{},\"recording\":{},\"keep_chain\":{},\"blocked\":{},\"current_id\":\"{}\",\"restarts\":{}}}",
        json_escape(&s.handle), json_escape(&s.manifest), s.max, s.window_ms,
        s.record.is_some(), s.keep_chain, s.blocked, json_escape(&s.current_id), s.restarts.len()
    )
}
fn roster_reply(st: &SupervisorState) -> String {
    let items: Vec<String> = st.services.iter().map(svc_json).collect();
    format!("{{\"ok\":true,\"roster\":[{}]}}", items.join(","))
}
fn chain_reply(s: &Svc) -> String {
    let items: Vec<String> = s
        .chain_buf
        .iter()
        .map(|c| format!("{{\"seq\":{},\"type\":\"{}\",\"data_hex\":\"{}\"}}", c.seq, json_escape(&c.event_type), c.data_hex))
        .collect();
    format!("{{\"ok\":true,\"handle\":\"{}\",\"events\":[{}]}}", json_escape(&s.handle), items.join(","))
}

/// Parse one JSON control op and apply it (edit desired → reconcile), returning a JSON reply.
fn handle_op(line: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return err_reply(&format!("bad JSON: {}", e)),
    };
    let op = v.get("op").and_then(|o| o.as_str()).unwrap_or("");
    let handle_of = || v.get("handle").and_then(|x| x.as_str()).map(|s| s.to_string());

    let mut reply = String::new();
    match op {
        "list" => SupervisorState::with_mut(|st| reply = roster_reply(st)),
        "status" => {
            let h = match handle_of() {
                Some(h) => h,
                None => return err_reply("status: handle required"),
            };
            SupervisorState::with_mut(|st| {
                reply = match st.services.iter().find(|s| s.handle == h) {
                    Some(s) => format!("{{\"ok\":true,\"service\":{}}}", svc_json(s)),
                    None => err_reply(&format!("unknown handle '{}'", h)),
                }
            });
        }
        "chain" => {
            let h = match handle_of() {
                Some(h) => h,
                None => return err_reply("chain: handle required"),
            };
            SupervisorState::with_mut(|st| {
                reply = match st.services.iter().find(|s| s.handle == h) {
                    Some(s) => chain_reply(s),
                    None => err_reply(&format!("unknown handle '{}'", h)),
                }
            });
        }
        "add" => {
            let cfg: ServiceCfg = match serde_json::from_value(v.get("service").cloned().unwrap_or(serde_json::Value::Null)) {
                Ok(c) => c,
                Err(e) => return err_reply(&format!("bad service: {}", e)),
            };
            let handle = cfg.handle.clone();
            SupervisorState::with_mut(|st| {
                if st.services.iter().any(|s| s.handle == handle) {
                    reply = err_reply(&format!("handle '{}' already exists", handle));
                    return;
                }
                match build_svc(cfg) {
                    Ok(svc) => {
                        st.services.push(svc);
                        log(format!("[supervisor] control: added {}", handle));
                        reply = ok_msg(&format!("added {}", handle));
                    }
                    Err(e) => reply = err_reply(&format!("spawn failed: {}", e)),
                }
            });
        }
        "remove" => {
            let h = match handle_of() {
                Some(h) => h,
                None => return err_reply("remove: handle required"),
            };
            SupervisorState::with_mut(|st| {
                reply = match st.services.iter().position(|s| s.handle == h) {
                    Some(i) => {
                        let id = st.services[i].current_id.clone();
                        if let Err(e) = runtime_stop_actor(&id) {
                            log(format!("[supervisor] control: stop {} failed: {}", h, e));
                        }
                        st.services.remove(i);
                        log(format!("[supervisor] control: removed {}", h));
                        ok_msg(&format!("removed {}", h))
                    }
                    None => err_reply(&format!("unknown handle '{}'", h)),
                }
            });
        }
        "apply" => {
            let cfgs: Vec<ServiceCfg> = match serde_json::from_value(v.get("services").cloned().unwrap_or(serde_json::Value::Null)) {
                Ok(c) => c,
                Err(e) => return err_reply(&format!("bad services: {}", e)),
            };
            let desired: Vec<String> = cfgs.iter().map(|c| c.handle.clone()).collect();
            SupervisorState::with_mut(|st| {
                let mut removed = 0u32;
                let mut i = 0;
                while i < st.services.len() {
                    if !desired.contains(&st.services[i].handle) {
                        let id = st.services[i].current_id.clone();
                        let _ = runtime_stop_actor(&id);
                        st.services.remove(i);
                        removed += 1;
                    } else {
                        i += 1;
                    }
                }
                let mut added = 0u32;
                for cfg in cfgs {
                    if !st.services.iter().any(|s| s.handle == cfg.handle) {
                        match build_svc(cfg) {
                            Ok(svc) => {
                                st.services.push(svc);
                                added += 1;
                            }
                            Err(e) => log(format!("[supervisor] control: apply spawn failed: {}", e)),
                        }
                    }
                }
                log(format!("[supervisor] control: applied +{} -{}", added, removed));
                reply = format!("{{\"ok\":true,\"added\":{},\"removed\":{}}}", added, removed);
            });
        }
        other => return err_reply(&format!("unknown op '{}'", other)),
    }
    reply
}
