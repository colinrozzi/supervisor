# 3-node HA store — supervisor-side hosting (rosters + unit)

The supervised, boot-durable wrapper for the fleet distribution store (store-dev's
converged 3-node design @ store repo /repo/ha-3node, 93f66c5). One supervisor per box
hosts that box's peer: `{index-node, content-holder}` as roster entries.

- **peerN-roster.json** — box N's roster. Each node: crash-restart with a bumped breaker
  (`max=10/60s`, vs the default 5 — a silent block = a down store dependency) + `keep_chain`
  (an in-memory black-box, queryable via `supervisor chain <handle>`).
- **store-supervisor@.service** — per-box systemd unit. Restart=always + boot-enable ⇒
  reboot durability; the roster's reconcile ⇒ crash-restart.

Deploy per box: place store-dev's WG-filled `peerN-index.toml`/`peerN-holder.toml` (+ the
node wasms + the static `store` CLI) under `/etc/store/`, drop `peerN-roster.json` as
`/etc/store/roster.json`, install + `systemctl enable --now store-supervisor`.

Boundary: supervision = per-box process resilience + reboot survival ONLY. Cross-machine
HA (read/content) = the store's RF=3 replication; write-HA = the genesis multi-writer
allow-list (`store init --allow`). Reconnect = mesh-dev's self-healing dial. All orthogonal.

NOT depending on the wireguard addresses: those live in store-dev's node manifests; these
rosters only reference the manifests by path, so they're final now. Optional later: an
authed control surface (remote-manage the store roster like the inbox) + a file-sink record.
