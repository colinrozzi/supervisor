//! `supervisor` — dev CLI for the supervisor reconciler.
//!
//! `supervisor spawn <roster.json>` generates the supervisor manifest from a roster
//! (baking in the handlers + permission grants), spawns it on an in-process theater
//! runtime, and prints two independently-toggled streams:
//!   --chain [compact|pretty]  the actors' chain (the record of every event)
//!   --logs                    theater's own runtime logs (the host's internals)
//! `--manifest <m.toml>` spawns a ready manifest raw instead.

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
}

#[derive(Parser)]
struct SpawnArgs {
    /// Roster file — JSON `{"services":[...]}` (the supervisor's init config).
    /// The manifest is generated around it. Omit when using --manifest.
    roster: Option<String>,

    /// Spawn this ready manifest instead of generating one from a roster.
    #[arg(long, conflicts_with = "roster")]
    manifest: Option<String>,

    /// Path to the built supervisor.wasm (roster mode).
    #[arg(long, default_value = "target/wasm32-unknown-unknown/release/supervisor.wasm")]
    wasm: String,

    /// Sandbox root for `file` record sinks (roster mode).
    #[arg(long)]
    record_dir: Option<String>,

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
    let cli = Cli::parse();
    let Cmd::Spawn(args) = &cli.cmd;

    // Runtime logs (theater's tracing): RUST_LOG wins (per-crate directives); else the
    // --logs level; else off.
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| args.logs.map(|l| l.as_str().to_string()).unwrap_or_else(|| "off".into()));
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = spawn(&cli.cmd).await {
        eprintln!("supervisor: {e:#}");
        std::process::exit(1);
    }
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
        let toml = generate_manifest(&roster, &args.wasm, args.record_dir.as_deref())?;
        if args.show_manifest {
            eprintln!("--- generated manifest ---\n{toml}\n--------------------------");
        }
        let manifest = ManifestConfig::from_toml_str(&toml)
            .map_err(|e| anyhow!("parsing generated manifest: {e}"))?;
        Ok((manifest, std::path::PathBuf::from(".")))
    }
}

/// Build a supervisor manifest TOML around a roster: package = supervisor.wasm,
/// initial_state = the (compacted) roster, and the handlers + permission grants the
/// supervisor needs — http-client / filesystem only when a service records to them.
fn generate_manifest(roster: &str, wasm: &str, record_dir: Option<&str>) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(roster).context("roster is not valid JSON")?;
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

    let wasm_abs = std::fs::canonicalize(wasm)
        .with_context(|| format!("supervisor wasm not found at {wasm} (build it, or pass --wasm)"))?;

    // Compact one-line JSON → TOML literal string (single quotes; double-quotes need no escape).
    let init_state = serde_json::to_string(&parsed).context("re-serializing roster")?;
    if init_state.contains('\'') {
        return Err(anyhow!("roster contains a single quote in a value; not supported yet"));
    }

    let mut m = String::new();
    m.push_str("name = \"supervisor\"\n");
    m.push_str("version = \"0.0.1\"\n");
    m.push_str(&format!("package = \"{}\"\n", wasm_abs.display()));
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

async fn spawn(cmd: &Cmd) -> Result<()> {
    let Cmd::Spawn(args) = cmd;

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

    // --- resolve + load wasm, then spawn (setup + auto-init) ---
    let wasm_path = if manifest.package.starts_with('/') || manifest.package.contains("://") {
        manifest.package.clone()
    } else {
        manifest_dir.join(&manifest.package).to_string_lossy().to_string()
    };
    let wasm_bytes = resolve_reference(&wasm_path)
        .await
        .map_err(|e| anyhow!("loading wasm from {wasm_path}: {e}"))?;

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
