use super::*;

/// xorshift for deterministic loss/reorder.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

/// A lossy, reordering, delaying datagram path between two links.
struct Net {
    rng: Rng,
    loss: u64,
    reorder: u64,
    delay: Duration,
    /// (arrival, to_b, datagram)
    q: Vec<(Instant, bool, Vec<u8>)>,
    sent: u64,
    dropped: u64,
}

impl Net {
    fn push(&mut self, now: Instant, to_b: bool, d: &[u8]) {
        self.sent += 1;
        if self.rng.chance(self.loss) {
            self.dropped += 1;
            return;
        }
        let mut at = now + self.delay;
        if self.rng.chance(self.reorder) {
            at += Duration::from_millis(1 + self.rng.next() % 8);
        }
        self.q.push((at, to_b, d.to_vec()));
    }
    fn due(&mut self, now: Instant) -> Vec<(bool, Vec<u8>)> {
        self.q.sort_by_key(|e| e.0);
        let n = self.q.iter().take_while(|e| e.0 <= now).count();
        self.q.drain(..n).map(|(_, b, d)| (b, d)).collect()
    }
}

fn records(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut r = Rng(seed);
    (0..n)
        .map(|i| {
            let len = match i % 5 {
                0 => 9,
                1 => 4096 + 29,
                2 => 0,
                3 => (r.next() % 20_000) as usize,
                _ => 1200,
            };
            (0..len).map(|j| (i * 31 + j) as u8).collect()
        })
        .collect()
}

/// Push `recs` A -> B over `net` (both links framed from the start) and
/// return what B delivered plus A's stats.
fn transfer(recs: &[Vec<u8>], mut net: Net) -> (Vec<Vec<u8>>, LinkStats, Net) {
    let mut a = ReliableLink::new();
    let mut b = ReliableLink::new();
    a.enable();
    b.enable();
    let t0 = Instant::now();
    let mut now = t0;
    let mut got: Vec<Vec<u8>> = Vec::new();
    let mut next = 0;
    while got.len() < recs.len() {
        assert!(
            now - t0 < Duration::from_secs(120),
            "stalled at {}/{}",
            got.len(),
            recs.len()
        );
        // App: keep A's queue fed (a few records per tick).
        while next < recs.len() && a.pending_bytes() < 64 * 1024 {
            let mut o = |d: &[u8]| {
                net.push(now, true, d);
                Ok(())
            };
            a.send(&recs[next], now, &mut o).unwrap();
            next += 1;
        }
        for (to_b, d) in net.due(now) {
            let mut sent = Vec::new();
            let mut o = |p: &[u8]| {
                sent.push(p.to_vec());
                Ok(())
            };
            if to_b {
                b.on_datagram(&d, now, &mut |r| got.push(r.to_vec()), &mut o);
            } else {
                a.on_datagram(&d, now, &mut |_| panic!("B sends no records"), &mut o);
            }
            for p in sent {
                net.push(now, !to_b, &p);
            }
        }
        let mut sent_b = Vec::new();
        b.end_batch(now, &mut |r| got.push(r.to_vec()), &mut |p| {
            sent_b.push(p.to_vec());
            Ok(())
        });
        for p in sent_b {
            net.push(now, false, &p);
        }
        let mut sent_a = Vec::new();
        a.poll(now, &mut |_| {}, &mut |p| {
            sent_a.push(p.to_vec());
            Ok(())
        });
        for p in sent_a {
            net.push(now, true, &p);
        }
        now += Duration::from_micros(500);
    }
    (got, a.stats(), net)
}

fn net(loss: u64, reorder: u64, seed: u64) -> Net {
    Net {
        rng: Rng(seed),
        loss,
        reorder,
        delay: Duration::from_millis(5),
        q: Vec::new(),
        sent: 0,
        dropped: 0,
    }
}

#[test]
fn lossless_path_delivers_in_order_with_small_datagrams() {
    let recs = records(200, 7);
    let (got, st, n) = transfer(&recs, net(0, 0, 1));
    assert_eq!(got, recs);
    assert_eq!(st.retransmits, 0);
    assert!(n.sent > 0);
}

#[test]
fn recovers_from_loss_and_reordering_in_order() {
    for (loss, reorder, seed) in [(5, 0, 3), (10, 20, 11), (30, 30, 99)] {
        let recs = records(300, seed);
        let (got, st, n) = transfer(&recs, net(loss, reorder, seed));
        assert_eq!(got.len(), recs.len());
        assert!(
            got == recs,
            "loss {loss}% reorder {reorder}%: records corrupted or reordered"
        );
        assert!(st.retransmits > 0, "loss {loss}%: nothing retransmitted?");
        eprintln!(
            "loss {loss}% reorder {reorder}%: {} datagrams, {} dropped, {} retx ({} fast, {} RTO)",
            n.sent, n.dropped, st.retransmits, st.fast_retransmits, st.timeouts
        );
    }
}

#[test]
fn datagrams_never_exceed_the_mtu_budget() {
    let mut a = ReliableLink::new();
    a.enable();
    let mut max = 0;
    let now = Instant::now();
    a.send(&vec![1u8; 65_000], now, &mut |d| {
        max = max.max(d.len());
        Ok(())
    })
    .unwrap();
    assert!(max <= DATA_HEADER + MAX_PAYLOAD, "{max}");
    // 55 fragments, the window lets INIT_CWND go now, the bucket BURST.
    assert!(a.in_flight() as f64 <= INIT_CWND.min(BURST));
    assert!(a.pending_bytes() > 0, "the rest waits for acks");
}

#[test]
fn raw_mode_is_byte_identical_to_plain_udp_and_switches_on_framed_rx() {
    let now = Instant::now();
    let mut a = ReliableLink::new();
    let mut wire = Vec::new();
    a.send(b"hello", now, &mut |d| {
        wire.push(d.to_vec());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        wire,
        vec![b"hello".to_vec()],
        "raw mode sends the bare record"
    );
    // An old peer's raw datagram is delivered as-is.
    let mut got = Vec::new();
    a.on_datagram(
        b"\x01legacy",
        now,
        &mut |r| got.push(r.to_vec()),
        &mut |_| Ok(()),
    );
    assert_eq!(got, vec![b"\x01legacy".to_vec()]);
    assert!(!a.is_enabled());
    // A framed ACK from a new peer flips us to framed sending.
    let mut ack = MAGIC.to_vec();
    ack.push(T_ACK);
    ack.extend_from_slice(&[0; 12]);
    a.on_datagram(&ack, now, &mut |_| {}, &mut |_| Ok(()));
    assert!(a.is_enabled());
}

#[test]
fn framed_records_wait_for_raw_records_sent_before_the_switch() {
    let now = Instant::now();
    let mut a = ReliableLink::new();
    let mut wire = Vec::new();
    let mut o = |d: &[u8]| {
        wire.push(d.to_vec());
        Ok(())
    };
    a.send(b"r0", now, &mut o).unwrap();
    a.send(b"r1", now, &mut o).unwrap();
    a.enable();
    a.send(b"f2", now, &mut o).unwrap();
    // B receives the framed one first, then the raw ones (reordered).
    let mut b = ReliableLink::new();
    let mut got = Vec::new();
    b.on_datagram(
        &wire[2],
        now,
        &mut |r| got.push(r.to_vec()),
        &mut |_| Ok(()),
    );
    b.end_batch(now, &mut |r| got.push(r.to_vec()), &mut |_| Ok(()));
    assert!(got.is_empty(), "held until both raw records arrived");
    b.on_datagram(
        &wire[0],
        now,
        &mut |r| got.push(r.to_vec()),
        &mut |_| Ok(()),
    );
    b.on_datagram(
        &wire[1],
        now,
        &mut |r| got.push(r.to_vec()),
        &mut |_| Ok(()),
    );
    b.end_batch(now, &mut |r| got.push(r.to_vec()), &mut |_| Ok(()));
    assert_eq!(got, vec![b"r0".to_vec(), b"r1".to_vec(), b"f2".to_vec()]);
}

#[test]
fn would_block_keeps_packets_queued() {
    let now = Instant::now();
    let mut a = ReliableLink::new();
    a.enable();
    a.send(&[7u8; 3000], now, &mut |_| {
        Err(io::ErrorKind::WouldBlock.into())
    })
    .unwrap();
    assert_eq!(a.in_flight(), 0);
    assert_eq!(a.pending_bytes(), 3000);
    let mut n = 0;
    a.poll(now, &mut |_| {}, &mut |_| {
        n += 1;
        Ok(())
    });
    assert_eq!(n, 3);
    assert_eq!(a.pending_bytes(), 0);
}
