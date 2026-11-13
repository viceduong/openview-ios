//! The optional readability watcher thread behind
//! [`RelayTunnelCarrier::set_read_waker`](super::RelayTunnelCarrier::set_read_waker).

use std::io;
use std::net::TcpStream;

/// Wake callback for
/// [`RelayTunnelCarrier::set_read_waker`](super::RelayTunnelCarrier::set_read_waker). Runs on the
/// watcher thread: it must only *schedule* a pump (e.g. dispatch onto the
/// shell's session queue) — never call back into the carrier or its owner
/// synchronously, and never block.
pub type ReadWake = Box<dyn FnMut() + Send>;

/// Watcher state shared with the thread: `armed` = the owner polled since
/// the last wake (so a still-readable socket means *new* bytes, not the
/// ones the owner has not drained yet); `stop` = the carrier is gone.
#[derive(Default)]
struct WatchState {
    armed: bool,
    stop: bool,
}

struct WatchShared {
    state: std::sync::Mutex<WatchState>,
    cv: std::sync::Condvar,
}

/// Handle to the readability watcher thread. Dropping it stops the thread:
/// once `drop` returns the wake callback is never called again (the
/// callback runs under the same lock `drop` takes).
pub(super) struct ReadWatcher {
    shared: std::sync::Arc<WatchShared>,
}

impl std::fmt::Debug for ReadWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadWatcher")
    }
}

/// How long one readiness wait blocks before the watcher re-checks `stop`.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(200);

impl ReadWatcher {
    pub(super) fn spawn(stream: TcpStream, mut wake: ReadWake) -> io::Result<Self> {
        let shared = std::sync::Arc::new(WatchShared {
            // Armed from the start: bytes already queued wake at once.
            state: std::sync::Mutex::new(WatchState {
                armed: true,
                stop: false,
            }),
            cv: std::sync::Condvar::new(),
        });
        let t = std::sync::Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ov-relay-watch".into())
            .spawn(move || {
                // `stream` (a clone of the carrier's socket) keeps the
                // descriptor valid for as long as this thread polls it.
                let sock = raw_of(&stream);
                loop {
                    // 1. Wait until the owner drained since the last wake.
                    {
                        let Ok(mut st) = t.state.lock() else { return };
                        while !st.armed && !st.stop {
                            st = match t.cv.wait(st) {
                                Ok(g) => g,
                                Err(_) => return,
                            };
                        }
                        if st.stop {
                            return;
                        }
                    }
                    // 2. Block until the socket is readable (data, EOF or
                    // error all count: the owner's next poll sorts it out).
                    loop {
                        match wait_readable(sock, WATCH_POLL) {
                            Ok(true) => break,
                            Ok(false) => {}
                            // EINTR and friends: retry after a breather.
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                        }
                        if t.state.lock().map_or(true, |s| s.stop) {
                            return;
                        }
                    }
                    // 3. Wake the owner (under the lock: never after drop).
                    let Ok(mut st) = t.state.lock() else { return };
                    if st.stop {
                        return;
                    }
                    st.armed = false;
                    wake();
                }
            })?;
        Ok(Self { shared })
    }

    /// The owner polled the socket: the next readability is news.
    pub(super) fn rearm(&self) {
        if let Ok(mut st) = self.shared.state.lock() {
            if !st.armed {
                st.armed = true;
                self.shared.cv.notify_all();
            }
        }
    }
}

impl Drop for ReadWatcher {
    fn drop(&mut self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.stop = true;
        }
        self.shared.cv.notify_all();
    }
}

#[cfg(unix)]
pub(super) type WatchSock = std::os::fd::RawFd;
#[cfg(windows)]
pub(super) type WatchSock = usize;

#[cfg(unix)]
pub(super) fn raw_of(s: &TcpStream) -> WatchSock {
    std::os::fd::AsRawFd::as_raw_fd(s)
}
#[cfg(windows)]
pub(super) fn raw_of(s: &TcpStream) -> WatchSock {
    std::os::windows::io::AsRawSocket::as_raw_socket(s) as usize
}

/// Block up to `timeout` until `sock` is readable (or hung up / errored).
/// `Ok(true)` = readable, `Ok(false)` = timed out.
fn wait_readable(sock: WatchSock, timeout: std::time::Duration) -> io::Result<bool> {
    Ok(wait_readable_any(&[sock], timeout)? != 0)
}

/// Block up to `timeout` until any of `socks` (at most 32) is readable (or
/// hung up / errored). Returns a bitmask of the ready sockets (bit `i` =
/// `socks[i]`); 0 = timed out. Declared by hand like the rest of this
/// workspace's OS calls (no extra crates; the iOS staticlib stays pure
/// Rust).
#[cfg(unix)]
pub(super) fn wait_readable_any(
    socks: &[WatchSock],
    timeout: std::time::Duration,
) -> io::Result<u32> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    type NFds = std::ffi::c_ulong;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type NFds = std::ffi::c_uint;
    extern "C" {
        fn poll(fds: *mut PollFd, nfds: NFds, timeout: i32) -> i32;
    }
    const POLLIN: i16 = 0x0001;
    assert!(socks.len() <= 32, "at most 32 sockets");
    let mut fds = [PollFd {
        fd: -1,
        events: 0,
        revents: 0,
    }; 32];
    for (p, &fd) in fds.iter_mut().zip(socks) {
        *p = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
    }
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fds` is a properly initialised array of `#[repr(C)]` pollfd
    // on this stack frame and `nfds` (= socks.len() <= 32) is within it;
    // `poll` only writes `revents` and keeps no pointer after returning.
    // The descriptors stay open for the call: the callers own the streams.
    let rc = unsafe { poll(fds.as_mut_ptr(), socks.len() as NFds, ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready_mask(fds[..socks.len()].iter().map(|p| p.revents)))
}

#[cfg(windows)]
pub(super) fn wait_readable_any(
    socks: &[WatchSock],
    timeout: std::time::Duration,
) -> io::Result<u32> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct WsaPollFd {
        fd: usize,
        events: i16,
        revents: i16,
    }
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
    }
    const POLLRDNORM: i16 = 0x0100;
    assert!(socks.len() <= 32, "at most 32 sockets");
    let mut fds = [WsaPollFd {
        fd: usize::MAX,
        events: 0,
        revents: 0,
    }; 32];
    for (p, &fd) in fds.iter_mut().zip(socks) {
        *p = WsaPollFd {
            fd,
            events: POLLRDNORM,
            revents: 0,
        };
    }
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fds` is a properly initialised array of `#[repr(C)]`
    // WSAPOLLFD on this stack frame and `nfds` (= socks.len() <= 32) is
    // within it; `WSAPoll` only writes `revents` and keeps no pointer after
    // returning. The sockets stay open for the call: the callers own the
    // streams.
    let rc = unsafe { WSAPoll(fds.as_mut_ptr(), socks.len() as u32, ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready_mask(fds[..socks.len()].iter().map(|p| p.revents)))
}

#[cfg(any(unix, windows))]
fn ready_mask(revents: impl Iterator<Item = i16>) -> u32 {
    revents
        .enumerate()
        .filter(|(_, r)| *r != 0)
        .fold(0, |m, (i, _)| m | (1 << i))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn wait_readable_any(
    socks: &[WatchSock],
    timeout: std::time::Duration,
) -> io::Result<u32> {
    std::thread::sleep(timeout);
    Ok((1u32 << socks.len()) - 1)
}

#[cfg(not(any(unix, windows)))]
pub(super) type WatchSock = ();
#[cfg(not(any(unix, windows)))]
pub(super) fn raw_of(_s: &TcpStream) -> WatchSock {}
