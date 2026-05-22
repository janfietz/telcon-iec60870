#!/usr/bin/env bash
# Spec 02 — Set + read back every monitor TypeID via the server CLI.
#
# Verifies that the process image accepts a value of the correct kind for
# each of the 10 supported monitor TypeIDs and returns it via `get`.

source "$(dirname "$0")/lib.sh"
setup_test "02_point_types"

start_server
start_client

# (ioa, kind, value_arg, expected_value_kind, expected_value)
declare -a CASES=(
    "100 sp-na true       single     true"
    "200 dp-na on         double     on"
    "300 me-na -0.25      normalized -0.25"
    "400 me-nb 1234       scaled     1234"
    "500 me-nc 42.5       float      42.5"
    "1100 sp-tb false     single     false"
    "1200 dp-tb off       double     off"
    "1300 me-td 0.5       normalized 0.5"
    "1400 me-te -32       scaled     -32"
    "1500 me-tf -7.25     float      -7.25"
)

for case_line in "${CASES[@]}"; do
    # shellcheck disable=SC2206
    parts=($case_line)
    ioa="${parts[0]}"; kind="${parts[1]}"; value_arg="${parts[2]}"
    exp_kind="${parts[3]}"; exp_value="${parts[4]}"

    # Use --value=… form so clap doesn't try to parse negative numbers as flags.
    resp=$(srv set --ioa "$ioa" --kind "$kind" --value="$value_arg")
    assert_ok "$resp" "set ioa=$ioa kind=$kind"

    resp=$(srv get --ioa "$ioa")
    assert_ok "$resp" "get ioa=$ioa"
    assert_jq "$resp" '.value.kind'  "$exp_kind"  "ioa=$ioa value.kind"

    # numeric vs string comparison
    actual=$(echo "$resp" | jq -r '.value.value | tostring')
    case "$exp_kind" in
        normalized|float)
            # Tolerate fp roundtrip jitter
            diff=$(python3 -c "print(abs($actual - $exp_value))")
            ok=$(python3 -c "print('y' if $diff < 1e-4 else 'n')")
            [[ "$ok" == "y" ]] || fail "ioa=$ioa value drift: $actual vs $exp_value (Δ=$diff)"
            ;;
        *)
            assert_eq "$actual" "$exp_value" "ioa=$ioa value"
            ;;
    esac
done

pass
