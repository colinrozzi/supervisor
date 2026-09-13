# Experiment 1 — prove the reconcile loop

**Status: GREEN.** End-to-end proof of the supervisor's core loop — now on the REAL
`Failed` path (packr-guest `0.24.1` + theater `82cc6217`): the child `panic!()`s, the
panic traps to `TerminationCause::Failed`, and the supervisor respawns only on `Failed`.
(History: first proven on `c3937bdc` with a `self.shutdown`→`Completed` stand-in, because
a guest panic used to loop instead of trap; 0.24.1 fixed the panic handler, so this is
the authentic crash loop.)

## What it proves

A supervisor with a one-service roster reconciles reality to that roster through
the full primitive path:

1. **init** → `runtime.spawn` the child + `lifecycle.monitor-filtered(id, terminations())`
   it (woken only on the child's terminal event, not its whole chain).
2. child `panic!()`s → the trap surfaces as the terminal `"terminated"` event with
   `TerminationCause::Failed` → delivered to `lifecycle-handlers.handle-actor-event`.
3. the supervisor **decodes the cause and respawns only on `Failed`** (Completed / Stopped /
   Killed / PeerKilled are intentional — left down), rate-limited.
4. after `max` (3) restarts inside `window_ms` (60s), the rate-limiter **trips** →
   the service is `BLOCKED`, no further respawn — and the supervisor itself stays up.

See `run.log` for the captured passing trace (crash → 3 respawns → block, all `Failed`).

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

3. **A guest `panic!()` used to loop instead of trap** — packr-guest ≤0.24.0's
   `#[panic_handler]` did `loop {}`, so a panic spun silently: no `ActorError`, no
   terminal event. RESOLVED in packr-guest **0.24.1** (panic handler → `wasm::unreachable()`),
   so a panic now traps → `TerminationCause::Failed`. *(Root-caused by theater-dev, fixed
   by pack-dev in pack #132.)*

## Respawn is gated on `Failed` (matches the v0 design)

The supervisor decodes `TerminationCause` from the terminal event `data` (packr-decode
→ find the cause variant) and **respawns only on `Failed`** — Completed / Stopped /
Killed / PeerKilled are intentional and left down. This is what makes `remove`/self-shutdown
not trigger a zombie respawn. If the cause can't be decoded, it respawns defensively (a
real crash must never be missed).
