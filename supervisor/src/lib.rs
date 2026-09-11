//! # supervisor — the reusable supervision pattern for theater
//!
//! **DRAFT surface** against the DECIDED primitive contract (theater-dev,
//! 2026-09-11 — see `../../docs/DESIGN.md`). Signatures firm up when theater-dev
//! publishes `runtime.pact` + `lifecycle` on the relocation branch; bodies are
//! sketched, not wired.
//!
//! ## What this is
//!
//! Theater dissolved the supervisor *handler* into two primitives — `runtime`
//! (spawn/stop/kill) and `lifecycle` (monitors + links) — and holds no lineage.
//! Supervision is therefore a **userland pattern**; this library *is* that
//! pattern, so no supervising actor rewrites it. A composing actor holds a
//! `Supervisor` in its `#[derive(State)]` cell and forwards its one
//! `handle-lifecycle-event` export here.
//!
//! ## The per-child recipe (from the contract)
//!
//! On `spawn`, for each child, compose up to three `lifecycle` subscriptions:
//!   1. **death-monitor** — established atomically by `runtime.spawn` *before the
//!      child's init runs* (no gap). Terminal event → `on_event` → restart policy.
//!   2. **chain-monitor** (optional) — a subset `filter` (Pattern over event
//!      case-names) → `on_event` → append to the child's black-box ring.
//!   3. **link** (`target: stop-self`, optional) — fate-sharing; the supervisor's
//!      own death auto-cascades to linked children (`PeerKilled`). No explicit
//!      stop-at-shutdown.
//!
//! Restart policy (POLICY, lives here): respawn **only** on `TerminationCause::
//! Failed`, subject to the rate limiter; `Completed`/`Stopped`/`Killed`/
//! `PeerKilled` are intentional → don't respawn. Restart = `stop`(already dead) +
//! fresh `spawn` (no resume; state rebuilds by replay). Children are addressed by
//! a **stable handle**; the theater id rotates on respawn.

#![allow(dead_code)]
extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

/// Restart policy — evaluated on a `Failed` terminal event.
pub struct RestartStrategy {
    /// Max restarts allowed within `window_ms` before the child is blocked
    /// (crash-loop → operator intervention + notify).
    pub max_restarts: u32,
    pub window_ms: u64,
    /// Event case-names to keep in the black box (the L2 subset filter). Empty =
    /// don't record (crash-catch only).
    pub record_kinds: Vec<String>,
    /// Whether this child is fate-linked to the supervisor (stop-self link).
    pub link: bool,
}

/// One supervised child. Lives in the composing actor's in-module state, so it's
/// a replayable projection of the chain.
pub struct Child {
    /// Stable handle (operator name / node seed-pubkey) — how everything
    /// addresses this child. Survives respawn.
    pub handle: String,
    /// Current theater actor id (rotates on every respawn).
    pub id: String,
    /// How to (re)spawn: manifest + init.
    pub manifest: String,
    pub init: Option<Vec<u8>>,
    /// Recent restart timestamps (ms), trimmed to the rate-limit window.
    pub restarts: Vec<u64>,
    /// True once the rate limiter tripped — no more auto-respawns until cleared.
    pub blocked: bool,
    /// Black-box ring (the recorded subset; capped). Sealed on the terminal event.
    pub chain: Vec<Vec<u8>>,
}

/// What `on_event` decided — the composing actor performs it via `runtime`.
pub enum Action {
    /// Nothing (a non-terminal event was recorded, or an intentional stop).
    None,
    /// Respawn this child (fresh spawn); update its id from the result.
    Respawn { handle: String },
    /// Rate limiter tripped: child blocked, escalate out-of-band (never via a
    /// supervised service — see the notify design).
    Escalate { handle: String, reason: String },
}

/// The supervisor's own state. The composing actor derives `State` on the struct
/// that embeds this.
pub struct Supervisor {
    pub children: Vec<Child>,
    pub strategy: RestartStrategy,
}

impl Supervisor {
    /// `runtime.spawn` (with the atomic death-monitor), optionally add the
    /// filtered chain-monitor + the stop-self link, and record handle→id.
    /// Returns the new theater id.
    pub fn spawn_and_monitor(&mut self, _handle: &str, _manifest: &str, _init: Option<Vec<u8>>) -> Result<String, String> {
        // TODO(pact): runtime.spawn{monitor:true} -> id; if strategy.record_kinds
        // non-empty, lifecycle.monitor(id, filter=record_kinds); if strategy.link,
        // lifecycle.link(id). Push Child{handle,id,...}.
        todo_stub()
    }

    /// The one `lifecycle.handle-lifecycle-event` callback, forwarded here.
    /// Decodes `data`; if it carries a `TerminationCause` → restart path; else →
    /// record into the subject's black box. Keyed by `subject` → children-set.
    pub fn on_event(&mut self, _subject: &str, _event_type: &str, _data: &[u8], _now_ms: u64) -> Action {
        // TODO(pact): decode ChainEventPayload.
        //   terminal(Failed)      -> rate-limit check -> Respawn or Escalate
        //   terminal(other)       -> None (intentional; drop the child)
        //   non-terminal (subset) -> child.chain.push(data), cap; None
        todo_stub_action()
    }

    /// Direct children = this supervisor's view-scope (subtree at this level).
    pub fn children(&self) -> &[Child] { &self.children }
}

fn todo_stub() -> Result<String, String> { Err(String::from("unimplemented (draft surface)")) }
fn todo_stub_action() -> Action { Action::None }

// Imports the composing actor needs (firm up on theater-dev's runtime.pact):
//   theater:simple/runtime  : spawn / spawn-and-wait / stop-actor / kill-actor /
//                             list-actors / get-actor-status|state|manifest
//   theater:simple/lifecycle: monitor / link (subscribe) — and it EXPORTS
//                             handle-lifecycle-event, which forwards to on_event.
//
// Out-of-band (still the supervisor's, outside the runtime): the external
// host-wedge stall-probe + the off-box notify escalator. Not in this library's
// runtime-facing surface.
