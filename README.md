# iec60870

A Rust implementation of the IEC 60870-5 telecontrol protocols used in SCADA
and electrical-grid systems.

## Status

**0.1.0-dev** — IEC 60870-5-**104** (TCP/IP) and IEC 60870-5-**101** (serial,
FT 1.2 framing) are complete and exercised by end-to-end tests.

| Feature | State |
| --- | --- |
| ASDU envelope, VSQ, COT, CA, IOA | ✅ |
| Information elements (SIQ, DIQ, QDS, NVA, SVA, R32, BCR, CP24/56Time2a) | ✅ |
| Monitor types (M_SP_NA_1, M_DP_NA_1, M_ME_NA/NB/NC_1, time-tagged variants) | ✅ |
| Control types (C_SC/DC/RC/SE_*_*, C_IC_NA_1, time-tagged variants) | ✅ |
| System types (M_EI, C_CI, C_RD, C_CS, C_RP) | ✅ |
| IEC 60870-5-104 APCI/APDU codec | ✅ |
| 15-bit `SeqNo` with wrapping arithmetic | ✅ |
| Connection state machine (STARTDT, STOPDT, TESTFR, t0..t3, k/w window) | ✅ |
| Async client and server over `tokio` | ✅ |
| TLS via `tokio-rustls` (feature `tls`) | ✅ |
| `EventHandler` trait with `DefaultLoggingHandler` | ✅ |
| End-to-end loopback integration test | ✅ |
| IEC 60870-5-101 (serial transport, FT 1.2 framing) | ✅ |
| File-transfer ASDUs (120-126) with provider hooks | ✅ |

## Workspace

| Crate | Description |
| --- | --- |
| [`iec60870-proto`](crates/iec60870-proto) | Sans-I/O protocol layer: ASDU codec, APCI framing for IEC 60870-5-104, and the connection state machine. No async, no sockets, no clocks — every entry point takes the current `Instant` explicitly. |
| [`iec60870`](crates/iec60870) | Async client and server built on `tokio`. Plain TCP, TLS via `tokio-rustls` (feature `tls`), serial via `tokio-serial` (feature `serial`). Hookable `EventHandler`. |

## Quickstart

Bring up an outstation on the standard port:

```bash
cargo run --example server_104
```

In another terminal, connect a client and watch the interrogation cycle:

```bash
RUST_LOG=iec60870=info cargo run --example client_104
```

You should see something like:

```
state changed state=Starting
state changed state=Active
[??] type_id=100 cot=Cot { cause: Cause(7), ... }   # ActivationCon
[SP] ioa=100 on=true quality=...                    # M_SP_NA_1
[ME] ioa=200 value=50 quality=...                   # M_ME_NC_1
[ME] ioa=201 value=51.5 quality=...
[??] type_id=100 cot=Cot { cause: Cause(10), ... }  # ActivationTermination
```

## Using the library

A minimal client that connects, sends a general interrogation, and prints
responses:

```rust,no_run
use iec60870::proto::asdu::{CommonAddress, Cot, Cause, Ioa, Vsq};
use iec60870::proto::asdu::types::{C_IC_NA_1, Qoi};
use iec60870::proto::frame104::Config;
use iec60870::{Client104, ClientEvent, Transport};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = Client104::connect(
        Transport::tcp("127.0.0.1:2404".parse()?),
        Config::default(),
    ).await?;

    let interrogation = C_IC_NA_1 { objects: vec![(Ioa(0), Qoi::GENERAL)] };
    client.send(
        Cot::with(Cause::ACTIVATION),
        CommonAddress(1),
        Vsq::single(1),
        &interrogation,
    ).await?;

    while let Some(ClientEvent::Asdu(bytes)) = client.recv().await {
        println!("received {} bytes", bytes.len());
    }
    Ok(())
}
```

For full wire-level control (custom addressing profile, hand-built ASDU),
build the ASDU yourself and ship it via `send_asdu`:

```rust,ignore
let bytes = Asdu::from_payload(cot, ca, vsq, &payload, AsduAddressing::IEC104)
    .encode_to_vec(AsduAddressing::IEC104);
client.send_asdu(bytes).await?;
```

### File transfer

The crate implements IEC 60870-5 file-transfer ASDUs (TypeIDs 120-126) with
a pluggable storage backend. The default
[`FsFileTransferProvider`](crates/iec60870/src/file_transfer/fs.rs) maps
each file to a path under a configured host directory using a stable
CRC-16/IBM hash of the relative path; implement
[`FileTransferProvider`](crates/iec60870/src/file_transfer/provider.rs)
yourself to back transfers with a database, object store, or anything else.

Outstation (serves files from `/var/lib/iec104/files`):

```rust,ignore
use iec60870::file_transfer::FsFileTransferProvider;
use iec60870::{DefaultLoggingHandler, Server104};

let provider = FsFileTransferProvider::new("/var/lib/iec104/files")?;
let server = Server104::bind(addr, Config::default())
    .await?
    .with_file_provider(provider);
let conn = server.accept_with(DefaultLoggingHandler).await?;
// FT ASDUs are intercepted automatically; conn.recv() yields only non-FT events.
```

Master (fetches a file by `NameOfFile`):

```rust,ignore
use iec60870::file_transfer::FsFileTransferProvider;
use iec60870::proto::asdu::types::file::NameOfFile;
use iec60870::proto::asdu::CommonAddress;

let sink = FsFileTransferProvider::new("/tmp/incoming")?;
let client = Client104::connect_with_file_provider(
    transport, Config::default(), sink, DefaultLoggingHandler,
).await?;
let ft = client.file_transfer().expect("ft handle");
let bytes = ft.fetch(CommonAddress(1), NameOfFile(0xBB3D)).await?;
println!("fetched {bytes} bytes");
```

End-to-end runnable examples live under `crates/iec60870/examples/`:

```bash
cargo run --example file_serve -- /tmp/iec104-files
cargo run --example file_get -- 127.0.0.1:2404 0xBB3D
```

### Custom event handlers

Implement [`EventHandler`](crates/iec60870/src/handler.rs) to hook into the
connection lifecycle:

```rust,ignore
use iec60870::EventHandler;
use iec60870::proto::frame104::{Apdu, State};

struct MetricsHandler { /* … */ }

impl EventHandler for MetricsHandler {
    fn on_frame_received(&self, _apdu: &Apdu) { /* increment counter */ }
    fn on_state_changed(&self, state: State) { /* update gauge */ }
}

let client = Client104::connect_with(transport, Config::default(), MetricsHandler { ... }).await?;
```

The library ships [`DefaultLoggingHandler`](crates/iec60870/src/handler.rs)
that routes everything to the `tracing` crate at appropriate levels.

### TLS

```rust,ignore
use std::sync::Arc;
use iec60870::{tls_client_connect, NoopHandler, Transport};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

let mut roots = RootCertStore::empty();
// roots.add(...)  -- load your CA chain

let cfg = Arc::new(
    ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
);

let client = tls_client_connect(
    Transport::Tls {
        addr: "scada.example:19998".parse()?,
        server_name: "scada.example".into(),
        client_config: cfg,
    },
    Config::default(),
    NoopHandler,
).await?;
```

The conventional TLS port for IEC 60870-5-104 (per IEC 62351-3) is **19998**;
constants for both that and the plaintext default (2404) are exported as
[`DEFAULT_PORT`](crates/iec60870/src/transport.rs) and `DEFAULT_TLS_PORT`.

## Design

The codebase follows the **sans-I/O** pattern popularised by `quinn-proto`
and `h2`: the protocol logic (codecs and the connection state machine) lives
in `iec60870-proto` and has zero dependency on `tokio`, sockets, or wall-clock
time. Every state-machine entry point accepts the current `Instant`
explicitly. The async crate, `iec60870`, drives that state machine over a
real transport.

Why this matters:

* The state machine is 100% deterministic — unit tests exercise k/w window
  saturation, t1 timer expiry, out-of-sequence frame detection, and the
  STARTDT/STOPDT/TESTFR handshake without any timing flakiness.
* The same state machine drives plain TCP, TLS, and (in future) serial
  transport without recompilation.
* The protocol crate can be reused for `no_std` / embedded contexts with
  minimal modification.

See [docs/protocol-notes.md](docs/protocol-notes.md) for byte-level reference
material on FT 1.2 framing, APCI/APDU layout, control-field bit maps, and
the t0..t3 timer set.

## Testing

```bash
cargo test --workspace --all-features
```

The workspace ships ~130 tests covering codecs, the connection state
machine, and end-to-end loopback for both IEC-104 and IEC-101. Property-based
tests under `proptest` cover the codec layer for round-trip integrity across
every input domain, and `cargo fuzz` targets live under [`fuzz/`](fuzz/) for
the APDU, ASDU, and FT 1.2 frame decoders.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
