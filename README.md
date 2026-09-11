# supervisor

Theater's userland supervisor — the OTP-`supervisor` for the theater actor runtime.

> **Migration in progress:** this project is the fresh start of what was
> **sentinel**. We're moving from `sentinel` → `supervisor` (name + identity +
> mailbox `supervisor-dev@colinrozzi.com`). The old `sentinel` repo is the
> reference we migrate *from*, not a codebase we mutate in place — this is a
> clean rebuild on the post-overhaul theater primitives.

## Why this exists (the identity)

Theater's recent overhaul **dissolved the supervisor _handler_** into two runtime
primitives:

- **`runtime`** — control: `spawn` / `resume` / `stop-actor` / `kill-actor` /
  `restart-actor` / `update-actor-package` (permission-gated).
- **`lifecycle`** — watching: a directed subscription `{subscriber, subject,
  filter, target}` where `target: stop-self` is a **link** (runtime-level
  fate-sharing) and `target: deliver-to-wasm` is a **monitor** (filtered events
  delivered to `handle-lifecycle-event`).

The runtime now holds **no lineage** — just a flat set of live actors. Supervision
is therefore *"a pattern, not a primitive"* (theater's own words): you compose it
from `runtime` (start/stop) + `lifecycle` (watch/link) + **policy that lives in the
actor**.

**But you shouldn't rewrite that pattern in every supervising actor** — Erlang
doesn't; it ships OTP `supervisor` as a library. **This project is that library**
(plus the canonical running instance). Supervision left the runtime and landed
here. So we name it what it is: `supervisor`.

## Shape (two things)

1. **The library** (`supervisor/`) — a reusable guest-side component any actor
   composes to become a supervisor: spawn children via `runtime`, monitor them via
   `lifecycle`, apply a restart strategy (rate-limit / intensity), track its own
   direct-children set + view-scope, and record their chains (the black box).
2. **The instance** — a running supervisor actor (the fleet's supervisor of prod
   nodes: mesh nodes, the mail spine, etc.), which is just an actor that composes
   (1) + a control face.

## Facets, unified under "supervision"

What used to read as separate sentinel features are all *things a supervisor does*:

- **Flight recorder** = the supervisor's **black box** — accumulate a child's
  chain via a filtered `lifecycle` monitor; seal + persist on the terminal event;
  serve it on query (post-mortem debugging for the fleet).
- **Deploy hook** = **supervised hot-swap** — `runtime.update-actor-package` on a
  supervised child.
- **Mesh control face** = **driving the supervisor remotely** — the RSM
  control-plane (`control-sm` rules + a mesh-faced system), so other actors manage
  children over the mesh.

## Status

KICKOFF — migration from `sentinel` beginning (see `docs/DESIGN.md`). Reference:
the `sentinel` repo (legacy phase-1/3 TCP supervisor + the `control/` RSM
control-plane + `docs/mesh-supervision-spec.md`).
