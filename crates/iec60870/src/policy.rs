//! Declarative ASDU acceptance policy.
//!
//! IEC 60870-5-104 has no built-in role/direction enforcement: any peer can
//! send any Type ID. In particular, control-direction ASDUs (`C_SC_*`,
//! `C_DC_*`, `C_SE_*`, `C_IC_NA_1`, `C_RP_NA_1`, `C_CS_NA_1`, …) should
//! normally flow only from a controlling station to a controlled station.
//! Without enforcement, a stray or hostile peer can issue commands directly
//! into an outstation.
//!
//! [`AsduPolicy`] lets the application declaratively restrict which ASDUs
//! the driver forwards to user code. Anything outside the allow-lists is
//! dropped at the driver boundary and logged at warn level. The state
//! machine itself is unaffected — sequence-number tracking, ACKs, and
//! timers all proceed normally — so a rejected ASDU does not break the
//! IEC-104 link.
//!
//! ```no_run
//! use iec60870::AsduPolicy;
//! use iec60870::proto::asdu::Cause;
//!
//! // A controlling station typically only expects monitor-direction
//! // ASDUs (Type IDs 1..=40) plus interrogation/clock confirms.
//! let policy = AsduPolicy::new()
//!     .allow_type_id_range(1, 40)
//!     .allow_cause(Cause::SPONTANEOUS)
//!     .allow_cause(Cause::INTERROGATED_GENERAL)
//!     .allow_cause(Cause::ACTIVATION_CON)
//!     .allow_cause(Cause::ACTIVATION_TERMINATION);
//! ```

use std::collections::HashSet;

use iec60870_proto::asdu::Cause;

/// Allow-list policy applied to ASDUs decoded from the peer.
///
/// Each axis ([`allowed_type_ids`](Self::allow_type_id),
/// [`allowed_causes`](Self::allow_cause),
/// [`allowed_common_addresses`](Self::allow_common_address)) is independent
/// and starts unrestricted. As soon as the first value is added on an axis,
/// that axis becomes a strict allow-list and **only** the listed values are
/// accepted.
///
/// The default policy is fully permissive — every ASDU is delivered.
#[derive(Debug, Default, Clone)]
pub struct AsduPolicy {
    pub(crate) allowed_type_ids: Option<HashSet<u8>>,
    pub(crate) allowed_causes: Option<HashSet<u8>>,
    pub(crate) allowed_common_addresses: Option<HashSet<u16>>,
}

impl AsduPolicy {
    /// New unrestricted policy. Equivalent to [`AsduPolicy::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow the given Type ID. Adding the first ID switches the Type-ID
    /// axis into allow-list mode.
    pub fn allow_type_id(mut self, type_id: u8) -> Self {
        self.allowed_type_ids
            .get_or_insert_with(HashSet::new)
            .insert(type_id);
        self
    }

    /// Allow every Type ID in the inclusive range `lo..=hi`.
    pub fn allow_type_id_range(mut self, lo: u8, hi: u8) -> Self {
        let set = self.allowed_type_ids.get_or_insert_with(HashSet::new);
        for t in lo..=hi {
            set.insert(t);
        }
        self
    }

    /// Allow the given cause of transmission.
    pub fn allow_cause(mut self, cause: Cause) -> Self {
        self.allowed_causes
            .get_or_insert_with(HashSet::new)
            .insert(cause.raw());
        self
    }

    /// Allow the given Common Address.
    pub fn allow_common_address(mut self, ca: u16) -> Self {
        self.allowed_common_addresses
            .get_or_insert_with(HashSet::new)
            .insert(ca);
        self
    }

    /// Returns `true` if the policy has any restriction set on any axis.
    pub fn is_restrictive(&self) -> bool {
        self.allowed_type_ids.is_some()
            || self.allowed_causes.is_some()
            || self.allowed_common_addresses.is_some()
    }

    /// Test whether an ASDU with the given header fields is permitted.
    pub fn allows(&self, type_id: u8, cause_raw: u8, ca: u16) -> bool {
        if let Some(set) = &self.allowed_type_ids {
            if !set.contains(&type_id) {
                return false;
            }
        }
        if let Some(set) = &self.allowed_causes {
            if !set.contains(&cause_raw) {
                return false;
            }
        }
        if let Some(set) = &self.allowed_common_addresses {
            if !set.contains(&ca) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_allows_everything() {
        let p = AsduPolicy::new();
        assert!(!p.is_restrictive());
        assert!(p.allows(100, 6, 1));
        assert!(p.allows(255, 63, 65_535));
    }

    #[test]
    fn type_id_allow_list_rejects_others() {
        let p = AsduPolicy::new().allow_type_id(1).allow_type_id(13);
        assert!(p.allows(1, 6, 1));
        assert!(p.allows(13, 6, 1));
        assert!(!p.allows(100, 6, 1));
    }

    #[test]
    fn cause_allow_list_rejects_others() {
        let p = AsduPolicy::new().allow_cause(Cause::SPONTANEOUS);
        assert!(p.allows(1, Cause::SPONTANEOUS.raw(), 1));
        assert!(!p.allows(1, Cause::ACTIVATION.raw(), 1));
    }

    #[test]
    fn common_address_allow_list_rejects_others() {
        let p = AsduPolicy::new()
            .allow_common_address(1)
            .allow_common_address(42);
        assert!(p.allows(1, 6, 1));
        assert!(p.allows(1, 6, 42));
        assert!(!p.allows(1, 6, 7));
    }

    #[test]
    fn type_id_range_is_inclusive() {
        let p = AsduPolicy::new().allow_type_id_range(1, 40);
        assert!(p.allows(1, 0, 0));
        assert!(p.allows(40, 0, 0));
        assert!(!p.allows(41, 0, 0));
    }

    #[test]
    fn axes_are_combined_with_and() {
        let p = AsduPolicy::new()
            .allow_type_id(1)
            .allow_cause(Cause::SPONTANEOUS);
        // type_id ok + cause ok → allowed
        assert!(p.allows(1, Cause::SPONTANEOUS.raw(), 1));
        // type_id ok + cause not ok → rejected
        assert!(!p.allows(1, Cause::ACTIVATION.raw(), 1));
        // type_id not ok + cause ok → rejected
        assert!(!p.allows(2, Cause::SPONTANEOUS.raw(), 1));
    }
}
