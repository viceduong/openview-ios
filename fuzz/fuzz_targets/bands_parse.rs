//! The screen band container (`ovapp::bands`): parse arbitrary bytes and,
//! when they parse, decode the bands. Neither may panic nor allocate beyond
//! the bound (1 MiB here, as a caller with a small frame would pass).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(p) = ovapp::bands::parse_bounded(data, 1 << 20) {
        let mut out = Vec::new();
        let _ = ovapp::bands::decode_bands(&p.bands, &mut out);
        assert!(out.len() <= 1 << 20);
    }
});
