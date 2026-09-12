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

## Sinks: `http` (green) and `file` (wired, blocked on a theater gap)

`record` is a tagged union — `{"kind":"http","url":…}` or `{"kind":"file","path":…}`
— backed by a `Sink` enum; `handle-actor-event` dispatches HTTP→POST, File→`append-file`.

- **http** — proven green E2E (above). POST each event via the http-client handler.
- **file** — wired against theater's filesystem handler (#207, rev `0b60fdb6`):
  `Sink::File(path)` appends each JSON line via `filesystem.append-file`, sandboxed to
  the handler's configured root. The code works (capability + path resolution both pass),
  but the write is **currently blocked by a theater permission gap**: a `theater spawn`
  root grants `file_system.allowed_paths = ["/"]`, and the handler resolves allowed-paths
  *relative to the sandbox root* — which rejects the absolute `/`, so every write is
  `permission-denied: … outside the allowed-paths`. `permission_policy` restrict can't fix
  it (a relative child entry fails restrict's `starts_with("/")` superset check; restrict
  can't widen to `None`). Reported to theater-dev; candidate fix is the handler mapping an
  allowed-path `/` to the sandbox root. The file sink verifies the same day that lands.

## Run it

```sh
export THEATER=/path/to/post-#206/theater
nix shell nixpkgs#gcc --command ./run.sh
```

`collector.rs` is a std-only HTTP collector (no deps) that appends each POST body
to a capture file and returns `200 Connection: close`. It's test scaffolding, not
part of the supervisor.
