//! APDU codec: serialise and deserialise the on-wire APDU.
//!
//! Wire layout (6 + ASDU octets):
//!
//! ```text
//! 0x68  L  C1  C2  C3  C4   ASDU...
//! ```
//!
//! `L = 4 + |ASDU|`. The low two bits of `C1` discriminate the format
//! (`..0` = I, `01` = S, `11` = U).

use bytes::{Buf, BufMut};

use crate::frame104::apdu::{Apdu, UFunction, MAX_ASDU_LEN};
use crate::frame104::seq::SeqNo;
use crate::{Error, Result};

/// Start-of-frame byte for all IEC 60870-5-104 APDUs.
pub const START: u8 = 0x68;

/// Stateless APDU codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct Codec;

impl Codec {
    /// Encode an APDU to `buf`. Returns the number of bytes written, or
    /// [`Error::AsduTooLong`] if the I-frame's ASDU exceeds
    /// [`MAX_ASDU_LEN`]. The buffer is left untouched on error so a
    /// rejected encode never produces a partial frame.
    pub fn encode<B: BufMut>(apdu: &Apdu, buf: &mut B) -> Result<usize> {
        if let Apdu::I { asdu, .. } = apdu {
            if asdu.len() > MAX_ASDU_LEN {
                return Err(Error::AsduTooLong {
                    len: asdu.len(),
                    max: MAX_ASDU_LEN,
                });
            }
        }
        let start = buf.remaining_mut();
        match apdu {
            Apdu::I { send, recv, asdu } => {
                let length = 4 + asdu.len() as u8;
                buf.put_u8(START);
                buf.put_u8(length);
                buf.put_u16_le(send.to_wire()); // C1 C2, low bit of C1 must be 0
                buf.put_u16_le(recv.to_wire()); // C3 C4
                buf.put_slice(asdu);
            }
            Apdu::S { recv } => {
                buf.put_u8(START);
                buf.put_u8(4);
                buf.put_u8(0x01); // C1: S-format discriminator
                buf.put_u8(0x00); // C2
                buf.put_u16_le(recv.to_wire()); // C3 C4
            }
            Apdu::U { function } => {
                buf.put_u8(START);
                buf.put_u8(4);
                buf.put_u8(function.first_octet());
                buf.put_u8(0x00);
                buf.put_u8(0x00);
                buf.put_u8(0x00);
            }
        }
        Ok(start - buf.remaining_mut())
    }

    /// Attempt to parse one APDU from the head of `buf`. Returns `Ok(None)`
    /// if the buffer doesn't yet contain a complete frame — callers should
    /// retry after more bytes arrive. Returns `Err` for malformed frames.
    pub fn decode<B: Buf>(buf: &mut B) -> Result<Option<Apdu>> {
        if buf.remaining() < 2 {
            return Ok(None);
        }
        // Peek the start byte and length without consuming until we know the
        // full frame is available. Buf::chunk doesn't guarantee contiguity
        // across calls so we use a small staging slice.
        let mut header = [0u8; 2];
        buf.copy_to_slice(&mut header);
        if header[0] != START {
            return Err(Error::InvalidStartByte {
                expected: START,
                got: header[0],
            });
        }
        let length = header[1] as usize;
        if length < 4 {
            return Err(Error::LengthMismatch {
                declared: length,
                actual: 4,
            });
        }
        if buf.remaining() < length {
            // Re-prepend nothing — the input is consumed and the caller must
            // accumulate elsewhere. Use the slice-based `decode_slice` if you
            // need re-entrant decoding without consuming on incomplete input.
            return Err(Error::Incomplete {
                needed: length,
                have: buf.remaining(),
            });
        }

        let c1 = buf.get_u8();
        let c2 = buf.get_u8();
        let c3 = buf.get_u8();
        let c4 = buf.get_u8();

        if c1 & 0x01 == 0 {
            // I-format
            let send = SeqNo::from_wire(u16::from_le_bytes([c1, c2]));
            let recv = SeqNo::from_wire(u16::from_le_bytes([c3, c4]));
            let asdu_len = length - 4;
            let mut asdu = vec![0u8; asdu_len];
            buf.copy_to_slice(&mut asdu);
            Ok(Some(Apdu::I { send, recv, asdu }))
        } else if c1 & 0x03 == 0x01 {
            // S-format
            if length != 4 {
                return Err(Error::LengthMismatch {
                    declared: length,
                    actual: 4,
                });
            }
            let recv = SeqNo::from_wire(u16::from_le_bytes([c3, c4]));
            Ok(Some(Apdu::S { recv }))
        } else {
            // U-format
            if length != 4 {
                return Err(Error::LengthMismatch {
                    declared: length,
                    actual: 4,
                });
            }
            let function = UFunction::from_first_octet(c1).ok_or(Error::UnsupportedFormat)?;
            // C2/C3/C4 must be zero in a valid U-frame; we ignore non-zero
            // for forward compatibility with vendor extensions.
            let _ = (c2, c3, c4);
            Ok(Some(Apdu::U { function }))
        }
    }

    /// Like [`decode`](Self::decode) but operates on a `&[u8]` slice without
    /// consuming bytes on incomplete or invalid input. Returns
    /// `Ok((apdu, consumed))` on success, `Ok(None)` if more bytes are needed.
    pub fn decode_slice(buf: &[u8]) -> Result<Option<(Apdu, usize)>> {
        if buf.len() < 2 {
            return Ok(None);
        }
        if buf[0] != START {
            return Err(Error::InvalidStartByte {
                expected: START,
                got: buf[0],
            });
        }
        let length = buf[1] as usize;
        if length < 4 {
            return Err(Error::LengthMismatch {
                declared: length,
                actual: 4,
            });
        }
        let total = 2 + length;
        if buf.len() < total {
            return Ok(None);
        }
        let mut slice = &buf[2..total];
        let apdu = Self::decode_payload(&mut slice, length)?;
        Ok(Some((apdu, total)))
    }

    fn decode_payload(buf: &mut &[u8], length: usize) -> Result<Apdu> {
        let c1 = buf.get_u8();
        let c2 = buf.get_u8();
        let c3 = buf.get_u8();
        let c4 = buf.get_u8();
        if c1 & 0x01 == 0 {
            let send = SeqNo::from_wire(u16::from_le_bytes([c1, c2]));
            let recv = SeqNo::from_wire(u16::from_le_bytes([c3, c4]));
            let asdu_len = length - 4;
            let mut asdu = vec![0u8; asdu_len];
            buf.copy_to_slice(&mut asdu);
            Ok(Apdu::I { send, recv, asdu })
        } else if c1 & 0x03 == 0x01 {
            if length != 4 {
                return Err(Error::LengthMismatch {
                    declared: length,
                    actual: 4,
                });
            }
            let recv = SeqNo::from_wire(u16::from_le_bytes([c3, c4]));
            Ok(Apdu::S { recv })
        } else {
            if length != 4 {
                return Err(Error::LengthMismatch {
                    declared: length,
                    actual: 4,
                });
            }
            let _ = (c2, c3, c4);
            let function = UFunction::from_first_octet(c1).ok_or(Error::UnsupportedFormat)?;
            Ok(Apdu::U { function })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn encode(apdu: &Apdu) -> Vec<u8> {
        let mut buf = BytesMut::new();
        Codec::encode(apdu, &mut buf).expect("encode");
        buf.to_vec()
    }

    #[test]
    fn encode_rejects_oversized_asdu() {
        let mut buf = BytesMut::new();
        let huge = vec![0u8; MAX_ASDU_LEN + 1];
        let result = Codec::encode(
            &Apdu::I {
                send: SeqNo::new(0),
                recv: SeqNo::new(0),
                asdu: huge,
            },
            &mut buf,
        );
        assert!(
            matches!(
                result,
                Err(Error::AsduTooLong {
                    len,
                    max,
                }) if len == MAX_ASDU_LEN + 1 && max == MAX_ASDU_LEN,
            ),
            "expected AsduTooLong, got {result:?}",
        );
        assert!(
            buf.is_empty(),
            "rejected encode must not write partial frames",
        );
    }

    #[test]
    fn startdt_act_wire_layout() {
        let bytes = encode(&Apdu::U {
            function: UFunction::StartDtAct,
        });
        assert_eq!(bytes, vec![0x68, 0x04, 0x07, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn s_frame_carries_only_nr() {
        let bytes = encode(&Apdu::S {
            recv: SeqNo::new(7),
        });
        // N(R) = 7, on wire = 7 << 1 = 14 = 0x0E
        assert_eq!(bytes, vec![0x68, 0x04, 0x01, 0x00, 0x0E, 0x00]);
    }

    #[test]
    fn i_frame_carries_ns_nr_and_asdu() {
        let bytes = encode(&Apdu::I {
            send: SeqNo::new(0x100),
            recv: SeqNo::new(0x200),
            asdu: vec![0xAA, 0xBB],
        });
        // N(S)<<1 = 0x200 LE → [0x00, 0x02]; N(R)<<1 = 0x400 LE → [0x00, 0x04]
        assert_eq!(bytes, vec![0x68, 0x06, 0x00, 0x02, 0x00, 0x04, 0xAA, 0xBB]);
    }

    #[test]
    fn decode_slice_roundtrip_u() {
        let bytes = encode(&Apdu::U {
            function: UFunction::TestFrAct,
        });
        let (apdu, consumed) = Codec::decode_slice(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            apdu,
            Apdu::U {
                function: UFunction::TestFrAct
            }
        );
    }

    #[test]
    fn decode_slice_roundtrip_s() {
        let bytes = encode(&Apdu::S {
            recv: SeqNo::new(0x1234),
        });
        let (apdu, consumed) = Codec::decode_slice(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            apdu,
            Apdu::S {
                recv: SeqNo::new(0x1234)
            }
        );
    }

    #[test]
    fn decode_slice_roundtrip_i_with_asdu() {
        let asdu = (0u8..40).collect::<Vec<u8>>();
        let bytes = encode(&Apdu::I {
            send: SeqNo::new(11),
            recv: SeqNo::new(22),
            asdu: asdu.clone(),
        });
        let (apdu, consumed) = Codec::decode_slice(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            apdu,
            Apdu::I {
                send: SeqNo::new(11),
                recv: SeqNo::new(22),
                asdu
            }
        );
    }

    #[test]
    fn decode_slice_incomplete_returns_none() {
        // Just the start byte
        assert_eq!(Codec::decode_slice(&[0x68]).unwrap(), None);
        // Header complete but body missing
        assert_eq!(Codec::decode_slice(&[0x68, 0x04, 0x07]).unwrap(), None);
    }

    #[test]
    fn decode_slice_rejects_bad_start_byte() {
        let r = Codec::decode_slice(&[0x69, 0x04, 0x07, 0x00, 0x00, 0x00]);
        assert!(matches!(
            r,
            Err(Error::InvalidStartByte {
                expected: 0x68,
                got: 0x69
            })
        ));
    }

    #[test]
    fn decode_slice_rejects_short_length() {
        let r = Codec::decode_slice(&[0x68, 0x03, 0x07, 0x00, 0x00]);
        assert!(matches!(
            r,
            Err(Error::LengthMismatch {
                declared: 3,
                actual: 4
            })
        ));
    }

    #[test]
    fn decode_slice_rejects_unknown_u_function() {
        let r = Codec::decode_slice(&[0x68, 0x04, 0xFF, 0x00, 0x00, 0x00]);
        assert!(matches!(r, Err(Error::UnsupportedFormat)));
    }

    #[test]
    fn decode_slice_does_not_overconsume() {
        // Two STARTDT_ACT frames back-to-back
        let one = encode(&Apdu::U {
            function: UFunction::StartDtAct,
        });
        let mut both = one.clone();
        both.extend_from_slice(&one);
        let (apdu1, consumed1) = Codec::decode_slice(&both).unwrap().unwrap();
        assert_eq!(consumed1, one.len());
        assert_eq!(
            apdu1,
            Apdu::U {
                function: UFunction::StartDtAct
            }
        );
        let (apdu2, consumed2) = Codec::decode_slice(&both[consumed1..]).unwrap().unwrap();
        assert_eq!(consumed2, one.len());
        assert_eq!(
            apdu2,
            Apdu::U {
                function: UFunction::StartDtAct
            }
        );
    }
}
