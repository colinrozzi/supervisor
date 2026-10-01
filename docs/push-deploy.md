# Push-deploy — ship an actor to a box that fetches nothing

`supervisor push` deploys an actor by **pushing** its wasm + config over the
authenticated `:9000` control plane, so the target box fetches from nowhere. It's the
deploy path for a **bare, disconnected box**: an empty supervisor and nothing else — no
store node, no reachable artifact host, no shell on the box.

## The model

`add`/`apply` name a manifest the *box* must resolve (a local file, or a URL it can
reach). That needs the box to have the artifacts, or to reach a host that does. `push`
inverts it: the operator holds the artifacts locally and sends them *to* the supervisor.

```
supervisor push <handle> --manifest <local.toml> --wasm <local.wasm> --profile <p>
```

- `--manifest` — a LOCAL manifest.toml. Its **content** is pushed inline. It carries
  config only: `name`, `version`, `initial_state`, `[[handler]]`s. **`version` is
  required** — theater rejects a manifest without it at parse time (bare
  `spawn-failed`/`bad-manifest`). The `package` field is a **placeholder** (the pushed
  bytes override it), e.g. `package = "inline:pushed"`.

Minimal pushable manifest:

```toml
name = "static-server"
version = "0.1.0"          # REQUIRED — omitting it fails the spawn at manifest parse
package = "inline:pushed"  # placeholder; the pushed wasm bytes override it
initial_state = """listen=0.0.0.0:80
content-type=text/html; charset=utf-8

<file body>
"""
[[handler]]
type = "self"
[[handler]]
type = "tcp"
```
- `--wasm` — a LOCAL `.wasm`. Its **bytes** are pushed (a JSON `u8` array in the op).

The CLI sends one `add` op with a `wasm` field. The supervisor, seeing `wasm` present,
treats `manifest` as inline content and spawns:

```
runtime.spawn("inline:<manifest toml>", None, Some(<wasm bytes>))
```

`runtime.spawn`'s `wasm-bytes` (3rd arg) overrides the manifest's `package` fetch
(`resolve_wasm` returns provided bytes directly), and the `inline:` scheme (theater
[#220]) resolves the manifest content with zero I/O. So **both halves arrive by push,
neither is fetched**.

## Durability — the supervisor is EPHEMERAL by design

The supervisor **holds the pushed bytes** in the service's in-module state — it must,
because respawning a *crashed child* is its core job and it can't respawn bytes it didn't
keep. That in-memory hold is what survives a **child** crash/restart.

It deliberately does **not** survive its own **process** restart. A fresh supervisor
process assumes **no state** from the prior run — an empty roster, a clean slate. That is
the correct, intended model (Colin's ruling): carrying a roster / pushed bytes across the
supervisor's own restart would leak context from a dead run into a new one. So a process
restart starts clean, and re-establishing what should be running is a **higher-level
concern above the supervisor** (a boot/deploy layer — e.g. store-dev's boot-from-store
auto-heal), explicitly **not** built into the supervisor and not currently assigned to any
layer. Until such a layer exists, a process restart = a manual re-push. Do **not** add
durable-chain persistence or reconcile-into-restored-state to the supervisor — the
simplicity (ephemeral node, single source of truth for durability lives above) is the point.

| Event | Pushed actor |
|---|---|
| child **crash** / control-op **restart** | respawned from the held in-memory bytes (rate-limited) — **survives** |
| supervisor **process restart** (systemd restart / upgrade / reboot) | clean slate — empty roster, pushed services gone **by design**; re-push (or a future boot-from-store layer re-establishes) |
| box **recycle** | same as a process restart: re-push / re-establish from above |

## Requirements

- The box's `supervisor` binary must embed a theater with the **`inline:`** scheme
  (theater #220+). Bump the binary; older binaries reject `inline:` as a bad manifest.
- The control plane is the authenticated one (TLS + ed25519) for off-box use — `push`
  rides the same channel as every other op, so an authorized key is required.

## Trade vs. HTTP boot-pull

Push (this) — box fetches nothing; simplest for a disconnected box; re-push on any supervisor process restart/recycle (ephemeral node, above).
HTTP boot-pull (a manifest/URL ref) — box re-fetches from a store/URL itself, which is
better for *auto-boot* of a recycled box. They coexist: a service is a ref service or a
pushed service per its roster entry. Boot-from-store (store content-GET) graduates the
recycle/auto-boot story later; push is the immediate, dependency-free path.

[#220]: https://github.com/colinrozzi/theater/pull/220
