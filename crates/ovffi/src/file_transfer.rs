//! Phone-to-peer file transfer (`ov_file_send`). Receiving goes through the
//! screen assembler's file sink.

use std::ffi::c_char;

use crate::cabi::{bytes, cstr};
use crate::error::{err_from_transport, OV_ERR_PANIC};
use crate::pump::send_record_bytes;
use crate::session::{lock, OvSession};

/// Send a file to the peer: begin + `FILE_XFER_DATA` chunks (16 KiB
/// default) + `FILE_XFER_COMPLETE`, all on channel 0 — the same file-id-keyed
/// path the desktop shells use, driven by `ovapp::file_transfer::FileTransfer::send`
/// (chunking/assembly logic shared, not duplicated). Safe to call from any
/// thread (the session mutex serializes it against the pump). Returns 0,
/// -1 (bad args), -2 (not established / no transport) or -4 (transport send
/// failure).
///
/// # Safety
/// `s` must be null or a live handle from
/// [`ov_session_create`](crate::ov_session_create); `name` null or a valid
/// NUL-terminated C string; `data` readable for `data_len` bytes (or null
/// with `data_len == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_file_send(
    s: *mut OvSession,
    name: *const c_char,
    data: *const u8,
    data_len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session, `name` null or a
        // NUL-terminated string, and `data` readable for `data_len` bytes,
        // all for the whole call (caller contract).
        let args = unsafe { (s.as_ref(), cstr(name), bytes(data, data_len)) };
        let (Some(s), Some(name), Some(bytes)) = args else {
            return -1;
        };
        lock(s)
            .and_then(|mut g| {
                // Temporarily take the session out so the send closure can touch
                // both the encrypt path and the transport (disjoint borrows of
                // `g` are not expressible inside one closure).
                let Some(mut session) = g.session.take() else {
                    return Err(-2);
                };
                let mut xfer = ovapp::file_transfer::FileTransfer::new();
                let mut send_err: Option<i32> = None;
                let res = xfer.send(name, bytes, 0, |cmd| {
                    if send_err.is_some() {
                        return Ok(());
                    }
                    // One command = one AEAD record on channel 0, sent through
                    // the transport (same path as `ov_session_send_record`).
                    let record = session.encrypt_record(0, &ovapp::terminal::encode(cmd));
                    let sent = send_record_bytes(&mut g, &record);
                    match sent {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            send_err = Some(err_from_transport(&e));
                            Err("transport".to_owned())
                        }
                    }
                });
                g.session = Some(session);
                match (res, send_err) {
                    (Err(_), Some(code)) => Err(code),
                    // Rejected by the chunker (e.g. an unusable name).
                    (Err(_), None) => Err(-1),
                    (Ok(()), _) => Ok(0),
                }
            })
            .unwrap_or(-2)
    })
}
