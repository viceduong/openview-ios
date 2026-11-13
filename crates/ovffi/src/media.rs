//! The direct path's media channel (video FEC + Opus over unreliable
//! datagrams): negotiation from the CAPS records and delivery to the shell
//! as ordinary channel-0 commands.

use crate::session::Inner;

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

/// Hand what the media channel delivered to the shell's record sink
/// as channel-0 commands `ov_screen_feed` understands: a VIDEO_UNIT per
/// complete access unit, a MEDIA_GAP for frames / audio packets lost for
/// good, an AUDIO per Opus packet — so the Swift side needs no change.
pub(crate) fn deliver_media(g: &mut Inner) {
    let Some(dir) = g.direct.as_mut() else { return };
    let events = dir.engine.take_media_events();
    let Some(f) = g.record_sink.f else { return };
    let user = g.record_sink.user;
    let emit = |p: &[u8]| f(user, 0, p.as_ptr(), p.len());
    for ev in events {
        match ev {
            ovtransport::media::MediaEvent::Video(unit) => {
                let mut p = Vec::with_capacity(1 + unit.len());
                p.push(ovapp::terminal::OpCode::VideoUnit as u8);
                p.extend_from_slice(&unit);
                emit(&p);
            }
            ovtransport::media::MediaEvent::VideoLost { frames } => {
                emit(&ovapp::terminal::encode(
                    &ovapp::terminal::Command::MediaGap {
                        kind: ovapp::terminal::GAP_VIDEO,
                        packets: frames.min(u32::from(u16::MAX)) as u16,
                        samples: 0,
                    },
                ));
            }
            ovtransport::media::MediaEvent::Audio { lost, payload } => {
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
