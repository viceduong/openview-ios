use super::*;

#[test]
fn roundtrip_preserves_samples() {
    let samples: Vec<i16> = (0..TICK_SAMPLES as i16).map(|i| i * 7).collect();
    let wire = encode_audio_frame(&samples, FLAG_NONE);
    assert_eq!(wire.len(), HEADER_LEN + TICK_SAMPLES as usize * 2);
    assert_eq!(wire[0], CODEC_RAW_S16LE);
    let (flags, decoded) = decode_audio_frame(&wire).unwrap();
    assert_eq!(flags, FLAG_NONE);
    assert_eq!(decoded, samples);
}

#[test]
fn decode_rejects_bad_payloads() {
    assert!(decode_audio_frame(&[]).is_err());
    assert!(decode_audio_frame(&[0; HEADER_LEN - 1]).is_err());
    // Unknown codec tag.
    assert!(decode_audio_frame(&[1, 0, 0, 0, 0, 0]).is_err());
    // Unknown flag bits.
    let mut bad = encode_audio_frame(&[1, 2], 0);
    bad[1] = 0x01;
    assert!(decode_audio_frame(&bad).is_err());
    // Sample count lying about the tail length.
    let mut bad = encode_audio_frame(&[1, 2], 0);
    bad[2..6].copy_from_slice(&3u32.to_be_bytes());
    assert!(decode_audio_frame(&bad).is_err());
}

#[test]
fn ring_buffer_zero_fills_on_underrun() {
    let mut rb = AudioRingBuffer::new(4);
    let mut out = [0i16; 3];
    rb.pop(&mut out);
    assert_eq!(out, [0; 3]);
    assert_eq!(rb.underruns(), 1);

    rb.push(&[1, 2, 3, 4, 5]);
    assert_eq!(rb.len(), 4); // oldest (1) dropped
    rb.pop(&mut out);
    assert_eq!(out, [2, 3, 4]);
    assert_eq!(rb.underruns(), 1);
    rb.pop(&mut out);
    assert_eq!(out, [5, 0, 0]);
    assert_eq!(rb.underruns(), 2);
}

#[test]
fn parse_accepts_both_codecs_and_rejects_bad_opus() {
    let pcm = encode_audio_frame(&[1, -2, 3], FLAG_NONE);
    assert_eq!(
        parse_audio_frame(&pcm).unwrap().1,
        AudioPayload::Pcm(vec![1, -2, 3])
    );
    let op = encode_opus_frame(&[0xAB, 0xCD], 960, FLAG_NONE);
    assert_eq!(
        parse_audio_frame(&op).unwrap().1,
        AudioPayload::Opus {
            samples: 960,
            packet: &[0xAB, 0xCD]
        }
    );
    // The raw-only decoder refuses Opus (an old viewer drops it).
    assert!(decode_audio_frame(&op).is_err());
    assert!(parse_audio_frame(&encode_opus_frame(&[], 960, 0)).is_err());
    assert!(parse_audio_frame(&encode_opus_frame(&[1], 0, 0)).is_err());
    assert!(parse_audio_frame(&encode_opus_frame(&[1], 48_000, 0)).is_err());
    assert!(parse_audio_frame(&encode_opus_frame(&vec![0; MAX_OPUS_PACKET + 1], 960, 0)).is_err());
    assert!(parse_audio_frame(&[2, 0, 0, 0, 0, 0]).is_err());
}

#[test]
fn encoder_sends_raw_pcm_to_a_peer_without_cap_opus() {
    let mut enc = AudioEncoder::new();
    let pcm = vec![7i16; TICK_SAMPLES as usize];
    let wire = enc.encode(&pcm, 0);
    assert_eq!(wire[0], CODEC_RAW_S16LE);
    assert_eq!(AudioDecoder::new().decode(&wire).unwrap(), pcm);
    assert!((enc.bitrate() - 768_000.0).abs() < 5_000.0, "raw ~768 kbps");
}

#[cfg(feature = "opus")]
#[test]
fn opus_roundtrip_is_small_and_faithful() {
    let mut enc = AudioEncoder::new();
    let mut dec = AudioDecoder::new();
    let mut g = SineGen::new(440.0, 8_000);
    let mut tick = vec![0i16; TICK_SAMPLES as usize];
    let mut energy_in = 0f64;
    let mut energy_out = 0f64;
    for i in 0..50 {
        g.tick(&mut tick);
        let wire = enc.encode(&tick, crate::terminal::CAP_OPUS);
        assert_eq!(wire[0], CODEC_OPUS);
        let out = dec.decode(&wire).unwrap();
        assert_eq!(out.len(), TICK_SAMPLES as usize);
        if i >= 5 {
            energy_in += tick.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
            energy_out += out.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        }
    }
    // ~48 kbps VBR vs 768 kbps raw; the tone survives (energy within 25%).
    let kbps = enc.bitrate() / 1000.0;
    assert!(kbps > 16.0 && kbps < 80.0, "opus bitrate {kbps:.1} kbps");
    let ratio = energy_out / energy_in;
    assert!((0.75..1.25).contains(&ratio), "energy ratio {ratio:.2}");
}

#[test]
fn jitter_buffer_prefills_then_plays_and_rebuffers_on_underrun() {
    let tick = TICK_SAMPLES as usize;
    let mut jb = JitterBuffer::with_ms(60, 120);
    let mut out = vec![0i16; tick];
    jb.push(&vec![1; tick * 2]); // 40 ms < 60 ms target
    assert!(!jb.pop(&mut out), "still prefilling");
    assert_eq!(jb.len(), tick * 2, "prefill consumes nothing");
    jb.push(&vec![1; tick]); // 60 ms: primed
    assert!(jb.pop(&mut out));
    assert!(out.iter().all(|&s| s == 1));
    assert!(jb.pop(&mut out));
    assert!(jb.pop(&mut out));
    assert_eq!(jb.underruns(), 0);
    jb.push(&vec![2; tick / 2]);
    assert!(jb.pop(&mut out), "partial tick still plays");
    assert_eq!(jb.underruns(), 1);
    assert!(out[tick / 2..].iter().all(|&s| s == 0));
    jb.push(&vec![3; tick]);
    assert!(!jb.pop(&mut out), "re-prefilling after the underrun");
}

#[test]
fn jitter_buffer_depth_is_bounded() {
    let tick = TICK_SAMPLES as usize;
    let mut jb = JitterBuffer::with_ms(60, 120);
    for i in 0..50 {
        jb.push(&vec![i as i16; tick]); // a 1 s burst
        assert!(jb.len() <= jb.max());
    }
    assert!(jb.depth_ms() <= 120);
    assert!(jb.dropped() > 0);
    // The newest audio survives the trim.
    let mut out = vec![0i16; tick];
    let mut last = 0;
    while jb.len() >= tick {
        jb.pop(&mut out);
        last = out[0];
    }
    assert_eq!(last, 49);
}

#[test]
fn audio_bus_drops_while_disabled_and_clears_on_disable() {
    let bus = AudioBus::new();
    let tick = vec![5i16; TICK_SAMPLES as usize];
    bus.push(&tick);
    assert_eq!(bus.status().0, 0, "disabled bus drops audio");
    bus.set_enabled(true);
    for _ in 0..3 {
        bus.push(&tick);
    }
    assert_eq!(bus.status().0, 60);
    let mut out = vec![0i16; TICK_SAMPLES as usize];
    assert!(bus.pop_tick(&mut out));
    assert_eq!(out, tick);
    bus.set_enabled(false);
    assert_eq!(bus.status().0, 0, "disabling discards buffered audio");
}

#[test]
fn tick_clock_has_no_drift_under_late_wakeups() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut c = TickClock::new(Duration::from_millis(20), t0);
    // Every wake-up is 3 ms late: the old `last = now` rule lost 3 ms per
    // tick (13% fewer ticks); the clock must still fire 500 times in 10 s.
    let mut fired = 0;
    let mut now = t0;
    while now < t0 + Duration::from_secs(10) {
        if c.tick(now) {
            fired += 1;
        }
        now += c.until_due(now) + Duration::from_millis(3);
    }
    assert!((499..=501).contains(&fired), "fired {fired}");
    // A long stall resyncs instead of bursting the backlog.
    now += Duration::from_secs(2);
    assert!(c.tick(now));
    assert!(!c.tick(now));
}

/// A mic source that blocks like a device and records how many ticks
/// the loop consumed: none may be lost.
struct CountingMic {
    n: i16,
}

impl MicSource for CountingMic {
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String> {
        std::thread::sleep(std::time::Duration::from_millis(1));
        self.n += 1;
        out.fill(self.n);
        Ok(())
    }
    fn self_paced(&self) -> bool {
        true
    }
}

#[test]
fn mic_loop_delivers_every_tick_in_order() {
    use std::sync::atomic::AtomicBool;
    let stop = AtomicBool::new(false);
    let mut mic = CountingMic { n: 0 };
    let mut got = Vec::new();
    run_mic_loop(&mut mic, &stop, |t| {
        got.push(t[0]);
        got.len() < 100
    });
    assert_eq!(
        got,
        (1..=100).collect::<Vec<i16>>(),
        "no tick lost or reordered"
    );
}

#[test]
fn synth_mic_loop_is_clock_paced() {
    use std::sync::atomic::AtomicBool;
    let stop = AtomicBool::new(false);
    let mut mic = SynthMic::new();
    let t0 = std::time::Instant::now();
    let mut n = 0;
    run_mic_loop(&mut mic, &stop, |_| {
        n += 1;
        n < 25
    });
    let el = t0.elapsed();
    // 25 ticks, the first immediate: >= 24 x 20 ms.
    assert!(el >= std::time::Duration::from_millis(470), "{el:?}");
    assert!(el < std::time::Duration::from_millis(1500), "{el:?}");
}

#[test]
fn sine_is_periodic_and_bounded() {
    let mut g = SineGen::new(440.0, 10_000);
    let mut buf = [0i16; TICK_SAMPLES as usize];
    g.tick(&mut buf);
    assert!(buf.iter().all(|&s| s.abs() <= 10_000));
    assert!(buf.iter().any(|&s| s != 0));
}

/// 10 ms Opus packets (with FEC) for a CAP_AUDIO10 peer, 20 ms
/// for an older Opus peer, raw PCM otherwise; any tick size in.
#[test]
fn encode_ticks_picks_the_negotiated_packet_size() {
    use crate::terminal::{CAP_AUDIO10, CAP_OPUS};
    let mut sine = SineGen::new(440.0, 8000);
    let mut tick = vec![0i16; 960];
    sine.tick(&mut tick);
    let mut raw = AudioEncoder::new();
    let p = raw.encode_ticks(&tick, 0);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0][0], CODEC_RAW_S16LE);
    if !cfg!(feature = "opus") {
        return;
    }
    let mut e10 = AudioEncoder::new();
    let mut dec = AudioDecoder::new();
    let ps = e10.encode_ticks(&tick, CAP_OPUS | CAP_AUDIO10);
    assert_eq!(ps.len(), 2, "two 10 ms packets per 20 ms");
    for p in &ps {
        assert_eq!(p[0], CODEC_OPUS);
        assert_eq!(u32::from_be_bytes([p[2], p[3], p[4], p[5]]), OPUS10_SAMPLES);
        assert_eq!(dec.decode(p).unwrap().len(), 480);
    }
    // FEC/PLC path never fails on a valid packet.
    assert!(dec.decode_fec(&ps[1], 480).is_ok());
    // 10 ms ticks into an old (20 ms) Opus peer: buffered to 960.
    let mut e20 = AudioEncoder::new();
    assert!(e20.encode_ticks(&tick[..480], CAP_OPUS).is_empty());
    let ps = e20.encode_ticks(&tick[480..], CAP_OPUS);
    assert_eq!(ps.len(), 1);
    assert_eq!(
        u32::from_be_bytes([ps[0][2], ps[0][3], ps[0][4], ps[0][5]]),
        960
    );
    assert!(
        e10.bitrate() > 1000.0 && e10.bitrate() < 200_000.0,
        "{}",
        e10.bitrate()
    );
}

/// Interleaved stereo: a 440 Hz tone left, a 660 Hz tone right.
fn stereo_tick(l: &mut SineGen, r: &mut SineGen, frames: usize) -> Vec<i16> {
    let (mut a, mut b) = (vec![0i16; frames], vec![0i16; frames]);
    l.tick(&mut a);
    r.tick(&mut b);
    a.into_iter().zip(b).flat_map(|(x, y)| [x, y]).collect()
}

/// Energy of every `step`-th sample from `start` (one channel of an
/// interleaved buffer).
fn energy(v: &[i16], start: usize, step: usize) -> f64 {
    v.iter()
        .skip(start)
        .step_by(step)
        .map(|&s| f64::from(s).powi(2))
        .sum()
}

#[test]
fn stereo_helpers_mix_and_split() {
    assert_eq!(downmix_stereo(&[100, 300, -4, 4, 7]), vec![200, 0]);
    assert_eq!(
        downmix_stereo(&[i16::MAX, i16::MAX, i16::MIN, i16::MIN]),
        vec![i16::MAX, i16::MIN]
    );
    assert_eq!(upmix_mono(&[1, -2]), vec![1, 1, -2, -2]);
}

/// The stereo gate drops to mono at once on a thin link, ignores unknown
/// rates, and only returns to stereo after the link held the resume rate
/// for the full hold time (no flapping around the threshold).
#[test]
fn stereo_gate_has_hysteresis() {
    let t0 = std::time::Instant::now();
    let s = |n: u64| t0 + std::time::Duration::from_secs(n);
    let mut g = StereoGate::new();
    assert!(!g.update(0, s(0)), "unknown rate: stay stereo");
    assert!(!g.update(STEREO_MIN_LINK_RATE, s(0)));
    assert!(g.update(STEREO_MIN_LINK_RATE - 1, s(1)), "thin: squeeze");
    assert!(g.update(0, s(2)), "unknown rate holds the squeeze");
    // Between the thresholds: stays squeezed.
    assert!(g.update(STEREO_MIN_LINK_RATE + 1, s(3)));
    // Good, but not for long enough; a dip restarts the hold.
    assert!(g.update(STEREO_RESUME_LINK_RATE, s(4)));
    assert!(g.update(STEREO_RESUME_LINK_RATE, s(8)));
    assert!(g.update(STEREO_MIN_LINK_RATE, s(9)));
    assert!(g.update(STEREO_RESUME_LINK_RATE, s(10)));
    assert!(g.update(STEREO_RESUME_LINK_RATE * 4, s(14)));
    assert!(
        !g.update(STEREO_RESUME_LINK_RATE, s(15)),
        "held 5 s: stereo"
    );
    assert!(!g.squeezed());
}

#[test]
fn stereo_jitter_buffer_keeps_audio_time_and_frames() {
    let mut jb = JitterBuffer::with_ms_channels(10, 20, 2);
    assert_eq!(jb.channels(), 2);
    assert_eq!(jb.target(), 960, "10 ms of stereo = 480 frames");
    jb.push(&vec![1; 960]);
    assert_eq!(jb.depth_ms(), 10);
    // Past the ceiling the trim keeps whole frames: L stays on even slots.
    let frames: Vec<i16> = (0..1500).flat_map(|_| [1i16, -1]).collect();
    jb.push(&frames);
    assert_eq!(jb.len() % 2, 0);
    let mut out = vec![0i16; 4];
    assert!(jb.pop(&mut out));
    assert_eq!(out, [1, -1, 1, -1]);
}

/// Mono fallback: a stereo source down-mixes for a peer without
/// CAP_STEREO (Opus) and for one without Opus (raw PCM, always mono).
#[test]
fn stereo_source_falls_back_to_mono() {
    use crate::terminal::{CAP_AUDIO10, CAP_OPUS};
    let (mut l, mut r) = (SineGen::new(440.0, 8000), SineGen::new(660.0, 8000));
    let tick = stereo_tick(&mut l, &mut r, 480);
    let mut raw = AudioEncoder::with_channels(2);
    let p = raw.encode_ticks(&tick, 0);
    assert_eq!(p.len(), 1);
    assert_eq!(raw.mode(), AudioMode::Pcm);
    assert_eq!(
        decode_audio_frame(&p[0]).unwrap().1,
        downmix_stereo(&tick),
        "raw PCM is the mono down-mix"
    );
    if !cfg!(feature = "opus") {
        return;
    }
    let mut enc = AudioEncoder::with_channels(2);
    let ps = enc.encode_ticks(&tick, CAP_OPUS | CAP_AUDIO10);
    assert_eq!(ps.len(), 1);
    assert_eq!(enc.mode(), AudioMode::OpusMono);
    let toc = ps[0][HEADER_LEN];
    assert_eq!(toc & 0x04, 0, "mono packet (TOC s bit clear)");
    assert_eq!(
        u32::from_be_bytes([ps[0][2], ps[0][3], ps[0][4], ps[0][5]]),
        480
    );
    // A mono source never goes stereo, CAP_STEREO or not.
    let mut mono = AudioEncoder::new();
    let ps = mono.encode_ticks(
        &tick[..480],
        CAP_OPUS | CAP_AUDIO10 | crate::terminal::CAP_STEREO,
    );
    assert_eq!(ps[0][HEADER_LEN] & 0x04, 0);
    assert_eq!(mono.mode(), AudioMode::OpusMono);
}

/// CAP negotiation: CAP_STEREO comes with the Opus decode caps.
#[test]
fn audio_caps_advertise_stereo_with_opus() {
    use crate::terminal::{CAP_AUDIO10, CAP_OPUS, CAP_STEREO};
    if cfg!(feature = "opus") {
        assert_eq!(audio_caps(), CAP_OPUS | CAP_AUDIO10 | CAP_STEREO);
    } else {
        assert_eq!(audio_caps(), 0);
    }
}

/// Stereo Opus round trip (10 ms, FEC on): 2-channel packets at ~128 kbps
/// whose channels stay apart after decoding; a mono decoder down-mixes the
/// same packets; on a thin link the stream is squeezed to mono / 64 kbps
/// without changing the decoder's layout.
#[cfg(feature = "opus")]
#[test]
fn opus_stereo_roundtrip_keeps_channels_apart() {
    use crate::terminal::{CAP_AUDIO10, CAP_OPUS, CAP_STEREO};
    let caps = CAP_OPUS | CAP_AUDIO10 | CAP_STEREO;
    let (mut l, mut r) = (SineGen::new(440.0, 8000), SineGen::new(660.0, 1000));
    let mut enc = AudioEncoder::with_channels(2);
    let mut dec = AudioDecoder::with_channels(2);
    let mut mono_dec = AudioDecoder::new();
    let (mut el, mut er, mut in_l, mut in_r) = (0f64, 0f64, 0f64, 0f64);
    for i in 0..100 {
        let tick = stereo_tick(&mut l, &mut r, 480);
        let ps = enc.encode_ticks(&tick, caps);
        assert_eq!(ps.len(), 1);
        let p = &ps[0];
        assert_eq!(p[0], CODEC_OPUS);
        assert_eq!(u32::from_be_bytes([p[2], p[3], p[4], p[5]]), 480);
        assert_ne!(p[HEADER_LEN] & 0x04, 0, "stereo packet (TOC s bit)");
        let out = dec.decode(p).unwrap();
        assert_eq!(out.len(), 960, "480 interleaved stereo frames");
        assert_eq!(mono_dec.decode(p).unwrap().len(), 480, "mono down-mix");
        if i >= 10 {
            in_l += energy(&tick, 0, 2);
            in_r += energy(&tick, 1, 2);
            el += energy(&out, 0, 2);
            er += energy(&out, 1, 2);
        }
    }
    assert_eq!(enc.mode(), AudioMode::OpusStereo);
    // Left is 8x louder than right (~18 dB) and stays so.
    let (ratio_in, ratio_out) = (in_l / in_r, el / er);
    assert!(
        ratio_out > ratio_in / 4.0 && ratio_out < ratio_in * 4.0,
        "L/R energy {ratio_out:.1} vs {ratio_in:.1}"
    );
    let kbps = enc.bitrate() / 1000.0;
    assert!(kbps > 64.0 && kbps < 200.0, "stereo bitrate {kbps:.1} kbps");
    // FEC / PLC produce stereo frames.
    let tick = stereo_tick(&mut l, &mut r, 480);
    let next = enc.encode_ticks(&tick, caps).remove(0);
    assert_eq!(dec.decode_fec(&next, 480).unwrap().len(), 960);
    assert_eq!(dec.conceal(480).unwrap().len(), 960);

    // Thin link: squeezed to mono-coded packets at ~64 kbps, still decoded
    // as 2 channels (L = R).
    let now = std::time::Instant::now();
    assert!(enc.set_link_rate(STEREO_MIN_LINK_RATE / 2, now));
    let (bytes0, samples0) = (enc.bytes, enc.samples);
    let mut last = Vec::new();
    let mut stereo_coded = Vec::new();
    for i in 0..100 {
        let tick = stereo_tick(&mut l, &mut r, 480);
        let p = enc.encode_ticks(&tick, caps).remove(0);
        if p[HEADER_LEN] & 0x04 != 0 {
            stereo_coded.push(i);
        }
        last = dec.decode(&p).unwrap();
        assert_eq!(last.len(), 960);
    }
    // libopus codes one transition packet in stereo, then mono.
    assert!(stereo_coded.iter().all(|&i| i < 3), "{stereo_coded:?}");
    assert_eq!(enc.mode(), AudioMode::OpusSqueezed);
    assert!(last.chunks(2).all(|f| f[0] == f[1]), "mono plays as L = R");
    let kbps = (enc.bytes - bytes0) as f64 * 8.0
        / ((enc.samples - samples0) as f64 / f64::from(SAMPLE_RATE))
        / 1000.0;
    assert!(kbps < 90.0, "squeezed bitrate {kbps:.1} kbps");
}

/// A stereo decoder plays old mono streams (Opus and raw PCM) as L = R.
#[cfg(feature = "opus")]
#[test]
fn stereo_decoder_upmixes_mono_streams() {
    let mut enc = AudioEncoder::new();
    let mut dec = AudioDecoder::with_channels(2);
    let mut g = SineGen::new(440.0, 8000);
    let mut tick = vec![0i16; 480];
    for _ in 0..5 {
        g.tick(&mut tick);
        let p = enc.encode(&tick, crate::terminal::CAP_OPUS);
        let out = dec.decode(&p).unwrap();
        assert_eq!(out.len(), 960);
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
    }
    let raw = encode_audio_frame(&[5, -6], FLAG_NONE);
    assert_eq!(dec.decode(&raw).unwrap(), vec![5, 5, -6, -6]);
    assert_eq!(dec.decode_fec(&raw, 480).unwrap().len(), 960);
}
