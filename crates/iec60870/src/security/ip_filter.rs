//! IP allow-listing for inbound IEC 60870-5-104 connections.
//!
//! An [`IpFilter`] is consulted by [`Server104`](crate::Server104) and
//! [`TlsServer`](crate::TlsServer) right after `TcpListener::accept()` returns,
//! before any TLS handshake. Peers whose IP address does not match are silently
//! dropped (TCP FIN), logged at `warn` level, and the accept loop continues —
//! the filter never bubbles up to callers as an error.
//!
//! Both IPv4 and IPv6 entries are supported. Entries may be single addresses
//! or CIDR ranges; mixed lists are fine.
//!
//! ```
//! use iec60870::IpFilter;
//!
//! let filter = IpFilter::from_strs(&["10.0.0.0/8", "192.168.1.5", "::1"]).unwrap();
//! assert!(filter.contains("10.4.5.6:0".parse().unwrap()));
//! assert!(filter.contains("192.168.1.5:0".parse().unwrap()));
//! assert!(filter.contains("[::1]:0".parse().unwrap()));
//! assert!(!filter.contains("8.8.8.8:0".parse().unwrap()));
//! ```

use std::net::{IpAddr, SocketAddr};

use ipnet::IpNet;

/// Decides which peer IPs may connect.
///
/// `IpFilter::allow_all()` (also [`Default`]) accepts every peer, matching
/// pre-security behaviour. Use [`from_strs`](Self::from_strs) to build a
/// concrete allow-list, or [`deny_all`](Self::deny_all) for a hard close.
#[derive(Debug, Clone)]
pub struct IpFilter {
    mode: Mode,
}

#[derive(Debug, Clone)]
enum Mode {
    AllowAny,
    Allow(Vec<IpNet>),
    DenyAll,
}

impl Default for IpFilter {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl IpFilter {
    /// Accept every peer. Equivalent to no filter being installed.
    pub fn allow_all() -> Self {
        Self {
            mode: Mode::AllowAny,
        }
    }

    /// Reject every peer. Useful for putting a server into a hard-closed state
    /// at runtime without unbinding the listener.
    pub fn deny_all() -> Self {
        Self {
            mode: Mode::DenyAll,
        }
    }

    /// Build an allow-list from CIDRs and/or bare addresses.
    ///
    /// Each entry is parsed as an [`IpNet`] first; if that fails, the entry is
    /// retried as a bare [`IpAddr`] and promoted to host-length
    /// (`/32` for IPv4, `/128` for IPv6).
    ///
    /// An empty slice yields an explicit empty allow-list — which is *not* the
    /// same as [`allow_all`](Self::allow_all). It rejects every peer.
    pub fn from_strs(entries: &[&str]) -> Result<Self, IpFilterParseError> {
        let mut nets = Vec::with_capacity(entries.len());
        for raw in entries {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let net = trimmed
                .parse::<IpNet>()
                .or_else(|_| trimmed.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| IpFilterParseError {
                    entry: trimmed.to_string(),
                })?;
            nets.push(net);
        }
        Ok(Self {
            mode: Mode::Allow(nets),
        })
    }

    /// Append a CIDR range to the allow-list. Promotes an `AllowAny` /
    /// `DenyAll` filter to an explicit allow-list before pushing.
    pub fn push_cidr(&mut self, net: IpNet) {
        self.ensure_allow_mode();
        if let Mode::Allow(nets) = &mut self.mode {
            nets.push(net);
        }
    }

    /// Append a single host address to the allow-list (promoted to `/32` or
    /// `/128`). Promotes an `AllowAny` / `DenyAll` filter to an explicit
    /// allow-list before pushing.
    pub fn push_addr(&mut self, addr: IpAddr) {
        self.push_cidr(IpNet::from(addr));
    }

    /// True when the filter has no entries. `allow_all()` is empty;
    /// `deny_all()` is empty; an explicit empty allow-list from
    /// `from_strs(&[])` is also empty.
    pub fn is_empty(&self) -> bool {
        match &self.mode {
            Mode::AllowAny | Mode::DenyAll => true,
            Mode::Allow(nets) => nets.is_empty(),
        }
    }

    /// Does this filter accept `peer`?
    ///
    /// - [`allow_all`](Self::allow_all): always `true`.
    /// - [`deny_all`](Self::deny_all): always `false`.
    /// - Otherwise: at least one allow-list entry must contain `peer.ip()`.
    pub fn contains(&self, peer: SocketAddr) -> bool {
        match &self.mode {
            Mode::AllowAny => true,
            Mode::DenyAll => false,
            Mode::Allow(nets) => nets.iter().any(|n| n.contains(&peer.ip())),
        }
    }

    fn ensure_allow_mode(&mut self) {
        if !matches!(self.mode, Mode::Allow(_)) {
            self.mode = Mode::Allow(Vec::new());
        }
    }
}

/// Returned by [`IpFilter::from_strs`] when an entry parses as neither a
/// CIDR range nor a bare IP address.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid IP filter entry: {entry:?}")]
pub struct IpFilterParseError {
    /// The offending input string.
    pub entry: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn allow_all_accepts_everything() {
        let f = IpFilter::allow_all();
        assert!(f.contains(sa("1.2.3.4:0")));
        assert!(f.contains(sa("[2001:db8::1]:0")));
    }

    #[test]
    fn deny_all_rejects_everything() {
        let f = IpFilter::deny_all();
        assert!(!f.contains(sa("1.2.3.4:0")));
        assert!(!f.contains(sa("[::1]:0")));
    }

    #[test]
    fn cidr_and_bare_addresses_mix() {
        let f =
            IpFilter::from_strs(&["10.0.0.0/8", "192.168.1.5", "::1", "2001:db8::/32"]).unwrap();
        assert!(f.contains(sa("10.4.5.6:0")));
        assert!(f.contains(sa("192.168.1.5:0")));
        assert!(!f.contains(sa("192.168.1.6:0")));
        assert!(f.contains(sa("[::1]:0")));
        assert!(f.contains(sa("[2001:db8:1::1]:0")));
        assert!(!f.contains(sa("[2001:db9::1]:0")));
        assert!(!f.contains(sa("8.8.8.8:0")));
    }

    #[test]
    fn explicit_empty_allow_list_rejects_all() {
        let f = IpFilter::from_strs(&[]).unwrap();
        assert!(f.is_empty());
        assert!(!f.contains(sa("127.0.0.1:0")));
    }

    #[test]
    fn invalid_entry_returns_error() {
        let err = IpFilter::from_strs(&["10.0.0.0/8", "not-an-ip"]).unwrap_err();
        assert_eq!(err.entry, "not-an-ip");
    }

    #[test]
    fn push_addr_promotes_from_allow_any() {
        let mut f = IpFilter::allow_all();
        f.push_addr("127.0.0.1".parse().unwrap());
        assert!(f.contains(sa("127.0.0.1:0")));
        assert!(!f.contains(sa("10.0.0.1:0")));
    }
}
