//! System-information ASDU types: end-of-initialisation, counter interrogation,
//! clock sync, read, test, reset.

#![allow(non_camel_case_types)]

use bytes::{Buf, BufMut};

use crate::asdu::header::{decode_ioa, encode_ioa, AsduAddressing, Ioa, Vsq};
use crate::asdu::ie::Cp56Time2a;
use crate::asdu::payload::AsduPayload;
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// COI / QCC / QRP qualifiers
// ---------------------------------------------------------------------------

/// Cause of Initialisation (1 octet) carried by `M_EI_NA_1`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Coi {
    /// 7-bit cause: 0 = local power switch on, 1 = local manual reset,
    /// 2 = remote reset, ≥ 32 = vendor-specific.
    pub cause: u8,
    /// `BS1` — `true` if initialisation occurred after parameter change.
    pub after_param_change: bool,
}

impl Coi {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.cause & 0x7F;
        if self.after_param_change {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            cause: b & 0x7F,
            after_param_change: b & 0x80 != 0,
        })
    }
}

/// Qualifier of Counter Interrogation Command (1 octet).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Qcc {
    /// `RQT` — 6-bit counter group: 0 = no, 1..4 = group 1..4, 5 = general.
    pub group: u8,
    /// `FRZ` — 2-bit freeze action: 0 = read, 1 = counter freeze without reset,
    /// 2 = freeze with reset, 3 = counter reset.
    pub freeze: u8,
}

impl Qcc {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8((self.group & 0x3F) | ((self.freeze & 0x03) << 6));
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            group: b & 0x3F,
            freeze: (b >> 6) & 0x03,
        })
    }
}

/// Qualifier of Reset Process Command (1 octet).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Qrp(pub u8);

impl Qrp {
    pub const LEN: usize = 1;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_u8()))
    }
}

// ---------------------------------------------------------------------------
// System-info ASDU types — all single-object (count = 1, SQ = 0)
//
// These are encoded as <IOA><qualifier-or-payload>. We model them with a
// concrete `ioa` field rather than a `Vec` because the standard fixes the
// count at 1.
// ---------------------------------------------------------------------------

/// `M_EI_NA_1` (TypeID 70) — end of initialisation. By convention the IOA is 0.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct M_EI_NA_1 {
    pub ioa: Ioa,
    pub coi: Coi,
}

impl AsduPayload for M_EI_NA_1 {
    const TYPE_ID: u8 = 70;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.coi.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let coi = Coi::decode(buf)?;
        Ok(Self { ioa, coi })
    }
}

/// `C_CI_NA_1` (TypeID 101) — counter interrogation command. By convention IOA = 0.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct C_CI_NA_1 {
    pub ioa: Ioa,
    pub qcc: Qcc,
}

impl AsduPayload for C_CI_NA_1 {
    const TYPE_ID: u8 = 101;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.qcc.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let qcc = Qcc::decode(buf)?;
        Ok(Self { ioa, qcc })
    }
}

/// `C_RD_NA_1` (TypeID 102) — read command. Information object is just the IOA.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct C_RD_NA_1 {
    pub ioa: Ioa,
}

impl AsduPayload for C_RD_NA_1 {
    const TYPE_ID: u8 = 102;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        Ok(Self { ioa })
    }
}

/// `C_CS_NA_1` (TypeID 103) — clock synchronisation. By convention IOA = 0,
/// payload is a CP56Time2a.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct C_CS_NA_1 {
    pub ioa: Ioa,
    pub time: Cp56Time2a,
}

impl AsduPayload for C_CS_NA_1 {
    const TYPE_ID: u8 = 103;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.time.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let time = Cp56Time2a::decode(buf)?;
        Ok(Self { ioa, time })
    }
}

/// `C_RP_NA_1` (TypeID 105) — reset process command.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct C_RP_NA_1 {
    pub ioa: Ioa,
    pub qrp: Qrp,
}

impl AsduPayload for C_RP_NA_1 {
    const TYPE_ID: u8 = 105;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.qrp.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let qrp = Qrp::decode(buf)?;
        Ok(Self { ioa, qrp })
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
    use crate::asdu::cot::{Cause, Cot};
    use crate::asdu::envelope::Asdu;
    use crate::asdu::header::CommonAddress;
    use bytes::BytesMut;

    #[test]
    fn coi_byte_layout() {
        let mut buf = BytesMut::new();
        Coi {
            cause: 2,
            after_param_change: true,
        }
        .encode(&mut buf);
        // BS1=1 → 0x80 | cause(2) = 0x82
        assert_eq!(&buf[..], &[0x82]);
    }

    #[test]
    fn qcc_packs_group_and_freeze() {
        let mut buf = BytesMut::new();
        Qcc {
            group: 5,
            freeze: 2,
        }
        .encode(&mut buf);
        // (2<<6) | 5 = 0x85
        assert_eq!(&buf[..], &[0x85]);
    }

    #[test]
    fn m_ei_na_1_roundtrip() {
        let payload = M_EI_NA_1 {
            ioa: Ioa(0),
            coi: Coi {
                cause: 0,
                after_param_change: false,
            },
        };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::INITIALIZED),
            CommonAddress(1),
            Vsq::single(1),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        // TID=70=0x46 VSQ=01 COT=04,00 CA=01,00 IOA=0,0,0 COI=0
        assert_eq!(
            &buf[..],
            &[0x46, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: M_EI_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn c_cs_na_1_roundtrip() {
        let payload = C_CS_NA_1 {
            ioa: Ioa(0),
            time: Cp56Time2a {
                milliseconds: 12345,
                minute: 30,
                hour: 12,
                day: 15,
                day_of_week: 2,
                month: 6,
                year: 24,
                ..Default::default()
            },
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
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: C_CS_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn c_rd_na_1_roundtrip() {
        let payload = C_RD_NA_1 { ioa: Ioa(0x123456) };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::REQUEST),
            CommonAddress(1),
            Vsq::single(1),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: C_RD_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded, payload);
    }
}
