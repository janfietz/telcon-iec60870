//! Monitor-direction ASDU types (those in the `M_*` family).
//!
//! Each type carries a list of information objects. When the ASDU's VSQ has
//! `SQ = 0`, every object includes its own IOA; when `SQ = 1`, only the first
//! object has an IOA and the rest are implicitly at consecutive addresses.

#![allow(non_camel_case_types)]

use bytes::{Buf, BufMut};

use crate::asdu::header::{decode_ioa, encode_ioa, AsduAddressing, Ioa, Vsq};
use crate::asdu::ie::Siq;
use crate::asdu::payload::AsduPayload;
use crate::error::Result;

/// `M_SP_NA_1` (TypeID 1) — single-point information without time tag.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct M_SP_NA_1 {
    pub objects: Vec<(Ioa, Siq)>,
}

impl AsduPayload for M_SP_NA_1 {
    const TYPE_ID: u8 = 1;

    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        if vsq.sequence {
            // Sequence mode: emit only the first IOA followed by N SIQs.
            if let Some((ioa, siq)) = self.objects.first() {
                encode_ioa(buf, *ioa, addressing.ioa_size);
                siq.encode(buf);
                for (_, siq) in &self.objects[1..] {
                    siq.encode(buf);
                }
            }
        } else {
            for (ioa, siq) in &self.objects {
                encode_ioa(buf, *ioa, addressing.ioa_size);
                siq.encode(buf);
            }
        }
    }

    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let count = vsq.count as usize;
        let mut objects = Vec::with_capacity(count);
        if vsq.sequence {
            if count == 0 {
                return Ok(Self { objects });
            }
            let base = decode_ioa(buf, addressing.ioa_size)?;
            for i in 0..count {
                let siq = Siq::decode(buf)?;
                objects.push((Ioa(base.0 + i as u32), siq));
            }
        } else {
            for _ in 0..count {
                let ioa = decode_ioa(buf, addressing.ioa_size)?;
                let siq = Siq::decode(buf)?;
                objects.push((ioa, siq));
            }
        }
        Ok(Self { objects })
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
    use crate::asdu::ie::Quality;
    use bytes::BytesMut;

    fn obj(ioa: u32, on: bool) -> (Ioa, Siq) {
        (
            Ioa(ioa),
            Siq {
                on,
                quality: Quality::default(),
            },
        )
    }

    #[test]
    fn type_id_is_one() {
        assert_eq!(M_SP_NA_1::TYPE_ID, 1);
    }

    #[test]
    fn single_mode_three_points_104_addressing() {
        let payload = M_SP_NA_1 {
            objects: vec![obj(100, true), obj(101, false), obj(200, true)],
        };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::SPONTANEOUS),
            CommonAddress(7),
            Vsq::single(3),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        // header: 01 03 03 00 07 00
        // ioa 100=0x64,00,00 / siq=01 / ioa 101=0x65,00,00 / siq=00 / ioa 200=0xC8,00,00 / siq=01
        assert_eq!(
            &buf[..],
            &[
                0x01, 0x03, 0x03, 0x00, 0x07, 0x00, 0x64, 0x00, 0x00, 0x01, 0x65, 0x00, 0x00, 0x00,
                0xC8, 0x00, 0x00, 0x01,
            ]
        );
        // Roundtrip
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: M_SP_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn sequence_mode_shares_ioa() {
        let payload = M_SP_NA_1 {
            objects: vec![obj(50, true), obj(51, false), obj(52, true)],
        };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::PERIODIC),
            CommonAddress(1),
            Vsq::sequence(3),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        // SQ=1 means: emit IOA once (3 bytes) then 3 SIQs.
        // header: 01 83 01 00 01 00 | IOA=50 (32 00 00) | 01 00 01
        assert_eq!(
            &buf[..],
            &[0x01, 0x83, 0x01, 0x00, 0x01, 0x00, 0x32, 0x00, 0x00, 0x01, 0x00, 0x01]
        );
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: M_SP_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn wrong_type_id_is_rejected() {
        let asdu = Asdu {
            type_id: 99,
            vsq: Vsq::single(0),
            cot: Cot::default(),
            ca: CommonAddress(0),
            payload: Vec::new(),
        };
        let r: Result<M_SP_NA_1> = asdu.decode_payload(AsduAddressing::IEC104);
        assert!(matches!(r, Err(crate::error::Error::UnknownAsduType(99))));
    }
}
