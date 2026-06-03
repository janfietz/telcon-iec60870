# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust workspace implementing the IEC 60870-5 telecontrol protocols (SCADA / electrical-grid).
Both **IEC 60870-5-104** (TCP/IP) and **IEC 60870-5-101** (serial, FT 1.2 framing) are implemented.

## Workspace layout

Three crates plus a `fuzz` package (excluded from the workspace):

- **`crates/iec60870-proto`** — Sans-I/O protocol core. No `async`, no sockets, **no clocks**: every
  state-machine entry point takes the current `Instant` explicitly. `#![forbid(unsafe_code)]`.
  - `asdu/` — ASDU codec shared by 101 and 104 (envelope, header, COT, information elements, type
    bodies under `asdu/types/{monitor,command,system,file}.rs`).
  - `frame104/` — APCI/APDU framing, the 15-bit wrapping `SeqNo` (`seq.rs`), and the connection
    state machine (`state.rs`: STARTDT/STOPDT/TESTFR, t0..t3, k/w window).
  - `frame101/` — FT 1.2 framing + link-layer state machine.
  - `file_transfer/` — file-transfer session logic (TypeIDs 120-126).
- **`crates/iec60870`** — Async client/server on `tokio` that *drives* the proto core over a
  transport. `driver.rs` / `driver101.rs` own a state machine + transport stream and shuttle
  APDUs/frames in both directions. Public surface: `Client104`, `Server104`, `Master101`,
  `Outstation101`, the `EventHandler` trait, deadband, redundancy, security/TLS, file transfer.
- **`crates/iec60870-test-tools`** — `publish = false`. Two long-running daemons, `iec-server`
  (outstation) and `iec-client` (master), controlled over a JSON-over-Unix-socket NDJSON protocol.
  Used by the end-to-end shell specs.

The dependency direction is strict: `proto` knows nothing about `tokio`; `iec60870` re-exports it as
`iec60870::proto`. When adding protocol behavior, decide first whether it belongs in the I/O-free
`proto` layer or the async `iec60870` layer — keep clocks and sockets out of `proto`.

## Feature flags (`iec60870` crate)

`default = []`. `tls` (tokio-rustls), `serial` (tokio-serial). Examples and modules are gated:
`serial.rs`/`tls.rs` only compile with their feature. CI runs `--all-features`.

## Common commands

```bash
cargo build --workspace
cargo test --workspace --all-features        # how CI runs tests
cargo test -p iec60870 --test loopback       # one integration test file
cargo test -p iec60870-proto seq             # tests matching a name
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run --example server_104               # see README for the example matrix
```

CI (`.github/workflows/ci.yml`) gates on: `cargo fmt --check`, the clippy line above
(`RUSTFLAGS=-Dwarnings`), `cargo test --all-features` on Linux/macOS/Windows, **MSRV 1.83**
(`clippy.toml` pins this — keep new code 1.83-compatible), `cargo doc` with
`RUSTDOCFLAGS=-Dwarnings`, and `cargo llvm-cov` with `--fail-under-lines 80`. Treat all of these as
required before claiming work is done.

## End-to-end test specs

Reproducible shell specs in `crates/iec60870-test-tools/tests/specs/` drive the two daemons over
real transports. They are **not** run by `cargo test` — run them explicitly:

```bash
cargo build -p iec60870-test-tools --bins                    # required first
./crates/iec60870-test-tools/tests/specs/run_all.sh          # whole suite (~30 s)
./crates/iec60870-test-tools/tests/specs/test_01_smoke.sh    # one spec
FILTER='test_0[1-5]_*' ./.../run_all.sh                      # subset by glob
KEEP_WORKDIR=1 RUST_LOG=iec60870=debug ./.../test_07_*.sh    # debug a failure
```

`SPECS.md` in that directory documents each spec's intent and expected outcome (including negative
tests where a failure response is the success condition). There are also `Skill`s for driving the
daemons (`iec60870-server`, `iec60870-client`, `iec60870-e2e-tests`) — prefer them over hand-rolling
daemon invocations.

## Fuzzing

`fuzz/` is a separate cargo-fuzz package (excluded from the workspace) with targets for APDU/ASDU
decode (101 and 104), 101 frame decode, and the state machine. Run with `cargo +nightly fuzz run <target>`.

## Conventions

- `rustfmt.toml`: `max_width = 100`, field-init and try shorthands on.
- Both library crates are `#![forbid(unsafe_code)]` and `#![warn(missing_debug_implementations, rust_2018_idioms)]`.
- Event/error enums carry `#[non_exhaustive]` for semver — preserve this when editing them.
- `docs/protocol-notes.md` is the local reference for byte layouts, defaults, and design decisions —
  read/update it (not just the code) when touching wire formats.
