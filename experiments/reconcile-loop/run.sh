#!/usr/bin/env bash
# Experiment 1 — prove the reconcile loop.
#
# Builds the supervisor + a self-terminating crash-child, generates the two
# manifests with absolute paths, and runs them on a local theater host. Watch:
#   spawn crasher -> monitor -> child self-terminates -> handle-lifecycle-event
#   -> reconcile respawn (x5) -> 6th termination trips the rate-limiter -> BLOCKED,
#   and the supervisor survives.
#
# Requirements:
#   - a rust toolchain WITH the wasm32-unknown-unknown target (auto-resolved from
#     the nix store below; override with $CARGO)
#   - a post-#204 theater host binary with `theater spawn` (set $THEATER)
#   - gcc on PATH for host build-scripts (e.g. `nix shell nixpkgs#gcc`)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/../.." && pwd)
EXP="$REPO/experiments/reconcile-loop"
RUNDIR="${RUNDIR:-/tmp/supervisor-exp1}"
mkdir -p "$RUNDIR"

# --- toolchain (needs wasm32 std) ---
CARGO="${CARGO:-$(ls -d /nix/store/*-rust-default-*/bin/cargo 2>/dev/null \
  | while read -r c; do tc=$(dirname "$(dirname "$c")"); \
      [ -e "$tc/lib/rustlib/wasm32-unknown-unknown" ] && echo "$c" && break; done)}"
[ -n "${CARGO:-}" ] || { echo "no rust toolchain with wasm32 target; set \$CARGO"; exit 1; }
echo "cargo: $CARGO"

# --- theater host ---
THEATER="${THEATER:-$(command -v theater || true)}"
[ -n "${THEATER:-}" ] || { echo "set \$THEATER to a post-#204 theater binary"; exit 1; }
echo "theater: $THEATER ($("$THEATER" --version 2>/dev/null))"

export CARGO_HOME="${CARGO_HOME:-$RUNDIR/cargo-home}"

echo "=== build supervisor ==="
( cd "$REPO" && "$CARGO" build --target wasm32-unknown-unknown --release )
echo "=== build crash-child ==="
( cd "$EXP/crash-child" && "$CARGO" build --target wasm32-unknown-unknown --release )

SUP_WASM="$REPO/target/wasm32-unknown-unknown/release/supervisor.wasm"
CHILD_WASM="$EXP/crash-child/target/wasm32-unknown-unknown/release/crash_child.wasm"

# --- generate manifests with absolute paths ---
cat > "$RUNDIR/crash-child.toml" <<EOF
name = "crash-child"
version = "0.1.0"
package = "$CHILD_WASM"

[[handler]]
type = "self"

[[handler]]
type = "timer"
EOF

cat > "$RUNDIR/supervisor.toml" <<EOF
name = "supervisor"
version = "0.0.1"
package = "$SUP_WASM"
initial_state = '{"services":[{"handle":"crasher","manifest":"$RUNDIR/crash-child.toml","max":5,"window_ms":60000}]}'

# The CONTROL capability (theater:simple/runtime) defaults to Disallow — grant it.
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
EOF

echo "=== run (25s; supervisor should survive) ==="
export THEATER_HOME="${THEATER_HOME:-$RUNDIR/theater-home}"
timeout 25 "$THEATER" spawn "$RUNDIR/supervisor.toml" --events --events-format short --log-level warn \
  | grep -E '\[supervisor\]|\[crash-child\]' | grep -v ChainEventPayload || true
