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
  config only: `initial_state` + `[[handler]]`s + `name`. The `package` field is a
  **placeholder** (the pushed bytes override it), e.g. `package = "inline:pushed"`.
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

## Durability — survives restart, only recycle re-pushes

The supervisor **holds the pushed bytes** in the service's in-module state. It must —
respawning a crashed child is its core job, and it can't respawn bytes it didn't keep.
Because in-module state is **chain-backed** (state = a replayable projection of the
chain), the bytes are persisted:

| Event | Pushed actor |
|---|---|
| child **crash** | respawned from held bytes (rate-limited, as any service) |
| supervisor **process restart** (systemd restart / upgrade) | roster replays from the chain → respawned from held bytes, **no external fetch** |
| box **recycle** (fresh disk / empty chain) | needs a **re-push** — this is where boot-from-store becomes the later self-healing/auto-boot layer |

So push is **ephemeral-on-recycle**, not ephemeral-on-restart.

## Requirements

- The box's `supervisor` binary must embed a theater with the **`inline:`** scheme
  (theater #220+). Bump the binary; older binaries reject `inline:` as a bad manifest.
- The control plane is the authenticated one (TLS + ed25519) for off-box use — `push`
  rides the same channel as every other op, so an authorized key is required.

## Trade vs. HTTP boot-pull

Push (this) — box fetches nothing; simplest for a disconnected box; re-push on recycle.
HTTP boot-pull (a manifest/URL ref) — box re-fetches from a store/URL itself, which is
better for *auto-boot* of a recycled box. They coexist: a service is a ref service or a
pushed service per its roster entry. Boot-from-store (store content-GET) graduates the
recycle/auto-boot story later; push is the immediate, dependency-free path.

[#220]: https://github.com/colinrozzi/theater/pull/220
