#!/usr/bin/env bash
# Orchestrates every test_*.sh in this directory and reports a summary.
#
# Exit code:
#   0 — all tests passed
#   1 — at least one failed
#   2 — runner setup error
#
# Environment:
#   RUST_LOG       — passed to each daemon (default: "iec60870=warn,iec60870_test_tools=info")
#   KEEP_WORKDIR=1 — preserve $WORKDIR on each test (forwarded)
#   FAIL_FAST=1    — stop on first failure
#   FILTER=<glob>  — only run tests whose filename matches the glob (e.g. "test_0[1-5]_*")

set -uo pipefail
export LC_NUMERIC=C  # printf %.1f wants a dot, not the locale decimal

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"

c_reset=$'\e[0m'
c_red=$'\e[31m'
c_green=$'\e[32m'
c_yellow=$'\e[33m'
c_dim=$'\e[2m'
c_bold=$'\e[1m'

# Bail early if binaries are missing.
SERVER_BIN="${IEC_SERVER_BIN:-$WT_ROOT/target/debug/iec-server}"
CLIENT_BIN="${IEC_CLIENT_BIN:-$WT_ROOT/target/debug/iec-client}"
if [[ ! -x "$SERVER_BIN" || ! -x "$CLIENT_BIN" ]]; then
    echo "${c_red}error${c_reset}: binaries missing — run \`cargo build -p iec60870-test-tools --bins\` first" >&2
    exit 2
fi

filter="${FILTER:-test_*.sh}"
mapfile -t TESTS < <(cd "$SCRIPT_DIR" && ls -1 $filter 2>/dev/null | sort)
if [[ ${#TESTS[@]} -eq 0 ]]; then
    echo "${c_red}error${c_reset}: no test scripts matched filter '$filter'" >&2
    exit 2
fi

passed=0
failed=0
declare -a FAIL_NAMES

printf "${c_bold}== running %d test specs against the test-tools binaries ==${c_reset}\n" "${#TESTS[@]}"
printf "${c_dim}server=%s%s\n" "$SERVER_BIN" "$c_reset"
printf "${c_dim}client=%s%s\n\n" "$CLIENT_BIN" "$c_reset"

for tst in "${TESTS[@]}"; do
    name="${tst%.sh}"
    name="${name#test_}"
    started=$(date +%s.%N)
    out_file=$(mktemp)
    # Hide noisy setup logs unless verbose.
    if "$SCRIPT_DIR/$tst" >"$out_file" 2>&1; then
        elapsed=$(printf '%.1f' "$(echo "$(date +%s.%N) - $started" | bc)")
        printf "${c_green}PASS${c_reset} %-30s ${c_dim}(%ss)${c_reset}\n" "$name" "$elapsed"
        passed=$((passed + 1))
    else
        elapsed=$(printf '%.1f' "$(echo "$(date +%s.%N) - $started" | bc)")
        printf "${c_red}FAIL${c_reset} %-30s ${c_dim}(%ss)${c_reset}\n" "$name" "$elapsed"
        # Tail the per-test output so the failure reason is visible.
        tail -10 "$out_file" | sed "s/^/${c_dim}  | ${c_reset}/"
        failed=$((failed + 1))
        FAIL_NAMES+=("$name")
        if [[ "${FAIL_FAST:-0}" == "1" ]]; then
            rm -f "$out_file"
            break
        fi
    fi
    rm -f "$out_file"
done

total=$((passed + failed))
printf "\n${c_bold}== summary: %d passed, %d failed out of %d ==${c_reset}\n" "$passed" "$failed" "$total"
if [[ "$failed" -gt 0 ]]; then
    printf "${c_red}failed:${c_reset} %s\n" "${FAIL_NAMES[*]}"
    exit 1
fi
printf "${c_green}all green${c_reset}\n"
exit 0
