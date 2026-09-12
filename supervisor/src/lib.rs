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
/// `record` true → watch the FULL chain (record every event); false → only terminations.
fn spawn_and_watch(handle: &str, manifest: &str, record: bool) -> Result<String, String> {
    let id = runtime_spawn(manifest)?;
    let sub = if record { monitor(id.clone()) } else { monitor_filtered(id.clone(), terminations()) };
    if let Err(e) = sub {
        log(format!("[supervisor] {} watch({}) failed: {}", handle, id, e));
    }
    log(format!(
        "[supervisor] spawned {} as {}{}",
        handle, id, if record { " (recording full chain)" } else { "" }
    ));
    Ok(id)
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
        let record = s.record.map(RecordCfg::into_sink);
        // initial reconcile: desired-absent -> spawn + watch.
        let current_id = match spawn_and_watch(&s.handle, &s.manifest, record.is_some()) {
            Ok(id) => id,
            Err(e) => {
                // A hard spawn failure at init is fatal — the operator needs to know.
                return err_result(&format!("supervisor: spawn {} failed: {}", s.handle, e));
            }
        };
        services.push(Svc {
            handle: s.handle,
            manifest: s.manifest,
            max: s.max.unwrap_or(DEFAULT_MAX),
            window_ms: s.window_ms.unwrap_or(DEFAULT_WINDOW_MS),
            current_id,
            restarts: Vec::new(),
            blocked: false,
            record,
            rec_seq: 0,
        });
    }
    log(format!("[supervisor] roster up: {} service(s)", services.len()));
    SupervisorState::set(SupervisorState { services });
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

    // Flight-recorder: a recording service watches its child's FULL chain, so write
    // every event to its sink. (A plain service is subscribed terminations-only and
    // never reaches here for non-terminals.) Clone the sink + bump seq under a short
    // borrow, then do the host I/O outside it.
    let sink = SupervisorState::with_mut(|st| {
        st.services
            .iter_mut()
            .find(|s| s.current_id == subject && s.record.is_some())
            .map(|s| {
                let seq = s.rec_seq;
                s.rec_seq += 1;
                (s.record.clone().unwrap(), s.handle.clone(), seq)
            })
    });
    if let Some((sink, handle, seq)) = sink {
        let line = format!(
            "{{\"handle\":\"{}\",\"child\":\"{}\",\"type\":\"{}\",\"seq\":{},\"data_hex\":\"{}\"}}",
            json_escape(&handle), json_escape(&subject), json_escape(&event_type), seq, hex(&data)
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
        // Reconcile: desired-but-now-absent -> respawn (recording carries across incarnations).
        let handle = svc.handle.clone();
        let manifest = svc.manifest.clone();
        let record = svc.record.is_some();
        match spawn_and_watch(&handle, &manifest, record) {
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
