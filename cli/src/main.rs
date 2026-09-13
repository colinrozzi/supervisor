//! `supervisor` — dev CLI for the supervisor reconciler.
//!
//! `supervisor spawn <roster.json>` generates the supervisor manifest from a roster
//! (baking in the handlers + permission grants), spawns it on an in-process theater
//! runtime, and prints two independently-toggled streams:
//!   --chain [compact|pretty]  the actors' chain (the record of every event)
//!   --logs                    theater's own runtime logs (the host's internals)
//! `--manifest <m.toml>` spawns a ready manifest raw instead.

mod client;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc;

use theater::chain::ChainEvent;
use theater::events::lifecycle::{ActorLifecycleEvent, TerminationCause};
use theater::events::wasm::WasmEventData;
use theater::events::{decode_chain_event_payload, ChainEventPayload};
use theater::messages::{default_init_state, TheaterCommand};
use theater::pack_bridge::Value;
use theater::theater_runtime::TheaterRuntime;
use theater::utils::{resolve_reference, ResourceCache};
use theater::{ManifestConfig, TheaterId};

/// supervisor.wasm embedded at build time (see build.rs). Empty if none was available
/// at build — then `--wasm` is required at runtime.
static EMBEDDED_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/supervisor.wasm"));

#[derive(Parser)]
#[command(name = "supervisor", about = "Dev CLI for the supervisor reconciler")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Spawn a roster (or a raw manifest) on an in-process theater runtime and
    /// print its chain and/or the runtime's logs.
    Spawn(SpawnArgs),
    /// List the roster of a running supervisor (via its control port).
    List {
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Show one service's spec + status.
    Status {
        handle: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Add a service to a running supervisor's desired roster (it reconciles).
    Add {
        handle: String,
        manifest: String,
        #[arg(long)]
        max: Option<u32>,
        #[arg(long)]
        window_ms: Option<u64>,
        /// Keep the child's chain in memory (queryable with `chain`).
        #[arg(long)]
        keep_chain: bool,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Remove a service (the supervisor stops it).
    Remove {
        handle: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Restart a service: stop the current incarnation, spawn a fresh one, and clear
    /// its crash-loop window (unblocks a tripped breaker).
    Restart {
        handle: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Replace the whole desired roster from a file (the supervisor diffs + reconciles:
    /// stops what's gone, spawns what's new, leaves the rest).
    Apply {
        roster: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Dump a service's in-memory chain (needs record or keep_chain).
    Chain {
        handle: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Generate an ed25519 client identity keypair. Writes the private key to <OUT>
    /// (chmod 600) and prints the public key hex — add that to the supervisor's
    /// `control.authorized_keys` to authorize this client.
    Keygen {
        /// Where to write the private key (default ~/.config/supervisor/keys/id_ed25519).
        #[arg(long)]
        out: Option<String>,
    },
    /// Update this `supervisor` binary in place from the latest GitHub release
    /// (download → checksum-verify → atomic replace).
    Upgrade {
        /// Install directory (default ~/.local/bin).
        #[arg(long)]
        install_dir: Option<String>,
    },
    /// One-time setup for an AUTHENTICATED supervisor on this box: generate a self-signed
    /// server TLS cert+key (no openssl needed), seed the authorized client keys, and emit
    /// the run command + a systemd unit.
    Bootstrap(BootstrapArgs),
}

#[derive(Parser)]
struct BootstrapArgs {
    /// Output directory for cert/key/run.sh/unit.
    #[arg(long)]
    out: String,
    /// Hostname clients connect to (cert CN + SAN).
    #[arg(long)]
    host: String,
    /// Roster JSON (the acceptor services the supervisor runs).
    #[arg(long)]
    roster: String,
    /// An authorized client ed25519 pubkey (hex). Repeatable. At least one required.
    #[arg(long = "authorized-key", value_name = "HEX", required = true)]
    authorized_key: Vec<String>,
    /// Control port. Default 9000.
    #[arg(long, default_value = "9000")]
    port: u16,
    /// Listener interface. Default 0.0.0.0 (off-box).
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// Optional IP SAN for the cert.
    #[arg(long)]
    ip: Option<String>,
    /// systemd unit name. Default "supervisor".
    #[arg(long, default_value = "supervisor")]
    unit_name: String,
    /// Path to the supervisor binary the unit runs (default: this binary).
    #[arg(long)]
    supervisor_bin: Option<String>,
}

/// How to reach a supervisor's control surface. `--profile` (authenticated TLS) takes
/// precedence; otherwise plaintext `--host`/`--port` (local dev).
#[derive(Parser)]
struct ConnArgs {
    /// Connect via this profile from ~/.config/supervisor/config (TLS + ed25519 auth).
    #[arg(long)]
    profile: Option<String>,
    /// Plaintext host (unauth, local). Default 127.0.0.1. Ignored when --profile is set.
    #[arg(long)]
    host: Option<String>,
    /// Control port (plaintext mode). Default 9000. Ignored when --profile is set.
    #[arg(long, default_value = "9000")]
    port: u16,
}
impl ConnArgs {
    fn target(&self) -> Result<client::Target> {
        client::resolve_target(self.profile.as_deref(), self.host.as_deref(), self.port)
    }
}

#[derive(Parser)]
struct SpawnArgs {
    /// Roster file — JSON `{"services":[...]}` (the supervisor's init config).
    /// The manifest is generated around it. Omit when using --manifest.
    roster: Option<String>,

    /// Spawn this ready manifest instead of generating one from a roster.
    #[arg(long, conflicts_with = "roster")]
    manifest: Option<String>,

    /// Path to the supervisor.wasm (roster mode). Defaults to the wasm embedded in
    /// this binary at build time; pass this to override (e.g. a dev rebuild).
    #[arg(long)]
    wasm: Option<String>,

    /// Sandbox root for `file` record sinks (roster mode).
    #[arg(long)]
    record_dir: Option<String>,

    /// Open the control surface for live `add`/`remove`/`list`/`status`/`chain`
    /// from another terminal. `--control-port` = 9000 (matches the client default),
    /// `--control-port N` for another port. Omit entirely to leave the surface off.
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "9000")]
    control_port: Option<u16>,

    /// Print the actors' chain. Optional mode: `--chain` = pretty, `--chain compact`.
    /// Default when neither --chain nor --logs is given: `--chain pretty`.
    #[arg(long, value_name = "MODE", num_args = 0..=1, default_missing_value = "pretty")]
    chain: Option<ChainMode>,

    /// Print theater's runtime logs (the host's internals) at this level:
    /// `--logs` = info, or `--logs error|warn|info|debug|trace`. Off by default.
    /// `RUST_LOG`, if set, overrides this (for per-crate directives).
    #[arg(long, value_name = "LEVEL", num_args = 0..=1, default_missing_value = "info")]
    logs: Option<LogLevel>,

    /// Print the generated manifest to stderr before spawning (roster mode).
    #[arg(long)]
    show_manifest: bool,

    /// Server TLS certificate (PEM) for an AUTHENTICATED control surface. Requires
    /// --tls-key and at least one --authorized-key. Hand this cert to clients to pin.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<String>,
    /// Server TLS private key (PEM). Pairs with --tls-cert.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<String>,
    /// An ed25519 client pubkey (hex) allowed to drive the control surface. Repeatable.
    /// When present, the control surface requires TLS + ed25519 auth.
    #[arg(long = "authorized-key", value_name = "HEX")]
    authorized_key: Vec<String>,
    /// Interface for the control listener. Default 127.0.0.1; set 0.0.0.0 for off-box
    /// (only meaningful with --authorized-key).
    #[arg(long)]
    control_bind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ChainMode {
    /// One terse line per event (type only) — a skim.
    Compact,
    /// Decoded, with messages + call args.
    Pretty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}
impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[tokio::main]
async fn main() {
    // Install the process-level rustls CryptoProvider (ring) once, up front. Both our
    // control client AND the embedded theater tcp handler's server-side TLS resolve the
    // provider from this global; without it, `upgrade-to-tls-server` panics.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let res = match &cli.cmd {
        Cmd::Spawn(args) => {
            // Runtime logs (theater's tracing): RUST_LOG wins; else the --logs level; else off.
            let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
                args.logs.map(|l| l.as_str().to_string()).unwrap_or_else(|| "off".into())
            });
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
                .with_writer(std::io::stderr)
                .init();
            spawn(args).await
        }
        Cmd::List { conn } => control_print(conn, &json_op("list", &[])),
        Cmd::Status { handle, conn } => control_print(conn, &json_op("status", &[("handle", handle)])),
        Cmd::Remove { handle, conn } => control_print(conn, &json_op("remove", &[("handle", handle)])),
        Cmd::Restart { handle, conn } => control_print(conn, &json_op("restart", &[("handle", handle)])),
        Cmd::Chain { handle, conn } => control_print(conn, &json_op("chain", &[("handle", handle)])),
        Cmd::Add { handle, manifest, max, window_ms, keep_chain, conn } => {
            control_print(conn, &add_op(handle, manifest, *max, *window_ms, *keep_chain))
        }
        Cmd::Apply { roster, conn } => apply_op(roster).and_then(|op| control_print(conn, &op)),
        Cmd::Keygen { out } => keygen(out.as_deref()),
        Cmd::Upgrade { install_dir } => upgrade(install_dir.as_deref()),
        Cmd::Bootstrap(args) => bootstrap(args),
    };
    if let Err(e) = res {
        eprintln!("supervisor: {e:#}");
        std::process::exit(1);
    }
}

/// Resolve the connection target, send one JSON op, and print the reply.
fn control_print(conn: &ConnArgs, op_json: &str) -> Result<()> {
    let target = conn.target()?;
    let reply = client::control_send(&target, op_json)?;
    println!("{reply}");
    Ok(())
}

/// Generate + persist a client identity keypair; print the pubkey to authorize.
fn keygen(out: Option<&str>) -> Result<()> {
    let path = match out {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/supervisor/keys/id_ed25519")
        }
    };
    if path.exists() {
        return Err(anyhow!("{} already exists — refusing to overwrite", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let (seed_hex, pubkey_hex) = client::generate_identity();
    std::fs::write(&path, &seed_hex).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!(
                "supervisor: WARNING: could not chmod 600 {} ({e}) — the private key may be world-readable; fix its permissions manually",
                path.display()
            );
        }
    }
    println!("private key written to {}", path.display());
    println!("public key (add to control.authorized_keys):\n{pubkey_hex}");
    Ok(())
}

/// Self-update this binary (+ the sidecar wasm) from the latest GitHub release.
fn upgrade(install_dir: Option<&str>) -> Result<()> {
    let dir = match install_dir {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".local/bin")
        }
    };
    let cfg = fleet_release::UpgradeConfig::new(
        "colinrozzi/supervisor",
        "supervisor",
        &["supervisor", "supervisor.wasm"],
        dir,
    );
    fleet_release::upgrade(&cfg)?;
    Ok(())
}

/// Generate a self-signed cert (host + optional IP SAN); returns (cert_pem, key_pem).
fn gen_self_signed(host: &str, ip: Option<&str>) -> Result<(String, String)> {
    let mut sans = vec![host.to_string()];
    if let Some(ip) = ip {
        sans.push(ip.to_string());
    }
    let ck = rcgen::generate_simple_self_signed(sans).context("generating self-signed cert")?;
    Ok((ck.cert.pem(), ck.key_pair.serialize_pem()))
}

/// One-time authenticated-supervisor setup: cert + authorized keys + run script + unit.
fn bootstrap(a: &BootstrapArgs) -> Result<()> {
    let out = std::path::PathBuf::from(&a.out);
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    // Sanity-check the roster is JSON before we commit to anything.
    let roster_txt = std::fs::read_to_string(&a.roster)
        .with_context(|| format!("reading roster {}", a.roster))?;
    let _: serde_json::Value =
        serde_json::from_str(&roster_txt).context("roster is not valid JSON")?;
    let roster_abs = std::fs::canonicalize(&a.roster)?;

    // Self-signed server cert+key (native — no openssl on the box).
    let cert_path = out.join("server-cert.pem");
    let key_path = out.join("server-key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let (cert_pem, key_pem) = gen_self_signed(&a.host, a.ip.as_deref())?;
        std::fs::write(&cert_path, cert_pem)?;
        std::fs::write(&key_path, key_pem)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)) {
                eprintln!("supervisor: WARNING: could not chmod 600 {} ({e})", key_path.display());
            }
        }
        println!("generated server cert: {} (distribute to clients to pin)", cert_path.display());
        println!("generated server key : {} (chmod 600; keep on this box)", key_path.display());
    } else {
        println!("reusing existing cert/key in {} (delete to regenerate)", out.display());
    }

    let sup = a.supervisor_bin.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| "supervisor".into())
    });

    // run.sh — the exact authenticated spawn invocation.
    let key_flags: String = a
        .authorized_key
        .iter()
        .map(|k| format!("--authorized-key {k} "))
        .collect();
    let run_path = out.join("run.sh");
    let run = format!(
        "#!/usr/bin/env bash\n# Runs the authenticated supervisor. Generated by `supervisor bootstrap`.\nexec \"{sup}\" spawn \"{roster}\" \\\n  --control-port {port} --control-bind {bind} \\\n  --tls-cert \"{cert}\" --tls-key \"{key}\" \\\n  {keys}\\\n  --logs warn\n",
        sup = sup,
        roster = roster_abs.display(),
        port = a.port,
        bind = a.bind,
        cert = cert_path.display(),
        key = key_path.display(),
        keys = key_flags,
    );
    std::fs::write(&run_path, run)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&run_path, std::fs::Permissions::from_mode(0o755));
    }
    println!("wrote run script: {}", run_path.display());

    // systemd unit.
    let unit_path = out.join(format!("{}.service", a.unit_name));
    let unit = format!(
        "[Unit]\nDescription=supervisor ({unit}) — authenticated reconciler + host for the supervised tree\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={run}\nRestart=always\nRestartSec=2\n# systemd stop/restart sends SIGTERM → the supervisor hard-kills the tree, then respawns\n# on restart (correct for wedge remediation; not a graceful drain — supervisor task #15).\n# Point any liveness watchdog at THIS unit.\n\n[Install]\nWantedBy=multi-user.target\n",
        unit = a.unit_name,
        run = run_path.display(),
    );
    std::fs::write(&unit_path, unit)?;
    println!("wrote systemd unit: {}", unit_path.display());

    println!("\nnext:");
    println!("  1. install:   cp {} /etc/systemd/system/ && systemctl daemon-reload", unit_path.display());
    println!("  2. start:     systemctl enable --now {}", a.unit_name);
    println!("  3. distribute {} to each authorized client; they set server_cert=<that path>,", cert_path.display());
    println!("     host={}, port={}, identity=<their `supervisor keygen` key> in ~/.config/supervisor/config", a.host, a.port);
    println!("  4. verify:    supervisor list --profile <name>");
    println!("\nauthorized keys seeded ({}):", a.authorized_key.len());
    for k in &a.authorized_key {
        println!("  - {k}");
    }
    Ok(())
}

/// Build a flat `{"op":…, k:v,…}` request (string values).
fn json_op(op: &str, fields: &[(&str, &str)]) -> String {
    let mut s = format!("{{\"op\":\"{}\"", op);
    for (k, v) in fields {
        s.push_str(&format!(",\"{}\":\"{}\"", k, json_escape(v)));
    }
    s.push('}');
    s
}

/// Build an `apply` request from a roster file (a `{"services":[…]}` object, or a bare
/// `[…]` array of service entries).
fn apply_op(roster_path: &str) -> Result<String> {
    let text = std::fs::read_to_string(roster_path)
        .with_context(|| format!("reading roster {roster_path}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).context("roster is not valid JSON")?;
    let services = match v {
        serde_json::Value::Object(mut o) => o
            .remove("services")
            .ok_or_else(|| anyhow!("roster object has no \"services\" array"))?,
        arr @ serde_json::Value::Array(_) => arr,
        _ => return Err(anyhow!("roster must be a {{\"services\":[…]}} object or a [...] array")),
    };
    Ok(serde_json::json!({ "op": "apply", "services": services }).to_string())
}

/// Build an `add` request with a nested service object.
fn add_op(handle: &str, manifest: &str, max: Option<u32>, window_ms: Option<u64>, keep_chain: bool) -> String {
    let mut svc = format!("{{\"handle\":\"{}\",\"manifest\":\"{}\"", json_escape(handle), json_escape(manifest));
    if let Some(m) = max {
        svc.push_str(&format!(",\"max\":{}", m));
    }
    if let Some(w) = window_ms {
        svc.push_str(&format!(",\"window_ms\":{}", w));
    }
    if keep_chain {
        svc.push_str(",\"keep_chain\":true");
    }
    svc.push('}');
    format!("{{\"op\":\"add\",\"service\":{}}}", svc)
}

fn load_manifest(args: &SpawnArgs) -> Result<(ManifestConfig, std::path::PathBuf)> {
    if let Some(path) = &args.manifest {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading manifest {path}"))?;
        let manifest = ManifestConfig::from_toml_str(&content)
            .map_err(|e| anyhow!("parsing manifest {path}: {e}"))?;
        let dir = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        Ok((manifest, dir))
    } else {
        let roster_path = args
            .roster
            .as_deref()
            .ok_or_else(|| anyhow!("give a roster file, or --manifest <path>"))?;
        let roster = std::fs::read_to_string(roster_path)
            .with_context(|| format!("reading roster {roster_path}"))?;
        let tls = match (&args.tls_cert, &args.tls_key) {
            (Some(c), Some(k)) => Some((c.as_str(), k.as_str())),
            _ => None,
        };
        let toml = generate_manifest(GenManifest {
            roster: &roster,
            wasm: args.wasm.as_deref(),
            record_dir: args.record_dir.as_deref(),
            control_port: args.control_port,
            control_bind: args.control_bind.as_deref(),
            authorized_keys: &args.authorized_key,
            tls,
        })?;
        if args.show_manifest {
            eprintln!("--- generated manifest ---\n{toml}\n--------------------------");
        }
        let manifest = ManifestConfig::from_toml_str(&toml)
            .map_err(|e| anyhow!("parsing generated manifest: {e}"))?;
        Ok((manifest, std::path::PathBuf::from(".")))
    }
}

/// Args for `generate_manifest` — the roster + the handler/permission knobs.
struct GenManifest<'a> {
    roster: &'a str,
    wasm: Option<&'a str>,
    record_dir: Option<&'a str>,
    control_port: Option<u16>,
    control_bind: Option<&'a str>,
    authorized_keys: &'a [String],
    /// (cert PEM path, key PEM path) → enables server TLS on the tcp handler.
    tls: Option<(&'a str, &'a str)>,
}

/// Build a supervisor manifest TOML around a roster: package = supervisor.wasm,
/// initial_state = the (compacted) roster, and the handlers + permission grants the
/// supervisor needs — http-client / filesystem only when a service records to them,
/// server TLS + authorized_keys when the control surface is authenticated.
fn generate_manifest(g: GenManifest<'_>) -> Result<String> {
    let GenManifest { roster, wasm, record_dir, control_port, control_bind, authorized_keys, tls } = g;
    if !authorized_keys.is_empty() && control_port.is_none() {
        return Err(anyhow!("--authorized-key needs the control surface — pass --control-port"));
    }
    if !authorized_keys.is_empty() && tls.is_none() {
        return Err(anyhow!("--authorized-key requires TLS — pass --tls-cert and --tls-key"));
    }
    let mut parsed: serde_json::Value =
        serde_json::from_str(roster).context("roster is not valid JSON")?;
    // Inject the control config into the roster's init state (opt-in): port + optional
    // bind + optional authorized_keys (which flips the surface to TLS + ed25519 auth).
    if let Some(p) = control_port {
        if let Some(obj) = parsed.as_object_mut() {
            let mut control = serde_json::json!({ "port": p });
            if let Some(bind) = control_bind {
                control["bind"] = serde_json::json!(bind);
            }
            if !authorized_keys.is_empty() {
                control["authorized_keys"] = serde_json::json!(authorized_keys);
            }
            obj.insert("control".into(), control);
        }
    }
    let mut http_hosts: Vec<String> = Vec::new();
    let mut needs_file = false;
    if let Some(services) = parsed.get("services").and_then(|s| s.as_array()) {
        for svc in services {
            if let Some(rec) = svc.get("record") {
                match rec.get("kind").and_then(|k| k.as_str()) {
                    Some("http") => {
                        if let Some(h) = rec.get("url").and_then(|u| u.as_str()).and_then(url_host) {
                            if !http_hosts.contains(&h) {
                                http_hosts.push(h);
                            }
                        }
                    }
                    Some("file") => needs_file = true,
                    _ => {}
                }
            }
        }
    }

    // package path: canonicalized --wasm if given, else a marker (the embedded wasm
    // bytes are passed to spawn directly, so this field isn't used to load the wasm).
    let package = match wasm {
        Some(w) => std::fs::canonicalize(w)
            .with_context(|| format!("supervisor wasm not found at {w}"))?
            .display()
            .to_string(),
        None => "supervisor.wasm".to_string(),
    };

    // Compact one-line JSON → TOML literal string (single quotes; double-quotes need no escape).
    let init_state = serde_json::to_string(&parsed).context("re-serializing roster")?;
    if init_state.contains('\'') {
        return Err(anyhow!("roster contains a single quote in a value; not supported yet"));
    }

    let mut m = String::new();
    m.push_str("name = \"supervisor\"\n");
    m.push_str("version = \"0.0.1\"\n");
    m.push_str(&format!("package = \"{}\"\n", package));
    m.push_str(&format!("initial_state = '{init_state}'\n\n"));

    m.push_str("[permission_policy.runtime]\n");
    m.push_str("type = \"restrict\"\n");
    m.push_str("config = { inspect = true, mutate = true }\n\n");

    if needs_file {
        m.push_str("[permission_policy.file_system]\n");
        m.push_str("type = \"restrict\"\n");
        m.push_str("config = { read = true, write = true, execute = false }\n\n");
    }

    for h in ["self", "runtime", "lifecycle", "timer"] {
        m.push_str(&format!("[[handler]]\ntype = \"{h}\"\n\n"));
    }
    if control_port.is_some() {
        m.push_str("[[handler]]\ntype = \"tcp\"\n");
        if let Some((cert, key)) = tls {
            let cert_abs = std::fs::canonicalize(cert)
                .with_context(|| format!("--tls-cert {cert} not found"))?;
            let key_abs = std::fs::canonicalize(key)
                .with_context(|| format!("--tls-key {key} not found"))?;
            m.push_str(&format!(
                "server_tls = {{ enabled = true, cert = \"{}\", key = \"{}\" }}\n",
                cert_abs.display(),
                key_abs.display()
            ));
        }
        m.push('\n');
    }
    if !http_hosts.is_empty() {
        let hosts = http_hosts.iter().map(|h| format!("\"{h}\"")).collect::<Vec<_>>().join(", ");
        m.push_str(&format!("[[handler]]\ntype = \"http-client\"\nallowed_hosts = [{hosts}]\n\n"));
    }
    if needs_file {
        let dir = record_dir
            .ok_or_else(|| anyhow!("a service records to a file — pass --record-dir <sandbox root>"))?;
        let dir_abs = std::fs::canonicalize(dir)
            .with_context(|| format!("--record-dir {dir} does not exist (create it first)"))?;
        m.push_str(&format!("[[handler]]\ntype = \"filesystem\"\npath = \"{}\"\n\n", dir_abs.display()));
    }

    Ok(m)
}

fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.split(':').next().unwrap_or(authority);
    (!host.is_empty()).then(|| host.to_string())
}

async fn spawn(args: &SpawnArgs) -> Result<()> {
    // Default: bare command streams the pretty chain; --logs alone means logs-only.
    let chain_mode = match (args.chain, args.logs.is_some()) {
        (Some(m), _) => Some(m),
        (None, true) => None,
        (None, false) => Some(ChainMode::Pretty),
    };

    let (manifest, manifest_dir) = load_manifest(args)?;

    // --- in-process theater runtime (mirrors theater-cli's spawn bring-up) ---
    let (theater_tx, theater_rx) = mpsc::unbounded_channel::<TheaterCommand>();
    let resource_cache = Arc::new(ResourceCache::new());
    let handler_registry = theater_stage::standard_handlers(
        theater_tx.clone(),
        &theater_stage::StandardHandlers {
            // actor self.log lines are chain events; we render them in --chain pretty,
            // so keep the handler from also printing them (no double output).
            show_actor_logs: false,
            resource_cache: resource_cache.clone(),
        },
    );
    let mut runtime = TheaterRuntime::new(
        theater_tx.clone(),
        theater_rx,
        handler_registry,
        resource_cache.clone(),
        theater_native::TokioSpawn,
    )
    .await
    .map_err(|e| anyhow!("creating runtime: {e}"))?;

    let (global_tx, mut global_rx) = mpsc::channel(256);
    runtime.add_global_subscription(global_tx);
    let runtime_handle = tokio::spawn(async move {
        if let Err(e) = runtime.run().await {
            eprintln!("supervisor: runtime error: {e}");
        }
    });

    // --- load the wasm bytes ---
    // roster mode with no --wasm → the wasm embedded at build time. Otherwise resolve
    // from the manifest's package path (raw --manifest, or an explicit --wasm).
    let wasm_bytes = if args.manifest.is_none() && args.wasm.is_none() {
        if EMBEDDED_WASM.is_empty() {
            return Err(anyhow!("this build has no embedded supervisor.wasm — pass --wasm <path>"));
        }
        EMBEDDED_WASM.to_vec()
    } else {
        let wasm_path = if manifest.package.starts_with('/') || manifest.package.contains("://") {
            manifest.package.clone()
        } else {
            manifest_dir.join(&manifest.package).to_string_lossy().to_string()
        };
        resolve_reference(&wasm_path)
            .await
            .map_err(|e| anyhow!("loading wasm from {wasm_path}: {e}"))?
    };

    let init_state = match manifest.initial_state.as_ref() {
        Some(s) => Value::String(s.clone()),
        None => default_init_state(),
    };
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    theater_tx
        .send(TheaterCommand::SpawnActor {
            wasm_bytes,
            name: Some(manifest.name.clone()),
            manifest: Some(manifest),
            init_state,
            response_tx,
            subscription_tx: None,
            parent_id: None,
        })
        .map_err(|e| anyhow!("sending spawn: {e}"))?;

    let root_id = match response_rx.await {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => return Err(anyhow!("actor failed to start: {e}")),
        Err(e) => return Err(anyhow!("spawn response dropped: {e}")),
    };
    eprintln!("supervisor: spawned root {root_id} (Ctrl+C to stop)\n");

    loop {
        tokio::select! {
            event = global_rx.recv() => {
                let Some((actor_id, ev)) = event else { break };
                match chain_mode {
                    Some(ChainMode::Pretty) => println!("{}", pretty_line(&ev, &actor_id)),
                    Some(ChainMode::Compact) => println!("{}", compact_line(&ev, &actor_id)),
                    None => {}
                }
                if actor_id == root_id && ev.event_type == "terminated" {
                    eprintln!("\nsupervisor: root {root_id} terminated — exiting");
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nsupervisor: stopping root {root_id} …");
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
                let _ = theater_tx.send(TheaterCommand::StopActor { actor_id: root_id, response_tx: stop_tx });
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stop_rx).await;
                break;
            }
        }
    }

    drop(theater_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), runtime_handle).await;
    Ok(())
}

// ---- chain rendering ----

fn sid(actor_id: &TheaterId) -> String {
    let s = actor_id.to_string();
    s[..8.min(s.len())].to_string()
}

/// Decoded, readable line: narration (`» msg`), host calls with args, lifecycle, wasm.
fn pretty_line(ev: &ChainEvent, actor_id: &TheaterId) -> String {
    let body = match decode_chain_event_payload(&ev.data) {
        Some(ChainEventPayload::Lifecycle(l)) => lifecycle_str(&l),
        Some(ChainEventPayload::HostFunction(h)) => {
            if h.interface.ends_with("/self") && h.function == "log" {
                format!("» {}", val_str(&h.input))
            } else {
                let arg = truncate(val_str(&h.input), 60);
                if arg.is_empty() {
                    format!("→ {}/{}", short_iface(&h.interface), h.function)
                } else {
                    format!("→ {}/{}({})", short_iface(&h.interface), h.function, arg)
                }
            }
        }
        Some(ChainEventPayload::Wasm(w)) => match w {
            WasmEventData::WasmCall { function_name, .. } => format!("⚙ call {function_name}"),
            WasmEventData::WasmResult { function_name, .. } => format!("⚙ result {function_name}"),
            WasmEventData::WasmError { function_name, message } => {
                format!("✖ wasm error in {function_name}: {}", truncate(message, 80))
            }
            _ => format!("⚙ {}", ev.event_type),
        },
        Some(ChainEventPayload::ReplaySummary(_)) => "↻ replay-summary".to_string(),
        None => ev.event_type.clone(),
    };
    format!("[{}] {}", sid(actor_id), body)
}

/// Terse skim: type only, no arg decoding.
fn compact_line(ev: &ChainEvent, actor_id: &TheaterId) -> String {
    let body = match decode_chain_event_payload(&ev.data) {
        Some(ChainEventPayload::Lifecycle(l)) => lifecycle_str(&l),
        Some(ChainEventPayload::HostFunction(h)) => {
            format!("{}/{}", short_iface(&h.interface), h.function)
        }
        Some(ChainEventPayload::Wasm(w)) => match w {
            WasmEventData::WasmCall { function_name, .. } => format!("wasm:call {function_name}"),
            WasmEventData::WasmResult { function_name, .. } => format!("wasm:result {function_name}"),
            WasmEventData::WasmError { function_name, .. } => format!("wasm:error {function_name}"),
            _ => ev.event_type.clone(),
        },
        Some(ChainEventPayload::ReplaySummary(_)) => "replay-summary".to_string(),
        None => ev.event_type.clone(),
    };
    format!("[{}] {}", sid(actor_id), body)
}

fn lifecycle_str(l: &ActorLifecycleEvent) -> String {
    match l {
        ActorLifecycleEvent::Spawned => "● spawned".to_string(),
        ActorLifecycleEvent::Paused => "⏸ paused".to_string(),
        ActorLifecycleEvent::Resumed => "▶ resumed".to_string(),
        ActorLifecycleEvent::Terminated { cause } => format!("✖ terminated ({})", cause_str(cause)),
    }
}

fn cause_str(c: &TerminationCause) -> String {
    match c {
        TerminationCause::Completed { .. } => "Completed".to_string(),
        TerminationCause::Failed { error } => format!("Failed: {}", truncate(error.clone(), 80)),
        TerminationCause::Stopped => "Stopped".to_string(),
        TerminationCause::Killed => "Killed".to_string(),
        TerminationCause::PeerKilled { peer } => {
            format!("PeerKilled by {}", &peer[..8.min(peer.len())])
        }
    }
}

/// Pull a readable string out of a packr Value (for log messages + call args).
fn val_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Tuple(items) if items.len() == 1 => val_str(&items[0]),
        Value::Option { value: Some(b), .. } => val_str(b),
        Value::Option { value: None, .. } => String::new(),
        other => format!("{other}"),
    }
}

fn truncate(s: String, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s
    }
}

fn short_iface(i: &str) -> String {
    i.strip_prefix("theater:simple/").unwrap_or(i).to_string()
}

/// Minimal JSON string escaping for the request fields the client builds.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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
