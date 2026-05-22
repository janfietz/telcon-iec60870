# iec60870-test-tools

A pair of long-running daemons — `iec-server` (outstation) and `iec-client`
(master) — for driving the [`iec60870`](../iec60870) crate from an agent or a
shell. Each daemon speaks either **IEC 60870-5-104** (TCP) or **IEC 60870-5-101**
(serial), and is controlled via a JSON-over-Unix-socket NDJSON protocol so
short-lived CLI subcommands can read state, mutate state, issue commands, and
move files.

This crate is for testing only — `publish = false`.

## Quickstart (104, TCP loopback)

```sh
# Terminal 1 — outstation
mkdir -p /tmp/iec-files
echo "123456789" > /tmp/iec-files/123456789      # NOF 0xBB3D, see below
cargo run -p iec60870-test-tools --bin iec-server -- \
    --control /tmp/iec-server.sock \
    daemon --transport tcp --addr 127.0.0.1:2404 \
    --files-dir /tmp/iec-files

# Terminal 2 — master
cargo run -p iec60870-test-tools --bin iec-client -- \
    daemon --transport tcp --addr 127.0.0.1:2404 \
    --control /tmp/iec-client.sock

# Terminal 3 — drive the system
iec-client interrogate --socket /tmp/iec-client.sock          # 50 points back
iec-server --control /tmp/iec-server.sock set --ioa 500 \
    --kind me-nc --value 42.0
iec-client read --ioa 500 --socket /tmp/iec-client.sock
iec-client cmd single --ioa 2100 --on --socket /tmp/iec-client.sock
iec-client file get --nof 47933 --socket /tmp/iec-client.sock # 0xBB3D
iec-client shutdown --socket /tmp/iec-client.sock
iec-server --control /tmp/iec-server.sock shutdown
```

## Quickstart (101, serial via socat pty)

```sh
# Terminal 1 — create a virtual serial pair
socat -d -d pty,raw,echo=0 pty,raw,echo=0
# socat prints two device paths, e.g. /dev/pts/3 and /dev/pts/4

# Terminal 2 — outstation
cargo run -p iec60870-test-tools --bin iec-server -- \
    --control /tmp/iec-server.sock \
    daemon --transport serial --serial /dev/pts/3 --baud 9600 \
    --link-addr 1

# Terminal 3 — master
cargo run -p iec60870-test-tools --bin iec-client -- \
    daemon --transport serial --serial /dev/pts/4 --baud 9600 \
    --link-addr 1 --control /tmp/iec-client.sock
```

File transfer is **not** supported on 101.

## Default IOA layout

The outstation pre-populates 50 points across all ten supported monitor types,
five points per type, with a simulator schedule attached to each so spontaneous
ASDUs flow without any external trigger.

| Range       | TypeID | Mnemonic   | Default simulator                                      |
|-------------|--------|------------|--------------------------------------------------------|
| 100 – 104   | 1      | M_SP_NA_1  | Toggle every 5 s, staggered phase                      |
| 200 – 204   | 3      | M_DP_NA_1  | Rotate Off ↔ On every 7 s                              |
| 300 – 304   | 9      | M_ME_NA_1  | Random walk ±0.05 / s, clamped to [-1, 1]              |
| 400 – 404   | 11     | M_ME_NB_1  | Step up by 1 every 2 s, wrap at 100                    |
| 500 – 504   | 13     | M_ME_NC_1  | Sine wave, period 10 s, amplitude 50, sampled at 500 ms |
| 1100 – 1104 | 30     | M_SP_TB_1  | Toggle 5 s (timestamped)                               |
| 1200 – 1204 | 31     | M_DP_TB_1  | Rotate 7 s (timestamped)                               |
| 1300 – 1304 | 34     | M_ME_TD_1  | Random walk (timestamped)                              |
| 1400 – 1404 | 35     | M_ME_TE_1  | Step up (timestamped)                                  |
| 1500 – 1504 | 36     | M_ME_TF_1  | Sine wave (timestamped)                                |

Commands sent by the master at any IOA are ACKed (ACTIVATION_CON +
ACTIVATION_TERMINATION) but do not auto-mirror to monitor points — the master
sees the ACK on the wire and the outstation logs an `AsduReceived` event, and
that's it. Use `iec-server set` to mutate monitor state explicitly.

## CLI surface

Run `iec-server --help` / `iec-client --help` for the full subcommand list.
The non-`daemon` subcommands are short-lived: they open the control socket,
send one JSON request, print the JSON response on stdout, and exit.

### `iec-server`

| Subcommand     | Purpose                                                    |
|----------------|------------------------------------------------------------|
| `daemon`       | Run the long-running outstation (blocks until `shutdown`)  |
| `get`          | Read one IOA's current value                               |
| `set`          | Set one IOA's value (also emits a spontaneous ASDU)        |
| `list`         | List configured IOAs, optionally filtered by `--type-id`   |
| `sim get`      | Read one IOA's simulator schedule                          |
| `sim set`      | Configure one IOA's simulator schedule (JSON)              |
| `events`       | Stream events as NDJSON until interrupted                  |
| `status`       | Show daemon status (transport, peer count, point count)    |
| `shutdown`     | Stop the daemon                                            |

### `iec-client`

| Subcommand        | Purpose                                                      |
|-------------------|--------------------------------------------------------------|
| `daemon`          | Run the long-running master (blocks until `shutdown`)        |
| `interrogate`     | Send a general or group interrogation; collect responses     |
| `cmd single`      | Issue `C_SC_NA_1` (single command)                           |
| `cmd double`      | Issue `C_DC_NA_1` (double command)                           |
| `cmd regulating`  | Issue `C_RC_NA_1` (regulating-step, lower/higher)            |
| `cmd setpoint`    | Issue `C_SE_NA_1` / `NB_1` / `NC_1` (set-point, three kinds) |
| `read`            | Read the most recent cached value for an IOA                 |
| `file get`        | Pull a file (104 only)                                       |
| `file put`        | Push a file (104 only)                                       |
| `events`          | Stream events as NDJSON until interrupted                    |
| `status`          | Show daemon status                                           |
| `shutdown`        | Stop the daemon                                              |

## JSON shapes

The complete request / response / event types are defined in
[`src/wire.rs`](src/wire.rs). A few highlights:

```jsonc
// Request — set IOA 500 to 42.0 (float)
{"op":"set","ioa":500,"value":{"kind":"float","value":42.0}}

// Request — single command (direct execute, on)
{"op":"cmd_single","ioa":2100,"on":true}

// Request — general interrogation, 5 s timeout
{"op":"interrogate","timeout_ms":5000}

// Response — successful get
{"ok":true,"ioa":500,"kind":"me_nc",
 "value":{"kind":"float","value":42.0},
 "quality":{"overflow":false,"blocked":false,"substituted":false,
            "not_topical":false,"invalid":false}}

// Streaming event — spontaneous send observed by the master
{"event":"asdu_received","cot":"spontaneous","type_id":13,"ioa":500,
 "value":{"kind":"float","value":29.39}}
```

## NameOfFile (NOF) computation

`FsFileTransferProvider` maps each on-disk filename to a 16-bit
[`NameOfFile`][1] via CRC-16/IBM of the relative path. The classic test
vector — file named exactly `123456789` — hashes to `0xBB3D` (47933). If you
add files at different names, compute the CRC-16/IBM externally to know which
NOF to request.

[1]: ../iec60870-proto/src/asdu/types/file.rs

## Verifying

```sh
cargo build -p iec60870-test-tools --bins
cargo clippy -p iec60870-test-tools --all-targets --no-deps -- -D warnings
cargo test  -p iec60870-test-tools
```

The integration test (`tests/loopback_ctl.rs`) spawns both daemons as
subprocesses, drives them through every supported request type, and
round-trips a file through the FT layer.

## Known limitations (v1)

- Single common address per daemon (`--coa`, default 1).
- Single peer for the server (multiple TCP peers connect, but spontaneous
  sends broadcast to all and there's no per-peer state).
- File transfer is 104-only; on 101 the `file get` / `file put` subcommands
  will return an error response.
- Commands are direct-execute only; no select-before-execute sequence.
- Counter interrogation (`C_CI_NA_1`) is ACKed but not implemented beyond
  that — no integrated-totals point class.
- The control socket has no authentication. Whoever can `open()` the socket
  path can drive the daemon. Intentional for a test rig.
