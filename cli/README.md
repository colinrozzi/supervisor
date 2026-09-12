# supervisor-cli — the `supervisor` dev CLI

A native binary that runs a supervisor (or any actor) on an **in-process theater
runtime** and streams its **decoded chain to stdout**. The dev loop: edit a roster,
`supervisor spawn`, watch the whole supervised tree live — no external `theater`
binary, no hand-written manifest boilerplate.

Standalone crate (its own workspace): it links `theater`/`theater-stage`/`theater-native`
as a native host, a separate dependency tree from the wasm guest in `../supervisor`.

## `supervisor spawn`

```sh
# roster-in (default): generate the manifest, spawn, stream the pretty chain
supervisor spawn roster.json --wasm path/to/supervisor.wasm

# a service that records to a file needs a sandbox root
supervisor spawn roster.json --wasm … --record-dir ./rec

# spawn a ready manifest raw instead
supervisor spawn --manifest supervisor.toml

# see the generated manifest
supervisor spawn roster.json --wasm … --show-manifest
```

### Output: two independent streams

- **`--chain [compact|pretty]`** — the actors' **chain** (the record of every event).
  `pretty` (default) decodes it: `» <log message>`, `→ <iface>/<fn>(<args>)`,
  `⚙ call|result <fn>`, `● spawned`, `✖ terminated (<cause>)`. `compact` is a terse
  type-only skim. Bare `--chain` = pretty.
- **`--logs [error|warn|info|debug|trace]`** — theater's **runtime logs** (the host's
  internals: manifest parse, permission calc, scheduling, errors). Bare `--logs` = `info`;
  off by default. `RUST_LOG` overrides (for per-crate directives, e.g. `RUST_LOG=theater=debug`).

The two compose. With neither flag, the default is `--chain pretty`; `--logs` alone
gives logs-only (chain off). Actor `self.log` output lives in the chain (rendered as
`»` lines in pretty), so there's no separate actor-log stream to toggle.

`roster.json` is the supervisor's init config — `{"services":[{handle, manifest,
max?, window_ms?, record?}, …]}`. From it the CLI **generates the manifest**:
`initial_state` = the roster, plus the handlers `self`/`runtime`/`lifecycle`/`timer`
and the `[permission_policy.runtime]` grant always, and `http-client` (with
`allowed_hosts` auto-derived from any `record` http URLs) / `filesystem` (sandbox =
`--record-dir`, with the fs permission grant) **only when a service records to them**.

Output: actor `self.log` lines print directly (`[id] …`), interleaved with the
decoded event stream — `● spawned`, `✖ terminated (Failed|Completed|…)`,
`→ <host call>`, `⚙ <wasm>`. `--format short` falls back to theater's raw line.
Exits when the root actor terminates, or on Ctrl+C.

## Build

Native build. As of theater #209 (rustls, no openssl) the recipe is just:

```sh
nix shell nixpkgs#gcc --command bash -c 'CC=gcc cargo build --release'
# → target/release/supervisor
```

Note: a binary built with the nix toolchain links its glibc/interpreter from
`/nix/store`, so it is **not portable** to another container as-is. A fleet-portable
artifact needs a static (musl) build — see the distribution note below.

## Distribution (fleet use)

Not yet published as a fleet tool. The gate is a **portable static binary**: the
nix-toolchain build hard-codes `/nix/store` paths (glibc + ELF interpreter) that won't
exist in another agent's container. That needs a musl rust toolchain (not in the image),
so publishing — a static build + a release + a `supervisor-upgrade` wrapper matching the
`inbox`/`tickets`/`theater-upgrade` pattern — is coordinated with the manager.

## Control surface (live roster mutation)

`spawn … --control-port N` opens a JSON-over-TCP control surface on the supervisor.
From another terminal, the control verbs are thin TCP clients to it:

Port defaults to **9000** on both sides — bare `--control-port` opens 9000, and the
client `--port` defaults to 9000 — so you can usually omit it:

```sh
supervisor spawn roster.json --control-port     # server on 9000 (or --control-port N)
supervisor list                                 # the live roster (desired + status)
supervisor status heartbeat
supervisor add worker ./worker.toml [--max 5 --window-ms 60000 --keep-chain]
supervisor remove worker                        # supervisor stops it
supervisor apply new-roster.json                # replace the whole roster (diff + reconcile)
supervisor chain heartbeat                      # in-memory chain (needs record or keep_chain)
```

`apply` is the GitOps move: hand it a full `{"services":[…]}` roster and the supervisor
converges — stops what's gone, spawns what's new, leaves unchanged handles running.

(Omit `--control-port` entirely to leave the surface off. All verbs take `--port N`.)

Each edit mutates the supervisor's *desired* roster and it reconciles: `add` spawns +
monitors, `remove` stops. See `docs/control-surface.md`.
