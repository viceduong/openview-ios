//! Arbitrary bytes into both sides of the handshake state machine, in every
//! auth mode. `feed` must never panic, whatever the peer sends.
#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use ovcore::handshake::{Auth, Handshake, Role};
use ovcore::identity::Identity;
use ovcore::throttle::AuthThrottle;

/// PBKDF2 is deliberately slow: stretch the PIN once.
fn psk() -> [u8; 32] {
    static PSK: OnceLock<[u8; 32]> = OnceLock::new();
    *PSK.get_or_init(|| ovcore::pin::pin_psk(b"123456"))
}

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else {
        return;
    };
    let auth = match sel % 3 {
        0 => Auth::Pin(psk()),
        1 => Auth::PairingCode("1234567890".into()),
        _ => Auth::DirectDevice,
    };
    let role = if sel & 0x80 == 0 {
        Role::Server
    } else {
        Role::Client
    };
    let id = Identity::from_master_seed(&[7u8; 32]);
    let mut hs = Handshake::new(role, 42, &id, auth);
    hs.set_throttle(Arc::new(AuthThrottle::default()));
    let _ = hs.take_events();
    // Split the input at a data-chosen point to exercise reassembly.
    let cut = usize::from(sel).min(rest.len());
    let _ = hs.feed(&rest[..cut]);
    let _ = hs.feed(&rest[cut..]);
    let _ = hs.take_events();
});
