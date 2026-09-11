# supervisor

Theater's reconciler — a declarative service-manager for the theater actor runtime.

> **Migration in progress:** this is the fresh rebuild of what was **sentinel**,
> on the post-overhaul theater. The old `sentinel` repo is the reference we
> migrate *from*, not code we mutate in place.

## The model: reconcile reality to a declared roster

You give the supervisor a **roster** — a declared set of services (each: a
handle + a manifest to run + a restart policy). Its whole job is a **reconcile
loop** that, *at all times, works to make reality match the roster*: spawn what's
declared-but-absent, restart what crashed, (later) stop what's no longer declared.

This is declarative desired-state reconciliation — the Kubernetes / systemd /
Nomad shape, for theater actors. The roster is *desired state* (spec); the live
process table is *actual state* (status); the supervisor drives actual → desired.
"Manage the fleet" is not a pile of imperative commands — it's **edit the desired
state, and the supervisor reconciles.** Restart, add/remove, deploy — all just
edits to the roster.

Where rosters ultimately come from (a git repo the supervisor reconciles against
= **GitOps for actors**) is the direction; see `docs/DESIGN.md`.

## Built on theater primitives, not a supervisor handler

Theater's overhaul dissolved the supervisor *handler* into two runtime
primitives, and the supervisor composes them directly (no dedicated handler, and
— for now — no reusable library; it's one concrete actor):

- **`runtime`** — `spawn` / `stop-actor` / `kill-actor` / `list-actors` (the
  control mechanism; the runtime is flat, holds no lineage).
- **`lifecycle`** — `monitor` (watch an actor's events → `handle-lifecycle-event`)
  and `link` (fate-share). All *policy* — restart strategy, what to record — lives
  here in the supervisor.

## v0 (what we're building first)

One actor. Roster arrives in its **init config**. Handlers: `runtime` +
`lifecycle` + `timer` (the reconcile tick) + `filesystem`/`http-client` (the
record sink). Per-entry: `{ handle, manifest (fs or http ref), restart?, record? }`.

- **Reconcile:** spawn declared-absent + monitor; respawn on termination (rate-limited).
  Nudged by `handle-actor-event`. (Gating respawn on `Failed` only is pending a theater
  panic-trap fix; see `experiments/reconcile-loop`.)
- **`record`** (opt-in per service, **built** — `experiments/record`): watch that actor's
  **full chain** and POST every event to a sink URL via the http-client handler — the
  flight-recorder / black box, as a roster arg. (Sink is HTTP today; a `file` sink lands
  when theater ports the filesystem handler.)

Everything past v0 — live roster mutation over the network, git-served rosters,
deploy/hot-swap, the external stall-probe, off-box notify — is "another way to
feed or edit the same desired state." See `docs/DESIGN.md`.
