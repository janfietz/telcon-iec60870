#![no_main]

use iec60870_proto::frame101::codec::Codec;
use iec60870_proto::frame101::frame::LinkAddressSize;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Exercise both address widths so both code paths are covered.
    let _ = Codec::decode_slice(data, LinkAddressSize::One);
    let _ = Codec::decode_slice(data, LinkAddressSize::Two);
});
