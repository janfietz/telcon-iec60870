#!/usr/bin/env bash
# Spec 08 — Lifecycle. Start, shutdown via CLI, restart on the same socket
# path, second shutdown.

source "$(dirname "$0")/lib.sh"
setup_test "08_lifecycle"

start_server
start_client

# 1. Both daemons up.
assert_ok "$(srv status)" "first server status"
assert_ok "$(cli status)" "first client status"

# 2. Shutdown both via the control socket.
assert_ok "$(cli shutdown)" "first client shutdown"
assert_ok "$(srv shutdown)" "first server shutdown"

# 3. After a small grace period the daemon processes should be gone.
sleep 0.4
kill -0 "$SERVER_PID" 2>/dev/null && fail "server still running after shutdown"
kill -0 "$CLIENT_PID" 2>/dev/null && fail "client still running after shutdown"
# Clear so teardown doesn't try to kill them.
SERVER_PID=""; CLIENT_PID=""

# 4. The control socket files should be safely overwritable by a fresh
#    daemon — start_server's `if socket.exists() { remove }` logic.
start_server
start_client

assert_ok "$(srv status)" "restart server status"
assert_ok "$(cli status)" "restart client status"

pass
