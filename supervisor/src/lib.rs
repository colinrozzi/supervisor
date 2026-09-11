//! # supervisor — the reconciler actor (v0, Experiment 1)
//!
//! Holds a roster (from init config) and reconciles reality to it: spawn each
//! declared service + `monitor` it; when one terminates, respawn it (rate-limited)
//! — edge-triggered on `handle-lifecycle-event(event-type == "terminated")`.
//! (Level-triggered timer reconcile + `record` sink + live roster mutation are
//! the next experiments; see docs/DESIGN.md.)
//!
//! Composes theater primitives directly (post-#204 @ c3937bdc): `runtime.spawn`,
//! `lifecycle.monitor`, `timer.now`, `self.log`. No supervisor handler, no store.

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
    imports {
        theater:simple/self {
            log: func(msg: string),
        }
        theater:simple/runtime {
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, runtime-error>,
        }
        theater:simple/lifecycle {
            monitor-filtered: func(subject: string, filter: value) -> result<_, string>,
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

// Post-#206: monitor(subject) now delivers the FULL CHAIN; monitor-filtered takes
// an arbitrary Pattern. For crash-catch we want only terminations — woken on a
// child's terminal event, nothing else — via the theater-guest `terminations()` preset.
#[import(module = "theater:simple/lifecycle", name = "monitor-filtered")]
fn monitor_filtered(subject: String, filter: Value) -> Result<(), String>;

use theater_guest::filters::terminations;

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
}

// ---- state (in-module cell; a chain projection) ----------------------------
// v0/experiment flat struct: spec (handle/manifest/max/window) + status
// (current_id/restarts/blocked) together. The clean spec/status split is the
// design ideal; this proves the loop.
#[derive(Clone, GraphValue, State)]
struct SupervisorState {
    services: Vec<Svc>,
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

/// spawn a service's manifest + watch it for terminations; returns the new child id.
fn spawn_and_monitor(handle: &str, manifest: &str) -> Result<String, String> {
    let id = runtime_spawn(manifest)?;
    if let Err(e) = monitor_filtered(id.clone(), terminations()) {
        log(format!("[supervisor] {} monitor-filtered({}) failed: {}", handle, id, e));
    }
    log(format!("[supervisor] spawned {} as {}", handle, id));
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
        // initial reconcile: desired-absent -> spawn + monitor.
        let current_id = match spawn_and_monitor(&s.handle, &s.manifest) {
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
        });
    }
    log(format!("[supervisor] roster up: {} service(s)", services.len()));
    SupervisorState::set(SupervisorState { services });
    ok_unit()
}

#[export(name = "theater:simple/lifecycle-handlers.handle-actor-event")]
fn handle_actor_event(input: Value) -> Value {
    // Host passes Tuple[subject, event-type, data].
    let (subject, event_type) = match &input {
        Value::Tuple(items) if items.len() >= 2 => {
            let s = if let Value::String(s) = &items[0] { s.clone() } else { String::from("?") };
            let e = if let Value::String(e) = &items[1] { e.clone() } else { String::from("?") };
            (s, e)
        }
        _ => return ok_unit(),
    };

    // We subscribed with monitor-filtered(terminations()), so we're woken only on
    // terminals — but guard anyway (cheap, and keeps the handler honest if the
    // filter ever widens). exp1 respawns on ANY terminated; gating on
    // TerminationCause::Failed is #9 (needs pack-dev's panic-trap fix to produce Failed).
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
        // Reconcile: desired-but-now-absent -> respawn.
        let handle = svc.handle.clone();
        let manifest = svc.manifest.clone();
        match spawn_and_monitor(&handle, &manifest) {
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
