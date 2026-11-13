//! Event-driven waits for the session loops (Phase 2): block until the
//! carrier's socket is readable, another thread signals (a captured frame
//! landed, a mic tick is ready), a window message arrives (viewer), or a
//! timeout — instead of polling on a fixed sleep.
//!
//! On Windows the socket is registered with `WSAEventSelect` (`FD_READ |
//! FD_WRITE | FD_CLOSE`) onto an auto-reset event, and the loop waits on it plus a
//! second "user" event with `WaitForMultipleObjects` (or
//! `MsgWaitForMultipleObjectsEx` when a window's messages must wake it too).
//! Winsock re-arms `FD_READ` on every `recv`, so draining the carrier until
//! `WouldBlock` (what every `poll_recv` does) is all the bookkeeping needed;
//! `FD_WRITE` fires when a send that hit a full buffer can make progress
//! again, which wakes a loop holding queued outbound bytes.
//! `WSAEventSelect` pins the socket in non-blocking mode, which is why only
//! carriers that never toggle the mode expose [`Carrier::raw_socket`].
//!
//! Like the rest of the Win32 code in this crate the FFI is declared by hand
//! (no `windows` crate). Elsewhere the socket is not watched: waits are
//! condvar-based on the user signal and simply time out, so callers pass a
//! short timeout when [`Wakeup::watches_socket`] is `false`.
//!
//! [`Carrier::raw_socket`]: ovtransport::Carrier::raw_socket

use std::sync::Arc;
use std::time::Duration;

use ovtransport::RawSock;

/// Why a wait returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woke {
    /// The socket has data, closed, or has send-buffer space again after a
    /// send was refused (`FD_WRITE`).
    Socket,
    /// A [`Waker`] fired.
    Notified,
    /// A window message is waiting ([`Wakeup::wait_or_message`] only).
    Message,
    /// The timeout elapsed.
    Timeout,
}

/// A wait point owned by one loop thread.
pub struct Wakeup {
    user: Arc<imp::Event>,
    sock: Option<imp::SocketEvent>,
}

/// A cheap, cloneable, thread-safe handle that wakes a [`Wakeup`].
#[derive(Clone)]
pub struct Waker(Arc<imp::Event>);

impl Waker {
    /// Wake the owning loop (idempotent until it wakes).
    pub fn wake(&self) {
        self.0.set();
    }
}

impl Wakeup {
    /// A wait point watching `sock` for readability (when given and the
    /// platform supports it).
    pub fn new(sock: Option<RawSock>) -> std::io::Result<Self> {
        Self::with_sockets(sock, &[])
    }

    /// A wait point watching `sock` plus `extra` sockets (a direct-path
    /// carrier's UDP sockets) — any of them readable wakes the wait.
    pub fn with_sockets(sock: Option<RawSock>, extra: &[RawSock]) -> std::io::Result<Self> {
        let user = Arc::new(imp::Event::new()?);
        let all: Vec<RawSock> = sock.into_iter().chain(extra.iter().copied()).collect();
        let sock = if all.is_empty() {
            None
        } else {
            imp::SocketEvent::register(&all)
        };
        Ok(Self { user, sock })
    }

    /// A handle other threads use to wake this loop.
    pub fn waker(&self) -> Waker {
        Waker(Arc::clone(&self.user))
    }

    /// Whether socket readability wakes [`wait`](Self::wait) (otherwise only
    /// wakers and the timeout do).
    pub fn watches_socket(&self) -> bool {
        self.sock.is_some()
    }

    /// Block until the socket is readable, a waker fires, or `timeout`.
    pub fn wait(&self, timeout: Duration) -> Woke {
        imp::wait(&self.user, self.sock.as_ref(), timeout, false)
    }

    /// Like [`wait`](Self::wait), but new window messages for this thread
    /// also wake it (Windows; elsewhere identical to `wait`).
    pub fn wait_or_message(&self, timeout: Duration) -> Woke {
        imp::wait(&self.user, self.sock.as_ref(), timeout, true)
    }
}

/// Wave 3: ask the OS for 1 ms timer resolution for this process (Windows:
/// `timeBeginPeriod(1)`; elsewhere a no-op). Waits are then precise enough
/// for the direct path's packet pacer instead of the default ~15.6 ms
/// granularity. Call once at startup; it stays in effect until exit.
pub fn high_res_timers() {
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(ms: u32) -> u32;
        }
        // SAFETY: plain Win32 call with a constant argument.
        unsafe {
            timeBeginPeriod(1);
        }
    }
}

/// Milliseconds for a Win32 wait, rounded up so a sub-millisecond timeout
/// still waits instead of spinning.
#[cfg_attr(not(windows), allow(dead_code))]
fn timeout_ms(timeout: Duration) -> u32 {
    let us = timeout.as_micros();
    us.div_ceil(1000).min(u128::from(u32::MAX - 1)) as u32
}

#[cfg(windows)]
mod imp {
    #![allow(non_snake_case)]

    use super::{timeout_ms, Woke};
    use std::time::Duration;

    type Handle = isize;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(
            attrs: *const core::ffi::c_void,
            manual: i32,
            initial: i32,
            name: *const u16,
        ) -> Handle;
        fn SetEvent(h: Handle) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn WaitForMultipleObjects(n: u32, handles: *const Handle, all: i32, ms: u32) -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn MsgWaitForMultipleObjectsEx(
            n: u32,
            handles: *const Handle,
            ms: u32,
            mask: u32,
            flags: u32,
        ) -> u32;
    }
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAEventSelect(s: usize, event: Handle, events: i32) -> i32;
    }

    const FD_READ: i32 = 0x01;
    const FD_WRITE: i32 = 0x02;
    const FD_CLOSE: i32 = 0x20;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x102;
    /// `QS_ALLINPUT`: any new message (input, paint, posted, timer, ...).
    const QS_ALLINPUT: u32 = 0x04FF;

    /// An owned auto-reset Win32 event.
    pub(super) struct Event(Handle);

    // SAFETY: an event handle is a kernel object reference; SetEvent / waits
    // on it are thread-safe by design.
    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}

    impl Event {
        pub(super) fn new() -> std::io::Result<Self> {
            // SAFETY: null attrs/name are valid; auto-reset, initially unset.
            let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            if h == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(h))
        }

        pub(super) fn set(&self) {
            // SAFETY: we own a valid event handle.
            unsafe { SetEvent(self.0) };
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: closing the handle we created, exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Sockets registered with `WSAEventSelect` onto one shared event.
    pub(super) struct SocketEvent {
        socks: Vec<usize>,
        event: Event,
    }

    impl SocketEvent {
        pub(super) fn register(socks: &[u64]) -> Option<Self> {
            let event = Event::new().ok()?;
            let mut ok = Vec::new();
            for &sock in socks {
                // SAFETY: `sock` is a live socket owned by the caller's
                // carrier; `event` outlives the registration (cancelled in Drop).
                let rc = unsafe {
                    WSAEventSelect(sock as usize, event.0, FD_READ | FD_WRITE | FD_CLOSE)
                };
                if rc == 0 {
                    ok.push(sock as usize);
                }
            }
            if ok.is_empty() {
                return None;
            }
            // Data may already be buffered (it arrived before registration
            // and no recv re-armed FD_READ since): start signalled so the
            // first wait returns at once and the caller drains it.
            event.set();
            Some(Self { socks: ok, event })
        }
    }

    impl Drop for SocketEvent {
        fn drop(&mut self) {
            // Cancel the registration (best effort: the socket may already
            // be closed, which is fine).
            // SAFETY: plain Winsock call; a stale socket just errors.
            for &s in &self.socks {
                unsafe { WSAEventSelect(s, 0, 0) };
            }
        }
    }

    pub(super) fn wait(
        user: &Event,
        sock: Option<&SocketEvent>,
        timeout: Duration,
        messages: bool,
    ) -> Woke {
        let mut handles = [user.0, 0];
        let mut n = 1u32;
        if let Some(s) = sock {
            handles[1] = s.event.0;
            n = 2;
        }
        let ms = timeout_ms(timeout);
        // SAFETY: `handles[..n]` are live event handles we own.
        let rc = unsafe {
            if messages {
                MsgWaitForMultipleObjectsEx(n, handles.as_ptr(), ms, QS_ALLINPUT, 0)
            } else {
                WaitForMultipleObjects(n, handles.as_ptr(), 0, ms)
            }
        };
        match rc {
            WAIT_TIMEOUT => Woke::Timeout,
            r if r == WAIT_OBJECT_0 => Woke::Notified,
            r if r == WAIT_OBJECT_0 + 1 && n == 2 => Woke::Socket,
            r if messages && r == WAIT_OBJECT_0 + n => Woke::Message,
            // WAIT_FAILED / abandoned: report a timeout; the caller re-polls.
            _ => Woke::Timeout,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Woke;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    /// A condvar-backed auto-reset signal.
    pub(super) struct Event {
        set: Mutex<bool>,
        cv: Condvar,
    }

    impl Event {
        pub(super) fn new() -> std::io::Result<Self> {
            Ok(Self {
                set: Mutex::new(false),
                cv: Condvar::new(),
            })
        }

        pub(super) fn set(&self) {
            if let Ok(mut s) = self.set.lock() {
                *s = true;
                self.cv.notify_all();
            }
        }
    }

    /// Socket readiness is not watched off-Windows.
    pub(super) struct SocketEvent;

    impl SocketEvent {
        pub(super) fn register(_socks: &[super::RawSock]) -> Option<Self> {
            None
        }
    }

    pub(super) fn wait(
        user: &Event,
        _sock: Option<&SocketEvent>,
        timeout: Duration,
        _messages: bool,
    ) -> Woke {
        let Ok(guard) = user.set.lock() else {
            return Woke::Timeout;
        };
        let Ok((mut guard, _)) = user.cv.wait_timeout_while(guard, timeout, |set| !*set) else {
            return Woke::Timeout;
        };
        if *guard {
            *guard = false;
            Woke::Notified
        } else {
            Woke::Timeout
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn waker_from_another_thread_ends_the_wait_early() {
        let w = Wakeup::new(None).unwrap();
        let k = w.waker();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            k.wake();
        });
        let t0 = Instant::now();
        assert_eq!(w.wait(Duration::from_secs(5)), Woke::Notified);
        assert!(t0.elapsed() < Duration::from_secs(2));
        t.join().unwrap();
        // Auto-reset: the next wait times out.
        assert_eq!(w.wait(Duration::from_millis(20)), Woke::Timeout);
    }

    #[test]
    fn timeout_rounds_up_to_whole_milliseconds() {
        assert_eq!(timeout_ms(Duration::ZERO), 0);
        assert_eq!(timeout_ms(Duration::from_micros(1)), 1);
        assert_eq!(timeout_ms(Duration::from_micros(2500)), 3);
    }

    #[cfg(windows)]
    #[test]
    fn socket_data_wakes_the_wait() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        use std::os::windows::io::AsRawSocket;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        b.set_nonblocking(true).unwrap();
        let w = Wakeup::new(Some(b.as_raw_socket())).unwrap();
        assert!(w.watches_socket());
        // The registration starts signalled (drain-first).
        assert_eq!(w.wait(Duration::from_millis(50)), Woke::Socket);
        assert_eq!(w.wait(Duration::from_millis(30)), Woke::Timeout);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            a.write_all(b"x").unwrap();
            a
        });
        let t0 = Instant::now();
        assert_eq!(w.wait(Duration::from_secs(5)), Woke::Socket);
        assert!(t0.elapsed() < Duration::from_secs(2));
        let _a = t.join().unwrap();
        drop(w);
        drop(b);
    }
}
