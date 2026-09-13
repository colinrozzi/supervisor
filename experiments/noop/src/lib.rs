//! noop — a trivial, harmless, long-lived actor. Inits OK and then just sits idle
//! (no timer, no crash, no children). A reusable fleet TEST FIXTURE: `supervisor apply`
//! it to prove spawn/reconcile off-box, then `supervisor remove` it — without touching
//! any real service. The reconcile-proof actor for a control-plane stand-up.
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
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
    }
}
#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);

fn ok_unit() -> Value {
    let unit = Value::Tuple(vec![]);
    Value::Result { ok_type: unit.infer_type(), err_type: ValueType::String, value: Ok(Box::new(unit)) }
}

#[export(name = "theater:simple/actor.init")]
fn init(_config: Value) -> Value {
    log(String::from("[noop] up — idle test fixture (spawn/reconcile proof)"));
    ok_unit()
}
