//! The [`Asdu`] envelope: type-id + VSQ + COT + CA header, plus the
//! information-objects payload kept either as typed bytes (via
//! [`AsduPayload`]) or as a raw byte slice (the always-available fallback
//! for unknown type ids).

use bytes::{Buf, BufMut};

use crate::asdu::cot::Cot;
use crate::asdu::header::{decode_ca, encode_ca, AsduAddressing, CommonAddress, CotSize, Vsq};
use crate::asdu::payload::AsduPayload;
use crate::error::{Error, Result};

/// The wire form of an ASDU after the header has been parsed.
///
/// The information-objects payload is held as raw bytes. Call
/// [`Asdu::decode_payload`] (or the typed convenience methods on the
/// concrete payload types) to interpret the bytes as a specific Type ID.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct Asdu {
    pub type_id: u8,
    pub vsq: Vsq,
    pub cot: Cot,
    pub ca: CommonAddress,
    /// Information-objects section, exactly as it appeared on the wire.
    pub payload: Vec<u8>,
}

impl Asdu {
    /// Header size in bytes for the given addressing profile (Type ID + VSQ + COT + CA).
    pub fn header_len(addressing: AsduAddressing) -> usize {
        1 + Vsq::LEN + addressing.cot_len() + addressing.ca_len()
    }

    /// Build an Asdu by serialising a typed payload.
    pub fn from_payload<P: AsduPayload>(
        cot: Cot,
        ca: CommonAddress,
        vsq: Vsq,
        payload: &P,
        addressing: AsduAddressing,
    ) -> Self {
        let mut bytes = Vec::new();
        payload.encode_information_objects(&mut bytes, vsq, addressing);
        Self {
            type_id: P::TYPE_ID,
            vsq,
            cot,
            ca,
            payload: bytes,
        }
    }

    /// Decode the raw `payload` bytes as the concrete type `P`. Returns
    /// [`Error::UnknownAsduType`] if the type id does not match `P::TYPE_ID`.
    pub fn decode_payload<P: AsduPayload>(&self, addressing: AsduAddressing) -> Result<P> {
        if self.type_id != P::TYPE_ID {
            return Err(Error::UnknownAsduType(self.type_id));
        }
        let mut slice: &[u8] = &self.payload;
        P::decode_information_objects(&mut slice, self.vsq, addressing)
    }

    /// Encode the complete ASDU (header + payload bytes) to `buf`.
    pub fn encode<B: BufMut>(&self, buf: &mut B, addressing: AsduAddressing) {
        buf.put_u8(self.type_id);
        self.vsq.encode(buf);
        match addressing.cot_size {
            CotSize::One => self.cot.encode_1(buf),
            CotSize::Two => self.cot.encode_2(buf),
        }
        encode_ca(buf, self.ca, addressing.ca_size);
        buf.put_slice(&self.payload);
    }

    /// Decode an ASDU. The `payload` field captures the remaining bytes of
    /// the input. Returns [`Error::Incomplete`] if the input is shorter than
    /// the header.
    pub fn decode<B: Buf>(buf: &mut B, addressing: AsduAddressing) -> Result<Self> {
        let header_len = Self::header_len(addressing);
        if buf.remaining() < header_len {
            return Err(Error::Incomplete {
                needed: header_len,
                have: buf.remaining(),
            });
        }
        let type_id = buf.get_u8();
        let vsq = Vsq::decode(buf)?;
        let cot = match addressing.cot_size {
            CotSize::One => Cot::decode_1(buf)?,
            CotSize::Two => Cot::decode_2(buf)?,
        };
        let ca = decode_ca(buf, addressing.ca_size)?;
        let mut payload = vec![0u8; buf.remaining()];
        buf.copy_to_slice(&mut payload);
        Ok(Self {
            type_id,
            vsq,
            cot,
            ca,
            payload,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asdu::cot::Cause;
    use bytes::BytesMut;

    #[test]
    fn header_len_iec104() {
        // type-id (1) + vsq (1) + cot (2) + ca (2)
        assert_eq!(Asdu::header_len(AsduAddressing::IEC104), 6);
    }

    #[test]
    fn encode_then_decode_104_roundtrip() {
        let asdu = Asdu {
            type_id: 9,
            vsq: Vsq::single(2),
            cot: Cot::with(Cause::SPONTANEOUS),
            ca: CommonAddress(0x1234),
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        // Layout: TID(1) | VSQ(1) | COT lo,hi (2) | CA lo,hi (2) | payload (4)
        assert_eq!(
            &buf[..],
            &[0x09, 0x02, 0x03, 0x00, 0x34, 0x12, 0xDE, 0xAD, 0xBE, 0xEF]
        );
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        assert_eq!(parsed, asdu);
    }

    #[test]
    fn encode_then_decode_101_short_address_roundtrip() {
        let addressing = AsduAddressing {
            cot_size: CotSize::One,
            ca_size: crate::asdu::header::CaSize::One,
            ioa_size: crate::asdu::header::IoaSize::One,
        };
        let asdu = Asdu {
            type_id: 1,
            vsq: Vsq::single(1),
            cot: Cot::with(Cause::INTERROGATED_GENERAL),
            ca: CommonAddress(0x10),
            payload: vec![0x07, 0x81], // IOA=7, SIQ=0x81
        };
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, addressing);
        assert_eq!(&buf[..], &[0x01, 0x01, 0x14, 0x10, 0x07, 0x81]);
        let mut slice: &[u8] = &buf;
        assert_eq!(Asdu::decode(&mut slice, addressing).unwrap(), asdu);
    }

    #[test]
    fn decode_rejects_short_header() {
        let bytes = [0x01, 0x02, 0x03]; // less than 6 bytes
        let mut slice: &[u8] = &bytes;
        assert!(matches!(
            Asdu::decode(&mut slice, AsduAddressing::IEC104),
            Err(Error::Incomplete { needed: 6, have: 3 })
        ));
    }
}
