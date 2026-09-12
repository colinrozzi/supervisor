#!/usr/bin/env bash
# Experiment: the `record` arg (flight-recorder / black box).
#
# A service with `record: { url }` is watched on its FULL chain; the supervisor
# POSTs every chain event to the sink URL via the http-client handler. This runs
# a local std-only collector, points a recording crasher at it, and shows every
# event landing (http 200) while the reconcile loop still trips the rate-limiter.
#
# Requirements (same as ../reconcile-loop/run.sh):
#   $THEATER = a post-#206 theater host; a wasm-capable rust toolchain ($CARGO);
#   gcc on PATH (nix shell nixpkgs#gcc) for host build-scripts + linking the collector.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/../.." && pwd)
EXP="$REPO/experiments/record"
CHILD="$REPO/experiments/reconcile-loop/crash-child"   # reuse the self-terminating child
RUNDIR="${RUNDIR:-/tmp/supervisor-record}"
PORT="${PORT:-8899}"
mkdir -p "$RUNDIR"

CARGO="${CARGO:-$(ls -d /nix/store/*-rust-default-*/bin/cargo 2>/dev/null \
  | while read -r c; do tc=$(dirname "$(dirname "$c")"); \
      [ -e "$tc/lib/rustlib/wasm32-unknown-unknown" ] && echo "$c" && break; done)}"
[ -n "${CARGO:-}" ] || { echo "no rust toolchain with wasm32 target; set \$CARGO"; exit 1; }
export PATH="$(dirname "$CARGO"):$PATH"
RUSTC="$(dirname "$CARGO")/rustc"
THEATER="${THEATER:-$(command -v theater || true)}"
[ -n "${THEATER:-}" ] || { echo "set \$THEATER to a post-#206 theater binary"; exit 1; }
export CARGO_HOME="${CARGO_HOME:-$RUNDIR/cargo-home}"

echo "=== build supervisor + crash-child + collector ==="
( cd "$REPO" && "$CARGO" build --target wasm32-unknown-unknown --release )
( cd "$CHILD" && "$CARGO" build --target wasm32-unknown-unknown --release )
"$RUSTC" -O -C linker=gcc "$EXP/collector.rs" -o "$RUNDIR/collector"

SUP_WASM="$REPO/target/wasm32-unknown-unknown/release/supervisor.wasm"
CHILD_WASM="$CHILD/target/wasm32-unknown-unknown/release/crash_child.wasm"
CAP="$RUNDIR/capture.jsonl"; : > "$CAP"

cat > "$RUNDIR/crash-child.toml" <<EOF
name = "crash-child"
version = "0.1.0"
package = "$CHILD_WASM"
[[handler]]
type = "self"
[[handler]]
type = "timer"
EOF

cat > "$RUNDIR/supervisor-record.toml" <<EOF
name = "supervisor"
version = "0.0.1"
package = "$SUP_WASM"
initial_state = '{"services":[{"handle":"crasher","manifest":"$RUNDIR/crash-child.toml","max":2,"window_ms":60000,"record":{"kind":"http","url":"http://127.0.0.1:$PORT/"}}]}'

[permission_policy.runtime]
type = "restrict"
config = { inspect = true, mutate = true }

[[handler]]
type = "self"
[[handler]]
type = "runtime"
[[handler]]
type = "lifecycle"
[[handler]]
type = "timer"
[[handler]]
type = "http-client"
allowed_hosts = ["127.0.0.1"]
EOF

echo "=== start collector on 127.0.0.1:$PORT ==="
"$RUNDIR/collector" "127.0.0.1:$PORT" "$CAP" &
COLL=$!; trap 'kill $COLL 2>/dev/null || true' EXIT
sleep 1

echo "=== run (18s) ==="
export THEATER_HOME="${THEATER_HOME:-$RUNDIR/theater-home}"
timeout 18 "$THEATER" spawn "$RUNDIR/supervisor-record.toml" --events --events-format short --log-level warn \
  | grep -E 'recorded crasher|record POST failed|terminated —|BLOCKED' | grep -v ChainEventPayload || true

echo "=== captured events ($(wc -l < "$CAP") lines) — types: ==="
grep -oE '"type":"[^"]*"' "$CAP" || true
