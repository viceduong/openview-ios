//! The anti-replay window against a model: it must never accept a
//! duplicate or a sequence number older than the window.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ovcore::fuzzing::replay_window(data);
});
