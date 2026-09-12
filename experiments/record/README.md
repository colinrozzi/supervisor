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

## Sinks: `http` and `file` — both green

`record` is a tagged union — `{"kind":"http","url":…}` or `{"kind":"file","path":…}`
— backed by a `Sink` enum; `handle-actor-event` dispatches HTTP→POST, File→`append-file`.
Both proven green E2E on theater `4e4e67d3` (15 events across 3 incarnations, reconcile
tripping at max=2).

- **http** — POST each event via the http-client handler (needs a collector; see below).
- **file** — append each JSON line via theater's filesystem handler
  (`filesystem.append-file`), sandboxed to the handler's configured root. Self-contained
  (no collector): the manifest declares `[[handler]] type="filesystem"` with a `path`, and
  `record:{kind:"file",path:"crasher.jsonl"}` lands at `<root>/crasher.jsonl`.

  *History:* the file sink was briefly blocked — a `theater spawn` root grants
  `file_system.allowed_paths=["/"]`, and the handler first resolved allowed-paths
  sandbox-relative, rejecting the absolute `/`. Fixed in theater **#208** (rev `4e4e67d3`):
  allowed-path entries are now root-relative and a bare `/` means the sandbox root, so the
  inherited `["/"]` = "anywhere in my sandbox". No permission grant needed in the manifest.

## Run it

```sh
export THEATER=/path/to/post-#206/theater
nix shell nixpkgs#gcc --command ./run.sh
```

`collector.rs` is a std-only HTTP collector (no deps) that appends each POST body
to a capture file and returns `200 Connection: close`. It's test scaffolding, not
part of the supervisor.
