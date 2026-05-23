#!/usr/bin/env bash
# Spec 12 — An explicit Set (server-side) still produces a SPONTANEOUS
# ASDU even when an enormous deadband is configured. The Set path is
# not gated by the deadband (only the simulator tick path is).

source "$(dirname "$0")/lib.sh"
setup_test "12_deadband_set_still_emits"

start_server
start_client

# Disable the simulator on IOA 400 so we control all sources of change.
sim_resp=$(srv sim set --ioa 400 --schedule '{"kind":"none"}')
assert_ok "$sim_resp" "disable simulator on IOA 400"

# Configure a huge threshold so no tick-driven spontaneous would ever fire.
db_resp=$(srv deadband set --ioa 400 \
    --policy '{"kind":"absolute","delta":1000000000.0}')
assert_ok "$db_resp" "set huge deadband on IOA 400"

# Subscribe to client events.
EVENTS_FILE="$WORKDIR/client-events.jsonl"
"$CLIENT_BIN" events --socket "$CSOCK" > "$EVENTS_FILE" 2>&1 &
EV_PID=$!
trap '[[ -n "${EV_PID:-}" ]] && kill "$EV_PID" 2>/dev/null || true; teardown' EXIT

# Give the events subscriber a moment to attach.
sleep 0.4

# Trigger an explicit Set on the server side.
set_resp=$(srv set --ioa 400 --kind me-nb --value 42)
assert_ok "$set_resp" "explicit set IOA 400 to 42"

# Give time for the ASDU to traverse and the client to log it.
sleep 0.6
kill "$EV_PID" 2>/dev/null || true
sleep 0.1

# Expect at least one spontaneous M_ME_NB_1 (TypeID 11) event for IOA 400.
n_spont_400=$(jq -c --argjson tid 11 \
    'select(.event=="asdu_received" and .cot=="3" and .type_id==$tid and .ioa==400)' \
    "$EVENTS_FILE" | wc -l)
[[ "$n_spont_400" -ge 1 ]] || \
    fail "Set was suppressed by deadband: no spontaneous event for IOA 400 (n=$n_spont_400, events=$EVENTS_FILE)"

pass
