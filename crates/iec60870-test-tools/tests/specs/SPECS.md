# IEC 60870-5 Test Specifications

Reproducible end-to-end test procedures for the `iec-server` (outstation)
and `iec-client` (master) test daemons. Each procedure is implemented as a
self-contained Bash script in this directory; this document describes the
intent, inputs, and expected outcomes — including the negative tests where
a *failure response* is the desired outcome.

## Running

```sh
# Build the binaries once.
cargo build -p iec60870-test-tools --bins

# Run every spec in sequence (~30 s on a fast machine).
./crates/iec60870-test-tools/tests/specs/run_all.sh

# Or one specific case:
./crates/iec60870-test-tools/tests/specs/test_01_smoke.sh

# Keep the temp workdir on success (useful for log forensics):
KEEP_WORKDIR=1 ./crates/iec60870-test-tools/tests/specs/test_07_file_transfer.sh

# Increase library logging when debugging:
RUST_LOG=iec60870=debug ./crates/iec60870-test-tools/tests/specs/test_01_smoke.sh
```

Each script prints exactly one of:

```
PASS <name>
FAIL <name> — <reason>
```

`run_all.sh` aggregates these into a final summary.

## Conventions

Every script:

- Sources `lib.sh` for shared helpers.
- Calls `setup_test "<name>"` which mints a unique workdir at
  `/tmp/iec-spec-<name>-<pid>-XXXX/`, picks a free TCP port, and registers an
  `EXIT` trap that gracefully shuts down both daemons and removes the workdir.
- Builds JSON requests by invoking the CLI subcommands (`srv …` / `cli …`)
  rather than crafting raw bytes — the procedures exercise the same path an
  agent would.
- Parses responses with `jq`; assertions use `assert_ok`, `assert_fail`,
  `assert_eq`, `assert_jq`.

Set `KEEP_WORKDIR=1` to preserve `$WORKDIR` for inspection (server.log,
client.log, server-files/, client-files/, server.sock, client.sock).

## Test catalogue

### Happy path (positive cases)

| # | Script | Goal | Key assertions |
|---|--------|------|----------------|
| 01 | `test_01_smoke.sh` | Smoke: bring up, hello-world ops, tear down | `status.peers=1`, `status.points=50`, clean shutdown |
| 02 | `test_02_point_types.sh` | Set + read back every monitor TypeID | Each of 10 ranges supports `set` with a correct value kind |
| 03 | `test_03_interrogation_general.sh` | C_IC_NA_1 (Qoi=20) returns full process image | `count==50`, COT=20 visible in server log |
| 04 | `test_04_interrogation_group.sh` | C_IC_NA_1 (Qoi=21..30) returns one TypeID range each | groups 1–10 each yield exactly 5 points |
| 05 | `test_05_commands.sh` | All control TypeIDs ACKed | C_SC, C_DC, C_RC, C_SE_NA/NB/NC each return `cot="activation_con"` `negative=false` |
| 06 | `test_06_spontaneous.sh` | Simulator emits spontaneous M_*_T*_1 ASDUs | events stream shows `asdu_received` with `cot="spontaneous"` for every type |
| 07 | `test_07_file_transfer.sh` | Round-trip a file via FT (104 only) | bytes==fixture size; F_SC/F_FR/F_SR/F_SG/F_LS/F_AF sequence in logs |
| 08 | `test_08_lifecycle.sh` | Graceful shutdown + restart cleans up sockets | `shutdown` exits 0, daemon process gone, socket removable |

### Negative path (expected failures)

These tests verify that **invalid input produces a clean error**, not a hang
or panic. They pass when the daemon returns `ok=false` with a non-empty
`error` string, exits non-zero, or closes the socket — depending on the case.

| # | Script | Bad input | Expected failure |
|---|--------|-----------|------------------|
| 09 | `test_09_failures.sh` | (multiple) | All of the below in one script: |
| 09.a | — | `get` on unknown IOA | `ok=false`, error mentions "not found" or "unknown" |
| 09.b | — | `set` with wrong `kind` for an IOA | `ok=false` with a type-mismatch error |
| 09.c | — | `cli file get` for unknown NOF | `ok=false`, error contains "idle timeout" (FT session times out at 30 s) |
| 09.d | — | `srv set --ioa N` where N isn't in the layout | `ok=false`, error mentions unknown IOA |
| 09.e | — | Server-side op invoked on the client daemon (`cli set …`) | impossible (no CLI subcommand), but `Request::Set` over the client socket → `ok=false`, error="not a client op" |
| 09.f | — | Bring up server on a port already in use | second daemon exits with `Address already in use` |

## Daemon configuration assumed by the specs

Server defaults populated by `setup_test`:

- Common Address (CA) = 1
- Bind address = `127.0.0.1:<random free port>`
- Files dir = `$WORKDIR/server-files/` (prepopulated with `123456789`
  containing 15 bytes — fixture for NOF 0xBB3D=47933)
- Control socket = `$WORKDIR/server.sock`

Client defaults:

- Connect address = the server's bind
- Control socket = `$WORKDIR/client.sock`
- Files dir = `$WORKDIR/client-files/`

## Default IOA layout (recap)

| Range       | TypeID | Mnemonic   | Kind for `set` `--kind` |
|-------------|--------|------------|--------------------------|
| 100 – 104   | 1      | M_SP_NA_1  | `sp-na` (bool)           |
| 200 – 204   | 3      | M_DP_NA_1  | `dp-na` (off/on)         |
| 300 – 304   | 9      | M_ME_NA_1  | `me-na` (f32 normalized) |
| 400 – 404   | 11     | M_ME_NB_1  | `me-nb` (i16 scaled)     |
| 500 – 504   | 13     | M_ME_NC_1  | `me-nc` (f32 float)      |
| 1100 – 1104 | 30     | M_SP_TB_1  | `sp-tb` (bool, with CP56)|
| 1200 – 1204 | 31     | M_DP_TB_1  | `dp-tb` (off/on, with CP56)|
| 1300 – 1304 | 34     | M_ME_TD_1  | `me-td` (f32 norm, with CP56)|
| 1400 – 1404 | 35     | M_ME_TE_1  | `me-te` (i16, with CP56) |
| 1500 – 1504 | 36     | M_ME_TF_1  | `me-tf` (f32, with CP56) |

Test fixtures (file transfer): NOF `47933` (=`0xBB3D`) is reserved by the
classic CRC-16/IBM test vector — a file literally named `123456789`.
