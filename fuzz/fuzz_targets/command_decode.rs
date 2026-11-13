//! The in-record command envelope (`ovcore::commands`) and the record
//! header parser must reject any byte string without panicking.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ovcore::commands::Message;

fuzz_target!(|data: &[u8]| {
    if let Some(m) = Message::decode(data) {
        // Whatever decodes must re-encode to a prefix of the input.
        let enc = m.encode();
        assert_eq!(&data[..enc.len()], &enc[..]);
        let _ = m.hello_parts();
        let _ = m.ready_random();
    }
    let _ = ovcore::framing::parse_header(data);
});
