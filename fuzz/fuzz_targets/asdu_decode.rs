#![no_main]

use iec60870_proto::asdu::envelope::Asdu;
use iec60870_proto::asdu::header::AsduAddressing;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = Asdu::decode(&mut &*data, AsduAddressing::IEC104);
});
