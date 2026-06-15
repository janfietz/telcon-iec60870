//! Transport selection — TCP (104) or serial (101) — for both daemons.
//!
//! Both `iec-server daemon` and `iec-client daemon` accept the same set of
//! flags so an agent can swap link layers with a one-word change. The clap
//! [`TransportArgs`] group enforces that `--addr` is mutually exclusive with
//! the serial-only flags, and resolves to a typed [`TransportChoice`].

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Args, ValueEnum};

/// Wire transport this daemon will use.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    /// IEC 60870-5-104 over TCP.
    Tcp,
    /// IEC 60870-5-101 over a serial port (or socat pty).
    Serial,
}

/// One-octet or two-octet link address.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LinkAddrSize {
    #[default]
    One,
    Two,
}

impl LinkAddrSize {
    pub fn to_proto(self) -> iec60870::proto::frame101::frame::LinkAddressSize {
        match self {
            LinkAddrSize::One => iec60870::proto::frame101::frame::LinkAddressSize::One,
            LinkAddrSize::Two => iec60870::proto::frame101::frame::LinkAddressSize::Two,
        }
    }
}

/// Shared transport flags for both daemons.
#[derive(Args, Debug, Clone)]
pub struct TransportArgs {
    /// Wire transport: `tcp` (IEC-104) or `serial` (IEC-101).
    #[arg(long, value_enum)]
    pub transport: TransportKind,

    /// TCP only: socket address (server `bind`, client `connect`).
    /// Defaults to `127.0.0.1:2404` if omitted.
    #[arg(long)]
    pub addr: Option<SocketAddr>,

    /// Serial only: device path (e.g. `/dev/ttyUSB0`, `/dev/pts/N`).
    #[arg(long)]
    pub serial: Option<PathBuf>,

    /// Serial only: baud rate.
    #[arg(long, default_value_t = 9600)]
    pub baud: u32,

    /// Serial only: link address.
    #[arg(long, default_value_t = 1)]
    pub link_addr: u16,

    /// Serial only: link-address octet count (1 or 2).
    #[arg(long, value_enum, default_value_t = LinkAddrSize::One)]
    pub link_addr_size: LinkAddrSize,

    /// ASDU common address (Common Address of the ASDU).
    #[arg(long, default_value_t = 1)]
    pub coa: u16,
}

/// Fully resolved transport choice ready to hand to the daemon.
#[derive(Debug, Clone)]
pub enum TransportChoice {
    Tcp {
        addr: SocketAddr,
        coa: u16,
    },
    Serial {
        path: PathBuf,
        baud: u32,
        link_addr: u16,
        link_addr_size: LinkAddrSize,
        coa: u16,
    },
}

impl TransportArgs {
    pub fn resolve(self) -> Result<TransportChoice> {
        let coa = self.coa;
        match self.transport {
            TransportKind::Tcp => {
                let addr = self
                    .addr
                    .unwrap_or_else(|| "127.0.0.1:2404".parse().unwrap());
                if self.serial.is_some() {
                    return Err(anyhow!(
                        "--serial is incompatible with --transport tcp; drop it or switch transports"
                    ));
                }
                Ok(TransportChoice::Tcp { addr, coa })
            }
            TransportKind::Serial => {
                let path = self
                    .serial
                    .ok_or_else(|| anyhow!("--serial <PATH> is required for --transport serial"))?;
                if self.addr.is_some() {
                    return Err(anyhow!(
                        "--addr is incompatible with --transport serial; drop it or switch transports"
                    ));
                }
                Ok(TransportChoice::Serial {
                    path,
                    baud: self.baud,
                    link_addr: self.link_addr,
                    link_addr_size: self.link_addr_size,
                    coa,
                })
            }
        }
    }
}
