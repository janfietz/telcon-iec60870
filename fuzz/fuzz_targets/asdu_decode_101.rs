#![no_main]
//! Fuzz [`Asdu::decode`] across all IEC 60870-5-101 addressing profiles.
//!
//! Companion to `asdu_decode.rs`, which only exercises the IEC-60870-5-104
//! profile (2-octet COT, 2-octet CA, 3-octet IOA). FT 1.2 deployments use
//! a configurable mix of CotSize, CaSize, and IoaSize; each combination
//! has a different decode shape, so the bounds-check discipline must hold
//! on all of them.

use iec60870_proto::asdu::envelope::Asdu;
use iec60870_proto::asdu::header::{AsduAddressing, CaSize, CotSize, IoaSize};
use libfuzzer_sys::fuzz_target;

const PROFILES: &[AsduAddressing] = &[
    AsduAddressing {
        cot_size: CotSize::One,
        ca_size: CaSize::One,
        ioa_size: IoaSize::One,
    },
    AsduAddressing {
        cot_size: CotSize::One,
        ca_size: CaSize::One,
        ioa_size: IoaSize::Two,
    },
    AsduAddressing {
        cot_size: CotSize::One,
        ca_size: CaSize::Two,
        ioa_size: IoaSize::Two,
    },
    AsduAddressing {
        cot_size: CotSize::Two,
        ca_size: CaSize::One,
        ioa_size: IoaSize::Two,
    },
    AsduAddressing {
        cot_size: CotSize::Two,
        ca_size: CaSize::Two,
        ioa_size: IoaSize::Two,
    },
    AsduAddressing {
        cot_size: CotSize::Two,
        ca_size: CaSize::Two,
        ioa_size: IoaSize::Three,
    },
];

fuzz_target!(|data: &[u8]| {
    for addressing in PROFILES {
        let _ = Asdu::decode(&mut &*data, *addressing);
    }
});
