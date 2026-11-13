//! The relay's WebSocket deframer, tunnel-frame cursor and CONNECT-line
//! parser on arbitrary client bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ovrelay::fuzzing::ws_frames(data);
    ovrelay::fuzzing::frame_cursor(data);
    ovrelay::fuzzing::connect_line(data);
});
