//! The direct path's media channel (video FEC + Opus over unreliable
//! datagrams): negotiation from the CAPS records and delivery to the shell
//! as ordinary channel-0 commands.

use ovapp::video::{VIDEO_END, VIDEO_KEYFRAME, VIDEO_RECOVERY, VIDEO_SUPERSEDED};
use ovtransport::media::{MediaEvent, PictureRequest};

use crate::direct::FfiDirectIo;
use crate::pump::send_record_bytes;
use crate::session::Inner;

/// `VideoHeader::flags` inside a media-channel unit.
fn unit_flags(unit: &[u8]) -> u8 {
    unit.get(8).copied().unwrap_or(0)
}

/// Whether a media-channel unit is a way back in for request `r` that the
/// shell's screen accepts: a keyframe with its parameter sets in-band (the
/// screen refuses others), or — answering a `VIDEO_RECOVER` only — a
/// recovery picture (an IDR request may mean the decoder's references are
/// gone).
fn answers(r: PictureRequest, unit: &[u8]) -> bool {
    use ovapp::video::{AccessUnit, VideoCodec, VIDEO_HEADER_LEN};
    let flags = unit_flags(unit);
    if flags & VIDEO_KEYFRAME != 0 {
        // `VideoHeader::codec` follows the flags.
        let codec = unit.get(9).and_then(|&c| VideoCodec::from_wire(c));
        let data = unit.get(VIDEO_HEADER_LEN..);
        return codec.zip(data).is_some_and(|(codec, data)| {
            AccessUnit::from_annexb_codec(codec, data).has_param_sets(codec)
        });
    }
    flags & VIDEO_RECOVERY != 0 && matches!(r, PictureRequest::Recover { .. })
}

/// The CAPS flags of a channel-0 CAPS record payload.
pub(crate) fn caps_of(channel: u8, payload: &[u8]) -> Option<u32> {
    (channel == 0 && payload.len() >= 5 && payload[0] == ovapp::terminal::OpCode::Caps as u8)
        .then(|| u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]))
}

/// Open the media channel once both CAPS carry CAP_MEDIA (and a
/// direct engine exists).
pub(crate) fn maybe_enable_media(g: &mut Inner) {
    let both = g
        .local_caps
        .is_some_and(|c| c & ovapp::terminal::CAP_MEDIA != 0)
        && g.peer_caps
            .is_some_and(|c| c & ovapp::terminal::CAP_MEDIA != 0);
    if !both {
        return;
    }
    let (Some(session), Some(dir)) = (g.session.as_ref(), g.direct.as_mut()) else {
        return;
    };
    if !dir.engine.media_enabled() {
        dir.engine
            .enable_media(ovtransport::media::session_media_keys(session));
    }
}

/// A VIDEO_IDR_REQUEST / VIDEO_RECOVER the shell sends while both CAPS
/// carry CAP_MEDIA_PLI goes out as a repeated PLI on the media channel
/// instead, when that is up (`true` = sent there; nothing goes reliably).
pub(crate) fn send_picture_request(g: &mut Inner, channel: u8, payload: &[u8]) -> bool {
    let Some(r) = picture_request(g.local_caps, g.peer_caps, channel, payload) else {
        return false;
    };
    let Some(dir) = g.direct.as_mut() else {
        return false;
    };
    let mut io = FfiDirectIo {
        transport: &mut g.transport,
        sink: g.direct_send_sink,
    };
    dir.engine
        .request_picture(r, std::time::Instant::now(), &mut io)
}

/// The PLI an outgoing record becomes: a channel-0 VIDEO_IDR_REQUEST /
/// VIDEO_RECOVER when both CAPS (`local`, `peer`) carry CAP_MEDIA_PLI.
fn picture_request(
    local: Option<u32>,
    peer: Option<u32>,
    channel: u8,
    payload: &[u8],
) -> Option<PictureRequest> {
    use ovapp::terminal::{Command, OpCode, CAP_MEDIA_PLI};
    let ops = [OpCode::VideoIdrRequest as u8, OpCode::VideoRecover as u8];
    let pli = local
        .zip(peer)
        .is_some_and(|(l, p)| l & p & CAP_MEDIA_PLI != 0);
    if channel != 0 || !pli || !payload.first().is_some_and(|op| ops.contains(op)) {
        return None;
    }
    match ovapp::terminal::decode(payload) {
        Ok(Command::VideoIdrRequest { have_seq }) => Some(PictureRequest::Keyframe { have_seq }),
        Ok(Command::VideoRecover { ltr_seq }) => Some(PictureRequest::Recover { ltr_seq }),
        _ => None,
    }
}

/// Send picture request `r` as its reliable record (VIDEO_IDR_REQUEST /
/// VIDEO_RECOVER on channel 0).
fn send_reliable_request(g: &mut Inner, r: PictureRequest) {
    let payload = ovapp::terminal::encode(&ovapp::terminal::Command::from(r));
    let Some(session) = g.session.as_mut() else {
        return;
    };
    let record = session.encrypt_record(0, &payload);
    let _ = send_record_bytes(g, &record);
}

/// Which video units of one delivery batch a newer picture in the same
/// batch supersedes: those followed, with no loss and no VIDEO_END in
/// between, by another unit. A burst (a hole repaired, frames released
/// together) then reaches the screen as its newest picture instead of a
/// fast-forward; the others are still decoded.
fn superseded(events: &[MediaEvent]) -> Vec<bool> {
    let mut out = vec![false; events.len()];
    let mut newer = false;
    for (i, e) in events.iter().enumerate().rev() {
        match e {
            MediaEvent::Video(u) if unit_flags(u) & VIDEO_END != 0 => newer = false,
            MediaEvent::Video(_) => {
                out[i] = newer;
                newer = true;
            }
            MediaEvent::VideoLost { .. } => newer = false,
            _ => {}
        }
    }
    out
}

/// Hand what the media channel delivered to the shell's record sink
/// as channel-0 commands `ov_screen_feed` understands: a VIDEO_UNIT per
/// complete access unit (flagged `VIDEO_SUPERSEDED` when a newer one of the
/// batch follows), a MEDIA_GAP for frames / audio packets lost for good,
/// an AUDIO per Opus packet — so the Swift side needs no change.
pub(crate) fn deliver_media(g: &mut Inner) {
    let Some(dir) = g.direct.as_mut() else { return };
    let events = dir.engine.take_media_events();
    let answered = |r: PictureRequest| {
        events
            .iter()
            .any(|e| matches!(e, MediaEvent::Video(u) if answers(r, u)))
    };
    // Stop repeating the PLI only for a picture that gets the screen back in.
    if dir.engine.picture_pending().is_some_and(answered) {
        dir.engine.picture_arrived();
    }
    // The PLI repeats went unanswered: the same request once reliably.
    let unanswered = events.iter().find_map(|e| match e {
        MediaEvent::PictureUnanswered(r) => Some(*r),
        _ => None,
    });
    if let Some(r) = unanswered.filter(|&r| !answered(r)) {
        send_reliable_request(g, r);
    }
    let Some(f) = g.record_sink.f else { return };
    let user = g.record_sink.user;
    let emit = |p: &[u8]| f(user, 0, p.as_ptr(), p.len());
    let superseded = superseded(&events);
    for (ev, superseded) in events.into_iter().zip(superseded) {
        match ev {
            MediaEvent::Video(unit) => {
                let mut p = Vec::with_capacity(1 + unit.len());
                p.push(ovapp::terminal::OpCode::VideoUnit as u8);
                p.extend_from_slice(&unit);
                if superseded {
                    if let Some(flags) = p.get_mut(1 + 8) {
                        *flags |= VIDEO_SUPERSEDED;
                    }
                }
                emit(&p);
            }
            MediaEvent::VideoLost { frames } => {
                emit(&ovapp::terminal::encode(
                    &ovapp::terminal::Command::MediaGap {
                        kind: ovapp::terminal::GAP_VIDEO,
                        packets: frames.min(u32::from(u16::MAX)) as u16,
                        samples: 0,
                    },
                ));
            }
            // Host-bound only (a viewer never receives a PLI); handled
            // above.
            MediaEvent::PictureLoss(_) | MediaEvent::PictureUnanswered(_) => {}
            MediaEvent::Audio { lost, payload } => {
                if lost > 0 {
                    let per = match ovapp::audio::parse_audio_frame(&payload) {
                        Ok((_, ovapp::audio::AudioPayload::Opus { samples, .. })) => samples,
                        Ok((_, ovapp::audio::AudioPayload::Pcm(s))) => s.len() as u32,
                        Err(_) => ovapp::audio::OPUS10_SAMPLES,
                    };
                    let packets = lost.min(u32::from(u16::MAX));
                    emit(&ovapp::terminal::encode(
                        &ovapp::terminal::Command::MediaGap {
                            kind: ovapp::terminal::GAP_AUDIO,
                            packets: packets as u16,
                            samples: per.saturating_mul(packets),
                        },
                    ));
                }
                let mut p = Vec::with_capacity(1 + payload.len());
                p.push(ovapp::terminal::OpCode::Audio as u8);
                p.extend_from_slice(&payload);
                emit(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recovery request becomes a PLI only when both peers set
    /// CAP_MEDIA_PLI (an older host keeps getting the reliable record).
    #[test]
    fn picture_requests_go_on_the_media_channel_only_with_the_cap() {
        use ovapp::terminal::{encode, Command, CAP_MEDIA, CAP_MEDIA_PLI};
        let rec = encode(&Command::VideoRecover { ltr_seq: 5 });
        let both = Some(CAP_MEDIA | CAP_MEDIA_PLI);
        let old = Some(CAP_MEDIA);
        let want = Some(PictureRequest::Recover { ltr_seq: 5 });
        assert_eq!(picture_request(both, both, 0, &rec), want);
        assert_eq!(picture_request(both, old, 0, &rec), None);
        assert_eq!(picture_request(old, both, 0, &rec), None);
        assert_eq!(picture_request(both, None, 0, &rec), None);
        let idr = encode(&Command::VideoIdrRequest { have_seq: 9 });
        assert_eq!(
            picture_request(both, both, 0, &idr),
            Some(PictureRequest::Keyframe { have_seq: 9 })
        );
        let ack = encode(&Command::VideoLtrAck { seq: 5 });
        assert_eq!(picture_request(both, both, 0, &ack), None);
    }

    /// Only a picture the screen accepts stops the PLI repeats: a recovery
    /// answers a `VIDEO_RECOVER` but not an IDR request; a keyframe needs
    /// its parameter sets.
    #[test]
    fn only_an_acceptable_picture_answers_the_request() {
        use ovapp::video::{VideoHeader, CODEC_H264};
        let unit = |flags: u8, annexb: &[u8]| {
            let h = VideoHeader {
                flags,
                codec: CODEC_H264,
                ..VideoHeader::default()
            };
            let mut u = Vec::new();
            h.encode(annexb.len() as u32, &mut u);
            u.extend_from_slice(annexb);
            u
        };
        let nal = |t: u8| [0, 0, 0, 1, t, 0xAA];
        let idr = [nal(7), nal(8), nal(5)].concat();
        let (key, rec) = (
            PictureRequest::Keyframe { have_seq: 1 },
            PictureRequest::Recover { ltr_seq: 1 },
        );
        assert!(answers(key, &unit(VIDEO_KEYFRAME, &idr)));
        assert!(answers(rec, &unit(VIDEO_KEYFRAME, &idr)));
        assert!(!answers(key, &unit(VIDEO_KEYFRAME, &nal(5))));
        assert!(answers(rec, &unit(VIDEO_RECOVERY, &nal(1))));
        assert!(!answers(key, &unit(VIDEO_RECOVERY, &nal(1))));
        assert!(!answers(rec, &unit(0, &nal(1))));
    }

    /// All but the newest unit of a burst are superseded; a loss or a
    /// VIDEO_END in between ends the burst.
    #[test]
    fn superseded_marks_all_but_the_newest_of_a_batch() {
        let v = |flags: u8| {
            let mut u = vec![0u8; 40];
            u[8] = flags;
            MediaEvent::Video(u)
        };
        let lost = MediaEvent::VideoLost { frames: 1 };
        let audio = MediaEvent::Audio {
            lost: 0,
            payload: Vec::new(),
        };
        let batch = [v(0), audio, v(0), v(0), lost, v(0), v(VIDEO_END), v(1)];
        assert_eq!(
            superseded(&batch),
            [true, false, true, false, false, false, false, false]
        );
    }
}
