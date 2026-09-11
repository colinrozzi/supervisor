//! # supervisor — the reconciler actor (v0)
//!
//! **SKELETON** against the post-#204 theater (rev `c3937bdc`) — see
//! `../../docs/DESIGN.md`. Bodies are shaped but stubbed; the exact `runtime`/
//! `lifecycle` guest bindings get wired from the in-tree patterns
//! (`monitor-test`, `link-test`, `supervisor-replay-test`).
//!
//! ## What it does
//!
//! Holds a **roster** (desired state, from init config) and reconciles reality to
//! it: spawn declared-absent services + monitor them; respawn `Failed` ones
//! (rate-limited). Level-triggered on a timer tick, nudged by
//! `handle-lifecycle-event`. All policy lives here; the runtime just spawns/kills.
//!
//! Composes theater primitives directly — no supervisor handler, no library:
//!   - `runtime.spawn` / `stop-actor` / `kill-actor`
//!   - `lifecycle.monitor` (watch → this actor's `handle-lifecycle-event`),
//!     `monitor-filtered` (for `record`), `link` (fate)
//!   - `timer.set-interval` (the reconcile tick), `self.log`
//!
//! State is in-module (a `#[derive(State)]` cell = a replayable chain projection).

#![allow(dead_code)]
extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

// ---- Roster: the DESIRED state (spec), parsed from init config -------------

/// One declared service. Pure spec — no runtime/status fields.
pub struct ServiceSpec {
    /// Stable handle: the map key, how you address it, survives respawn.
    pub handle: String,
    /// Manifest ref — a filesystem path or http(s):// URL (served from a git
    /// repo). Passed straight to runtime.spawn; theater resolves it. Carries
    /// package + initial_state + handlers (manifest-only; no separate fields).
    pub manifest: String,
    /// Restart policy.
    pub restart: RestartPolicy,
    /// Optional: record this service's chain to a sink (the black box, opt-in).
    pub record: Option<RecordSink>,
}

pub enum RestartStrategy { OnFailure, Always, Never }

pub struct RestartPolicy {
    pub strategy: RestartStrategy,
    pub max: u32,      // rate limiter: at most `max` restarts...
    pub window_ms: u64,// ...within this window, else block + escalate.
}

/// Where a service's recorded chain events go (filesystem or network — NOT the
/// theater store; theater removed store:// resolution).
pub enum RecordSink {
    File(String),   // a filesystem path
    Http(String),   // an http(s):// endpoint to POST events to
}

// ---- Actual state (status) — derived, held per service ---------------------

pub struct Running {
    pub handle: String,     // -> the ServiceSpec it satisfies
    pub id: String,         // current theater actor id (rotates on respawn)
    pub restarts: Vec<u64>, // recent restart timestamps (rate-limit window)
    pub blocked: bool,      // rate limiter tripped -> needs intervention
}

/// The supervisor's in-module state: desired roster + actual process table.
pub struct SupervisorState {
    pub roster: Vec<ServiceSpec>, // desired
    pub actual: Vec<Running>,     // status (derived)
}

// ---- The reconcile loop (the whole job) ------------------------------------

impl SupervisorState {
    /// Drive actual -> desired. Called on init, each timer tick, and each
    /// lifecycle event. Level-triggered: a full diff, so a missed event
    /// self-heals next tick.
    pub fn reconcile(&mut self, _now_ms: u64) {
        // TODO(bindings): for each ServiceSpec with no live Running:
        //   id = runtime.spawn(spec.manifest, None, None)?      // theater resolves fs/http ref
        //   lifecycle.monitor(id)                               // -> handle-lifecycle-event
        //   if spec.record: lifecycle.monitor-filtered(id, ..)  // -> record sink
        //   if spec.link:   lifecycle.link(id)                  // fate
        //   record Running{handle, id, ...}
        // (v0 roster is fixed from init, so "stop the undesired" waits for v0.1.)
    }

    /// A monitored actor produced an event. Terminal (TerminationCause) ->
    /// restart policy; non-terminal -> write to that service's record sink.
    pub fn on_lifecycle_event(&mut self, _subject: &str, _event_type: &str, _data: &[u8], _now_ms: u64) {
        // TODO(bindings): decode ChainEventPayload.
        //   terminal Failed        -> rate-limit check -> respawn (reconcile) or block+escalate
        //   terminal other         -> intentional; drop from actual
        //   non-terminal + record  -> append to the sink (File/Http)
    }
}

// ---- Theater exports (v0) --------------------------------------------------
// #[export "theater:simple/actor.init"]                  -> parse roster from
//     init-state, SupervisorState::set(...), arm the reconcile timer, reconcile()
// #[export "theater:simple/timer.handle-tick"]           -> reconcile()  (the tick)
// #[export "theater:simple/lifecycle-handlers.handle-lifecycle-event"]
//                                                        -> on_lifecycle_event(...)
//
// No store, no tcp, no supervisor-handler exports. Manifests resolve via
// runtime.spawn (fs/http). Deferred: live roster mutation (v0.1 control-plane),
// git-served rosters, deploy, the external stall-probe + off-box notify.
