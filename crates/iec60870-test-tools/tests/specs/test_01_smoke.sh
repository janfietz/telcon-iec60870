#!/usr/bin/env bash
# Spec 01 — Smoke. Bring up both daemons, basic ops, clean shutdown.

source "$(dirname "$0")/lib.sh"
setup_test "01_smoke"

start_server
start_client

# 1. Server status reports 1 peer + 50 points.
resp=$(srv status)
assert_ok "$resp" "server status"
assert_jq "$resp" '.peers'  '1'  "peer count"
assert_jq "$resp" '.points' '50' "point count"

# 2. Client status reports running.
resp=$(cli status)
assert_ok "$resp" "client status"
assert_jq "$resp" '.status' 'running' "client running"

# 3. Server list returns 50 points.
resp=$(srv list)
assert_ok "$resp" "list"
assert_eq "$(echo "$resp" | jq '.points | length')" 50 "list length"

# 4. Server get on a default IOA succeeds.
resp=$(srv get --ioa 100)
assert_ok "$resp" "get ioa=100"
assert_jq "$resp" '.kind' 'sp_na' "ioa 100 kind"

# 5. Clean shutdown via control socket.
resp=$(cli shutdown)
assert_ok "$resp" "client shutdown"
resp=$(srv shutdown)
assert_ok "$resp" "server shutdown"

# Daemons should exit on their own; teardown will reap.
sleep 0.3
if kill -0 "$SERVER_PID" 2>/dev/null; then fail "server still running after shutdown"; fi
if kill -0 "$CLIENT_PID" 2>/dev/null; then fail "client still running after shutdown"; fi

pass
