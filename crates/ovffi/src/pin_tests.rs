//! PIN (SPAKE2) handshakes through the C entry points: success, a wrong PIN
//! reported as `OV_ERR_AUTH_FAILED` on both sides (the client's REJECT must
//! reach the host), and the host's rate limit reported as
//! `OV_ERR_RATE_LIMITED` with a retry-after.
//!
//! Everything runs in ONE test: the host side uses the process-wide PIN
//! throttle, so the attempts must happen in a known order.

use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::{
    ov_session_begin_pin, ov_session_create, ov_session_free, ov_session_on_datagram,
    ov_session_pump, ov_session_retry_after_secs, ov_session_set_datagram_sink,
    ov_session_set_state_sink, OvSession, OV_ERR_AUTH_FAILED, OV_ERR_RATE_LIMITED,
};

/// One side of an in-process pair: its outbound datagrams and whether it
/// reached the established state.
#[derive(Default)]
struct Side {
    out: Mutex<Vec<Vec<u8>>>,
    established: AtomicBool,
}

extern "C" fn datagram_sink(user: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: `user` is the `Side` registered below, alive for the attempt.
    let side = unsafe { &*(user as *const Side) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    side.out.lock().unwrap().push(bytes);
}

extern "C" fn state_sink(user: *mut c_void, state: i32, _fp: *const u8, _len: usize) {
    // SAFETY: as above.
    let side = unsafe { &*(user as *const Side) };
    if state == 1 {
        side.established.store(true, Ordering::SeqCst);
    }
}

/// Move everything `from` sent into `to`'s inbound queue.
fn deliver(from: &Side, to: *mut OvSession) {
    for d in from.out.lock().unwrap().drain(..) {
        assert_eq!(
            unsafe { ov_session_on_datagram(to, d.as_ptr(), d.len()) },
            0
        );
    }
}

struct Outcome {
    client_rc: i32,
    server_rc: i32,
    client_ok: bool,
    server_ok: bool,
    retry_after: i64,
}

/// One device-ID / PIN handshake between a client that dials with
/// `client_pin` and a host expecting `host_pin`.
fn attempt(client_pin: &str, host_pin: &str) -> Outcome {
    let code = CString::new("000000").unwrap();
    let client_side = Box::new(Side::default());
    let server_side = Box::new(Side::default());
    let cu = &*client_side as *const Side as *mut c_void;
    let su = &*server_side as *const Side as *mut c_void;
    unsafe {
        let client = ov_session_create(0, code.as_ptr());
        let server = ov_session_create(1, code.as_ptr());
        assert!(!client.is_null() && !server.is_null());
        assert_eq!(ov_session_set_datagram_sink(client, cu, datagram_sink), 0);
        assert_eq!(ov_session_set_datagram_sink(server, su, datagram_sink), 0);
        assert_eq!(ov_session_set_state_sink(client, cu, state_sink), 0);
        assert_eq!(ov_session_set_state_sink(server, su, state_sink), 0);
        assert_eq!(
            ov_session_begin_pin(client, client_pin.as_ptr(), client_pin.len()),
            0
        );
        assert_eq!(
            ov_session_begin_pin(server, host_pin.as_ptr(), host_pin.len()),
            0
        );

        let (mut client_rc, mut server_rc) = (0, 0);
        for _ in 0..8 {
            let c = ov_session_pump(client);
            if c != 0 {
                client_rc = c;
            }
            deliver(&client_side, server);
            let s = ov_session_pump(server);
            if s != 0 {
                server_rc = s;
            }
            deliver(&server_side, client);
        }
        let outcome = Outcome {
            client_rc,
            server_rc,
            client_ok: client_side.established.load(Ordering::SeqCst),
            server_ok: server_side.established.load(Ordering::SeqCst),
            retry_after: ov_session_retry_after_secs(client),
        };
        ov_session_free(client);
        ov_session_free(server);
        outcome
    }
}

#[test]
fn pin_success_wrong_pin_and_rate_limit_through_the_ffi() {
    // 1. Right PIN: both sides establish (the host only after CONFIRM).
    let ok = attempt("hunter22", "hunter22");
    assert!(
        ok.client_ok && ok.server_ok,
        "right PIN must establish both sides"
    );
    assert_eq!((ok.client_rc, ok.server_rc), (0, 0));
    assert_eq!(ok.retry_after, -1);

    // 2. Wrong PIN: an auth failure on both sides. The host learns it from
    //    the client's REJECT, which the pump must flush before returning.
    let bad = attempt("wrong-pin", "hunter22");
    assert!(!bad.client_ok && !bad.server_ok);
    assert_eq!(bad.client_rc, OV_ERR_AUTH_FAILED, "client: wrong PIN");
    assert_eq!(bad.server_rc, OV_ERR_AUTH_FAILED, "host: client rejected");
    assert_eq!(bad.retry_after, -1);

    // 3. Keep guessing: the host's throttle locks and answers with
    //    REJECT(rate limited), which is not an auth failure.
    let mut limited = None;
    for _ in 0..30 {
        let o = attempt("wrong-pin", "hunter22");
        if o.client_rc == OV_ERR_RATE_LIMITED {
            limited = Some(o);
            break;
        }
        assert_eq!(o.client_rc, OV_ERR_AUTH_FAILED);
    }
    let limited = limited.expect("the host must rate-limit repeated wrong PINs");
    assert_eq!(limited.server_rc, OV_ERR_RATE_LIMITED);
    assert!(limited.retry_after > 0, "retry-after must be reported");
}
