//! Information elements per IEC 60870-5-4.
//!
//! Every element exposes a constant `LEN` (its wire size in octets), an
//! `encode` method that writes to a `BufMut`, and a `decode` constructor that
//! reads from a `Buf`. All multi-octet integers are little-endian on the wire.

use bytes::{Buf, BufMut};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Quality bits — shared layout across SIQ, DIQ, QDS, BCR qualifier.
// ---------------------------------------------------------------------------

/// Quality flags that appear in the high nibble of SIQ/DIQ/QDS.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Quality {
    /// `BL` — blocked for transmission.
    pub blocked: bool,
    /// `SB` — value substituted by the operator.
    pub substituted: bool,
    /// `NT` — not topical (value has not been updated within the expected window).
    pub not_topical: bool,
    /// `IV` — invalid.
    pub invalid: bool,
}

impl Quality {
    const BL: u8 = 0x10;
    const SB: u8 = 0x20;
    const NT: u8 = 0x40;
    const IV: u8 = 0x80;

    pub(crate) fn to_bits(self) -> u8 {
        let mut b = 0u8;
        if self.blocked {
            b |= Self::BL;
        }
        if self.substituted {
            b |= Self::SB;
        }
        if self.not_topical {
            b |= Self::NT;
        }
        if self.invalid {
            b |= Self::IV;
        }
        b
    }

    pub(crate) fn from_bits(b: u8) -> Self {
        Self {
            blocked: b & Self::BL != 0,
            substituted: b & Self::SB != 0,
            not_topical: b & Self::NT != 0,
            invalid: b & Self::IV != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// SIQ — Single-point Information with Quality (1 octet)
// ---------------------------------------------------------------------------

/// Single-point information with quality bits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Siq {
    /// SPI — single-point state. `false` = OFF, `true` = ON.
    pub on: bool,
    pub quality: Quality,
}

impl Siq {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.quality.to_bits();
        if self.on {
            b |= 0x01;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            on: b & 0x01 != 0,
            quality: Quality::from_bits(b),
        })
    }
}

// ---------------------------------------------------------------------------
// DIQ — Double-point Information with Quality (1 octet)
// ---------------------------------------------------------------------------

/// Double-point state, per IEC 60870-5-4 §3.2.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DoublePoint {
    /// 00 — indeterminate or intermediate state.
    #[default]
    Intermediate,
    /// 01 — determined state OFF.
    Off,
    /// 10 — determined state ON.
    On,
    /// 11 — indeterminate state.
    Indeterminate,
}

impl DoublePoint {
    fn to_bits(self) -> u8 {
        match self {
            Self::Intermediate => 0b00,
            Self::Off => 0b01,
            Self::On => 0b10,
            Self::Indeterminate => 0b11,
        }
    }

    fn from_bits(b: u8) -> Self {
        match b & 0b11 {
            0b00 => Self::Intermediate,
            0b01 => Self::Off,
            0b10 => Self::On,
            _ => Self::Indeterminate,
        }
    }
}

/// Double-point information with quality bits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Diq {
    pub state: DoublePoint,
    pub quality: Quality,
}

impl Diq {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.quality.to_bits() | self.state.to_bits());
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            state: DoublePoint::from_bits(b),
            quality: Quality::from_bits(b),
        })
    }
}

// ---------------------------------------------------------------------------
// QDS — Quality Descriptor (1 octet)
// ---------------------------------------------------------------------------

/// Quality descriptor as used by measured values: quality bits plus an overflow flag.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Qds {
    /// `OV` — measured value exceeded the configured range.
    pub overflow: bool,
    pub quality: Quality,
}

impl Qds {
    pub const LEN: usize = 1;
    const OV: u8 = 0x01;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.quality.to_bits();
        if self.overflow {
            b |= Self::OV;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            overflow: b & Self::OV != 0,
            quality: Quality::from_bits(b),
        })
    }
}

// ---------------------------------------------------------------------------
// NVA — Normalized Value (2 octets, i16 little-endian)
// ---------------------------------------------------------------------------

/// Normalized value: 16-bit signed integer mapped to `[-1, 1 - 2^-15]`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nva(pub i16);

impl Nva {
    pub const LEN: usize = 2;

    /// Reinterpret as a floating-point value in `[-1.0, 1.0 - 2^-15]`.
    pub fn as_f32(self) -> f32 {
        self.0 as f32 / 32768.0
    }

    /// Construct from a floating-point value, clamping to representable range.
    pub fn from_f32(v: f32) -> Self {
        let scaled = (v.clamp(-1.0, 1.0 - 2.0_f32.powi(-15)) * 32768.0).round() as i32;
        Self(scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_i16_le(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_i16_le()))
    }
}

// ---------------------------------------------------------------------------
// SVA — Scaled Value (2 octets, i16 little-endian)
// ---------------------------------------------------------------------------

/// Scaled (signed 16-bit integer) value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sva(pub i16);

impl Sva {
    pub const LEN: usize = 2;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_i16_le(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_i16_le()))
    }
}

// ---------------------------------------------------------------------------
// R32 — Short Floating Point (4 octets, IEEE 754 binary32 little-endian)
// ---------------------------------------------------------------------------

/// IEEE 754 binary32 short floating-point value.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct R32(pub f32);

impl R32 {
    pub const LEN: usize = 4;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_f32_le(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_f32_le()))
    }
}

// ---------------------------------------------------------------------------
// BCR — Binary Counter Reading (5 octets)
// ---------------------------------------------------------------------------

/// Binary counter reading: 32-bit signed counter plus a 1-octet qualifier.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bcr {
    pub value: i32,
    /// 5-bit sequence number (0..=31).
    pub sequence: u8,
    /// `CY` — carry: counter has overflowed at least once since the last reset.
    pub carry: bool,
    /// `CA` — counter has been adjusted since the last reset.
    pub adjusted: bool,
    /// `IV` — invalid.
    pub invalid: bool,
}

impl Bcr {
    pub const LEN: usize = 5;
    const CY: u8 = 0x20;
    const CA: u8 = 0x40;
    const IV: u8 = 0x80;
    const SQ_MASK: u8 = 0x1F;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_i32_le(self.value);
        let mut q = self.sequence & Self::SQ_MASK;
        if self.carry {
            q |= Self::CY;
        }
        if self.adjusted {
            q |= Self::CA;
        }
        if self.invalid {
            q |= Self::IV;
        }
        buf.put_u8(q);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let value = buf.get_i32_le();
        let q = buf.get_u8();
        Ok(Self {
            value,
            sequence: q & Self::SQ_MASK,
            carry: q & Self::CY != 0,
            adjusted: q & Self::CA != 0,
            invalid: q & Self::IV != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// CP24Time2a — 3 octets: milliseconds (u16 LE) + minute (6 bits) + IV
// ---------------------------------------------------------------------------

/// 3-octet time tag (milliseconds within an hour + minute + IV).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cp24Time2a {
    /// Milliseconds within the minute: 0..59_999.
    pub milliseconds: u16,
    /// Minute: 0..59.
    pub minute: u8,
    /// `IV` — time tag is invalid.
    pub invalid: bool,
}

impl Cp24Time2a {
    pub const LEN: usize = 3;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u16_le(self.milliseconds);
        let mut b = self.minute & 0x3F;
        if self.invalid {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let milliseconds = buf.get_u16_le();
        let b = buf.get_u8();
        Ok(Self {
            milliseconds,
            minute: b & 0x3F,
            invalid: b & 0x80 != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// CP56Time2a — 7 octets, full date+time with millisecond precision
// ---------------------------------------------------------------------------

/// 7-octet time tag with full date and millisecond precision.
///
/// Per IEC 60870-5-4 §6.8 the year is stored as `0..99` and conventionally
/// refers to the 21st century. This struct preserves the byte verbatim and
/// does not interpret the century — callers should add 2000 (or whichever
/// epoch is appropriate for their system) when converting to civil time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cp56Time2a {
    /// Milliseconds within the minute: 0..59_999.
    pub milliseconds: u16,
    /// Minute: 0..59.
    pub minute: u8,
    /// Hour: 0..23.
    pub hour: u8,
    /// Day of month: 1..31.
    pub day: u8,
    /// Day of week: 1..7 (Mon..Sun). 0 means "not used".
    pub day_of_week: u8,
    /// Month: 1..12.
    pub month: u8,
    /// Year: 0..99 (raw, no century offset).
    pub year: u8,
    /// `SU` — summer time / daylight saving in effect.
    pub summer_time: bool,
    /// `IV` — time tag is invalid.
    pub invalid: bool,
    /// `GEN` — bit 6 of the minute octet. Companion standards vary; preserve verbatim.
    pub genuine: bool,
}

impl Cp56Time2a {
    pub const LEN: usize = 7;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u16_le(self.milliseconds);
        let mut b = self.minute & 0x3F;
        if self.genuine {
            b |= 0x40;
        }
        if self.invalid {
            b |= 0x80;
        }
        buf.put_u8(b);
        let mut h = self.hour & 0x1F;
        if self.summer_time {
            h |= 0x80;
        }
        buf.put_u8(h);
        let d = (self.day & 0x1F) | ((self.day_of_week & 0x07) << 5);
        buf.put_u8(d);
        buf.put_u8(self.month & 0x0F);
        buf.put_u8(self.year & 0x7F);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let milliseconds = buf.get_u16_le();
        let min_b = buf.get_u8();
        let hour_b = buf.get_u8();
        let day_b = buf.get_u8();
        let month_b = buf.get_u8();
        let year_b = buf.get_u8();
        Ok(Self {
            milliseconds,
            minute: min_b & 0x3F,
            genuine: min_b & 0x40 != 0,
            invalid: min_b & 0x80 != 0,
            hour: hour_b & 0x1F,
            summer_time: hour_b & 0x80 != 0,
            day: day_b & 0x1F,
            day_of_week: (day_b >> 5) & 0x07,
            month: month_b & 0x0F,
            year: year_b & 0x7F,
        })
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn ensure<B: Buf>(buf: &B, n: usize) -> Result<()> {
    if buf.remaining() < n {
        Err(Error::Incomplete {
            needed: n,
            have: buf.remaining(),
        })
    } else {
        Ok(())
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

    fn roundtrip<T: PartialEq + std::fmt::Debug>(
        value: T,
        len: usize,
        encode: impl Fn(&T, &mut BytesMut),
        decode: impl Fn(&mut &[u8]) -> Result<T>,
    ) {
        let mut buf = BytesMut::new();
        encode(&value, &mut buf);
        assert_eq!(buf.len(), len, "encoded length mismatch");
        let mut slice: &[u8] = &buf;
        let decoded = decode(&mut slice).expect("decode");
        assert_eq!(value, decoded);
        assert!(slice.is_empty(), "decoder did not consume all input");
    }

    #[test]
    fn siq_known_pattern() {
        // SPI=1, IV=1 → 0x81
        let mut buf = BytesMut::new();
        Siq {
            on: true,
            quality: Quality {
                invalid: true,
                ..Default::default()
            },
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x81]);

        let mut slice: &[u8] = &[0x91];
        let s = Siq::decode(&mut slice).unwrap();
        assert!(s.on);
        assert!(s.quality.invalid);
        assert!(s.quality.blocked);
    }

    #[test]
    fn diq_double_point_states() {
        for (bits, want) in [
            (0b00, DoublePoint::Intermediate),
            (0b01, DoublePoint::Off),
            (0b10, DoublePoint::On),
            (0b11, DoublePoint::Indeterminate),
        ] {
            let mut slice: &[u8] = &[bits];
            let d = Diq::decode(&mut slice).unwrap();
            assert_eq!(d.state, want, "for bits {bits:#04b}");
        }
    }

    #[test]
    fn qds_overflow_bit() {
        let mut buf = BytesMut::new();
        Qds {
            overflow: true,
            quality: Quality::default(),
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x01]);
    }

    #[test]
    fn nva_extremes() {
        let mut buf = BytesMut::new();
        Nva(i16::MIN).encode(&mut buf);
        assert_eq!(&buf[..], &[0x00, 0x80]); // -32768 LE
        buf.clear();
        Nva(i16::MAX).encode(&mut buf);
        assert_eq!(&buf[..], &[0xFF, 0x7F]); // 32767 LE
    }

    #[test]
    fn nva_f32_conversion() {
        assert!((Nva::from_f32(-1.0).as_f32() - -1.0).abs() < f32::EPSILON);
        assert!((Nva::from_f32(0.0).as_f32() - 0.0).abs() < f32::EPSILON);
        // Saturating clamp on overflow
        assert_eq!(Nva::from_f32(2.0).0, i16::MAX);
        assert_eq!(Nva::from_f32(-2.0).0, i16::MIN);
    }

    #[test]
    fn r32_known_pattern() {
        let mut buf = BytesMut::new();
        R32(1.0_f32).encode(&mut buf);
        // 0x3F800000 little-endian
        assert_eq!(&buf[..], &[0x00, 0x00, 0x80, 0x3F]);
    }

    #[test]
    fn bcr_qualifier_bits() {
        let mut buf = BytesMut::new();
        Bcr {
            value: 0x0102_0304,
            sequence: 0x1F,
            carry: true,
            adjusted: true,
            invalid: true,
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x04, 0x03, 0x02, 0x01, 0xFF]);
    }

    #[test]
    fn cp24time2a_known_pattern() {
        // 12345 ms, minute 30, IV=1
        let mut buf = BytesMut::new();
        Cp24Time2a {
            milliseconds: 12345,
            minute: 30,
            invalid: true,
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x39, 0x30, 0x9E]);
    }

    #[test]
    fn cp56time2a_known_pattern() {
        // 2024-12-25 (Wed = 3) 15:42:33.500
        // milliseconds = 42*1000? no: ms within minute = 33*1000+500 = 33500 → 0x82DC
        let t = Cp56Time2a {
            milliseconds: 33_500,
            minute: 42,
            hour: 15,
            day: 25,
            day_of_week: 3,
            month: 12,
            year: 24,
            summer_time: false,
            invalid: false,
            genuine: false,
        };
        let mut buf = BytesMut::new();
        t.encode(&mut buf);
        assert_eq!(buf.len(), Cp56Time2a::LEN);
        let mut slice: &[u8] = &buf;
        assert_eq!(Cp56Time2a::decode(&mut slice).unwrap(), t);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        let mut empty: &[u8] = &[];
        assert!(matches!(
            Cp56Time2a::decode(&mut empty),
            Err(Error::Incomplete { needed: 7, have: 0 })
        ));
    }

    // ----- Property-based round-trip tests -----

    fn arb_quality() -> impl Strategy<Value = Quality> {
        (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(b, s, n, i)| {
            Quality {
                blocked: b,
                substituted: s,
                not_topical: n,
                invalid: i,
            }
        })
    }

    proptest! {
        #[test]
        fn prop_siq_roundtrip(on: bool, q in arb_quality()) {
            roundtrip(
                Siq { on, quality: q },
                Siq::LEN,
                |v, b| v.encode(b),
                |s| Siq::decode(s),
            );
        }

        #[test]
        fn prop_diq_roundtrip(s in 0u8..4, q in arb_quality()) {
            let state = DoublePoint::from_bits(s);
            roundtrip(
                Diq { state, quality: q },
                Diq::LEN,
                |v, b| v.encode(b),
                |s| Diq::decode(s),
            );
        }

        #[test]
        fn prop_qds_roundtrip(overflow: bool, q in arb_quality()) {
            roundtrip(
                Qds { overflow, quality: q },
                Qds::LEN,
                |v, b| v.encode(b),
                |s| Qds::decode(s),
            );
        }

        #[test]
        fn prop_nva_roundtrip(v in any::<i16>()) {
            roundtrip(Nva(v), Nva::LEN, |x, b| x.encode(b), |s| Nva::decode(s));
        }

        #[test]
        fn prop_sva_roundtrip(v in any::<i16>()) {
            roundtrip(Sva(v), Sva::LEN, |x, b| x.encode(b), |s| Sva::decode(s));
        }

        #[test]
        fn prop_r32_roundtrip(bits in any::<u32>()) {
            // Cover all bit patterns including NaN/Inf. Compare on bit pattern,
            // not value, because NaN != NaN.
            let value = f32::from_bits(bits);
            let mut buf = BytesMut::new();
            R32(value).encode(&mut buf);
            let mut slice: &[u8] = &buf;
            let R32(decoded) = R32::decode(&mut slice).unwrap();
            prop_assert_eq!(decoded.to_bits(), value.to_bits());
        }

        #[test]
        fn prop_bcr_roundtrip(
            v in any::<i32>(),
            sq in 0u8..32,
            cy: bool,
            ca: bool,
            iv: bool,
        ) {
            roundtrip(
                Bcr { value: v, sequence: sq, carry: cy, adjusted: ca, invalid: iv },
                Bcr::LEN,
                |x, b| x.encode(b),
                |s| Bcr::decode(s),
            );
        }

        #[test]
        fn prop_cp24_roundtrip(ms in 0u16..60_000, min in 0u8..60, iv: bool) {
            roundtrip(
                Cp24Time2a { milliseconds: ms, minute: min, invalid: iv },
                Cp24Time2a::LEN,
                |x, b| x.encode(b),
                |s| Cp24Time2a::decode(s),
            );
        }

        #[test]
        fn prop_cp56_roundtrip(
            ms in 0u16..60_000,
            minute in 0u8..60,
            hour in 0u8..24,
            day in 1u8..32,
            dow in 0u8..8,
            month in 1u8..13,
            year in 0u8..100,
            su: bool,
            iv: bool,
            gen: bool,
        ) {
            roundtrip(
                Cp56Time2a {
                    milliseconds: ms,
                    minute,
                    hour,
                    day,
                    day_of_week: dow,
                    month,
                    year,
                    summer_time: su,
                    invalid: iv,
                    genuine: gen,
                },
                Cp56Time2a::LEN,
                |x, b| x.encode(b),
                |s| Cp56Time2a::decode(s),
            );
        }
    }
}
