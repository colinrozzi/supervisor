//! crash-child — inits OK (so spawn returns a live id), arms a fast timer, then
//! `panic!()`s on the first tick. Since packr-guest 0.24.1 the panic handler TRAPS
//! (wasm unreachable) instead of looping, so the trap surfaces as
//! TerminationCause::Failed → the supervisor's monitor fires → reconcile respawns it
//! (rate-limited). The roster child for the reconcile loop — the real Failed path.
#![no_std]
extern crate alloc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use packr_guest::{export, import, pack_types, Value, ValueType};
packr_guest::setup_guest!();

pack_types! {
    imports {
        theater:simple/self {
            log: func(msg: string),
        }
        theater:simple/timer { set-interval: func(name: string, interval-ms: u64) -> result<string, string>, }
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:simple/timer.handle-tick: func(name: string) -> result<_, string>,
    }
}
#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);
#[import(module = "theater:simple/timer", name = "set-interval")]
fn set_interval(name: String, interval_ms: u64) -> Result<String, String>;

fn ok_unit() -> Value {
    let unit = Value::Tuple(vec![]);
    Value::Result { ok_type: unit.infer_type(), err_type: ValueType::String, value: Ok(Box::new(unit)) }
}

#[export(name = "theater:simple/actor.init")]
fn init(_config: Value) -> Value {
    log(String::from("[crash-child] init — arming self-terminate timer (300ms)"));
    let _ = set_interval(String::from("boom"), 300);
    ok_unit()
}
#[export(name = "theater:simple/timer.handle-tick")]
fn handle_tick(_input: Value) -> Value {
    log(String::from("[crash-child] tick — crashing on purpose"));
    // packr-guest 0.24.1's panic handler traps (wasm unreachable) instead of looping,
    // so this surfaces as TerminationCause::Failed → the supervisor respawns it.
    panic!("crash-child: deliberate crash");
}
