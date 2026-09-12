//! `supervisor` — dev CLI for the supervisor reconciler.
//!
//! `supervisor spawn <roster.json>` generates the supervisor manifest from a roster
//! (baking in the handlers + permission grants), spawns it on an in-process theater
//! runtime, and streams the decoded chain to stdout. `--manifest <m.toml>` spawns a
//! ready manifest raw instead. This is the dev loop: edit the roster, `supervisor
//! spawn`, watch the whole supervised tree's chain live — no `theater` binary, no
//! hand-written manifest boilerplate.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc;

use theater::chain::ChainEvent;
use theater::events::lifecycle::{ActorLifecycleEvent, TerminationCause};
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
    /// stream its decoded chain to stdout.
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

    /// Sandbox root for `file` record sinks (roster mode). Required if any
    /// service records to a file.
    #[arg(long)]
    record_dir: Option<String>,

    /// How to render chain events.
    #[arg(long, value_enum, default_value = "pretty")]
    format: Format,

    /// Print the generated manifest to stderr before spawning (roster mode).
    #[arg(long)]
    show_manifest: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    /// One decoded, human-readable line per event.
    Pretty,
    /// theater's raw short format.
    Short,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Spawn(args) => spawn(args).await,
    };
    if let Err(e) = res {
        eprintln!("supervisor: {e:#}");
        std::process::exit(1);
    }
}

/// Resolve the manifest to spawn: a raw `--manifest`, or one generated from a roster.
/// Returns the parsed manifest and the directory its `package` path resolves against.
fn load_manifest(args: &SpawnArgs) -> Result<(ManifestConfig, std::path::PathBuf)> {
    if let Some(path) = &args.manifest {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {path}"))?;
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
/// initial_state = the roster JSON, and the handlers + permission grants the
/// supervisor needs — http-client / filesystem only when a service records to them.
fn generate_manifest(roster: &str, wasm: &str, record_dir: Option<&str>) -> Result<String> {
    // Validate the roster is JSON and discover which record sinks are in play.
    let parsed: serde_json::Value =
        serde_json::from_str(roster).context("roster is not valid JSON")?;
    let mut http_hosts: Vec<String> = Vec::new();
    let mut needs_file = false;
    if let Some(services) = parsed.get("services").and_then(|s| s.as_array()) {
        for svc in services {
            if let Some(rec) = svc.get("record") {
                match rec.get("kind").and_then(|k| k.as_str()) {
                    Some("http") => {
                        if let Some(url) = rec.get("url").and_then(|u| u.as_str()) {
                            if let Some(h) = url_host(url) {
                                if !http_hosts.contains(&h) {
                                    http_hosts.push(h);
                                }
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

    // The roster JSON goes in verbatim as initial_state; single-quote it (TOML
    // literal string) so its double-quotes don't need escaping. Reject the rare
    // roster that contains a single quote rather than mangle it.
    if roster.contains('\'') {
        return Err(anyhow!("roster contains a single quote; not supported yet"));
    }
    let init_state = roster.trim();

    let mut m = String::new();
    m.push_str("name = \"supervisor\"\n");
    m.push_str("version = \"0.0.1\"\n");
    m.push_str(&format!("package = \"{}\"\n", wasm_abs.display()));
    m.push_str(&format!("initial_state = '{init_state}'\n\n"));

    // runtime CONTROL cap defaults to Disallow — must be granted to spawn/monitor.
    m.push_str("[permission_policy.runtime]\n");
    m.push_str("type = \"restrict\"\n");
    m.push_str("config = { inspect = true, mutate = true }\n\n");

    if needs_file {
        // read+write, no allowed_paths restriction (the sandbox root is the boundary).
        m.push_str("[permission_policy.file_system]\n");
        m.push_str("type = \"restrict\"\n");
        m.push_str("config = { read = true, write = true, execute = false }\n\n");
    }

    for h in ["self", "runtime", "lifecycle", "timer"] {
        m.push_str(&format!("[[handler]]\ntype = \"{h}\"\n\n"));
    }
    if !http_hosts.is_empty() {
        let hosts = http_hosts
            .iter()
            .map(|h| format!("\"{h}\""))
            .collect::<Vec<_>>()
            .join(", ");
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

/// Extract the host from an http(s) URL without pulling in a url crate.
fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.split(':').next().unwrap_or(authority); // drop :port
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

async fn spawn(args: SpawnArgs) -> Result<()> {
    let (manifest, manifest_dir) = load_manifest(&args)?;

    // --- in-process theater runtime (mirrors theater-cli's spawn bring-up) ---
    let (theater_tx, theater_rx) = mpsc::unbounded_channel::<TheaterCommand>();
    let resource_cache = Arc::new(ResourceCache::new());
    let handler_registry = theater_stage::standard_handlers(
        theater_tx.clone(),
        &theater_stage::StandardHandlers {
            show_actor_logs: true, // actor self.log lines print directly — we want them
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

    // --- resolve + load the wasm, then spawn (setup + auto-init) ---
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
    eprintln!("supervisor: spawned root {root_id} — streaming chain (Ctrl+C to stop)\n");

    // --- stream events until the root terminates or Ctrl+C ---
    loop {
        tokio::select! {
            event = global_rx.recv() => {
                let Some((actor_id, ev)) = event else { break };
                match args.format {
                    Format::Short => print!("{}", short_line(&ev, &actor_id)),
                    Format::Pretty => println!("{}", pretty_line(&ev, &actor_id)),
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

fn short_line(ev: &ChainEvent, actor_id: &TheaterId) -> String {
    let id = actor_id.to_string();
    format!("[{}] {}\n", &id[..8.min(id.len())], ev)
}

/// One decoded, readable line per event: `[id] <symbol> <summary>`.
fn pretty_line(ev: &ChainEvent, actor_id: &TheaterId) -> String {
    let id = actor_id.to_string();
    let sid = &id[..8.min(id.len())];
    let summary = match decode_chain_event_payload(&ev.data) {
        Some(ChainEventPayload::Lifecycle(ActorLifecycleEvent::Spawned)) => "● spawned".to_string(),
        Some(ChainEventPayload::Lifecycle(ActorLifecycleEvent::Paused)) => "⏸ paused".to_string(),
        Some(ChainEventPayload::Lifecycle(ActorLifecycleEvent::Resumed)) => "▶ resumed".to_string(),
        Some(ChainEventPayload::Lifecycle(ActorLifecycleEvent::Terminated { cause })) => {
            format!("✖ terminated ({})", cause_str(&cause))
        }
        Some(ChainEventPayload::HostFunction(_)) => format!("→ {}", ev.event_type),
        Some(ChainEventPayload::Wasm(_)) => format!("⚙ {}", ev.event_type),
        Some(ChainEventPayload::ReplaySummary(_)) => format!("↻ {}", ev.event_type),
        None => ev.event_type.clone(),
    };
    format!("[{sid}] {summary}")
}

fn cause_str(cause: &TerminationCause) -> &'static str {
    match cause {
        TerminationCause::Completed { .. } => "Completed",
        TerminationCause::Failed { .. } => "Failed",
        TerminationCause::Stopped => "Stopped",
        TerminationCause::Killed => "Killed",
        TerminationCause::PeerKilled { .. } => "PeerKilled",
    }
}
