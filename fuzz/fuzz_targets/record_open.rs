//! The record layer on arbitrary records: header parsing, the anti-replay
//! window and AEAD failure paths (`Session::decrypt_record`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ovcore::fuzzing::record_open(data);
});
