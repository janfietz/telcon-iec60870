//! Control-direction ASDU types (those in the `C_*` family).
//!
//! All command types carry information objects that begin with an IOA and end
//! with a 1-octet **command qualifier** (Single Command Object, Double Command
//! Object, Regulating-step Command Object, Qualifier of Set-Point, Qualifier
//! of Interrogation, ...) that specifies whether the command is a select or
//! execute, and a 5-bit type-specific Q field.

#![allow(non_camel_case_types)]

use bytes::{Buf, BufMut};

use crate::asdu::header::{AsduAddressing, Ioa, Vsq};
use crate::asdu::ie::{Cp56Time2a, DoublePoint, Nva, Sva, R32};
use crate::asdu::io_list::{decode_io_list, encode_io_list};
use crate::asdu::payload::AsduPayload;
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Single Command Object (SCO) — used by C_SC_NA_1 / C_SC_TA_1
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sco {
    /// `SCS` — command state. `false` = OFF, `true` = ON.
    pub on: bool,
    /// `QU` — 5-bit type-specific qualifier (0 = no additional definition).
    pub qualifier: u8,
    /// `S/E` — `true` = SELECT, `false` = EXECUTE.
    pub select: bool,
}

impl Sco {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = (self.qualifier & 0x1F) << 2;
        if self.on {
            b |= 0x01;
        }
        if self.select {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            on: b & 0x01 != 0,
            qualifier: (b >> 2) & 0x1F,
            select: b & 0x80 != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Double Command Object (DCO) — used by C_DC_NA_1 / C_DC_TA_1
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dco {
    /// `DCS` — command state (Off, On — note that 00 and 11 are reserved/not
    /// permitted for commands; encoding still allows the full Intermediate
    /// and Indeterminate variants so the codec is loss-less).
    pub state: DoublePoint,
    pub qualifier: u8,
    pub select: bool,
}

impl Dco {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let state = match self.state {
            DoublePoint::Intermediate => 0b00,
            DoublePoint::Off => 0b01,
            DoublePoint::On => 0b10,
            DoublePoint::Indeterminate => 0b11,
        };
        let mut b = state | ((self.qualifier & 0x1F) << 2);
        if self.select {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        let state = match b & 0b11 {
            0b00 => DoublePoint::Intermediate,
            0b01 => DoublePoint::Off,
            0b10 => DoublePoint::On,
            _ => DoublePoint::Indeterminate,
        };
        Ok(Self {
            state,
            qualifier: (b >> 2) & 0x1F,
            select: b & 0x80 != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Regulating-step Command Object (RCO) — used by C_RC_NA_1
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepDirection {
    /// 00 — not permitted.
    #[default]
    NotPermitted0,
    /// 01 — step LOWER.
    Lower,
    /// 10 — step HIGHER.
    Higher,
    /// 11 — not permitted.
    NotPermitted3,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rco {
    pub direction: StepDirection,
    pub qualifier: u8,
    pub select: bool,
}

impl Rco {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let dir = match self.direction {
            StepDirection::NotPermitted0 => 0b00,
            StepDirection::Lower => 0b01,
            StepDirection::Higher => 0b10,
            StepDirection::NotPermitted3 => 0b11,
        };
        let mut b = dir | ((self.qualifier & 0x1F) << 2);
        if self.select {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        let direction = match b & 0b11 {
            0b00 => StepDirection::NotPermitted0,
            0b01 => StepDirection::Lower,
            0b10 => StepDirection::Higher,
            _ => StepDirection::NotPermitted3,
        };
        Ok(Self {
            direction,
            qualifier: (b >> 2) & 0x1F,
            select: b & 0x80 != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Qualifier of Set-Point (QOS) — used by C_SE_*
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Qos {
    /// `QL` — 7-bit qualifier (0 = default).
    pub qualifier: u8,
    /// `S/E` — `true` = SELECT, `false` = EXECUTE.
    pub select: bool,
}

impl Qos {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.qualifier & 0x7F;
        if self.select {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            qualifier: b & 0x7F,
            select: b & 0x80 != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Qualifier of Interrogation (QOI) — used by C_IC_NA_1
// ---------------------------------------------------------------------------

/// Qualifier of Interrogation per IEC 60870-5-101 §7.2.6.22.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Qoi(pub u8);

impl Qoi {
    pub const LEN: usize = 1;

    /// General interrogation (QOI = 20).
    pub const GENERAL: Self = Self(20);
    /// Group N interrogation (QOI = 20 + N), `N` in 1..=16.
    pub const fn group(n: u8) -> Self {
        Self(20 + n)
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_u8()))
    }
}

// ---------------------------------------------------------------------------
// Macro to stamp out command types: one IOA + one value
// ---------------------------------------------------------------------------

trait CmdIeWrite {
    fn write<B: BufMut>(&self, buf: &mut B);
}
trait CmdIeRead: Sized {
    fn read<B: Buf>(buf: &mut B) -> Result<Self>;
}

macro_rules! cmd_payload {
    (
        $(#[$attr:meta])*
        $name:ident, type_id = $tid:literal, value: $value:ty
    ) => {
        $(#[$attr])*
        #[derive(Debug, Default, Clone, PartialEq)]
        pub struct $name {
            pub objects: Vec<(Ioa, $value)>,
        }

        impl AsduPayload for $name {
            const TYPE_ID: u8 = $tid;

            fn encode_information_objects<B: BufMut>(
                &self,
                buf: &mut B,
                vsq: Vsq,
                addressing: AsduAddressing,
            ) {
                encode_io_list(buf, &self.objects, vsq, addressing, |b, v| {
                    <$value as CmdIeWrite>::write(v, b)
                });
            }

            fn decode_information_objects<B: Buf>(
                buf: &mut B,
                vsq: Vsq,
                addressing: AsduAddressing,
            ) -> Result<Self> {
                let objects = decode_io_list(buf, vsq, addressing, |b| {
                    <$value as CmdIeRead>::read(b)
                })?;
                Ok(Self { objects })
            }
        }
    };
}

// Atomic CmdIe impls
impl CmdIeWrite for Sco {
    fn write<B: BufMut>(&self, buf: &mut B) {
        Sco::encode(*self, buf);
    }
}
impl CmdIeRead for Sco {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Sco::decode(buf)
    }
}
impl CmdIeWrite for Dco {
    fn write<B: BufMut>(&self, buf: &mut B) {
        Dco::encode(*self, buf);
    }
}
impl CmdIeRead for Dco {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Dco::decode(buf)
    }
}
impl CmdIeWrite for Rco {
    fn write<B: BufMut>(&self, buf: &mut B) {
        Rco::encode(*self, buf);
    }
}
impl CmdIeRead for Rco {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Rco::decode(buf)
    }
}
impl CmdIeWrite for Qoi {
    fn write<B: BufMut>(&self, buf: &mut B) {
        Qoi::encode(*self, buf);
    }
}
impl CmdIeRead for Qoi {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Qoi::decode(buf)
    }
}

// Composite (set-point, QOS) and time-tagged variants
impl CmdIeWrite for (Nva, Qos) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl CmdIeRead for (Nva, Qos) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Nva::decode(buf)?, Qos::decode(buf)?))
    }
}
impl CmdIeWrite for (Sva, Qos) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl CmdIeRead for (Sva, Qos) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Sva::decode(buf)?, Qos::decode(buf)?))
    }
}
impl CmdIeWrite for (R32, Qos) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl CmdIeRead for (R32, Qos) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((R32::decode(buf)?, Qos::decode(buf)?))
    }
}
impl CmdIeWrite for (Sco, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl CmdIeRead for (Sco, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Sco::decode(buf)?, Cp56Time2a::decode(buf)?))
    }
}
impl CmdIeWrite for (Nva, Qos, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
        self.2.encode(buf);
    }
}
impl CmdIeRead for (Nva, Qos, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((
            Nva::decode(buf)?,
            Qos::decode(buf)?,
            Cp56Time2a::decode(buf)?,
        ))
    }
}
impl CmdIeWrite for (Sva, Qos, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
        self.2.encode(buf);
    }
}
impl CmdIeRead for (Sva, Qos, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((
            Sva::decode(buf)?,
            Qos::decode(buf)?,
            Cp56Time2a::decode(buf)?,
        ))
    }
}
impl CmdIeWrite for (R32, Qos, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
        self.2.encode(buf);
    }
}
impl CmdIeRead for (R32, Qos, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((
            R32::decode(buf)?,
            Qos::decode(buf)?,
            Cp56Time2a::decode(buf)?,
        ))
    }
}

// ---------------------------------------------------------------------------
// Type definitions
// ---------------------------------------------------------------------------

cmd_payload!(
    /// `C_SC_NA_1` (TypeID 45) — single command.
    C_SC_NA_1, type_id = 45, value: Sco
);

cmd_payload!(
    /// `C_DC_NA_1` (TypeID 46) — double command.
    C_DC_NA_1, type_id = 46, value: Dco
);

cmd_payload!(
    /// `C_RC_NA_1` (TypeID 47) — regulating-step command.
    C_RC_NA_1, type_id = 47, value: Rco
);

cmd_payload!(
    /// `C_SE_NA_1` (TypeID 48) — set-point command, normalised.
    C_SE_NA_1, type_id = 48, value: (Nva, Qos)
);

cmd_payload!(
    /// `C_SE_NB_1` (TypeID 49) — set-point command, scaled.
    C_SE_NB_1, type_id = 49, value: (Sva, Qos)
);

cmd_payload!(
    /// `C_SE_NC_1` (TypeID 50) — set-point command, short floating point.
    C_SE_NC_1, type_id = 50, value: (R32, Qos)
);

cmd_payload!(
    /// `C_SC_TA_1` (TypeID 58) — single command with CP56Time2a.
    C_SC_TA_1, type_id = 58, value: (Sco, Cp56Time2a)
);

cmd_payload!(
    /// `C_SE_TA_1` (TypeID 61) — set-point normalised with CP56Time2a.
    C_SE_TA_1, type_id = 61, value: (Nva, Qos, Cp56Time2a)
);

cmd_payload!(
    /// `C_SE_TB_1` (TypeID 62) — set-point scaled with CP56Time2a.
    C_SE_TB_1, type_id = 62, value: (Sva, Qos, Cp56Time2a)
);

cmd_payload!(
    /// `C_SE_TC_1` (TypeID 63) — set-point float with CP56Time2a.
    C_SE_TC_1, type_id = 63, value: (R32, Qos, Cp56Time2a)
);

// Interrogation reuses the same shape but is more commonly invoked with a
// single IOA = 0 and the QOI value.
cmd_payload!(
    /// `C_IC_NA_1` (TypeID 100) — interrogation command. By IEC convention
    /// the IOA is fixed at 0 and there is a single information object per
    /// ASDU; this codec does not enforce that, callers may submit lists.
    C_IC_NA_1, type_id = 100, value: Qoi
);

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
    use crate::asdu::cot::{Cause, Cot};
    use crate::asdu::envelope::Asdu;
    use crate::asdu::header::CommonAddress;
    use bytes::BytesMut;

    fn roundtrip_iec104<P>(payload: &P, vsq: Vsq)
    where
        P: AsduPayload + Clone + PartialEq + std::fmt::Debug,
    {
        let asdu = Asdu::from_payload(
            Cot::with(Cause::ACTIVATION),
            CommonAddress(1),
            vsq,
            payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: P = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(&decoded, payload);
    }

    #[test]
    fn sco_byte_layout() {
        let mut buf = BytesMut::new();
        // SELECT=1, QU=4, ON=1 → 0x80 | (4<<2) | 0x01 = 0x91
        Sco {
            on: true,
            qualifier: 4,
            select: true,
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x91]);
    }

    #[test]
    fn dco_byte_layout() {
        let mut buf = BytesMut::new();
        // EXECUTE, QU=0, state=On (0b10) → 0x02
        Dco {
            state: DoublePoint::On,
            qualifier: 0,
            select: false,
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x02]);
    }

    #[test]
    fn qoi_constants() {
        assert_eq!(Qoi::GENERAL.0, 20);
        assert_eq!(Qoi::group(1).0, 21);
        assert_eq!(Qoi::group(16).0, 36);
    }

    #[test]
    fn c_sc_na_1_roundtrip() {
        let payload = C_SC_NA_1 {
            objects: vec![(
                Ioa(100),
                Sco {
                    on: true,
                    qualifier: 0,
                    select: false,
                },
            )],
        };
        roundtrip_iec104(&payload, Vsq::single(1));
    }

    #[test]
    fn c_dc_na_1_select_then_execute() {
        let select = C_DC_NA_1 {
            objects: vec![(
                Ioa(200),
                Dco {
                    state: DoublePoint::On,
                    qualifier: 0,
                    select: true,
                },
            )],
        };
        let execute = C_DC_NA_1 {
            objects: vec![(
                Ioa(200),
                Dco {
                    state: DoublePoint::On,
                    qualifier: 0,
                    select: false,
                },
            )],
        };
        roundtrip_iec104(&select, Vsq::single(1));
        roundtrip_iec104(&execute, Vsq::single(1));
    }

    #[test]
    fn c_se_nc_1_setpoint_float() {
        let payload = C_SE_NC_1 {
            objects: vec![(
                Ioa(500),
                (
                    R32(42.0),
                    Qos {
                        qualifier: 0,
                        select: false,
                    },
                ),
            )],
        };
        roundtrip_iec104(&payload, Vsq::single(1));
    }

    #[test]
    fn c_ic_na_1_general_interrogation_byte_layout() {
        let payload = C_IC_NA_1 {
            objects: vec![(Ioa(0), Qoi::GENERAL)],
        };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::ACTIVATION),
            CommonAddress(1),
            Vsq::single(1),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        // TID=100=0x64 VSQ=01 COT=06,00 CA=01,00 IOA=0,0,0 QOI=20=0x14
        assert_eq!(
            &buf[..],
            &[0x64, 0x01, 0x06, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x14]
        );
    }
}
