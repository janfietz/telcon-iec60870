//! FT 1.2 codec: encode and decode [`Frame101`] values to/from raw bytes.
//!
//! The codec is **stateless** — it holds no per-connection state and can be
//! shared freely. Both encode and decode paths are allocation-free except for
//! the `asdu: Vec<u8>` field of [`Frame101::Variable`].
//!
//! ## Wire layouts
//!
//! ```text
//! Single:   [0xE5]  or  [0xA2]
//!
//! Fixed:    0x10  C  LA..  CS  0x16
//!
//! Variable: 0x68  L  L  0x68  C  LA..  ASDU..  CS  0x16
//! ```
//!
//! `CS` is the arithmetic sum (mod 256) of the bytes from `C` through the
//! last ASDU byte. This is **not** CRC-16.

use bytes::BufMut;

use crate::error::{Error, Result};
use crate::frame101::frame::{Frame101, LinkAddress, LinkAddressSize, SingleChar};

/// Start byte of a fixed-length frame.
pub const START_FIXED: u8 = 0x10;
/// Start byte of a variable-length frame.
pub const START_VAR: u8 = 0x68;
/// End byte common to fixed- and variable-length frames.
pub const END: u8 = 0x16;

/// Stateless FT 1.2 codec.
///
/// # Examples
///
/// ```
/// use iec60870_proto::frame101::codec::Codec;
/// use iec60870_proto::frame101::frame::{Frame101, LinkAddress, LinkAddressSize, SingleChar};
///
/// // Encode an ACK single-character frame.
/// let mut buf = Vec::new();
/// Codec::encode(&Frame101::Single(SingleChar::Ack), &mut buf, LinkAddressSize::One);
/// assert_eq!(buf, [0xE5]);
///
/// // Decode it back.
/// let (frame, consumed) = Codec::decode_slice(&buf, LinkAddressSize::One).unwrap().unwrap();
/// assert_eq!(frame, Frame101::Single(SingleChar::Ack));
/// assert_eq!(consumed, 1);
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct Codec;

impl Codec {
    /// Encode `frame` into `buf`. Returns the number of bytes written.
    ///
    /// The link address is encoded with the supplied `addr_size`. For
    /// [`Frame101::Single`] the `addr_size` is ignored.
    pub fn encode<B: BufMut>(frame: &Frame101, buf: &mut B, addr_size: LinkAddressSize) -> usize {
        match frame {
            Frame101::Single(sc) => {
                buf.put_u8(sc.as_byte());
                1
            }
            Frame101::Fixed { control, address } => {
                let cs = checksum_fixed(*control, *address, addr_size);
                buf.put_u8(START_FIXED);
                buf.put_u8(*control);
                address.encode_to(buf, addr_size);
                buf.put_u8(cs);
                buf.put_u8(END);
                2 + addr_size.len() + 2 // start + control + LA + CS + end
            }
            Frame101::Variable {
                control,
                address,
                asdu,
            } => {
                let length = (1 + addr_size.len() + asdu.len()) as u8;
                let cs = checksum_variable(*control, *address, addr_size, asdu);
                buf.put_u8(START_VAR);
                buf.put_u8(length);
                buf.put_u8(length); // duplicated
                buf.put_u8(START_VAR);
                buf.put_u8(*control);
                address.encode_to(buf, addr_size);
                buf.put_slice(asdu);
                buf.put_u8(cs);
                buf.put_u8(END);
                // 4 header + control + LA + asdu + CS + END
                4 + 1 + addr_size.len() + asdu.len() + 1 + 1
            }
        }
    }

    /// Attempt to decode one frame from the head of `buf`.
    ///
    /// Returns `Ok(None)` if the buffer does not contain a complete frame —
    /// the caller should buffer more bytes and retry. Bytes are **never**
    /// consumed on `Ok(None)` or on `Err`.
    ///
    /// # Errors
    ///
    /// * [`Error::InvalidStartByte`] — the leading byte is not a known frame
    ///   discriminator and is not a single-character frame value.
    /// * [`Error::LengthOctetsDiffer`] — the two length bytes in a
    ///   variable-length frame do not match.
    /// * [`Error::InvalidEndByte`] — the end byte is not `0x16`.
    /// * [`Error::ChecksumMismatch`] — the computed checksum does not match the
    ///   transmitted checksum.
    pub fn decode_slice(
        buf: &[u8],
        addr_size: LinkAddressSize,
    ) -> Result<Option<(Frame101, usize)>> {
        if buf.is_empty() {
            return Ok(None);
        }

        match buf[0] {
            // ---- single-character frames ------------------------------------
            b if SingleChar::from_byte(b).is_some() => {
                let sc = SingleChar::from_byte(b).unwrap();
                Ok(Some((Frame101::Single(sc), 1)))
            }

            // ---- fixed-length frame (0x10) ---------------------------------
            START_FIXED => {
                // Need: start(1) + control(1) + LA(addr_size) + CS(1) + end(1)
                let total = 1 + 1 + addr_size.len() + 1 + 1;
                if buf.len() < total {
                    return Ok(None);
                }
                let control = buf[1];
                let la_start = 2;
                let la_end = la_start + addr_size.len();
                let address = LinkAddress::decode_from(&buf[la_start..la_end], addr_size).unwrap();
                let cs_wire = buf[la_end];
                let end = buf[la_end + 1];

                if end != END {
                    return Err(Error::InvalidEndByte {
                        expected: END,
                        got: end,
                    });
                }

                let cs_calc = checksum_fixed(control, address, addr_size);
                if cs_calc != cs_wire {
                    return Err(Error::ChecksumMismatch {
                        expected: cs_calc,
                        got: cs_wire,
                    });
                }

                Ok(Some((Frame101::Fixed { control, address }, total)))
            }

            // ---- variable-length frame (0x68) ------------------------------
            START_VAR => {
                // Minimum variable frame: 0x68 L L 0x68 C LA.. CS 0x16
                // That is 4 header + 1 control + addr_len + 0 ASDU + 1 CS + 1 end
                let _min_total = 4 + 1 + addr_size.len() + 1 + 1;
                if buf.len() < 6 {
                    // Not even enough to read both length octets + second 0x68
                    return Ok(None);
                }

                let l1 = buf[1];
                let l2 = buf[2];
                if l1 != l2 {
                    return Err(Error::LengthOctetsDiffer {
                        first: l1,
                        second: l2,
                    });
                }
                // L counts: control(1) + LA(addr_size) + ASDU; must be at least
                // 1 (control) + addr_size.
                let length = l1 as usize;
                let min_length = 1 + addr_size.len();
                if length < min_length {
                    return Err(Error::LengthMismatch {
                        declared: length,
                        actual: min_length,
                    });
                }

                // second start byte at offset 3
                if buf.len() < 4 {
                    return Ok(None);
                }
                // We do not reject a mismatched second start early — we wait
                // for the full frame to arrive first.

                // total = 4 (header) + length (C + LA + ASDU) + 1 CS + 1 END
                let total = 4 + length + 2;
                if buf.len() < total {
                    return Ok(None);
                }

                // Now we have enough bytes to validate everything.
                let second_start = buf[3];
                if second_start != START_VAR {
                    return Err(Error::InvalidStartByte {
                        expected: START_VAR,
                        got: second_start,
                    });
                }

                let control = buf[4];
                let la_start = 5;
                let la_end = la_start + addr_size.len();
                let address = LinkAddress::decode_from(&buf[la_start..la_end], addr_size).unwrap();
                let asdu_start = la_end;
                let asdu_end = 4 + length; // offset from buf start
                let asdu = buf[asdu_start..asdu_end].to_vec();
                let cs_wire = buf[asdu_end];
                let end = buf[asdu_end + 1];

                if end != END {
                    return Err(Error::InvalidEndByte {
                        expected: END,
                        got: end,
                    });
                }

                let cs_calc = checksum_variable(control, address, addr_size, &asdu);
                if cs_calc != cs_wire {
                    return Err(Error::ChecksumMismatch {
                        expected: cs_calc,
                        got: cs_wire,
                    });
                }

                Ok(Some((
                    Frame101::Variable {
                        control,
                        address,
                        asdu,
                    },
                    total,
                )))
            }

            // ---- unknown start byte ----------------------------------------
            got => Err(Error::InvalidStartByte {
                expected: START_FIXED, // arbitrary; indicates "one of 0x10/0x68/0xE5/0xA2"
                got,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Checksum helpers
// ---------------------------------------------------------------------------

/// Arithmetic checksum (mod 256) for a fixed-length frame.
///
/// Covers: `control` + all LA bytes.
fn checksum_fixed(control: u8, address: LinkAddress, size: LinkAddressSize) -> u8 {
    let mut sum: u16 = control as u16;
    match size {
        LinkAddressSize::One => sum += address.0,
        LinkAddressSize::Two => {
            let bytes = address.0.to_le_bytes();
            sum += bytes[0] as u16;
            sum += bytes[1] as u16;
        }
    }
    (sum & 0xFF) as u8
}

/// Arithmetic checksum (mod 256) for a variable-length frame.
///
/// Covers: `control` + all LA bytes + all ASDU bytes.
fn checksum_variable(control: u8, address: LinkAddress, size: LinkAddressSize, asdu: &[u8]) -> u8 {
    let mut sum: u16 = checksum_fixed(control, address, size) as u16;
    for &b in asdu {
        sum = (sum + b as u16) & 0xFF;
    }
    sum as u8
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn encode(frame: &Frame101, addr_size: LinkAddressSize) -> Vec<u8> {
        let mut buf = Vec::new();
        Codec::encode(frame, &mut buf, addr_size);
        buf
    }

    // ---- single-character frames -------------------------------------------

    #[test]
    fn single_ack_wire() {
        let bytes = encode(&Frame101::Single(SingleChar::Ack), LinkAddressSize::One);
        assert_eq!(bytes, [0xE5]);
    }

    #[test]
    fn single_nack_wire() {
        let bytes = encode(&Frame101::Single(SingleChar::Nack), LinkAddressSize::One);
        assert_eq!(bytes, [0xA2]);
    }

    #[test]
    fn decode_ack() {
        let (frame, consumed) = Codec::decode_slice(&[0xE5], LinkAddressSize::One)
            .unwrap()
            .unwrap();
        assert_eq!(frame, Frame101::Single(SingleChar::Ack));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn decode_nack() {
        let (frame, consumed) = Codec::decode_slice(&[0xA2], LinkAddressSize::One)
            .unwrap()
            .unwrap();
        assert_eq!(frame, Frame101::Single(SingleChar::Nack));
        assert_eq!(consumed, 1);
    }

    // ---- fixed-length frame ------------------------------------------------

    /// Spec example: control=0x49, addr=0x01 → CS = 0x49 + 0x01 = 0x4A.
    #[test]
    fn fixed_frame_spec_example() {
        let frame = Frame101::Fixed {
            control: 0x49,
            address: LinkAddress(0x01),
        };
        let bytes = encode(&frame, LinkAddressSize::One);
        assert_eq!(bytes, [0x10, 0x49, 0x01, 0x4A, 0x16]);
    }

    #[test]
    fn fixed_frame_decode_spec_example() {
        let (frame, consumed) =
            Codec::decode_slice(&[0x10, 0x49, 0x01, 0x4A, 0x16], LinkAddressSize::One)
                .unwrap()
                .unwrap();
        assert_eq!(
            frame,
            Frame101::Fixed {
                control: 0x49,
                address: LinkAddress(0x01),
            }
        );
        assert_eq!(consumed, 5);
    }

    // ---- variable-length frame ---------------------------------------------

    /// Spec example: control=0x73, addr=0x01, asdu=[0x09, 0x01].
    /// L = 1 (C) + 1 (LA) + 2 (ASDU) = 4
    /// CS = 0x73 + 0x01 + 0x09 + 0x01 = 0x7E
    #[test]
    fn variable_frame_spec_example() {
        let frame = Frame101::Variable {
            control: 0x73,
            address: LinkAddress(0x01),
            asdu: vec![0x09, 0x01],
        };
        let bytes = encode(&frame, LinkAddressSize::One);
        assert_eq!(
            bytes,
            [0x68, 0x04, 0x04, 0x68, 0x73, 0x01, 0x09, 0x01, 0x7E, 0x16]
        );
    }

    #[test]
    fn variable_frame_decode_spec_example() {
        let wire = [0x68u8, 0x04, 0x04, 0x68, 0x73, 0x01, 0x09, 0x01, 0x7E, 0x16];
        let (frame, consumed) = Codec::decode_slice(&wire, LinkAddressSize::One)
            .unwrap()
            .unwrap();
        assert_eq!(
            frame,
            Frame101::Variable {
                control: 0x73,
                address: LinkAddress(0x01),
                asdu: vec![0x09, 0x01],
            }
        );
        assert_eq!(consumed, 10);
    }

    // ---- error cases -------------------------------------------------------

    #[test]
    fn checksum_mismatch_fixed() {
        // CS should be 0x4A but we put 0xFF
        let r = Codec::decode_slice(&[0x10, 0x49, 0x01, 0xFF, 0x16], LinkAddressSize::One);
        assert!(matches!(
            r,
            Err(Error::ChecksumMismatch {
                expected: 0x4A,
                got: 0xFF
            })
        ));
    }

    #[test]
    fn checksum_mismatch_variable() {
        // corrupt CS byte (0x7E → 0x00)
        let wire = [0x68u8, 0x04, 0x04, 0x68, 0x73, 0x01, 0x09, 0x01, 0x00, 0x16];
        let r = Codec::decode_slice(&wire, LinkAddressSize::One);
        assert!(matches!(r, Err(Error::ChecksumMismatch { .. })));
    }

    #[test]
    fn length_octets_differ() {
        // L1=0x04, L2=0x05 — mismatch
        let wire = [0x68u8, 0x04, 0x05, 0x68, 0x73, 0x01, 0x09, 0x01, 0x7E, 0x16];
        let r = Codec::decode_slice(&wire, LinkAddressSize::One);
        assert!(matches!(
            r,
            Err(Error::LengthOctetsDiffer {
                first: 4,
                second: 5
            })
        ));
    }

    #[test]
    fn invalid_end_byte_fixed() {
        // Replace 0x16 with 0xFF
        let r = Codec::decode_slice(&[0x10, 0x49, 0x01, 0x4A, 0xFF], LinkAddressSize::One);
        assert!(matches!(
            r,
            Err(Error::InvalidEndByte {
                expected: 0x16,
                got: 0xFF
            })
        ));
    }

    #[test]
    fn invalid_end_byte_variable() {
        let wire = [0x68u8, 0x04, 0x04, 0x68, 0x73, 0x01, 0x09, 0x01, 0x7E, 0xFF];
        let r = Codec::decode_slice(&wire, LinkAddressSize::One);
        assert!(matches!(
            r,
            Err(Error::InvalidEndByte {
                expected: 0x16,
                got: 0xFF
            })
        ));
    }

    // ---- incomplete input returns Ok(None) ---------------------------------

    #[test]
    fn incomplete_single_char_empty() {
        assert_eq!(
            Codec::decode_slice(&[], LinkAddressSize::One).unwrap(),
            None
        );
    }

    #[test]
    fn incomplete_fixed_frame() {
        // Only start byte
        assert_eq!(
            Codec::decode_slice(&[0x10], LinkAddressSize::One).unwrap(),
            None
        );
        // start + control only
        assert_eq!(
            Codec::decode_slice(&[0x10, 0x49], LinkAddressSize::One).unwrap(),
            None
        );
    }

    #[test]
    fn incomplete_variable_frame() {
        // Only start byte
        assert_eq!(
            Codec::decode_slice(&[0x68], LinkAddressSize::One).unwrap(),
            None
        );
        // start + two length bytes + second start — body missing
        assert_eq!(
            Codec::decode_slice(&[0x68, 0x04, 0x04, 0x68], LinkAddressSize::One).unwrap(),
            None
        );
    }

    // ---- no over-consumption -----------------------------------------------

    #[test]
    fn no_overconsume_two_fixed_frames() {
        let frame = Frame101::Fixed {
            control: 0x49,
            address: LinkAddress(0x01),
        };
        let one = encode(&frame, LinkAddressSize::One);
        let mut both = one.clone();
        both.extend_from_slice(&one);

        let (f1, c1) = Codec::decode_slice(&both, LinkAddressSize::One)
            .unwrap()
            .unwrap();
        assert_eq!(c1, one.len());
        let (f2, c2) = Codec::decode_slice(&both[c1..], LinkAddressSize::One)
            .unwrap()
            .unwrap();
        assert_eq!(c2, one.len());
        assert_eq!(f1, f2);
    }

    // ---- two-octet address -------------------------------------------------

    #[test]
    fn fixed_frame_two_octet_addr() {
        let frame = Frame101::Fixed {
            control: 0x49,
            address: LinkAddress(0x0102),
        };
        let bytes = encode(&frame, LinkAddressSize::Two);
        // CS = 0x49 + 0x02 + 0x01 = 0x4C (little-endian: low byte 0x02, high byte 0x01)
        assert_eq!(bytes, [0x10, 0x49, 0x02, 0x01, 0x4C, 0x16]);
        let (decoded, consumed) = Codec::decode_slice(&bytes, LinkAddressSize::Two)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn variable_frame_two_octet_addr() {
        let frame = Frame101::Variable {
            control: 0x73,
            address: LinkAddress(0x0102),
            asdu: vec![0x09, 0x01],
        };
        let bytes = encode(&frame, LinkAddressSize::Two);
        let (decoded, consumed) = Codec::decode_slice(&bytes, LinkAddressSize::Two)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(consumed, bytes.len());
    }

    // ---- proptest ----------------------------------------------------------

    proptest! {
        #[test]
        fn prop_variable_roundtrip_one_octet(
            control: u8,
            addr in 0u16..=255,
            asdu in proptest::collection::vec(any::<u8>(), 0..32),
        ) {
            let frame = Frame101::Variable {
                control,
                address: LinkAddress(addr),
                asdu: asdu.clone(),
            };
            let bytes = encode(&frame, LinkAddressSize::One);
            let (decoded, consumed) = Codec::decode_slice(&bytes, LinkAddressSize::One)
                .unwrap()
                .unwrap();
            prop_assert_eq!(decoded, frame);
            prop_assert_eq!(consumed, bytes.len());
        }

        #[test]
        fn prop_variable_roundtrip_two_octet(
            control: u8,
            addr: u16,
            asdu in proptest::collection::vec(any::<u8>(), 0..32),
        ) {
            let frame = Frame101::Variable {
                control,
                address: LinkAddress(addr),
                asdu: asdu.clone(),
            };
            let bytes = encode(&frame, LinkAddressSize::Two);
            let (decoded, consumed) = Codec::decode_slice(&bytes, LinkAddressSize::Two)
                .unwrap()
                .unwrap();
            prop_assert_eq!(decoded, frame);
            prop_assert_eq!(consumed, bytes.len());
        }

        #[test]
        fn prop_fixed_roundtrip_one_octet(
            control: u8,
            addr in 0u16..=255,
        ) {
            let frame = Frame101::Fixed {
                control,
                address: LinkAddress(addr),
            };
            let bytes = encode(&frame, LinkAddressSize::One);
            let (decoded, consumed) = Codec::decode_slice(&bytes, LinkAddressSize::One)
                .unwrap()
                .unwrap();
            prop_assert_eq!(decoded, frame);
            prop_assert_eq!(consumed, bytes.len());
        }
    }
}
