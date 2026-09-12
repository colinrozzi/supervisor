# Control surface (v0.1) — live roster mutation

Status: **BUILT + proven E2E (2026-09-12)** on theater `4e4e67d3`. Deliberately boring:
**JSON over TCP, server in the supervisor actor, a thin native client in the CLI.**
No HTTP (theater has no http-*server* handler yet), no same-actor client, no auth.

Verified: `list`/`status`/`add`/`remove`/`chain` against a running supervisor — `add`
reconciles a spawn (new `current_id`), `remove` reconciles a `stop-actor` (child
`terminated (Stopped)`, dropped from the roster), `chain` returns the in-memory buffer.

Plus a **`chain <handle>`** op (Colin's add): returns a service's in-memory ring of
recent chain events (`{seq, type, data_hex}`). Populated when a service is watched
full-chain — i.e. it `record`s, or sets `keep_chain: true` (a per-service opt-in that
keeps the chain queryable without a sink). Capped at `CHAIN_BUF_MAX` (200).

## 1. Principle — edit desired, reconcile

A client never says "spawn this actor." It **edits the desired roster**, and the
existing reconcile loop drives reality to match. Live mutation is the same mechanism
as crash-restart, just triggered by a control edit instead of a termination:

- **add** a service → it's now desired-but-absent → reconcile **spawns + monitors** it.
- **remove** a handle → it's now present-but-undesired → reconcile **stops it**
  (`runtime.stop-actor`) and forgets its status. *(This "stop-the-undesired" arm never
  fired in v0 because the roster was immutable; it comes alive here.)*
- **apply** a whole roster → swap desired → reconcile diffs and converges (GitOps shape).
- **list / status** → read desired + actual.

## 2. State reshape — `desired` vs `actual`

This is where the spec/status split finally earns its keep: the reconcile becomes a
real diff keyed by handle.

```rust
struct SupervisorState {
    desired: Map<String, ServiceSpec>,   // the roster (what a client edits)
    actual:  Map<String, ServiceStatus>, // observed (the supervisor owns this)
    control_port: Option<u16>,           // from init; None = no control surface
}
struct ServiceSpec   { manifest: String, max: u32, window_ms: u64, record: Option<Sink> }
struct ServiceStatus { current_id: String, restarts: Vec<u64>, blocked: bool }
```

`handle` is the join key between the two maps, not a field inside either.

## 3. Reconcile — now a real diff

Factor the loop into one `reconcile()` run on: init, each control edit, each
`handle-actor-event`, (later) a timer tick.

- `handle ∈ desired`, no live `actual` entry → **spawn + watch** (+ record sink), record status.
- `handle ∈ actual`, not in `desired` → **`stop-actor(current_id)`**, drop status. *(new)*
- terminal event for a desired handle → **respawn** per its restart policy (rate-limited). *(v0 behavior)*

Because an actor is sequential, control edits serialize with lifecycle/reconcile
events automatically — no races on `desired`, no locking.

## 4. Transport + protocol

- The supervisor manifest gains a **`tcp` handler**; init config carries a **control
  port**. On init it `tcp.listen`s and accept-loops. Each connection = one request.
- **Wire:** one JSON request per connection → one JSON reply → close. (Keep-alive/framing
  can come later; one-shot is simplest.)
- **Requests** (the `service` / roster shape is identical to the init roster entry —
  one schema everywhere):

  ```json
  {"op":"list"}
  {"op":"status","handle":"crasher"}
  {"op":"add","service":{"handle":"worker","manifest":"./worker.toml","max":5}}
  {"op":"remove","handle":"crasher"}
  {"op":"apply","services":[ … full roster … ]}
  ```
- **Replies:** `{"ok":true, …}` or `{"ok":false,"error":"…"}`. `list`/`status` return
  `{handle, spec, status}` entries (desired + observed together).

## 5. CLI

`spawn` is unchanged (it hosts the supervisor; pass `--control-port N` to open the
surface). The control verbs are a **thin native TCP client** — connect, send the op,
print the reply — no embedded runtime:

```
supervisor spawn roster.json --control-port 9000     # server (hosts + listens)
supervisor list                 --port 9000
supervisor status crasher       --port 9000
supervisor add worker ./w.toml  --port 9000 [--max 5 --window-ms 60000]
supervisor remove crasher       --port 9000
```

(If we ever want zero schema-drift between client and server, a shared `proto` crate
with the op/roster serde types — used by both the wasm actor and the native client —
is the lever. Not needed to start; the op shape is a handful of fields.)

## 6. Deferred (all "another feed for the same desired state")

- **Auth / non-localhost.** v0.1 is localhost, unauthenticated.
- **HTTP surface.** Nicer/REST-ier, but needs theater to port an http-*server* handler
  (only http-*client* exists). TCP now; HTTP later if wanted.
- **apply-from-git** (GitOps): a reconciler that `apply`s a roster pulled from a repo.
- **Hot-swap / deploy:** edit a service's manifest → reconcile (stop-old/spawn-new).
- **supervisor-to-supervisor** (federation): the one real case for a supervisor acting
  as a *client*. Revisit on purpose if/when hierarchy is real — not as a CLI shortcut.

## 7. Build order

1. State reshape → `desired`/`actual` maps; extract `reconcile()`.
2. Reconcile-with-stop (the new undesired→stop arm).
3. `tcp` listen/accept loop + op handler (parse → mutate desired → reconcile → reply).
4. Manifest: `tcp` handler + control port in init (opt-in; absent = pure v0).
5. CLI: native client subcommands + `--port`.
