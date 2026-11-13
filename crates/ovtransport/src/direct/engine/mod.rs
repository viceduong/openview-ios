//! The sans-IO direct-path engine: path probing, promotion / demotion and
//! the cross-path record ordering (see the [`crate::direct`] module docs).

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use super::link::{apply_rate, new_link, rudp_out, Link, LinkCc};
use super::media_state::MediaState;
use super::wire::{marker, open_probe, probe};
use super::*;
use crate::cc::Lane;
use crate::media;
use crate::rudp;

mod media_channel;

struct ProbeRound {
    paths: Vec<u32>,
    nonce: [u8; 16],
    started: Instant,
    last_sent: Option<Instant>,
}

/// The sans-IO direct-path engine (see the module docs).
pub struct DirectEngine {
    is_server: bool,
    key: [u8; 32],
    armed: bool,
    epoch: Instant,
    // ---- sender ----
    next_idx: u64,
    pub(super) on_direct: bool,
    replay: VecDeque<(u64, Vec<u8>)>,
    replay_bytes: usize,
    last_primary_tx: Instant,
    // ---- receiver ----
    synced: bool,
    delivered: u64,
    relay_next: u64,
    pending: BTreeMap<u64, Vec<u8>>,
    // ---- path ----
    pub(super) link: Option<Link>,
    probe: Option<ProbeRound>,
    candidates: Vec<u32>,
    reprobe_at: Option<Instant>,
    reprobe_delay: Duration,
    commit_nonce: [u8; 16],
    stats: DirectStats,
    pub(super) events: Vec<String>,
    /// The media channel (both peers advertised `CAP_MEDIA`).
    media: Option<MediaState>,
    /// The relay (primary) is gone: see [`DEAD_AFTER_RELAYLESS`].
    primary_lost: bool,
    /// When the current link last went active (for path-event logs).
    direct_since: Option<Instant>,
}

impl DirectEngine {
    /// A disarmed engine (pass-through until [`arm`](Self::arm)).
    pub fn new(is_server: bool, key: [u8; 32]) -> Self {
        let now = Instant::now();
        Self {
            is_server,
            key,
            armed: false,
            epoch: now,
            next_idx: 0,
            on_direct: false,
            replay: VecDeque::new(),
            replay_bytes: 0,
            last_primary_tx: now,
            synced: false,
            delivered: 0,
            relay_next: 0,
            pending: BTreeMap::new(),
            link: None,
            probe: None,
            candidates: Vec::new(),
            reprobe_at: None,
            reprobe_delay: REPROBE_AFTER,
            commit_nonce: rand16(),
            stats: DirectStats::default(),
            events: Vec::new(),
            media: None,
            primary_lost: false,
            direct_since: None,
        }
    }

    pub(super) fn make_link(&self, path: u32, now: Instant, confirmed: bool) -> Link {
        let mut l = new_link(path, now, confirmed);
        if self.media.is_some() {
            l.cc = Some(LinkCc::new(self.epoch));
            apply_rate(&mut l);
        }
        l
    }

    /// The peer advertised `CAP_DIRECT`: probes and markers may flow.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    /// Whether [`arm`](Self::arm) was called.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Whether this end is the host (session server).
    pub fn is_server(&self) -> bool {
        self.is_server
    }

    /// Where sending currently goes.
    pub fn state(&self) -> DirectState {
        if self.on_direct {
            DirectState::Direct
        } else if self.probe.is_some() {
            DirectState::Probing
        } else {
            DirectState::Relay
        }
    }

    /// The direct path in use (sending), if any.
    pub fn active_path(&self) -> Option<u32> {
        self.link
            .as_ref()
            .filter(|_| self.on_direct)
            .map(|l| l.path)
    }

    /// Counters.
    pub fn stats(&self) -> DirectStats {
        self.stats
    }

    /// Smoothed RTT of the direct link, once measured.
    pub fn link_rtt(&self) -> Option<Duration> {
        self.link.as_ref().and_then(|l| l.rudp.srtt())
    }

    /// Human-readable path events since the last call (promotions,
    /// demotions, probe results) for the shells' logs.
    pub fn take_events(&mut self) -> Vec<String> {
        std::mem::take(&mut self.events)
    }

    /// Outbound bytes queued on the direct link.
    pub fn pending_bytes(&self) -> usize {
        self.link.as_ref().map_or(0, |l| {
            l.rudp.pending_bytes() + l.cc.as_ref().map_or(0, |c| c.pacer.queued_bytes())
        })
    }

    fn now_us(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_micros() as u64
    }

    /// Viewer: probe these candidate paths (replaces earlier candidates; a
    /// round starts now).
    pub fn start_probe(&mut self, paths: Vec<u32>, now: Instant) {
        if self.is_server || !self.armed || paths.is_empty() {
            return;
        }
        self.candidates = paths;
        self.begin_round(now);
    }

    fn begin_round(&mut self, now: Instant) {
        if self.candidates.is_empty() {
            return;
        }
        self.stats.probe_rounds += 1;
        self.reprobe_at = None;
        self.probe = Some(ProbeRound {
            paths: self.candidates.clone(),
            nonce: rand16(),
            started: now,
            last_sent: None,
        });
    }

    // ---------------- sending ----------------

    /// Send one record on the current path.
    pub fn send(
        &mut self,
        record: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
    ) -> Result<(), TransportError> {
        let idx = self.next_idx;
        self.next_idx += 1;
        if self.on_direct {
            if let Some(link) = self.link.as_mut() {
                let mut framed = Vec::with_capacity(9 + record.len());
                framed.push(K_DATA);
                framed.extend_from_slice(&idx.to_be_bytes());
                framed.extend_from_slice(record);
                let path = link.path;
                // A socket error is a loss: rudp's RTO retransmits, and a
                // dead path is demoted by the liveness check.
                let Link { rudp, cc, .. } = &mut *link;
                let _ = rudp.send(&framed, now, &mut rudp_out(cc, io, path, now));
                link.last_tx = now;
                self.stats.records_direct_tx += 1;
                self.replay_bytes += record.len();
                self.replay.push_back((idx, record.to_vec()));
                while self.replay_bytes > REPLAY_LIMIT {
                    match self.replay.pop_front() {
                        Some((_, r)) => self.replay_bytes -= r.len(),
                        None => break,
                    }
                }
                self.pace(now, io);
                return Ok(());
            }
        }
        self.last_primary_tx = now;
        io.primary_send(record)
    }

    pub(super) fn promote(&mut self, now: Instant, io: &mut dyn DirectIo) {
        if self.on_direct || self.link.is_none() {
            return;
        }
        // Everything below next_idx went out on the relay already.
        if io.primary_send(&marker(M_SYNC_UP, self.next_idx)).is_err() {
            return;
        }
        self.last_primary_tx = now;
        self.on_direct = true;
        self.direct_since = Some(now);
        self.stats.promotions += 1;
        self.reprobe_delay = REPROBE_AFTER;
        let path = self.link.as_ref().map_or(0, |l| l.path);
        self.events.push(format!(
            "direct path#{path} active (relay kept as fallback)"
        ));
    }

    /// Drop the direct link and move sending back to the relay, replaying
    /// what the peer has not acknowledged.
    pub(super) fn demote(&mut self, now: Instant, io: &mut dyn DirectIo, why: &str) {
        let had_link = self.link.take().map(|l| l.path);
        if self.on_direct {
            self.on_direct = false;
            self.stats.demotions += 1;
            let from = self.replay.front().map_or(self.next_idx, |(i, _)| *i);
            let _ = io.primary_send(&marker(M_SYNC_DOWN, from));
            for (_, r) in std::mem::take(&mut self.replay) {
                self.stats.records_replayed += 1;
                let _ = io.primary_send(&r);
            }
            self.replay_bytes = 0;
            self.last_primary_tx = now;
        }
        if let Some(p) = had_link {
            self.events
                .push(format!("direct path#{p} dropped ({why}); back on relay"));
        }
        if !self.is_server && !self.candidates.is_empty() {
            self.reprobe_at = Some(now + self.reprobe_delay);
            self.reprobe_delay = (self.reprobe_delay * 2).min(REPROBE_MAX);
        }
    }

    // ---------------- receiving ----------------

    fn deliver_indexed(&mut self, idx: u64, rec: Vec<u8>, deliver: &mut dyn FnMut(&[u8])) {
        if !self.synced {
            if self.pending.len() < PENDING_LIMIT {
                self.pending.insert(idx, rec);
            }
            return;
        }
        if idx < self.delivered {
            self.stats.duplicates_dropped += 1;
            return;
        }
        if idx > self.delivered {
            if self.pending.len() < PENDING_LIMIT {
                self.pending.insert(idx, rec);
            }
            return;
        }
        deliver(&rec);
        self.delivered += 1;
        self.flush_pending(deliver);
    }

    fn flush_pending(&mut self, deliver: &mut dyn FnMut(&[u8])) {
        // Drop stale entries, then release the contiguous run.
        while let Some((&i, _)) = self.pending.first_key_value() {
            if i < self.delivered {
                self.pending.pop_first();
                self.stats.duplicates_dropped += 1;
            } else {
                break;
            }
        }
        while let Some(r) = self.pending.remove(&self.delivered) {
            deliver(&r);
            self.delivered += 1;
        }
    }

    /// One datagram from the primary (relay) path: a marker is consumed,
    /// a record is delivered in order.
    pub fn on_primary(
        &mut self,
        d: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        if is_marker(d) {
            let kind = d[12];
            let n = u64::from_be_bytes(d[13..21].try_into().unwrap_or([0; 8]));
            match kind {
                M_SYNC_UP | M_SYNC_DOWN => {
                    if !self.synced {
                        self.synced = true;
                        self.delivered = n;
                    }
                    self.relay_next = n;
                    self.flush_pending(deliver);
                    // The peer gave up on the direct path: follow it (our
                    // own sending there is likely failing too).
                    if kind == M_SYNC_DOWN && self.link.is_some() {
                        self.demote(now, io, "peer fell back to relay");
                    }
                }
                _ => {} // NOP keepalive (or a future kind)
            }
            return;
        }
        if self.synced {
            let idx = self.relay_next;
            self.relay_next += 1;
            self.deliver_indexed(idx, d.to_vec(), deliver);
        } else {
            deliver(d);
        }
    }

    /// One datagram from direct path `path` (probe or rudp packet).
    pub fn on_direct(
        &mut self,
        path: u32,
        d: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        if !self.armed {
            return;
        }
        if is_probe(d) {
            self.on_probe(path, d, now, io);
            return;
        }
        if d.first() == Some(&media::V2_MAGIC) {
            self.on_v2(path, d, now, io, deliver);
            return;
        }
        if !rudp::is_framed(d) {
            return;
        }
        let Some(link) = self.link.as_mut().filter(|l| l.path == path) else {
            return; // not our path (stale / unrelated)
        };
        link.last_rx = now;
        link.confirmed = true;
        let mut recs: Vec<Vec<u8>> = Vec::new();
        let Link { rudp, cc, .. } = link;
        rudp.on_datagram(
            d,
            now,
            &mut |r| recs.push(r.to_vec()),
            &mut rudp_out(cc, io, path, now),
        );
        self.on_link_records(recs, now, deliver);
    }

    fn on_link_records(
        &mut self,
        recs: Vec<Vec<u8>>,
        now: Instant,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        for r in recs {
            match r.first() {
                Some(&K_DATA) if r.len() >= 9 => {
                    let idx = u64::from_be_bytes(r[1..9].try_into().unwrap_or([0; 8]));
                    self.stats.records_direct_rx += 1;
                    self.deliver_indexed(idx, r[9..].to_vec(), deliver);
                }
                Some(&K_DACK) if r.len() >= 9 => {
                    let n = u64::from_be_bytes(r[1..9].try_into().unwrap_or([0; 8]));
                    while self.replay.front().is_some_and(|(i, _)| *i < n) {
                        if let Some((_, rec)) = self.replay.pop_front() {
                            self.replay_bytes -= rec.len();
                        }
                    }
                }
                _ => {}
            }
        }
        let _ = now;
    }

    fn on_probe(&mut self, path: u32, d: &[u8], now: Instant, io: &mut dyn DirectIo) {
        let Some((ty, nonce, t_us)) = open_probe(&self.key, d) else {
            self.stats.bad_probes += 1;
            return;
        };
        match (self.is_server, ty) {
            (true, P_REQ) => {
                let _ = io.direct_send(path, &probe(&self.key, P_RESP, &nonce, t_us));
            }
            (true, P_REBIND) => {
                if let Some(l) = self.link.as_mut() {
                    if l.path != path {
                        let old = l.path;
                        l.path = path;
                        l.last_rx = now;
                        l.confirmed = true;
                        self.stats.rebinds += 1;
                        self.events.push(format!(
                            "viewer's address changed: path#{old} -> path#{path} (NAT/VPN rebinding); link moved"
                        ));
                    }
                }
            }
            (true, P_COMMIT) => {
                if self.link.as_ref().is_some_and(|l| l.path == path) {
                    return; // repeated COMMIT
                }
                if self.link.is_some() {
                    // The viewer moved to another path: settle the old one
                    // on the relay first, then switch.
                    self.demote(now, io, "viewer committed another path");
                }
                self.link = Some(self.make_link(path, now, true));
                self.events
                    .push(format!("viewer committed direct path#{path}"));
                self.promote(now, io);
            }
            (false, P_RESP) => {
                let Some(round) = self.probe.as_ref() else {
                    return;
                };
                if round.nonce != nonce || !round.paths.contains(&path) {
                    return;
                }
                self.stats.probe_responses += 1;
                if self.link.is_some() {
                    return; // a slower candidate; the first one won
                }
                let rtt = self.now_us(now).saturating_sub(t_us);
                self.stats.probe_rtt_us = rtt;
                self.probe = None;
                self.link = Some(self.make_link(path, now, false));
                self.events.push(format!(
                    "probe answered on path#{path} (rtt {:.2} ms)",
                    rtt as f64 / 1000.0
                ));
                let c = probe(&self.key, P_COMMIT, &self.commit_nonce, self.now_us(now));
                let _ = io.direct_send(path, &c);
                self.promote(now, io);
            }
            _ => {}
        }
    }

    // ---------------- timers ----------------

    /// Run timers: rudp ACK/retransmit, keepalives, liveness, probe rounds.
    /// Call after every receive batch and whenever
    /// [`next_deadline`](Self::next_deadline) expires.
    pub fn poll(&mut self, now: Instant, io: &mut dyn DirectIo, deliver: &mut dyn FnMut(&[u8])) {
        if !self.armed {
            return;
        }
        // Probing.
        if self.probe.is_none() && self.link.is_none() && self.reprobe_at.is_some_and(|t| now >= t)
        {
            self.begin_round(now);
        }
        let key = self.key;
        let t_now = self.now_us(now);
        let mut failed_round = false;
        if let Some(round) = self.probe.as_mut() {
            if now.saturating_duration_since(round.started) >= PROBE_ROUND {
                failed_round = true;
            } else if round
                .last_sent
                .is_none_or(|t| now.saturating_duration_since(t) >= PROBE_INTERVAL)
            {
                round.last_sent = Some(now);
                let p = probe(&key, P_REQ, &round.nonce, t_now);
                for &path in &round.paths {
                    let _ = io.direct_send(path, &p);
                    self.stats.probes_sent += 1;
                }
            }
        }
        if failed_round {
            self.probe = None;
            self.events
                .push("no candidate answered; staying on relay".into());
            self.reprobe_at = Some(now + self.reprobe_delay);
            self.reprobe_delay = (self.reprobe_delay * 2).min(REPROBE_MAX);
        }
        // Link liveness + keepalive + DACKs.
        let dead_after = if self.primary_lost {
            DEAD_AFTER_RELAYLESS
        } else {
            DEAD_AFTER
        };
        let dead = self
            .link
            .as_ref()
            .is_some_and(|l| now.saturating_duration_since(l.last_rx) >= dead_after);
        if dead {
            let why = if self.primary_lost {
                "silent, relay gone too"
            } else {
                "silent"
            };
            self.demote(now, io, why);
        }
        let delivered = self.delivered;
        let commit = (!self.is_server).then(|| probe(&key, P_COMMIT, &self.commit_nonce, t_now));
        let mut recs: Vec<Vec<u8>> = Vec::new();
        if let Some(link) = self.link.as_mut() {
            let path = link.path;
            let idle = now.saturating_duration_since(link.last_tx) >= KEEPALIVE;
            let dack_due = delivered != link.dacked
                && (delivered - link.dacked >= DACK_EVERY
                    || now.saturating_duration_since(link.last_dack) >= DACK_INTERVAL);
            // Viewer: a silent link may mean our NAT / VPN mapping changed
            // (the host's replies go to the old one): tell the host where we
            // are now, authenticated.
            if let Some(c) = commit.as_ref().filter(|_| link.confirmed) {
                let silent = now.saturating_duration_since(link.last_rx) >= REBIND_AFTER;
                if silent
                    && link
                        .last_rebind
                        .is_none_or(|t| now.saturating_duration_since(t) >= REBIND_INTERVAL)
                {
                    link.last_rebind = Some(now);
                    let _ = c;
                    let _ = io.direct_send(path, &probe(&key, P_REBIND, &self.commit_nonce, t_now));
                    self.stats.rebinds += 1;
                }
            }
            if idle || dack_due {
                if idle && !link.confirmed {
                    if let Some(c) = &commit {
                        let _ = io.direct_send(path, c);
                    }
                }
                let mut m = Vec::with_capacity(9);
                m.push(K_DACK);
                m.extend_from_slice(&delivered.to_be_bytes());
                let Link { rudp, cc, .. } = &mut *link;
                let _ = rudp.send(&m, now, &mut rudp_out(cc, io, path, now));
                link.last_tx = now;
                link.last_dack = now;
                link.dacked = delivered;
            }
            let Link { rudp, cc, .. } = &mut *link;
            rudp.end_batch(
                now,
                &mut |r| recs.push(r.to_vec()),
                &mut rudp_out(cc, io, path, now),
            );
            // Everything we sent is acknowledged by the peer's link: the
            // replay buffer is no longer needed.
            if link.rudp.in_flight() == 0
                && link.rudp.pending_bytes() == 0
                && link
                    .cc
                    .as_ref()
                    .is_none_or(|c| c.pacer.queued(Lane::Reliable) == 0)
            {
                self.replay.clear();
                self.replay_bytes = 0;
            }
        }
        self.on_link_records(recs, now, deliver);
        self.poll_v2(now, io);
        // Relay keepalive while it idles behind the direct path.
        if self.on_direct
            && now.saturating_duration_since(self.last_primary_tx) >= PRIMARY_KEEPALIVE
            && io.primary_send(&marker(M_NOP, 0)).is_ok()
        {
            self.last_primary_tx = now;
        }
    }

    /// When [`poll`](Self::poll) is next needed.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        if !self.armed {
            return None;
        }
        let mut d: Option<Duration> = None;
        let mut take = |x: Duration| d = Some(d.map_or(x, |y: Duration| y.min(x)));
        if let Some(r) = &self.probe {
            take(r.last_sent.map_or(Duration::ZERO, |t| {
                (t + PROBE_INTERVAL).saturating_duration_since(now)
            }));
        }
        if let Some(t) = self.reprobe_at {
            take(t.saturating_duration_since(now));
        }
        if let Some(l) = &self.link {
            take((l.last_tx + KEEPALIVE).saturating_duration_since(now));
            let dead_after = if self.primary_lost {
                DEAD_AFTER_RELAYLESS
            } else {
                DEAD_AFTER
            };
            take((l.last_rx + dead_after).saturating_duration_since(now));
            if !self.is_server && l.confirmed {
                let mut due = l.last_rx + REBIND_AFTER;
                if let Some(t) = l.last_rebind {
                    due = due.max(t + REBIND_INTERVAL);
                }
                take(due.saturating_duration_since(now));
            }
            if l.dacked != self.delivered {
                take((l.last_dack + DACK_INTERVAL).saturating_duration_since(now));
            }
            if let Some(x) = l.rudp.next_deadline(now) {
                take(x);
            }
            if let Some(cc) = &l.cc {
                if let Some(x) = cc.pacer.next_release(now) {
                    take(x);
                }
                if let Some(x) = cc.fb.next_due(now) {
                    take(x);
                }
            }
        }
        if let Some(x) = self.media.as_ref().and_then(|m| m.rx.next_deadline(now)) {
            take(x);
        }
        if self.on_direct {
            take((self.last_primary_tx + PRIMARY_KEEPALIVE).saturating_duration_since(now));
        }
        d
    }
}

fn rand16() -> [u8; 16] {
    let v = ovcore::crypto::random_bytes(16);
    let mut n = [0u8; 16];
    n.copy_from_slice(&v);
    n
}
