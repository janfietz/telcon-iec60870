#![no_main]

use iec60870_proto::frame104::Codec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = Codec::decode_slice(data);
});
