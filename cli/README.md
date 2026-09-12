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

Native build; the theater host pulls `openssl-sys` (via reqwest). With nix:

```sh
OSSL_DEV=$(nix build nixpkgs#openssl.dev --no-link --print-out-paths)
OSSL_OUT=$(nix build nixpkgs#openssl.out --no-link --print-out-paths)
nix shell nixpkgs#gcc nixpkgs#pkg-config --command bash -c '
  export CC=gcc OPENSSL_LIB_DIR=$OSSL_OUT/lib OPENSSL_INCLUDE_DIR=$OSSL_DEV/include OPENSSL_NO_VENDOR=1
  cargo build --release'
# → target/release/supervisor
```

## Next subcommands (not built yet)

- `up` — launcher (spawn + detach / manage).
- `add` / `remove` / `list` — live roster mutation over a control surface (v0.1).
