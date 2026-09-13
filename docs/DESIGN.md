# supervisor — design

Status: v0 DESIGN LOCKED 2026-09-11 (the sentinel→supervisor revisit). A clean
rebuild on the post-overhaul theater; not a port of the old code.

## 1. The model — reconcile reality to a declared roster

The supervisor holds a **roster** (declared *desired* state: the set of services
that should be running) and runs a **reconcile loop** that continuously drives
*actual* → *desired*. Declarative desired-state reconciliation (k8s / systemd /
Nomad), for theater actors.

- **Spec vs status.** The roster is pure *spec* (what should run). Current ids,
  restart counts, health, last-cause are *status* — derived by the supervisor,
  held separately, never in the roster.
- **Everything is an edit to desired.** Restart-on-crash = the loop noticing a
  `Failed` actor is now desired-but-absent → respawn. Add/remove = edit the
  roster → reconcile. Deploy = edit a manifest → reconcile. One mechanism, not
  three subsystems.
- **Level-triggered, edge-nudged.** A periodic full diff (self-heals even if an
  event is missed) that events (`handle-lifecycle-event`, roster changes) nudge
  to run sooner.
- **Direction: GitOps for actors.** Rosters + manifests served from git repos;
  the supervisor reconciles reality to the repo. (v0 seeds the roster from init;
  git/filesystem/network feeding is a future feed method.)

## 2. Built on theater primitives (post-#204, rev c3937bdc)

Theater dissolved the supervisor *handler*; the supervisor composes the primitives
directly. **No supervisor handler. No reusable library (yet) — one concrete
actor** (a library is premature with a single consumer; extract later only if a
real second consumer earns it — and the single-central-service-manager model
suggests there won't be one).

- **`runtime`** (mutate: spawn / stop-actor / kill-actor; inspect: list-actors /
  get-*). Flat — no lineage; the supervisor owns "who I run" in its own state.
- **`lifecycle`** — `monitor` / `monitor-filtered` (watch → `handle-lifecycle-event`),
  `link` (fate = stop-self, auto-cascades on the supervisor's own death),
  `subscribe-to-spawns` (births). Terminal events carry `TerminationCause`
  (`Completed`/`Failed`/`Stopped`/`Killed`/`PeerKilled`) — **respawn only on
  `Failed`**; the rest are intentional.

## 3. v0 — what we build first

**One actor.** Handlers: `runtime` + `lifecycle` + `timer` (reconcile tick) +
`filesystem`/`http-client` (the record sink). **No `store` handler** (manifests
are resolved by `spawn` from fs/http refs; no store-backed anything), **no TCP
command surface**, **no library**.

**Roster** arrives in the actor's **init config** (`initial_state`). Each entry:

```
handle   : stable name (map key; how you address it; survives respawn)
manifest : a filesystem path or http(s):// ref — served from a git repo; passed
           straight to runtime.spawn (theater resolves it). Carries package +
           initial_state + handlers. (v0: manifest-only — no separate package/init.)
restart  : policy — strategy (on-failure | always | never) + rate-limit (max/window)
record?  : OPTIONAL — monitor this actor's chain, write its events to a sink
           (a file, or an http(s) endpoint). The flight-recorder / black box, opt-in.
           BUILT (experiments/record): a recording service is watched full-chain
           (bare monitor); handle-actor-event writes every event to a sink. `record` is
           a tagged union: {kind:"http",url} (POST via http-client — proven green) or
           {kind:"file",path} (append via theater's filesystem handler #207). The file
           sink is wired + builds but its E2E is blocked on a theater permission gap (a
           `theater spawn` root's allowed_paths=["/"] can't resolve sandbox-relative;
           reported to theater-dev). HTTP sink is the working default meanwhile.
```

**State** (in a `#[derive(State)]` cell; a chain projection): `desired` (the
roster) + `actual` (per-service: current id, restart timestamps, blocked).

**Reconcile:** on init + each timer tick + each `handle-lifecycle-event`, diff
desired vs actual and act — spawn declared-absent (+ monitor, + `record` sink if
set, + link if set); on a `Failed` terminal, respawn subject to the rate-limiter,
or block + (later) escalate. (Stop-the-undesired only bites once the roster can
change — v0.1.)

## 4. Deferred (all "another way to feed/edit the same desired state")

- **Live roster mutation over the network** (v0.1) — networked clients add/remove
  services. This is the RSM control-plane (`control-sm`) **reframed declaratively**:
  clients author desired-state edits; the supervisor reconciles. **Specced** in
  `docs/control-surface.md` — JSON over TCP, server in the actor, thin native CLI client.
- **Git-served / filesystem rosters** — the GitOps feed.
- **Deploy / hot-swap** — edit a manifest → reconcile (stop-old/spawn-new); a
  dedicated atomic `update-package` (drain + chain-continuity) if zero-downtime is
  needed.
- **External stall/delivery-probe** (host-wedge class) — a timer-driven health check
  (e.g. inbox's #69 read-200 + loopback-/send-delivers-2xx) that restarts the tree on N
  consecutive failures. Catches "alive but not delivering" (the SMTP :25 wedge), which
  crash-catch structurally can't. **Deferred (Colin, 2026-09-13): this is APPLICATION-LEVEL
  logic — what "healthy" means is service-specific — not the generic supervisor's concern
  yet.** So the supervisor is crash-catch-only for now; the inbox cutover ships labeled
  crash-catch + flight-recorder, NOT wedge-fixed. Revisit when a probe layer is scoped
  (an app-side health actor, or a supervisor-driven probe via config).
- **Off-box notify escalator** (#43) — inherently outside the runtime; carried from
  sentinel's mesh-supervision spec.
- **A reusable supervisor library** — only if a real second consumer appears.

## 5. Migration from sentinel

- **CARRIES (re-expressed):** crash→restart + rate-limit (now = reconcile on
  `Failed`); the flight-recorder (now = the `record` arg); the RSM control-plane
  (now = declarative roster mutation, v0.1); the external stall-probe design;
  packr-guest 0.24 + in-module-state + `self.*`.
- **DROPPED:** the TCP + bearer command surface; `subscribe-to-child` (→ lifecycle
  monitors); the supervisor-handler exports; the `store` handler; the library idea.

## 6. Primitive contract reference (theater-dev, PR #204 @ c3937bdc)

`runtime`: `spawn(manifest, init-state, wasm-bytes) -> result<string, runtime-error>`,
`spawn-and-wait`, `stop-actor(id)`, `kill-actor(id)`, `list-actors`, `get-actor-*`.
`runtime-error` / `spawn-failure` are richly typed. Restart = library-composed
`stop + spawn` (no resume; state rebuilds by replay; address by stable handle, id
rotates). `lifecycle`: `link`/`unlink`, `monitor`/`monitor-filtered(subject, filter)`
(#205)/`unmonitor`, `subscribe-to-actor`, `subscribe-to-spawns`. Callback:
`lifecycle-handlers.handle-lifecycle-event(subject, event-type, data)` — one export
for all monitored actors; terminal = `event-type="terminated"` + `TerminationCause`.
