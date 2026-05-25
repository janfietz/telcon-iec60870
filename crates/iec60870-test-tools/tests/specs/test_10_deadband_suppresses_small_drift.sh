#!/usr/bin/env bash
# Spec 10 — A configured deadband suppresses below-threshold spontaneous
# emissions.
#
# Setup: configure IOA 300 (a normalized random-walk point with step
# 0.05) with `Percent { pct: 200, floor: 1.0 }`. The threshold becomes
# max(|last|, 1.0) * 2.0 = 2.0 in engineering units — vastly larger
# than any single random-walk step of 0.05 (and any plausible drift
# over a few seconds), so the simulator should suppress every tick
# after the first-sample emit.

source "$(dirname "$0")/lib.sh"
setup_test "10_deadband_suppresses_small_drift"

start_server
start_client

# Configure a very large percent threshold so almost every tick is suppressed.
srv_resp=$(srv deadband set --ioa 300 \
    --policy '{"kind":"percent","pct":200.0,"floor":1.0}')
assert_ok "$srv_resp" "set deadband policy on IOA 300"

# Subscribe to client events.
EVENTS_FILE="$WORKDIR/client-events.jsonl"
"$CLIENT_BIN" events --socket "$CSOCK" > "$EVENTS_FILE" 2>&1 &
EV_PID=$!
trap '[[ -n "${EV_PID:-}" ]] && kill "$EV_PID" 2>/dev/null || true; teardown' EXIT

# Collect for ~3 seconds (~3 random-walk ticks at 1000 ms each).
sleep 3.0
kill "$EV_PID" 2>/dev/null || true
sleep 0.1

# Count spontaneous M_ME_NA_1 (TypeID 9) events for IOA 300.
n_spont_300=$(jq -c --argjson tid 9 \
    'select(.event=="asdu_received" and .cot=="3" and .type_id==$tid and .ioa==300)' \
    "$EVENTS_FILE" | wc -l)

# We allow up to 2 emits (first sample + possibly one large-drift outlier).
# Without the deadband we would expect ~3 (one per tick).
[[ "$n_spont_300" -le 2 ]] || \
    fail "deadband ineffective: $n_spont_300 spontaneous emits for IOA 300 (expected ≤ 2)"

pass
