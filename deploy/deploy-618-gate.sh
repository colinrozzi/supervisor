#!/usr/bin/env bash
# deploy-618-gate.sh — coordinated #618 bump of a mesh-hub supervisor via an init-flood gate.
#
# WHY: the #618 fd-fix (#225/#229, in the re-pinned supervisor-v0.4.6 @ theater 1019120) is
# correct + wanted, but a cold restart of the chat hub under the live fleet re-mesh floods
# the composite's actor.init with inbound peer gossip on the sequential op-loop → init is
# STARVED and never returns (binary-agnostic flood-saturation; even v0.4.2 stalls under a
# heavy enough flood). This script gates INBOUND to the hub's mesh listener during the fresh
# binary's init so init runs flood-free, then OPENS the gate once init has returned
# (control port bound) so peers reconnect into the already-initialized hub.
#
# This is a STOPGAP deploy path until (A) mesh-dev defers listen/dial out of init + (B)
# theater's init-gate land (and the client-centric chat redesign removes the N-node flood at
# the root). ONE coordinated cutover — NOT a restart-storm (rapid restarts escalate the flood).
#
# RUN ON THE BOX AS ROOT (claude). Confirm the PLACEHOLDERS below against the real unit first.
#
# SAFETY: the iptables DROP rule is removed on EVERY exit path (trap) — leaving it would
# permanently sever the hub's mesh. Any failure rolls back to the known-good OLD binary.
set -uo pipefail

# ---- CONFIRM THESE AGAINST THE REAL UNIT BEFORE RUNNING -----------------------------------
UNIT="${UNIT:-chat-supervisor}"
NEW_BIN="${NEW_BIN:-/usr/local/bin/supervisor-v0.4.6}"   # re-pinned theater 1019120 build (#618 fix)
OLD_BIN="${OLD_BIN:-/usr/local/bin/supervisor-v0.4.2}"   # known-good rollback
RUN_SH="${RUN_SH:-/var/lib/chat-supervisor/run.sh}"      # the unit's ExecStart script (points at the binary)
GATE_PORTS="${GATE_PORTS:-9201}"    # mesh-peer INBOUND listener(s) to gate during init.
                                    #   :9201 = the hub node's mesh listener (the flood port from the trace).
                                    #   ADD :9610 if the driver gateway also takes mesh-peer inbound — CONFIRM w/ chat-dev.
                                    #   Do NOT gate :443 (chat-web, a separate single actor, #225-covered) or :9200 (control).
READY_PORT="${READY_PORT:-9200}"    # supervisor control port — binds only once actor.init has RETURNED (the signal).
READY_TIMEOUT="${READY_TIMEOUT:-120}"   # max wait (s) for init to return behind the gate.
FREE_TIMEOUT="${FREE_TIMEOUT:-90}"      # max wait (s) for old sockets (incl. TIME_WAIT) to clear before rebind.
# -------------------------------------------------------------------------------------------

log(){ printf '%s deploy-618-gate: %s\n' "$(date -u +%H:%M:%S)" "$*"; }

add_gate(){ local p; for p in $GATE_PORTS; do iptables -I INPUT -p tcp --dport "$p" -j DROP; done; }
del_gate(){ local p; for p in $GATE_PORTS; do iptables -D INPUT -p tcp --dport "$p" -j DROP 2>/dev/null || true; done; }

# CRITICAL: always remove the gate on exit (success, failure, or signal). Idempotent.
GATE_ON=0
cleanup(){ if [ "$GATE_ON" = 1 ]; then log "TRAP cleanup: removing gate (belt-and-suspenders)"; del_gate; GATE_ON=0; fi; }
trap cleanup EXIT INT TERM

listening(){ ss -ltnH "sport = :$1" 2>/dev/null | grep -q LISTEN; }       # is anything LISTENing on port?
any_socket(){ ss -tanH "sport = :$1" 2>/dev/null | grep -q .; }            # any socket at all (incl TIME_WAIT)?
point_run_sh(){ sed -i -E "s#(/usr/local/bin/supervisor-v)[0-9][0-9.]*#\\1${1##*supervisor-v}#g" "$RUN_SH"; }

rollback(){
  log "ROLLBACK -> $OLD_BIN"
  point_run_sh "$OLD_BIN"
  systemctl restart "$UNIT"; sleep 3
  if listening "$READY_PORT"; then log "rollback OK: :$READY_PORT bound (old binary serving)"; else log "WARN rollback: :$READY_PORT NOT bound — INVESTIGATE"; fi
}

main(){
  log "START UNIT=$UNIT NEW=$NEW_BIN gate=[$GATE_PORTS] ready=:$READY_PORT"
  [ -x "$NEW_BIN" ] || { log "FATAL new binary missing/not exec: $NEW_BIN"; exit 2; }

  # 1. stop old; wait for ALL involved ports fully free (incl TIME_WAIT, since tcp_listen has
  #    no SO_REUSEADDR yet — theater-dev's SO_REUSEADDR fix will remove this wait later).
  systemctl stop "$UNIT"; log "stopped $UNIT"
  local p i
  for p in $GATE_PORTS $READY_PORT; do
    for ((i=0; i<FREE_TIMEOUT; i+=2)); do any_socket "$p" || break; sleep 2; done
    if any_socket "$p"; then log "FATAL :$p still has a socket after ${FREE_TIMEOUT}s (TIME_WAIT?) — aborting before any gate/start"; exit 3; fi
  done
  log "ports free: [$GATE_PORTS $READY_PORT]"

  # 2. GATE the mesh inbound BEFORE starting the new binary (so init runs flood-free).
  add_gate; GATE_ON=1; log "GATE UP — DROP inbound :[$GATE_PORTS]; fleet peers can't flood init"

  # 3. point run.sh at the new binary + start.
  point_run_sh "$NEW_BIN"; systemctl start "$UNIT"; log "started $NEW_BIN"

  # 4. wait for the init-returned signal (control port bound).
  for ((i=0; i<READY_TIMEOUT; i+=2)); do listening "$READY_PORT" && { log "init RETURNED — :$READY_PORT bound after ~${i}s"; break; }; sleep 2; done
  if ! listening "$READY_PORT"; then
    log "FATAL init did NOT return (:$READY_PORT unbound after ${READY_TIMEOUT}s) — gate did not help / other issue"
    rollback; exit 4    # trap removes the gate
  fi

  # 5. OPEN the gate — peers reconnect into the now-initialized hub.
  del_gate; GATE_ON=0; log "GATE DOWN — :[$GATE_PORTS] open; peers reconnecting into initialized hub"

  # 6. verify.
  sleep 5
  listening "$READY_PORT" && log "VERIFY: :$READY_PORT still serving post-open" || log "WARN :$READY_PORT dropped after opening gate — INVESTIGATE (flood may still bite; consider (A)+(B) before retry)"
  log "DONE — #618 bump deployed on $UNIT via the init-flood gate."
  log "POST-CHECKS (manual): mesh members climb; chat.colinrozzi.com 200; a :9200 control-plane restart of a listening actor rebinds cleanly (the #618 win)."
}
main
