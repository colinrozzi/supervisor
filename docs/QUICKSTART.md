# Using the supervisor — quickstart

The supervisor keeps a **declared set of actors running**: you give it a roster
(each service = a handle + a manifest + a restart policy), it spawns them, and it
**respawns any that crash** (rate-limited, so a crash loop trips a circuit breaker
instead of thrashing). It can **record** an actor's chain to a file/HTTP sink, and
you can **edit the roster live** over a control port. It's one portable binary that
embeds the theater runtime — no separate `theater` install.

## 1. Install

The release binary is static (runs in any container). Grab the upgrade wrapper once,
then it self-updates:

```sh
curl -fsSL https://raw.githubusercontent.com/colinrozzi/supervisor/main/dist/supervisor-upgrade \
  -o ~/.local/bin/supervisor-upgrade && chmod +x ~/.local/bin/supervisor-upgrade
supervisor-upgrade            # installs the latest release → ~/.local/bin/supervisor
supervisor --help
```

(Or one-shot, if `gh` is authed: `gh release download --repo colinrozzi/supervisor
--pattern supervisor -O ~/.local/bin/supervisor && chmod +x ~/.local/bin/supervisor`.)

After the first install, the binary self-updates — no wrapper needed:

```sh
supervisor upgrade            # download + checksum-verify + atomic-replace from the latest release
```

## 2. Write a roster

JSON — one entry per actor you want kept alive. `manifest` is a path or an
`http(s)://` ref to that actor's `manifest.toml` (so it can live in a git repo).

```json
{
  "services": [
    { "handle": "acceptor", "manifest": "/srv/inbox/acceptor.toml", "max": 5, "window_ms": 60000 },
    { "handle": "router",   "manifest": "/srv/inbox/router.toml",
      "record": { "kind": "file", "path": "router.chain.jsonl" } }
  ]
}
```

- `max` / `window_ms` — restart policy: at most `max` restarts within `window_ms`, then
  the service is **BLOCKED** (crash-loop breaker). Defaults: 5 / 60000.
- `record` (optional) — `{"kind":"http","url":…}` or `{"kind":"file","path":…}`: writes
  every chain event to the sink (the flight-recorder). `"keep_chain": true` instead keeps
  the chain in memory, queryable via `supervisor chain <handle>`.

## 3. Run it

```sh
supervisor spawn roster.json --control-port        # spawns the roster, opens control on :9000
```

- The supervisor **respawns only on a real crash** (`Failed`) — a clean exit / an
  intentional stop is left down.
- Watch what's happening: `--chain pretty` (decoded event stream) and/or `--logs info`
  (theater runtime logs). Omit both for quiet.
- For a file `record` sink, add `--record-dir <sandbox-root>`.
- In production, run this line as your service unit (systemd, etc.) — it *is* the host
  process for the supervised tree.

## 4. Drive it live (from another shell)

```sh
supervisor list                          # the roster + status (running/blocked/restarts)
supervisor status acceptor
supervisor add worker /srv/inbox/worker.toml   # add → it spawns + supervises it
supervisor remove worker                        # remove → it stops it
supervisor apply new-roster.json                # replace the whole roster (diff + reconcile)
supervisor chain router                         # dump a recorded/kept chain
```

Every edit changes the *desired* roster and the supervisor reconciles reality to it —
GitOps for actors. (`--port N` on any verb if you didn't use the default 9000.)

## Notes

- **One supervisor per tree** — it owns "who I run" in its own state; run one per system.
- **The manifest carries everything** — package + init state + handlers — so the supervisor
  just needs the ref; it doesn't need to know your actor's internals.
- **It's the host.** `supervisor spawn` runs the theater runtime in-process and spawns your
  actors under it; there's no separate `theater` daemon.
- Prod cutovers of an existing live service (e.g. moving inbox under a supervisor) are
  coordinated with the manager — dev/experiment freely, but sequence a prod re-parent.
