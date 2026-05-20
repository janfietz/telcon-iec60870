//! Monitor-direction ASDU types (those in the `M_*` family).
//!
//! Each type carries a list of information objects. When the ASDU's VSQ has
//! `SQ = 0`, every object includes its own IOA; when `SQ = 1`, only the first
//! object has an IOA and the rest are implicitly at consecutive addresses.
//!
//! Time-tagged variants (`*_TB_1`, `*_TD_1`, ..., `*_TF_1`) always use
//! `SQ = 0` by convention -- the time tag is per-object.

#![allow(non_camel_case_types)]

use bytes::{Buf, BufMut};

use crate::asdu::header::{AsduAddressing, Ioa, Vsq};
use crate::asdu::ie::{Cp56Time2a, Diq, Nva, Qds, Siq, Sva, R32};
use crate::asdu::io_list::{decode_io_list, encode_io_list};
use crate::asdu::payload::AsduPayload;
use crate::error::Result;

// Macro to cut down repetition for the simple "IOA + one IE" pattern.
macro_rules! io_payload {
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
                    <$value as IeWrite>::write(v, b)
                });
            }

            fn decode_information_objects<B: Buf>(
                buf: &mut B,
                vsq: Vsq,
                addressing: AsduAddressing,
            ) -> Result<Self> {
                let objects = decode_io_list(buf, vsq, addressing, |b| {
                    <$value as IeRead>::read(b)
                })?;
                Ok(Self { objects })
            }
        }
    };
}

// Adapter traits so the macro can call IE methods uniformly across value types.
trait IeWrite {
    fn write<B: BufMut>(&self, buf: &mut B);
}
trait IeRead: Sized {
    fn read<B: Buf>(buf: &mut B) -> Result<Self>;
}

impl IeWrite for Siq {
    fn write<B: BufMut>(&self, buf: &mut B) {
        Siq::encode(*self, buf);
    }
}
impl IeRead for Siq {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Siq::decode(buf)
    }
}

impl IeWrite for Diq {
    fn write<B: BufMut>(&self, buf: &mut B) {
        Diq::encode(*self, buf);
    }
}
impl IeRead for Diq {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Diq::decode(buf)
    }
}

// Composite (value, QDS) pairs are flattened on the wire.
impl IeWrite for (Nva, Qds) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl IeRead for (Nva, Qds) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Nva::decode(buf)?, Qds::decode(buf)?))
    }
}

impl IeWrite for (Sva, Qds) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl IeRead for (Sva, Qds) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Sva::decode(buf)?, Qds::decode(buf)?))
    }
}

impl IeWrite for (R32, Qds) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl IeRead for (R32, Qds) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((R32::decode(buf)?, Qds::decode(buf)?))
    }
}

impl IeWrite for (Siq, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl IeRead for (Siq, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Siq::decode(buf)?, Cp56Time2a::decode(buf)?))
    }
}

impl IeWrite for (Diq, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
    }
}
impl IeRead for (Diq, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((Diq::decode(buf)?, Cp56Time2a::decode(buf)?))
    }
}

impl IeWrite for (Nva, Qds, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
        self.2.encode(buf);
    }
}
impl IeRead for (Nva, Qds, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((
            Nva::decode(buf)?,
            Qds::decode(buf)?,
            Cp56Time2a::decode(buf)?,
        ))
    }
}

impl IeWrite for (Sva, Qds, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
        self.2.encode(buf);
    }
}
impl IeRead for (Sva, Qds, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((
            Sva::decode(buf)?,
            Qds::decode(buf)?,
            Cp56Time2a::decode(buf)?,
        ))
    }
}

impl IeWrite for (R32, Qds, Cp56Time2a) {
    fn write<B: BufMut>(&self, buf: &mut B) {
        self.0.encode(buf);
        self.1.encode(buf);
        self.2.encode(buf);
    }
}
impl IeRead for (R32, Qds, Cp56Time2a) {
    fn read<B: Buf>(buf: &mut B) -> Result<Self> {
        Ok((
            R32::decode(buf)?,
            Qds::decode(buf)?,
            Cp56Time2a::decode(buf)?,
        ))
    }
}

// ---------------------------------------------------------------------------
// Without time tag
// ---------------------------------------------------------------------------

io_payload!(
    /// `M_SP_NA_1` (TypeID 1) — single-point information.
    M_SP_NA_1, type_id = 1, value: Siq
);

io_payload!(
    /// `M_DP_NA_1` (TypeID 3) — double-point information.
    M_DP_NA_1, type_id = 3, value: Diq
);

io_payload!(
    /// `M_ME_NA_1` (TypeID 9) — measured value, normalised, with quality.
    M_ME_NA_1, type_id = 9, value: (Nva, Qds)
);

io_payload!(
    /// `M_ME_NB_1` (TypeID 11) — measured value, scaled, with quality.
    M_ME_NB_1, type_id = 11, value: (Sva, Qds)
);

io_payload!(
    /// `M_ME_NC_1` (TypeID 13) — measured value, short floating point, with quality.
    M_ME_NC_1, type_id = 13, value: (R32, Qds)
);

// ---------------------------------------------------------------------------
// With CP56Time2a time tag
// ---------------------------------------------------------------------------

io_payload!(
    /// `M_SP_TB_1` (TypeID 30) — single-point info with CP56Time2a.
    M_SP_TB_1, type_id = 30, value: (Siq, Cp56Time2a)
);

io_payload!(
    /// `M_DP_TB_1` (TypeID 31) — double-point info with CP56Time2a.
    M_DP_TB_1, type_id = 31, value: (Diq, Cp56Time2a)
);

io_payload!(
    /// `M_ME_TD_1` (TypeID 34) — measured value, normalised, with CP56Time2a.
    M_ME_TD_1, type_id = 34, value: (Nva, Qds, Cp56Time2a)
);

io_payload!(
    /// `M_ME_TE_1` (TypeID 35) — measured value, scaled, with CP56Time2a.
    M_ME_TE_1, type_id = 35, value: (Sva, Qds, Cp56Time2a)
);

io_payload!(
    /// `M_ME_TF_1` (TypeID 36) — measured value, float, with CP56Time2a.
    M_ME_TF_1, type_id = 36, value: (R32, Qds, Cp56Time2a)
);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asdu::cot::{Cause, Cot};
    use crate::asdu::envelope::Asdu;
    use crate::asdu::header::CommonAddress;
    use crate::asdu::ie::{DoublePoint, Quality};
    use bytes::BytesMut;

    fn roundtrip_iec104<P>(payload: &P, vsq: Vsq)
    where
        P: AsduPayload + Clone + PartialEq + std::fmt::Debug,
    {
        let asdu = Asdu::from_payload(
            Cot::with(Cause::SPONTANEOUS),
            CommonAddress(1),
            vsq,
            payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        assert_eq!(parsed.type_id, P::TYPE_ID);
        let decoded: P = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(&decoded, payload);
    }

    #[test]
    fn sp_na_1_roundtrip() {
        let payload = M_SP_NA_1 {
            objects: vec![
                (
                    Ioa(100),
                    Siq {
                        on: true,
                        quality: Quality::default(),
                    },
                ),
                (
                    Ioa(101),
                    Siq {
                        on: false,
                        quality: Quality {
                            invalid: true,
                            ..Default::default()
                        },
                    },
                ),
            ],
        };
        roundtrip_iec104(&payload, Vsq::single(2));
        roundtrip_iec104(&payload, Vsq::sequence(2));
    }

    #[test]
    fn dp_na_1_roundtrip() {
        let payload = M_DP_NA_1 {
            objects: vec![
                (
                    Ioa(10),
                    Diq {
                        state: DoublePoint::On,
                        quality: Quality::default(),
                    },
                ),
                (
                    Ioa(11),
                    Diq {
                        state: DoublePoint::Off,
                        quality: Quality::default(),
                    },
                ),
            ],
        };
        roundtrip_iec104(&payload, Vsq::single(2));
    }

    #[test]
    fn me_nb_1_scaled_with_quality() {
        let payload = M_ME_NB_1 {
            objects: vec![
                (Ioa(500), (Sva(1234), Qds::default())),
                (
                    Ioa(501),
                    (
                        Sva(-32768),
                        Qds {
                            overflow: true,
                            quality: Quality::default(),
                        },
                    ),
                ),
            ],
        };
        roundtrip_iec104(&payload, Vsq::single(2));
    }

    #[test]
    fn me_nc_1_float_with_quality_sequence_mode() {
        let payload = M_ME_NC_1 {
            objects: vec![
                (Ioa(1000), (R32(50.0), Qds::default())),
                (Ioa(1001), (R32(51.5), Qds::default())),
                (Ioa(1002), (R32(-10.0), Qds::default())),
            ],
        };
        roundtrip_iec104(&payload, Vsq::sequence(3));
    }

    #[test]
    fn me_tf_1_float_with_time() {
        let time = Cp56Time2a {
            milliseconds: 12_345,
            minute: 30,
            hour: 12,
            day: 1,
            day_of_week: 1,
            month: 6,
            year: 24,
            ..Default::default()
        };
        let payload = M_ME_TF_1 {
            objects: vec![(Ioa(7), (R32(42.5), Qds::default(), time))],
        };
        roundtrip_iec104(&payload, Vsq::single(1));
    }

    #[test]
    fn sp_tb_1_with_time() {
        let time = Cp56Time2a {
            milliseconds: 0,
            minute: 0,
            hour: 0,
            day: 1,
            day_of_week: 1,
            month: 1,
            year: 24,
            ..Default::default()
        };
        let payload = M_SP_TB_1 {
            objects: vec![(
                Ioa(100),
                (
                    Siq {
                        on: true,
                        quality: Quality::default(),
                    },
                    time,
                ),
            )],
        };
        roundtrip_iec104(&payload, Vsq::single(1));
    }

    #[test]
    fn known_byte_layout_me_nc_1_single_mode() {
        // One object at IOA=1, value=1.0, quality clear
        let payload = M_ME_NC_1 {
            objects: vec![(Ioa(1), (R32(1.0), Qds::default()))],
        };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::SPONTANEOUS),
            CommonAddress(1),
            Vsq::single(1),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        // TID=13 VSQ=01 COT=03,00 CA=01,00 IOA=01,00,00 R32=00,00,80,3F QDS=00
        assert_eq!(
            &buf[..],
            &[0x0D, 0x01, 0x03, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3F, 0x00]
        );
    }
}
