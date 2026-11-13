//! Session callback types (C function pointers) and [`Sink`], the
//! `(callback, user pointer)` pair every registered callback is stored as.

use std::ffi::{c_char, c_void};

/// Outbound datagram sink (handshake + record datagrams). Called on the
/// `ov_session_pump` caller thread.
pub type DatagramSink = extern "C" fn(user: *mut c_void, data: *const u8, len: usize);
/// Decrypted inbound record sink (payload bytes; channel reported separately).
pub type RecordSink = extern "C" fn(user: *mut c_void, channel: u8, data: *const u8, len: usize);
/// State transition sink: `state = 1` means established; the peer's 16-byte
/// fingerprint follows (all-zero when unknown).
pub type StateSink = extern "C" fn(user: *mut c_void, state: i32, fp: *const u8, fp_len: usize);
/// Direct-path send sink: put `data` on the wire of path
/// `path_id` (the NWConnection the shell opened for that candidate).
pub type DirectSendSink =
    extern "C" fn(user: *mut c_void, path_id: u32, data: *const u8, len: usize);
/// Direct-path open sink: the host offered candidate `addr`
/// (NUL-terminated `"ip:port"` / `"[v6]:port"`); open a UDP NWConnection
/// to it, feed its datagrams with `ov_session_on_direct_datagram(path_id)`
/// and send what `DirectSendSink` hands out for `path_id`.
pub type DirectPathSink = extern "C" fn(user: *mut c_void, path_id: u32, addr: *const c_char);
/// Relay wake sink (event-driven receive): the Rust-owned relay socket has
/// inbound bytes — schedule an [`ov_session_pump_relay`](crate::ov_session_pump_relay)
/// on the shell's session thread. Fires on a Rust watcher thread (the one
/// exception to "sinks fire on the pump caller's thread"): it must only
/// dispatch, never call into the session synchronously or block. Wakes are
/// coalesced: after one wake the next fires only once a pump drained the
/// socket again.
pub type WakeSink = extern "C" fn(user: *mut c_void);

/// A registered C callback and the opaque `user` pointer handed back to it.
#[derive(Clone, Copy)]
pub(crate) struct Sink<F> {
    pub(crate) f: Option<F>,
    pub(crate) user: *mut c_void,
}

impl<F> Sink<F> {
    /// No callback registered.
    pub(crate) fn none() -> Self {
        Self {
            f: None,
            user: core::ptr::null_mut(),
        }
    }

    /// `f` with `user`, or [`Sink::none`] for a null callback.
    pub(crate) fn new(f: Option<F>, user: *mut c_void) -> Self {
        match f {
            Some(f) => Self { f: Some(f), user },
            None => Self::none(),
        }
    }
}

impl Sink<DatagramSink> {
    /// Hand one byte chunk to the host callback (a method, so closures
    /// capture the whole `Send` wrapper rather than its raw-pointer field).
    pub(crate) fn emit_bytes(&self, bytes: &[u8]) -> bool {
        if let Some(f) = self.f {
            f(self.user, bytes.as_ptr(), bytes.len());
        }
        true
    }
}

impl Sink<WakeSink> {
    /// Fire the wake callback (a method for the same capture reason).
    pub(crate) fn fire(&self) {
        if let Some(f) = self.f {
            f(self.user);
        }
    }
}

// SAFETY: the user pointer is opaque to Rust — only the host callback
// dereferences it, under the host's own threading contract — so moving the
// wrapper between threads is fine.
unsafe impl<F> Send for Sink<F> {}
