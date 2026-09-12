// Embed supervisor.wasm into the CLI so the released binary is self-contained.
// Source, in order: $SUPERVISOR_WASM, then the repo's built wasm. If neither exists
// (a bare `cargo build` with no wasm around) an empty placeholder is embedded and the
// CLI asks for --wasm at runtime.
use std::{env, fs, path::Path};

fn main() {
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("supervisor.wasm");
    let candidates = [
        env::var("SUPERVISOR_WASM").ok(),
        Some("../target/wasm32-unknown-unknown/release/supervisor.wasm".to_string()),
    ];
    let src = candidates.into_iter().flatten().find(|p| Path::new(p).exists());
    match src {
        Some(p) => {
            fs::copy(&p, &out).expect("copy supervisor.wasm into OUT_DIR");
            println!("cargo:rerun-if-changed={p}");
        }
        None => {
            fs::write(&out, []).expect("write empty wasm placeholder");
        }
    }
    println!("cargo:rerun-if-env-changed=SUPERVISOR_WASM");
}
