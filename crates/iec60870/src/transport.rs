//! Transport selection for IEC 60870-5-104.
//!
//! The [`Transport`] enum encapsulates the choice of plaintext TCP or
//! TLS-secured TCP. TLS is gated behind the `tls` cargo feature.

use std::net::SocketAddr;

#[cfg(feature = "tls")]
use std::sync::Arc;

/// How a client should reach a peer (or how the server should accept).
#[derive(Debug, Clone)]
pub enum Transport {
    /// Plain TCP connection.
    Tcp { addr: SocketAddr },
    /// TLS over TCP. The server name is used for SNI and certificate
    /// hostname validation by the client side; ignored server-side.
    #[cfg(feature = "tls")]
    Tls {
        addr: SocketAddr,
        server_name: String,
        client_config: Arc<tokio_rustls::rustls::ClientConfig>,
    },
}

impl Transport {
    /// Convenience constructor for the plain-TCP variant.
    pub fn tcp(addr: SocketAddr) -> Self {
        Self::Tcp { addr }
    }

    pub fn addr(&self) -> SocketAddr {
        match self {
            Self::Tcp { addr } => *addr,
            #[cfg(feature = "tls")]
            Self::Tls { addr, .. } => *addr,
        }
    }
}

/// Default IEC 60870-5-104 port (plaintext).
pub const DEFAULT_PORT: u16 = 2404;

/// Default IEC 62351-3 TLS port for IEC 60870-5-104.
pub const DEFAULT_TLS_PORT: u16 = 19998;
