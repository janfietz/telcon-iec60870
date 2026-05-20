//! Cause of Transmission (COT) per IEC 60870-5-101 §7.2.3 and -104 §7.2.3.
//!
//! Wire format:
//!
//! ```text
//! Byte 0 :  T  | P/N | cause (bits 0..5)
//! Byte 1 :  originator address (only in 2-octet mode; IEC 104 always 2 octets)
//! ```
//!
//! * `T` (bit 7) — test bit
//! * `P/N` (bit 6) — positive/negative confirm (0 = positive, 1 = negative)
//!
//! IEC 60870-5-101 makes the originator-address octet optional (system parameter).
//! IEC 60870-5-104 fixes the COT at two octets.

use bytes::{Buf, BufMut};

use crate::error::{Error, Result};

/// Cause value (6-bit, 0..=63). Named constants cover the standard ones.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cause(pub u8);

impl Cause {
    pub const PERIODIC: Self = Self(1);
    pub const BACKGROUND: Self = Self(2);
    pub const SPONTANEOUS: Self = Self(3);
    pub const INITIALIZED: Self = Self(4);
    pub const REQUEST: Self = Self(5);
    pub const ACTIVATION: Self = Self(6);
    pub const ACTIVATION_CON: Self = Self(7);
    pub const DEACTIVATION: Self = Self(8);
    pub const DEACTIVATION_CON: Self = Self(9);
    pub const ACTIVATION_TERMINATION: Self = Self(10);
    pub const RETURN_REMOTE: Self = Self(11);
    pub const RETURN_LOCAL: Self = Self(12);
    pub const FILE_TRANSFER: Self = Self(13);
    /// Interrogated by station interrogation (general).
    pub const INTERROGATED_GENERAL: Self = Self(20);
    /// Interrogated by group 1..16 interrogation: codes 21..=36.
    pub const fn interrogated_group(n: u8) -> Self {
        Self(20 + n)
    }
    /// Requested by general counter request.
    pub const REQUESTED_COUNTER_GENERAL: Self = Self(37);
    /// Requested by counter group 1..4: codes 38..=41.
    pub const fn requested_counter_group(n: u8) -> Self {
        Self(37 + n)
    }
    pub const UNKNOWN_TYPE_ID: Self = Self(44);
    pub const UNKNOWN_CAUSE: Self = Self(45);
    pub const UNKNOWN_CA: Self = Self(46);
    pub const UNKNOWN_IOA: Self = Self(47);
}

/// Cause of Transmission as carried inside an ASDU header.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cot {
    pub cause: Cause,
    /// `P/N` — set when the cause carries a negative confirmation.
    pub negative: bool,
    /// `T` — set when the ASDU was generated for test purposes.
    pub test: bool,
    /// Originator address. Only used when the system is configured for the
    /// 2-octet COT (always the case for IEC 60870-5-104).
    pub originator: u8,
}

impl Cot {
    /// Length in octets when the system parameter selects the 1-octet COT.
    pub const LEN_1: usize = 1;
    /// Length in octets when the system parameter selects the 2-octet COT
    /// (always for IEC 60870-5-104).
    pub const LEN_2: usize = 2;

    const CAUSE_MASK: u8 = 0x3F;
    const PN: u8 = 0x40;
    const T: u8 = 0x80;

    /// Convenience constructor for a positive, non-test activation.
    pub const fn act() -> Self {
        Self::with(Cause::ACTIVATION)
    }

    /// Convenience constructor for a positive, non-test cause with originator 0.
    pub const fn with(cause: Cause) -> Self {
        Self {
            cause,
            negative: false,
            test: false,
            originator: 0,
        }
    }

    fn first_octet(self) -> u8 {
        let mut b = self.cause.0 & Self::CAUSE_MASK;
        if self.negative {
            b |= Self::PN;
        }
        if self.test {
            b |= Self::T;
        }
        b
    }

    fn parse_first_octet(b: u8) -> Self {
        Self {
            cause: Cause(b & Self::CAUSE_MASK),
            negative: b & Self::PN != 0,
            test: b & Self::T != 0,
            originator: 0,
        }
    }

    /// Encode in 1-octet form (no originator address).
    pub fn encode_1<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.first_octet());
    }

    /// Encode in 2-octet form (originator address as the second byte).
    pub fn encode_2<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.first_octet());
        buf.put_u8(self.originator);
    }

    /// Decode in 1-octet form.
    pub fn decode_1<B: Buf>(buf: &mut B) -> Result<Self> {
        if buf.remaining() < Self::LEN_1 {
            return Err(Error::Incomplete {
                needed: Self::LEN_1,
                have: buf.remaining(),
            });
        }
        Ok(Self::parse_first_octet(buf.get_u8()))
    }

    /// Decode in 2-octet form.
    pub fn decode_2<B: Buf>(buf: &mut B) -> Result<Self> {
        if buf.remaining() < Self::LEN_2 {
            return Err(Error::Incomplete {
                needed: Self::LEN_2,
                have: buf.remaining(),
            });
        }
        let first = buf.get_u8();
        let originator = buf.get_u8();
        let mut cot = Self::parse_first_octet(first);
        cot.originator = originator;
        Ok(cot)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use proptest::prelude::*;

    #[test]
    fn known_constants_have_correct_values() {
        assert_eq!(Cause::SPONTANEOUS.0, 3);
        assert_eq!(Cause::ACTIVATION.0, 6);
        assert_eq!(Cause::ACTIVATION_TERMINATION.0, 10);
        assert_eq!(Cause::INTERROGATED_GENERAL.0, 20);
        assert_eq!(Cause::interrogated_group(1).0, 21);
        assert_eq!(Cause::interrogated_group(16).0, 36);
        assert_eq!(Cause::REQUESTED_COUNTER_GENERAL.0, 37);
        assert_eq!(Cause::requested_counter_group(4).0, 41);
        assert_eq!(Cause::UNKNOWN_TYPE_ID.0, 44);
        assert_eq!(Cause::UNKNOWN_IOA.0, 47);
    }

    #[test]
    fn first_octet_packs_flags() {
        let cot = Cot {
            cause: Cause::SPONTANEOUS,
            negative: true,
            test: true,
            originator: 0,
        };
        let mut buf = BytesMut::new();
        cot.encode_1(&mut buf);
        // 0x80 (T) | 0x40 (P/N) | 0x03 (cause=3) == 0xC3
        assert_eq!(&buf[..], &[0xC3]);
    }

    #[test]
    fn cause_above_63_is_truncated_on_encode() {
        // Cause is a 6-bit field; values above 63 are masked off, not refused.
        // This is deliberate: the wire format simply cannot carry them, so we
        // truncate rather than panic. Decoder will roundtrip the truncated value.
        let cot = Cot::with(Cause(0xFF));
        let mut buf = BytesMut::new();
        cot.encode_1(&mut buf);
        assert_eq!(&buf[..], &[0x3F]);
    }

    #[test]
    fn two_octet_carries_originator() {
        let cot = Cot {
            cause: Cause::ACTIVATION_CON,
            negative: false,
            test: false,
            originator: 0xA5,
        };
        let mut buf = BytesMut::new();
        cot.encode_2(&mut buf);
        assert_eq!(&buf[..], &[0x07, 0xA5]);
    }

    #[test]
    fn decode_1_leaves_originator_zero() {
        let mut bytes: &[u8] = &[0xC3];
        let cot = Cot::decode_1(&mut bytes).unwrap();
        assert_eq!(cot.cause, Cause::SPONTANEOUS);
        assert!(cot.negative);
        assert!(cot.test);
        assert_eq!(cot.originator, 0);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        let mut empty: &[u8] = &[];
        assert!(matches!(
            Cot::decode_1(&mut empty),
            Err(Error::Incomplete { needed: 1, have: 0 })
        ));
        let mut short: &[u8] = &[0x03];
        assert!(matches!(
            Cot::decode_2(&mut short),
            Err(Error::Incomplete { needed: 2, have: 1 })
        ));
    }

    fn arb_cot(width: u8) -> impl Strategy<Value = Cot> {
        // Cause is 6 bits — restrict the input domain so we can test exact roundtrip
        // without considering encode-side truncation.
        let originator_max = if width == 1 { 0 } else { 255 };
        (0u8..64, any::<bool>(), any::<bool>(), 0u8..=originator_max).prop_map(|(c, n, t, o)| Cot {
            cause: Cause(c),
            negative: n,
            test: t,
            originator: o,
        })
    }

    proptest! {
        #[test]
        fn prop_cot_1_roundtrip(cot in arb_cot(1)) {
            let mut buf = BytesMut::new();
            cot.encode_1(&mut buf);
            prop_assert_eq!(buf.len(), Cot::LEN_1);
            let mut slice: &[u8] = &buf;
            let decoded = Cot::decode_1(&mut slice).unwrap();
            prop_assert_eq!(cot, decoded);
            prop_assert!(slice.is_empty());
        }

        #[test]
        fn prop_cot_2_roundtrip(cot in arb_cot(2)) {
            let mut buf = BytesMut::new();
            cot.encode_2(&mut buf);
            prop_assert_eq!(buf.len(), Cot::LEN_2);
            let mut slice: &[u8] = &buf;
            let decoded = Cot::decode_2(&mut slice).unwrap();
            prop_assert_eq!(cot, decoded);
            prop_assert!(slice.is_empty());
        }
    }
}
