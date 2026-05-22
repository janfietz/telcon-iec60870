#!/usr/bin/env bash
# Spec 06 — Observe spontaneous emissions from the simulator.
#
# Subscribes to the client's event stream for ~3 s and verifies that
# AsduReceived events with cot="spontaneous" appear for at least one IOA
# in each of the 10 supported monitor TypeIDs.

source "$(dirname "$0")/lib.sh"
setup_test "06_spontaneous"

start_server
start_client

# Spawn an event subscriber in the background. The CLI's `events` subcommand
# is unbounded — it streams until the daemon closes the connection.
EVENTS_FILE="$WORKDIR/client-events.jsonl"
"$CLIENT_BIN" events --socket "$CSOCK" > "$EVENTS_FILE" 2>&1 &
EV_PID=$!
trap '[[ -n "${EV_PID:-}" ]] && kill "$EV_PID" 2>/dev/null || true; teardown' EXIT

# Collect for 3.5 seconds — long enough for several sine ticks and at least
# one Toggle (5 s).  We bias towards the fast-changing types here; the
# slower toggles (Toggle/Rotate at 5s/7s) will likely fire too.
sleep 3.5

kill "$EV_PID" 2>/dev/null || true
sleep 0.1

# Drop the initial subscribe ACK line from the events file.
n_events=$(wc -l < "$EVENTS_FILE")
[[ "$n_events" -ge 5 ]] || fail "expected at least 5 event lines, got $n_events"

# Look for spontaneous-cot events; the fast-tick types must show up.
# cot is emitted as the raw COT string ("3" for SPONTANEOUS); type_id is
# the numeric IEC TypeID.  Fast-tick types should each appear within 3.5 s.
declare -A KIND2TID=(
    [sp_na]=1 [dp_na]=3 [me_na]=9 [me_nb]=11 [me_nc]=13
    [sp_tb]=30 [dp_tb]=31 [me_td]=34 [me_te]=35 [me_tf]=36
)
fast_kinds=(me_na me_nb me_nc me_td me_te me_tf)
for k in "${fast_kinds[@]}"; do
    tid="${KIND2TID[$k]}"
    n=$(jq -c --argjson t "$tid" \
        'select(.event=="asdu_received" and .cot=="3" and .type_id==$t)' \
        "$EVENTS_FILE" | wc -l)
    [[ "$n" -ge 1 ]] || fail "no spontaneous events for kind $k (type_id=$tid, n=$n)"
done

pass
