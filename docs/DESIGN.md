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

## Migrating from sentinel — what carries, what's dropped

- CARRIES (re-expressed on the new model): crash→restart w/ rate-limit; the
  flight-recorder (chain capture + persist-at-death + get_chain); the RSM
  control-plane (control-sm + mesh face); the external stall-probe design
  (mesh-supervision-spec v3); packr-guest 0.24 + in-module-state + `self.*`.
- DROPPED: the TCP+bearer command surface (D1); `subscribe-to-child` + the
  subscribe opt-out (D2); the supervisor-handler exports trio → the single
  `handle-lifecycle-event` on a monitor (D3).
