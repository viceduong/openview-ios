//! Tests that drive the crate through its C entry points, as the iOS shell
//! does.
//!
//! The test code hands the entry points pointers it owns for the duration of
//! each call, so its many `unsafe` blocks are not annotated one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

mod direct;
mod ffi;
mod header;
mod lan;
mod latency;
mod pin;
mod relay;

thread_local! {
    static THROTTLE: std::sync::Arc<ovcore::throttle::AuthThrottle> =
        std::sync::Arc::new(ovcore::throttle::AuthThrottle::default());
}

/// The PIN throttle for handshakes created on the current test thread (see
/// `session.rs`): per-thread, so parallel tests never share lockout state.
pub(crate) fn thread_throttle() -> std::sync::Arc<ovcore::throttle::AuthThrottle> {
    THROTTLE.with(std::sync::Arc::clone)
}
