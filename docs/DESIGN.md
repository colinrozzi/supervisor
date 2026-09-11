# supervisor — design (migration kickoff)

Status: KICKOFF 2026-09-11. Captures the decisions from the sentinel→supervisor
revisit. This is a clean rebuild on the post-overhaul theater, not a port of the
old code.

## The two overhauls that forced the revisit

1. **Sentinel-side (RSM control-plane).** Sentinel gained a *mesh face*:
   `control-sm` (rules: membership + `command_allow` + `(author,corr_id)` journal)
   ⊕ mesh core ⊕ a custom effectful system, so actors drive supervision over the
   mesh (`list`/`start`/`stop`/`get_chain`). Plus the flight-recorder
   (persist-at-death + `get_chain`).
2. **Theater-side (engine-axis + supervision rebuild).** Theater migrated onto a
   packr-core **capture-based runtime** (wasmtime ripped → browser/embedded
   portable), **in-module state** (state left the call surface; it's a replayable
   projection of the chain), an **open handler registry**, and — the big one —
   **supervision rebuilt as Erlang-style links/monitors** (PR #167).

## The new supervision model (theater)

One directed primitive, a **lifecycle subscription** `{subscriber, subject,
filter, target}`:

- `filter` — predicate over raw chain-event types, applied **host-side** (a
  non-matching event never enters wasm; kills the old subscribe-firehose cost).
- `target: stop-self` → **link** (runtime stops the subscriber; fate-sharing; no
  wasm).
- `target: deliver-to-wasm` → **monitor** (event → `handle-lifecycle-event`; the
  actor filters + reacts = **policy in the actor**).

Runtime holds no lineage (flat actor set). Only runtime action is `stop-self`;
everything richer (restart, cascade) is a monitor where the actor decides.

## Decisions taken in the revisit

### D1 — Drop the TCP command surface (legacy)
Sentinel carried *two* control surfaces: the old TCP+bearer JSON one
(`list`/`start`/`stop`/`get_chain`/`mesh_submit`) and the mesh RSM control-plane.
The mesh face supersedes it. Retiring TCP deletes: the `tcp` handler, the bearer
token + its `store` use, and all `cmd_*`/dispatch/JSON-request machinery. **One
control path (mesh).**

### D2 — Remove `subscribe-to-child`; watching goes through `lifecycle` monitors
Death detection needs no subscribe (spawning auto-wires the death callback).
`subscribe-to-child` only bought non-terminal chain events — exactly what a
**filtered** `lifecycle` monitor does, but host-side-filtered and not limited to
children. Bonus: filtering dissolves the old chain-amplification wedge that forced
the per-child `subscribe=false` opt-out (sentinel #26) — so that opt-out + the
`subscribe` bool go away too.

### D3 — Remove the supervisor *handler* entirely
Every responsibility distributes cleanly:
- spawn / stop-child → **`runtime`** (already has spawn/stop/kill/restart).
- watch / death → **`lifecycle`** monitor.
- fate-sharing / stop-children-on-teardown → **`lifecycle`** links.
- restart *policy* → the actor.
- children-set + view-scope → the supervising actor's own in-module state.
So supervision is a **pattern composed from `runtime` + `lifecycle`** — no
dedicated handler.

### D4 — The reusable supervisor pattern is a LIBRARY, not a handler
Erlang keeps the VM primitive-only and ships OTP `supervisor` as a library. We do
the same: primitives in `runtime`+`lifecycle`; the reusable bits (restart strategy
/ intensity limits, direct-children bookkeeping, view-scope, spawn-then-monitor
convenience, chain recording) become a **guest-side component** actors compose.
**This project is that library** (+ the running instance). → the rename.

## Open questions (to resolve as we build)

- **Auto-death-monitor ergonomics.** Without the supervisor handler you
  `runtime.spawn` then explicitly `lifecycle`-monitor. The library bundles
  "spawn-and-monitor" into one call so it's not extra ceremony.
- **View-scope for `runtime.list-actors` `scope: subtree`.** With no supervisor
  handler to answer from a direct-children set, subtree-scope becomes the
  supervising actor's own bookkeeping (or `list-actors` goes flat + caller
  filters). → a question for theater-dev; both handle-lifecycle-event definitions
  (supervisor-handlers.pact AND lifecycle-handlers.pact) are a smell to resolve.
- **Instance vs library split** — what the canonical running supervisor is vs what
  any actor pulls in.
- **Notify / stall-probe** (carried over from sentinel, still out-of-band): the
  external host-wedge probe and the off-box notify escalator are inherently
  outside the runtime and remain the supervisor's, unchanged by the overhaul.

## The primitive contract (DECIDED with theater-dev, 2026-09-11)

theater-dev decided the runtime+lifecycle contract the library composes; it's
being built (relocation branch), exact signatures to follow. What we get:

- **`runtime` (mutate):** `spawn` / `spawn-and-wait` / `stop-actor` / `kill-actor`
  (relocated here from the supervisor handler); `(inspect)` `list-actors` +
  `get-actor-status/state/manifest`. `list-actors` is **flat** — no lineage in the
  runtime; the library filters to its own children-set.
- **No `restart-actor` primitive** — recovery is a **fresh spawn** (the model has
  no resume; state rebuilds by replay). Restart = library-composed `stop + spawn`.
  We address children by a **stable handle** (name / node seed-pubkey) → current
  (rotating) theater-id, so a new id on respawn is a non-issue. No identity-
  preserving restart needed.
- **No `update-actor-package` yet** — supervised hot-swap = library-composed
  `stop-old + spawn-new-package` for v1 (brief restart on deploy is fine). A
  dedicated atomic update-package is a **follow-up** primitive if/when we need
  zero-downtime (drain in-flight) + chain continuity across a version bump.
- **R2 — no spawn→monitor gap:** `spawn` (opt-in) atomically establishes the
  spawner as a deliver-to-wasm **death-monitor** on the child, *before the child's
  init runs* — so a fast/init-time crash can't slip through.
- **L1 — `TerminationCause`** (decodable off the terminal payload): `Completed`
  (clean) / `Failed` (crash/panic/host-error) / `Stopped` (graceful or runtime
  shutdown) / `Killed` (force) / `PeerKilled{peer}` (fate cascade). **Restart
  policy: respawn ONLY on `Failed`; everything else is intentional → don't
  respawn.** (external-stop's whole job is now just "cause == Stopped/Killed".)
- **L2 — filter is a Pattern over event case-names** → a chain-monitor records a
  **subset** (dissolves the old amplification wedge; retires the `subscribe` bool).
- **L3 — links + AUTO-CASCADE:** establish a `stop-self` link per fate-shared
  child; the supervisor's own termination auto-cascades to linked children
  (emergent ripple, each records `PeerKilled`). No explicit stop-each-at-shutdown.
- **C1 — one callback:** single `theater:simple/lifecycle.handle-lifecycle-event`
  for all monitored actors (the supervisor-handlers duplicate is deleted).
- **C2 — view-scope = library:** runtime stays flat; the library owns its
  children-set + subtree view-scope in its in-module state.

**The library's per-child composition:** on `spawn` → (a) atomic terminal
death-monitor [→ restart policy, rate-limited, on `Failed` only], (b) optional
subset-filtered chain-monitor [→ the black box], (c) a `stop-self` link [fate].
All land on the one `handle-lifecycle-event`; the callback dispatches on the
payload (`TerminationCause` present → restart path; else → record path). All
policy in the library; zero lineage in the runtime.

## Migrating from sentinel — what carries, what's dropped

- CARRIES (re-expressed on the new model): crash→restart w/ rate-limit; the
  flight-recorder (chain capture + persist-at-death + get_chain); the RSM
  control-plane (control-sm + mesh face); the external stall-probe design
  (mesh-supervision-spec v3); packr-guest 0.24 + in-module-state + `self.*`.
- DROPPED: the TCP+bearer command surface (D1); `subscribe-to-child` + the
  subscribe opt-out (D2); the supervisor-handler exports trio → the single
  `handle-lifecycle-event` on a monitor (D3).
