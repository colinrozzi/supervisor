# Experiment 1 — prove the reconcile loop

**Status: GREEN (2026-09-11).** The first end-to-end proof that the supervisor's
core loop works — re-verified on the post-#206 lifecycle reshape (theater rev
`c197d707`: full-chain `monitor`, `monitor-filtered`, `handle-actor-event`).
Originally proven on `c3937bdc`.

## What it proves

A supervisor with a one-service roster reconciles reality to that roster through
the full primitive path:

1. **init** → `runtime.spawn` the child + `lifecycle.monitor-filtered(id, terminations())`
   it (woken only on the child's terminal event, not its whole chain).
2. child self-terminates → the runtime emits the terminal `"terminated"` event →
   the filtered monitor delivers it to `lifecycle-handlers.handle-actor-event`.
3. the supervisor **reconciles**: desired-but-now-absent → respawn (rate-limited).
4. after `max` (5) restarts inside `window_ms` (60s), the rate-limiter **trips** →
   the service is `BLOCKED`, no further respawn — and the supervisor itself stays up.

See `run.log` for the captured passing trace (spawn → 5 respawns → block).

## Run it

```sh
export THEATER=/path/to/post-#204/theater      # a `theater spawn`-capable host
nix shell nixpkgs#gcc --command ./run.sh       # gcc for host build-scripts
```

`run.sh` resolves a wasm-capable rust toolchain from the nix store, builds both
wasm, generates the two manifests with absolute paths, and runs for 25s.

## Findings banked from getting here

Three things that bit us and are worth remembering (all now encoded in the
supervisor / manifest, not just here):

1. **`result<T,E>` host→guest encoding is ABI-vintage-dependent.** `runtime.spawn`'s
   result decodes as packr-native `Value::Result { value: Ok/Err }` on this host,
   *not* the tagged `Value::Variant { tag: 0/1 }` the c3937bdc in-tree template
   assumes. `supervisor/src/lib.rs::runtime_spawn` accepts **both** shapes so it's
   skew-proof.

2. **The `runtime` CONTROL capability defaults to `Disallow`.** An actor that
   drives `theater:simple/runtime` (spawn/list/stop) must be granted it explicitly
   — the supervisor manifest carries
   `[permission_policy.runtime] type="restrict", config={inspect=true, mutate=true}`.
   Without it, `spawn` is refused before it reaches the handler.

3. **A trap inside a timer `handle-tick` callback is currently swallowed by the
   host** — it produces no `ActorError`, no terminal event, no log; the actor just
   goes silent. So a `panic!()`-style hard crash does *not* exercise the `Failed`
   path here. This child self-terminates via `self.shutdown` (→ `TerminationCause::
   Completed`), the deterministic terminal path, which is all the loop needs.
   *(Reported to theater-dev — the timer-callback trap should surface as a
   supervised `Failed` termination.)*

## Caveat on this experiment vs the v0 design

The v0 design says **respawn only on `Failed`** (Completed/Stopped/Killed are
intentional). This experiment respawns on *any* `"terminated"` — it does not yet
decode `TerminationCause` from the event `data`. That's the immediate next
refinement (and needs the timer-trap gap above fixed to test the real `Failed`
path). Here, the self-shutdown stands in as "the child died" purely to prove the
spawn→monitor→event→reconcile→rate-limit machinery end-to-end.
