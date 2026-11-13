use super::*;
use std::net::TcpListener;
use std::thread;

/// A minimal fake relay: accepts one CONNECT, replies `O`, then echoes
/// frames back (so PING->PONG and DATA->DATA round trips work). Answers
/// `XT` if `reject` is set.
fn fake_relay(reject: bool) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut connect = [0u8; 6];
        stream.read_exact(&mut connect).unwrap();
        if reject {
            let _ = stream.write_all(b"XT");
            let _ = stream.write_all(&connect[1..5]);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        stream.write_all(b"O").unwrap();
        // 'A' line: sid + relay-observed peer address (fake 0.0.0.0:0).
        stream.write_all(b"A").unwrap();
        stream.write_all(&connect[1..5]).unwrap();
        stream.write_all(&[0u8; 6]).unwrap();
        // Echo frames until EOF.
        let mut rx = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => rx.extend_from_slice(&buf[..n]),
            }
            // Echo complete frames back.
            while rx.len() >= 3 {
                let len = u16::from_be_bytes([rx[1], rx[2]]) as usize;
                if rx.len() < 3 + len {
                    break;
                }
                let frame: Vec<u8> = rx.drain(..3 + len).collect();
                let _ = stream.write_all(&frame);
            }
        }
    });
    (addr, handle)
}

#[test]
fn connect_and_echo_ping_pong() {
    let (addr, server) = fake_relay(false);
    let mut c = RelayTunnelCarrier::connect(addr, 7, TunnelRole::Initiator).unwrap();
    assert_eq!(c.session_id(), 7);

    // PING round trip: the fake relay echoes it back, but the echo is a
    // PING — the carrier auto-answers PONG and the relay echoes that
    // back as a PONG, updating last_pong. The pump loop also consumes
    // the relay's `A` line, so is_attached is asserted afterwards.
    c.ping(0xDEAD_BEEF).unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    while (c.last_pong().is_none() || !c.is_attached()) && Instant::now() < deadline {
        let _ = c.poll_recv(&mut |_| {});
        thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(c.is_attached(), "relay never confirmed attach");
    assert!(c.last_pong().is_some(), "no PONG came back");
    // Close before joining: the relay thread blocks on read until EOF.
    c.close().unwrap();
    drop(c);
    server.join().unwrap();
}

#[test]
fn data_roundtrip_through_fake_relay() {
    let (addr, server) = fake_relay(false);
    let mut c = RelayTunnelCarrier::connect(addr, 9, TunnelRole::Responder).unwrap();
    c.send(b"payload").unwrap();
    let mut got = Vec::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    while got.is_empty() && Instant::now() < deadline {
        c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        if got.is_empty() {
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    assert_eq!(got, vec![&b"payload"[..]]);
    assert_eq!(c.stats().sent_datagrams, 1);
    assert_eq!(c.stats().recv_datagrams, 1);
    c.close().unwrap();
    drop(c);
    server.join().unwrap();
}

/// A fake relay that accepts (`O`), then after `delay` either attaches
/// (`A` + one DATA frame right behind it) or times the park out (`XT`).
fn delayed_relay(delay: std::time::Duration, attach: bool) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut connect = [0u8; 6];
        stream.read_exact(&mut connect).unwrap();
        stream.write_all(b"O").unwrap();
        thread::sleep(delay);
        if attach {
            let mut out = vec![b'A'];
            out.extend_from_slice(&connect[1..5]);
            out.extend_from_slice(&[0u8; 6]);
            out.extend_from_slice(&tunnel::encode_frame(tunnel::DATA, b"first").unwrap());
            stream.write_all(&out).unwrap();
            // Hold the stream open until the client hangs up.
            let mut sink = [0u8; 64];
            while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
        } else {
            let _ = stream.write_all(b"XT");
            let _ = stream.write_all(&connect[1..5]);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    (addr, handle)
}

#[test]
fn wait_attached_waits_past_a_short_handshake_window() {
    // The peer attaches 600 ms after we park — longer than a 2 s
    // handshake pump would tolerate only by luck; wait_attached holds on.
    let (addr, server) = delayed_relay(std::time::Duration::from_millis(600), true);
    let mut c = RelayTunnelCarrier::connect(addr, 0x0A0B, TunnelRole::Initiator).unwrap();
    assert!(!c.is_attached());
    let t0 = Instant::now();
    assert!(c.wait_attached(std::time::Duration::from_secs(10)).unwrap());
    assert!(c.is_attached());
    assert!(t0.elapsed() >= std::time::Duration::from_millis(500));
    // The DATA frame that rode in behind the `A` line is not lost.
    let mut got = Vec::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    while got.is_empty() && Instant::now() < deadline {
        c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
    }
    assert_eq!(got, vec![b"first".to_vec()]);
    c.close().unwrap();
    drop(c);
    server.join().unwrap();
}

#[test]
fn wait_attached_reports_park_timeout_as_closed() {
    let (addr, server) = delayed_relay(std::time::Duration::from_millis(100), false);
    let mut c = RelayTunnelCarrier::connect(addr, 3, TunnelRole::Responder).unwrap();
    let err = c
        .wait_attached(std::time::Duration::from_secs(10))
        .unwrap_err();
    assert!(matches!(err, TransportError::Closed));
    server.join().unwrap();
}

#[test]
fn wait_attached_times_out_without_a_verdict() {
    let (addr, server) = delayed_relay(std::time::Duration::from_millis(800), true);
    let mut c = RelayTunnelCarrier::connect(addr, 4, TunnelRole::Initiator).unwrap();
    assert!(!c
        .wait_attached(std::time::Duration::from_millis(100))
        .unwrap());
    assert!(c.wait_attached(std::time::Duration::from_secs(10)).unwrap());
    c.close().unwrap();
    drop(c);
    server.join().unwrap();
}

#[test]
fn full_send_buffer_completes_without_blocking_mode() {
    // A reader that drains slowly: the writer hits WouldBlock and must
    // finish via the retry path (the socket stays non-blocking).
    let (addr, server) = fake_relay(false);
    let mut c = RelayTunnelCarrier::connect(addr, 8, TunnelRole::Initiator).unwrap();
    let big = vec![0x5Au8; 60 * 1024];
    for _ in 0..64 {
        c.send(&big).unwrap();
        // Drain echoes so the fake relay's writes never back up.
        let _ = c.poll_recv(&mut |_| {});
    }
    assert_eq!(c.stats().sent_datagrams, 64);
    c.close().unwrap();
    drop(c);
    server.join().unwrap();
}

/// A full send buffer queues instead of blocking: sends return at once
/// (backlog visible via `pending_send`), polls drain it, and the peer
/// gets every byte in order.
#[test]
fn full_send_buffer_queues_and_drains_in_order() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let reader = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut connect = [0u8; 6];
        stream.read_exact(&mut connect).unwrap();
        let mut out = vec![b'O', b'A'];
        out.extend_from_slice(&connect[1..5]);
        out.extend_from_slice(&[0u8; 6]);
        stream.write_all(&out).unwrap();
        // Not reading yet: the client's send buffer fills up.
        thread::sleep(std::time::Duration::from_millis(400));
        let mut all = Vec::new();
        let _ = stream.read_to_end(&mut all);
        all
    });
    let mut c = RelayTunnelCarrier::connect(addr, 6, TunnelRole::Initiator).unwrap();
    let chunk = 16 * 1024;
    let mut n = 0usize;
    let mut slowest = std::time::Duration::ZERO;
    // Keep sending into the stalled peer until the kernel buffers are
    // full and the carrier starts queueing (well before its cap).
    while c.pending_send() == 0 && n < 4096 {
        let t = Instant::now();
        c.send(&vec![(n % 251) as u8; chunk]).unwrap();
        slowest = slowest.max(t.elapsed());
        n += 1;
    }
    assert!(
        c.pending_send() > 0,
        "a stalled peer must leave bytes queued"
    );
    assert!(c.pending_send() <= TX_SOFT_CAP);
    // No send waited for the stalled reader (it sleeps 400 ms).
    assert!(
        slowest < std::time::Duration::from_millis(200),
        "{slowest:?}"
    );
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while c.pending_send() > 0 && Instant::now() < deadline {
        let _ = c.poll_recv(&mut |_| {});
        thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(c.pending_send(), 0);
    c.close().unwrap();
    drop(c);
    let all = reader.join().unwrap();
    let mut fr = FrameReader::default();
    fr.feed(&all);
    let mut got = Vec::new();
    while let Ok(Some((kind, payload))) = fr.next_ref() {
        if kind == tunnel::DATA {
            got.push(payload.to_vec());
        }
    }
    assert_eq!(got.len(), n);
    for (i, p) in got.iter().enumerate() {
        assert_eq!(
            p,
            &vec![(i % 251) as u8; chunk],
            "datagram {i} intact and in order"
        );
    }
}

/// The read waker fires once per drain: data wakes it, no further wake
/// comes until the owner polled, and dropping it stops the thread.
#[test]
fn read_waker_fires_on_data_and_coalesces_until_polled() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let (addr, server) = fake_relay(false);
    let mut c = RelayTunnelCarrier::connect(addr, 11, TunnelRole::Initiator).unwrap();
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = Arc::clone(&wakes);
    assert!(c
        .set_read_waker(Some(Box::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        })))
        .unwrap());
    let wait_for = |n: usize| {
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while wakes.load(Ordering::SeqCst) < n && Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(1));
        }
        wakes.load(Ordering::SeqCst)
    };
    // The `A` line may already be buffered: drain whatever woke us.
    let _ = c.poll_recv(&mut |_| {});
    let base = wakes.load(Ordering::SeqCst);
    // Echoed DATA arrives: one wake.
    c.send(b"x").unwrap();
    assert!(wait_for(base + 1) > base, "data must wake the owner");
    // Not polled yet: more data does not wake again.
    c.send(b"y").unwrap();
    thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        wakes.load(Ordering::SeqCst),
        base + 1,
        "wakes coalesce until polled"
    );
    let mut got = Vec::new();
    c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
    // Polled: the next arrival wakes again.
    c.send(b"z").unwrap();
    assert!(wait_for(base + 2) >= base + 2);
    c.close().unwrap();
    assert!(!c.has_read_waker());
    drop(c);
    server.join().unwrap();
}

#[test]
fn relay_rejection_surfaces_closed() {
    let (addr, server) = fake_relay(true);
    let err = RelayTunnelCarrier::connect(addr, 5, TunnelRole::Initiator).unwrap_err();
    assert!(matches!(err, TransportError::Closed));
    server.join().unwrap();
}
