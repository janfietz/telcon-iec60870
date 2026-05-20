//! ASDU header: Type ID, VSQ, COT, CA, plus the addressing-width configuration
//! that determines how those fields are serialised.

use bytes::{Buf, BufMut};

use crate::asdu::cot::Cot;
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Variable Structure Qualifier
// ---------------------------------------------------------------------------

/// Variable Structure Qualifier — top bit selects single-IOA sequence mode,
/// low 7 bits hold the number of information objects.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Vsq {
    /// `SQ` — when `true`, the ASDU contains a single IOA followed by
    /// `count` consecutive information elements. When `false`, each
    /// information object carries its own IOA.
    pub sequence: bool,
    /// Number of information objects (0..=127). Values above 127 are
    /// truncated on encode to match the 7-bit wire field.
    pub count: u8,
}

impl Vsq {
    pub const LEN: usize = 1;
    const SQ: u8 = 0x80;
    const COUNT_MASK: u8 = 0x7F;

    /// Build a VSQ where each information object carries its own IOA.
    pub const fn single(count: u8) -> Self {
        Self {
            sequence: false,
            count,
        }
    }

    /// Build a VSQ describing a sequence of `count` elements sharing a single IOA.
    pub const fn sequence(count: u8) -> Self {
        Self {
            sequence: true,
            count,
        }
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.count & Self::COUNT_MASK;
        if self.sequence {
            b |= Self::SQ;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        if buf.remaining() < Self::LEN {
            return Err(Error::Incomplete {
                needed: Self::LEN,
                have: buf.remaining(),
            });
        }
        let b = buf.get_u8();
        Ok(Self {
            sequence: b & Self::SQ != 0,
            count: b & Self::COUNT_MASK,
        })
    }
}

// ---------------------------------------------------------------------------
// Addressing
// ---------------------------------------------------------------------------

/// Common Address of ASDU.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommonAddress(pub u16);

/// Information Object Address.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ioa(pub u32);

/// Width of the Cause of Transmission field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CotSize {
    /// 1 octet — IEC 60870-5-101 configured for no originator address.
    One,
    /// 2 octets — IEC 60870-5-104 (always) and IEC 60870-5-101 with originator.
    #[default]
    Two,
}

/// Width of the Common Address field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaSize {
    One,
    #[default]
    Two,
}

/// Width of the Information Object Address field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoaSize {
    One,
    Two,
    #[default]
    Three,
}

/// Addressing-width configuration that turns wire bytes into structured
/// fields. These are deployment parameters fixed at link / system setup time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AsduAddressing {
    pub cot_size: CotSize,
    pub ca_size: CaSize,
    pub ioa_size: IoaSize,
}

impl AsduAddressing {
    /// Default profile for IEC 60870-5-104: 2-octet COT, 2-octet CA, 3-octet IOA.
    pub const IEC104: Self = Self {
        cot_size: CotSize::Two,
        ca_size: CaSize::Two,
        ioa_size: IoaSize::Three,
    };

    /// A common IEC 60870-5-101 profile: 2-octet COT (with originator),
    /// 2-octet CA, 2-octet IOA. Override individual fields as required.
    pub const IEC101_DEFAULT: Self = Self {
        cot_size: CotSize::Two,
        ca_size: CaSize::Two,
        ioa_size: IoaSize::Two,
    };

    pub(crate) fn cot_len(self) -> usize {
        match self.cot_size {
            CotSize::One => Cot::LEN_1,
            CotSize::Two => Cot::LEN_2,
        }
    }

    pub(crate) fn ca_len(self) -> usize {
        match self.ca_size {
            CaSize::One => 1,
            CaSize::Two => 2,
        }
    }
}

pub(crate) fn encode_ca<B: BufMut>(buf: &mut B, ca: CommonAddress, size: CaSize) {
    match size {
        CaSize::One => buf.put_u8(ca.0 as u8),
        CaSize::Two => buf.put_u16_le(ca.0),
    }
}

pub(crate) fn decode_ca<B: Buf>(buf: &mut B, size: CaSize) -> Result<CommonAddress> {
    let len = match size {
        CaSize::One => 1,
        CaSize::Two => 2,
    };
    if buf.remaining() < len {
        return Err(Error::Incomplete {
            needed: len,
            have: buf.remaining(),
        });
    }
    Ok(match size {
        CaSize::One => CommonAddress(buf.get_u8() as u16),
        CaSize::Two => CommonAddress(buf.get_u16_le()),
    })
}

pub(crate) fn encode_ioa<B: BufMut>(buf: &mut B, ioa: Ioa, size: IoaSize) {
    match size {
        IoaSize::One => buf.put_u8(ioa.0 as u8),
        IoaSize::Two => buf.put_u16_le(ioa.0 as u16),
        IoaSize::Three => {
            let v = ioa.0;
            buf.put_u8(v as u8);
            buf.put_u8((v >> 8) as u8);
            buf.put_u8((v >> 16) as u8);
        }
    }
}

pub(crate) fn decode_ioa<B: Buf>(buf: &mut B, size: IoaSize) -> Result<Ioa> {
    let len = match size {
        IoaSize::One => 1,
        IoaSize::Two => 2,
        IoaSize::Three => 3,
    };
    if buf.remaining() < len {
        return Err(Error::Incomplete {
            needed: len,
            have: buf.remaining(),
        });
    }
    Ok(match size {
        IoaSize::One => Ioa(buf.get_u8() as u32),
        IoaSize::Two => Ioa(buf.get_u16_le() as u32),
        IoaSize::Three => {
            let b0 = buf.get_u8() as u32;
            let b1 = buf.get_u8() as u32;
            let b2 = buf.get_u8() as u32;
            Ioa(b0 | (b1 << 8) | (b2 << 16))
        }
    })
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
    fn vsq_packs_sq_bit() {
        let mut buf = BytesMut::new();
        Vsq::sequence(5).encode(&mut buf);
        assert_eq!(&buf[..], &[0x85]);
        buf.clear();
        Vsq::single(5).encode(&mut buf);
        assert_eq!(&buf[..], &[0x05]);
    }

    #[test]
    fn vsq_count_truncates_above_127() {
        let mut buf = BytesMut::new();
        Vsq {
            sequence: false,
            count: 200,
        }
        .encode(&mut buf);
        // 200 = 0xC8, masked to 0x48; SQ bit must remain unset
        assert_eq!(&buf[..], &[0x48]);
    }

    #[test]
    fn ioa_three_octet_little_endian() {
        let mut buf = BytesMut::new();
        encode_ioa(&mut buf, Ioa(0x123456), IoaSize::Three);
        assert_eq!(&buf[..], &[0x56, 0x34, 0x12]);
        let mut slice: &[u8] = &buf;
        assert_eq!(
            decode_ioa(&mut slice, IoaSize::Three).unwrap(),
            Ioa(0x123456)
        );
    }

    #[test]
    fn ca_two_octet_little_endian() {
        let mut buf = BytesMut::new();
        encode_ca(&mut buf, CommonAddress(0xBEEF), CaSize::Two);
        assert_eq!(&buf[..], &[0xEF, 0xBE]);
    }

    #[test]
    fn short_buffers_rejected() {
        let mut empty: &[u8] = &[];
        assert!(matches!(
            decode_ioa(&mut empty, IoaSize::Three),
            Err(Error::Incomplete { needed: 3, have: 0 })
        ));
        let mut empty2: &[u8] = &[];
        assert!(matches!(
            decode_ca(&mut empty2, CaSize::Two),
            Err(Error::Incomplete { needed: 2, have: 0 })
        ));
    }

    proptest! {
        #[test]
        fn prop_vsq_roundtrip(sq: bool, count in 0u8..128) {
            let v = Vsq { sequence: sq, count };
            let mut buf = BytesMut::new();
            v.encode(&mut buf);
            let mut slice: &[u8] = &buf;
            prop_assert_eq!(Vsq::decode(&mut slice).unwrap(), v);
        }

        #[test]
        fn prop_ioa_roundtrip(addr in 0u32..(1 << 24)) {
            let mut buf = BytesMut::new();
            encode_ioa(&mut buf, Ioa(addr), IoaSize::Three);
            let mut slice: &[u8] = &buf;
            prop_assert_eq!(decode_ioa(&mut slice, IoaSize::Three).unwrap(), Ioa(addr));
        }

        #[test]
        fn prop_ca_roundtrip(addr: u16) {
            let mut buf = BytesMut::new();
            encode_ca(&mut buf, CommonAddress(addr), CaSize::Two);
            let mut slice: &[u8] = &buf;
            prop_assert_eq!(decode_ca(&mut slice, CaSize::Two).unwrap(), CommonAddress(addr));
        }
    }
}
