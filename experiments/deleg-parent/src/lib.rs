//! deleg-parent — MULTI-LEVEL delegation fixture. On init it `runtime.spawn`s a
//! grandchild (the hosted noop), mirroring inbox's acceptor→spine shape: the supervisor
//! spawns THIS with `runtime: inherit`, and this must inherit runtime THROUGH to spawn its
//! own child. init returns Ok ONLY if the delegated spawn succeeds — so a bootstrapped
//! supervisor `apply`ing this either shows it running (delegation works) or reports a spawn
//! failure (delegation gap). The dry-run gate before the real inbox multi-level cutover.
#![no_std]
extern crate alloc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::{format, vec};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, Value, ValueType};
packr_guest::setup_guest!();

/// The grandchild: the hosted noop leaf (resolved over http by the runtime).
const CHILD: &str = "https://raw.githubusercontent.com/colinrozzi/supervisor/main/experiments/noop/manifest.toml";

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
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
    }
}
#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);
#[import(module = "theater:simple/runtime", name = "spawn")]
fn runtime_spawn_raw(manifest: String, init_state: Option<Value>, wasm_bytes: Option<Vec<u8>>) -> Value;

fn ok_unit() -> Value {
    let u = Value::Tuple(vec![]);
    Value::Result { ok_type: u.infer_type(), err_type: ValueType::String, value: Ok(Box::new(u)) }
}
fn err_result(m: &str) -> Value {
    Value::Result { ok_type: ValueType::Tuple(vec![]), err_type: ValueType::String, value: Err(Box::new(Value::String(String::from(m)))) }
}
fn case_name(v: Value) -> String {
    match v {
        Value::Variant { case_name, payload, .. } => {
            match payload.into_iter().next() {
                Some(Value::String(s)) => { let mut o = case_name; o.push_str(": "); o.push_str(&s); o }
                _ => case_name,
            }
        }
        Value::String(s) => s,
        _ => String::from("unknown"),
    }
}

#[export(name = "theater:simple/actor.init")]
fn init(_config: Value) -> Value {
    log(String::from("[deleg-parent] init — delegating: runtime.spawn grandchild (noop)"));
    match runtime_spawn_raw(String::from(CHILD), None, None) {
        Value::Result { value: Ok(inner), .. } => match *inner {
            Value::String(id) => { log(format!("[deleg-parent] DELEGATION OK — grandchild {}", id)); ok_unit() }
            _ => { log(String::from("[deleg-parent] spawn ok, odd payload")); ok_unit() }
        },
        Value::Result { value: Err(e), .. } => {
            let c = case_name(*e);
            log(format!("[deleg-parent] DELEGATION FAILED: {}", c));
            err_result(&format!("delegation failed: {}", c))
        }
        Value::Variant { tag: 0, payload, .. } => {
            log(format!("[deleg-parent] DELEGATION OK — grandchild {:?}", payload.into_iter().next()));
            ok_unit()
        }
        Value::Variant { tag: 1, payload, .. } => {
            let c = payload.into_iter().next().map(case_name).unwrap_or_else(|| String::from("unknown"));
            log(format!("[deleg-parent] DELEGATION FAILED: {}", c));
            err_result(&format!("delegation failed: {}", c))
        }
        Value::String(id) => { log(format!("[deleg-parent] DELEGATION OK — grandchild {}", id)); ok_unit() }
        _ => { log(String::from("[deleg-parent] spawn: unexpected result")); err_result("unexpected spawn result") }
    }
}
