//! The engine's media channel: v2 framing, video / audio sends, feedback,
//! NACKs and pacing.

use std::time::{Duration, Instant};

use super::super::link::{apply_rate, rudp_out, Link, LinkCc, Paced};
use super::super::media_state::{MediaState, SentFrame};
use super::super::*;
use super::DirectEngine;
use crate::cc::{Lane, PacketReport};
use crate::media::{self, MediaEvent, MediaKeys};

impl DirectEngine {
    /// Both peers advertised `CAP_MEDIA`: switch the direct link to the v2
    /// framing (congestion control, pacing) and open the media channel.
    /// Idempotent — the first keys stay (a media sender must never restart
    /// its packet numbers under the same key).
    pub fn enable_media(&mut self, keys: MediaKeys) {
        if self.media.is_some() {
            return;
        }
        self.media = Some(MediaState::new(keys));
        let epoch = self.epoch;
        if let Some(l) = self.link.as_mut() {
            if l.cc.is_none() {
                l.cc = Some(LinkCc::new(epoch));
                apply_rate(l);
            }
        }
        self.events
            .push("media channel + delay-based congestion control enabled".into());
    }

    /// The relay carrier died while the direct path carried the session:
    /// keep the link through longer silences (nothing to fall back to).
    pub fn primary_lost(&mut self, now: Instant) {
        if !self.primary_lost {
            self.primary_lost = true;
            let since = self
                .direct_since
                .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
            let rx = self.link.as_ref().map_or(f64::NAN, |l| {
                now.saturating_duration_since(l.last_rx).as_secs_f64()
            });
            self.events.push(format!(
                "relay gone {since:.1} s after the direct path went active (direct last heard {rx:.2} s ago); \
                 direct path keeps the session, given up after {} s of silence",
                DEAD_AFTER_RELAYLESS.as_secs()
            ));
        }
    }

    /// Whether [`enable_media`](Self::enable_media) was called.
    pub fn media_enabled(&self) -> bool {
        self.media.is_some()
    }

    /// Whether media can go out now (v2 link active and confirmed).
    pub fn media_ready(&self) -> bool {
        self.media.is_some()
            && self.on_direct
            && self
                .link
                .as_ref()
                .is_some_and(|l| l.cc.is_some() && l.confirmed)
    }

    /// The congestion controller's target rate (bits/s) while the v2 link
    /// carries the session.
    pub fn cc_target(&self) -> Option<f64> {
        self.media_ready()
            .then(|| self.link.as_ref()?.cc.as_ref().map(|c| c.gcc.target_bps()))
            .flatten()
    }

    /// The controller's one-way queueing-delay estimate.
    pub fn cc_queue_delay(&self) -> Option<Duration> {
        self.media_ready()
            .then(|| self.link.as_ref()?.cc.as_ref().map(|c| c.gcc.queue_delay()))
            .flatten()
    }

    /// The FEC ratio the next video frame gets.
    pub fn fec_ratio(&self) -> f64 {
        media::fec_ratio(
            self.link
                .as_ref()
                .and_then(|l| l.cc.as_ref())
                .map_or(0.0, |c| c.gcc.loss()),
        )
    }

    /// Media counters (`None` = media not enabled).
    pub fn media_stats(&self) -> Option<MediaStats> {
        let m = self.media.as_ref()?;
        let mut s = m.stats;
        s.rx = m.rx.stats();
        if let Some(cc) = self.link.as_ref().and_then(|l| l.cc.as_ref()) {
            s.cc = cc.gcc.stats();
            s.target_bps = cc.gcc.target_bps();
            s.loss = cc.gcc.loss();
            s.queue_delay_ms = cc.gcc.queue_delay().as_secs_f64() * 1000.0;
        }
        s.fec_ratio = self.fec_ratio();
        Some(s)
    }

    /// Media received since the last call (video units in frame order,
    /// losses, audio packets).
    pub fn take_media_events(&mut self) -> Vec<MediaEvent> {
        self.media
            .as_mut()
            .map(|m| m.rx.take_events())
            .unwrap_or_default()
    }

    /// Send one video unit (`VideoHeader || Annex-B`) on the media channel:
    /// FEC-coded shards into the pacer's video lane. `false` = not sent
    /// (media not ready, or the unit is unusable) — send it reliably.
    pub fn send_media_video(
        &mut self,
        unit: &[u8],
        keyframe: bool,
        now: Instant,
        io: &mut dyn DirectIo,
    ) -> bool {
        if !self.media_ready() {
            return false;
        }
        let ratio = self.fec_ratio();
        let (Some(link), Some(ms)) = (self.link.as_mut(), self.media.as_mut()) else {
            return false;
        };
        let Some(cc) = link.cc.as_mut() else {
            return false;
        };
        let id = ms.next_frame;
        let Ok(msgs) = media::fec_encode(unit, id, keyframe, ratio) else {
            return false;
        };
        ms.next_frame = id.wrapping_add(1);
        let k = unit.len().div_ceil(media::MAX_SHARD) as u16;
        for m in &msgs {
            let item = Paced::Media(m.clone());
            cc.pacer.push(Lane::Media, item.wire_len(), item);
        }
        ms.stats.frames_tx += 1;
        ms.stats.shards_tx += msgs.len() as u64;
        ms.stats.parity_tx += (msgs.len() - usize::from(k)) as u64;
        ms.remember(
            SentFrame {
                id,
                k,
                msgs,
                at: now,
                resends: 0,
            },
            now,
        );
        self.pace(now, io);
        true
    }

    /// Send one AUDIO payload on the media channel (unpaced: it is tiny and
    /// latency-critical). `false` = not sent (send it reliably).
    pub fn send_media_audio(
        &mut self,
        payload: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
    ) -> bool {
        if !self.media_ready() {
            return false;
        }
        let (Some(link), Some(ms)) = (self.link.as_mut(), self.media.as_mut()) else {
            return false;
        };
        let Some(cc) = link.cc.as_mut() else {
            return false;
        };
        let aseq = ms.next_aseq;
        ms.next_aseq = aseq.wrapping_add(1);
        let item = Paced::Media(media::audio_msg(aseq, payload));
        let _ = cc.send_now(Some(&mut ms.tx), now, link.path, &item, io);
        link.last_tx = now;
        ms.stats.audio_tx += 1;
        true
    }

    /// Release what the pacer allows now.
    pub(super) fn pace(&mut self, now: Instant, io: &mut dyn DirectIo) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let Some(cc) = link.cc.as_mut() else { return };
        let path = link.path;
        let mut tx = self.media.as_mut().map(|m| &mut m.tx);
        let mut sent = false;
        while let Some((lane, item)) = cc.pacer.pop(now) {
            match cc.emit(tx.as_deref_mut(), now, path, &item, io) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    cc.pacer.push_front(lane, item.wire_len(), item);
                    break;
                }
                _ => sent = true,
            }
        }
        if sent {
            link.last_tx = now;
        }
    }

    /// A v2 datagram on the link's path.
    pub(super) fn on_v2(
        &mut self,
        path: u32,
        d: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        if d.len() < media::V2_HEADER {
            return;
        }
        let Some(link) = self.link.as_mut().filter(|l| l.path == path) else {
            return;
        };
        let tseq = u32::from_be_bytes(d[2..6].try_into().unwrap_or([0; 4]));
        match d[1] {
            media::KIND_RUDP => {
                if let Some(cc) = link.cc.as_mut() {
                    let at = cc.us(now);
                    cc.fb.on_packet(tseq, at);
                }
                link.last_rx = now;
                link.confirmed = true;
                let mut recs: Vec<Vec<u8>> = Vec::new();
                let Link { rudp, cc, .. } = link;
                rudp.on_datagram(
                    &d[media::V2_HEADER..],
                    now,
                    &mut |r| recs.push(r.to_vec()),
                    &mut rudp_out(cc, io, path, now),
                );
                self.on_link_records(recs, now, deliver);
            }
            media::KIND_MEDIA => {
                let Some(ms) = self.media.as_mut() else {
                    return;
                };
                let Some((pn, pt)) = media::open(&ms.rx_key, d) else {
                    ms.stats.bad_rx += 1;
                    return;
                };
                if !ms.replay.check(pn) {
                    ms.stats.replayed_rx += 1;
                    return;
                }
                ms.replay.commit(pn);
                link.last_rx = now;
                link.confirmed = true;
                if let Some(cc) = link.cc.as_mut() {
                    let at = cc.us(now);
                    cc.fb.on_packet(tseq, at);
                }
                match pt.first() {
                    Some(&media::M_VIDEO) => ms.rx.on_shard(&pt, now),
                    Some(&media::M_AUDIO) => ms.rx.on_audio(&pt),
                    Some(&media::M_FEEDBACK) => {
                        let rtt = link.rudp.srtt();
                        if let (Some(cc), Some(pk)) =
                            (link.cc.as_mut(), crate::cc::parse_feedback(&pt[1..]))
                        {
                            let reports: Vec<PacketReport> = pk
                                .iter()
                                .filter_map(|&(t, arr)| {
                                    let (send_us, size) = cc.hist.get(t)?;
                                    let arrival_us = arr.map(|a| cc.gcc.unwrap_remote(a));
                                    Some(PacketReport {
                                        tseq: t,
                                        send_us,
                                        size,
                                        arrival_us,
                                    })
                                })
                                .collect();
                            cc.gcc.on_feedback(now, &reports, rtt);
                            apply_rate(link);
                        }
                    }
                    Some(&media::M_NACK) => {
                        if let (Some((f, idx)), Some(cc)) =
                            (media::parse_nack(&pt), link.cc.as_mut())
                        {
                            ms.stats.nack_rx += 1;
                            if let Some(sf) = ms.sent.iter_mut().find(|s| s.id == f) {
                                if sf.resends < 2 {
                                    sf.resends += 1;
                                    let want: Vec<usize> = if idx.is_empty() {
                                        (0..usize::from(sf.k)).collect()
                                    } else {
                                        idx.iter().map(|&i| usize::from(i)).collect()
                                    };
                                    for i in want {
                                        if let Some(m) = sf.msgs.get(i) {
                                            let mut m = m.clone();
                                            m[15] |= media::SHARD_RETX;
                                            let item = Paced::Media(m);
                                            cc.pacer.push(Lane::Reliable, item.wire_len(), item);
                                            ms.stats.shards_resent += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => ms.stats.bad_rx += 1,
                }
            }
            _ => {}
        }
    }

    /// v2 timers: feedback, media loss decisions (NACK), pacing.
    pub(super) fn poll_v2(&mut self, now: Instant, io: &mut dyn DirectIo) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let path = link.path;
        let rtt = link.rudp.srtt();
        let Some(cc) = link.cc.as_mut() else { return };
        if let Some(ms) = self.media.as_mut() {
            if cc.fb.due(now) {
                for m in cc.fb.build(now, media::M_FEEDBACK) {
                    let _ = cc.send_now(Some(&mut ms.tx), now, path, &Paced::Media(m), io);
                    ms.stats.feedback_tx += 1;
                }
            }
            let MediaState { rx, tx, .. } = ms;
            rx.poll(now, rtt, &mut |f, idx| {
                cc.send_now(
                    Some(tx),
                    now,
                    path,
                    &Paced::Media(media::nack_msg(f, idx)),
                    io,
                )
                .is_ok()
            });
        }
        self.pace(now, io);
    }
}
