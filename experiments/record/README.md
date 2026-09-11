# Experiment — `record` (the flight-recorder / black box)

**Status: GREEN (2026-09-11)** on theater `c197d707` (post-#206 full-chain monitor).

## What it proves

A service declared with `record: { url }` is watched on its **full chain** (not
just terminations), and the supervisor **POSTs every chain event to the sink URL**
via the `http-client` handler — while the reconcile loop keeps working.

`capture.sample.jsonl` is a real run: 3 incarnations of a self-terminating child,
5 events each, all delivered (http 200):

```
wasm (timer.handle-tick call) → self/log → self/shutdown → wasm (result) → terminated
```

Each line is one event:

```json
{"handle":"crasher","child":"<id>","type":"terminated","seq":9,"data_hex":"434752…"}
```

`data_hex` is the raw packr `ChainEventPayload` (starts with the `CGRF` magic).
Decode it and you get the faithful event — the `terminated` one even carries
`TerminationCause::Completed`, which is what respawn-only-on-`Failed` (#9) will read.

## How it works

- **Subscription:** a recording service uses bare `monitor(child)` (full chain,
  post-#206 default); a plain service uses `monitor-filtered(child, terminations())`.
  One subscription either way; recording carries across respawns.
- **Sink:** `handle-actor-event` → for a recording service, build a JSON line and
  `http-client.request` a POST to `record.url`. Recording is independent of the
  crash-catch: every event is recorded; only `terminated` drives the reconcile.

## Sink: HTTP today, file later

The v0 sink is **HTTP** because **theater has no `filesystem` handler in the open
handler registry at `c197d707`** — only an in-progress doc
(`crates/theater/changes/in-progress/docs/filesystem.rs.doc.md`). The manifest still
carries a `FileSystemHandlerConfig`, but no crate implements `theater:simple/filesystem`.
When it lands, a `file` sink slots in behind the same `record` arg (add a `kind`).

## Run it

```sh
export THEATER=/path/to/post-#206/theater
nix shell nixpkgs#gcc --command ./run.sh
```

`collector.rs` is a std-only HTTP collector (no deps) that appends each POST body
to a capture file and returns `200 Connection: close`. It's test scaffolding, not
part of the supervisor.
