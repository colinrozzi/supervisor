//! # supervisor — the reusable supervision pattern for theater
//!
//! **SKELETON / KICKOFF** — this sketches the intended surface; the migration
//! from `sentinel` fills it in (see `../../docs/DESIGN.md`). Nothing here is
//! wired to theater yet.
//!
//! ## What this library is
//!
//! Theater's overhaul dissolved the supervisor *handler* into two primitives:
//! `runtime` (spawn/stop/kill/restart) and `lifecycle` (links + monitors). The
//! runtime holds no lineage. Supervision is therefore a **pattern**, composed —
//! and this library is that pattern, so every supervising actor doesn't rewrite
//! it (the OTP-`supervisor` role, in userland).
//!
//! An actor that wants to supervise composes this library and gets:
//!   - `spawn_and_monitor(manifest, init)` — one call: `runtime.spawn` the child
//!     + establish a terminal-filtered `lifecycle` monitor on it.
//!   - a restart strategy (rate-limit / intensity window) applied when a child's
//!     terminal `handle-lifecycle-event` arrives — POLICY LIVES HERE, in the
//!     actor, not the runtime.
//!   - direct-children bookkeeping + view-scope (the runtime no longer tracks a
//!     tree; the supervisor owns "who I started").
//!   - optional chain recording (the black box): a wider `lifecycle` monitor
//!     (host-side filtered to the event kinds worth keeping) that accumulates a
//!     child's chain and seals + persists it on the terminal event.
//!
//! ## Intended surface (subject to change during the migration)
//!
//! ```ignore
//! /// Restart policy — POLICY, evaluated by the supervisor actor on a terminal
//! /// lifecycle event (mechanism = runtime.spawn/stop; this decides when).
//! pub struct RestartStrategy { pub max_restarts: u32, pub window_ms: u64 }
//!
//! /// One supervised child's bookkeeping (in-module state; a chain projection).
//! pub struct Child {
//!     pub id: String,             // current runtime actor id (rotates on restart)
//!     pub manifest: String,       // how to (re)spawn it
//!     pub restarts: Vec<u64>,     // recent restart timestamps (rate-limit window)
//!     pub blocked: bool,          // rate limiter tripped -> operator intervention
//!     pub chain: Vec<u8>,         // black-box ring (optional; filtered)
//! }
//!
//! /// The supervisor's own state — held in a #[derive(State)] cell by the
//! /// composing actor (in-module state; rebuilt by replay).
//! pub struct Supervisor { pub children: Vec<Child>, pub strategy: RestartStrategy }
//!
//! impl Supervisor {
//!     pub fn spawn_and_monitor(&mut self, manifest: &str, init: Option<Vec<u8>>) -> Result<String, String>;
//!     /// Called from the composing actor's `handle-lifecycle-event`. Decodes the
//!     /// terminal ChainEventPayload for the cause, applies the restart strategy,
//!     /// respawns (or blocks + escalates). Intentional stop vs crash is read
//!     /// FROM the terminal payload (the old error/exit/external-stop trio is one
//!     /// callback now).
//!     pub fn on_lifecycle_event(&mut self, subject: &str, event_type: &str, data: &[u8]) -> Action;
//!     /// Direct children (view-scope = subtree at this supervisor).
//!     pub fn children(&self) -> &[Child];
//! }
//! ```
//!
//! ## Out of scope for the runtime, still the supervisor's job (out-of-band)
//! - external host-wedge stall-probe (a wedged host can't self-report)
//! - off-box notify escalator (an alert can't route through what it supervises)
//!
//! These carry over from sentinel's `docs/mesh-supervision-spec.md` unchanged by
//! the theater overhaul.

#![allow(dead_code)]

// TODO(migration): port sentinel's rate-limiter + chain-ring + control-sm rules
// onto runtime + lifecycle per docs/DESIGN.md. Keep policy here; primitives in
// theater. Nothing is implemented yet.
