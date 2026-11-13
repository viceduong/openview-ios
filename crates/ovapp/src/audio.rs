//! `ovapp::audio` — the audio media payload (v0.21).
//!
//! Audio is a **new media channel parallel to video**: one AEAD record per
//! 20 ms "tick" of microphone audio, flowing host → viewer on the same
//! command channel as everything else. The wire format is deliberately
//! trivial and self-describing, mirroring the [`crate::record`] container
//! style:
//!
//! ```text
//! +--------+--------+----------------+---------------------------+
//! | codec  | flags  | samples (u32BE)| s16le PCM samples         |
//! | (1 B)  | (1 B)  | (4 B)          | (2 * samples bytes)       |
//! +--------+--------+----------------+---------------------------+
//! ```
//!
//! Codec `0` is raw little-endian `i16` PCM at 48 kHz mono. Codec `1`
//! (Phase 4) is one Opus packet (20 ms, 48 kHz mono) in place of the PCM
//! tail; `samples` then carries the decoded sample count (960). A sender
//! only emits Opus to a peer that advertised [`crate::terminal::CAP_OPUS`],
//! so an old peer keeps receiving raw PCM (768 kbps; Opus is ~48 kbps).
//!
//! Everything in the wire/jitter part is pure: no platform calls, no panics
//! — malformed input is an `Err`, never a crash, so a hostile relay cannot
//! take the viewer down. The Opus encoder/decoder itself is the C libopus
//! (via the `opus` crate) behind the non-default `opus` feature, which only
//! the desktop binaries enable: the iOS staticlib stays pure Rust and hands
//! Opus packets to Swift (AVAudioConverter) instead.

/// Capture/playback sample rate (Hz).
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per audio tick (20 ms at 48 kHz).
pub const TICK_SAMPLES: u32 = 960;
/// Tick duration in milliseconds.
pub const TICK_MS: u32 = 20;

/// Payload codec tag: raw s16le PCM at [`SAMPLE_RATE`], mono.
pub const CODEC_RAW_S16LE: u8 = 0;

/// Payload codec tag: one Opus packet (Phase 4; [`crate::terminal::CAP_OPUS`]).
pub const CODEC_OPUS: u8 = 1;

/// Opus target bitrate (bits/s, VBR): transparent for speech, fine for
/// system sounds — 16x less than raw s16le (768 kbps).
pub const OPUS_BITRATE: i32 = 48_000;

/// Wave 1: samples of a 10 ms Opus packet (sent to a peer with
/// [`crate::terminal::CAP_AUDIO10`]; 20 ms packets otherwise).
pub const OPUS10_SAMPLES: u32 = 480;

/// Wave 1: expected packet loss the Opus encoder provisions in-band FEC
/// for (LBRR). Our records travel reliably today, so FEC only costs a few
/// kbit/s; it pays off once audio rides an unreliable datagram path (a
/// receiver then decodes the lost frame from the next packet with
/// [`AudioDecoder::decode_fec`]).
pub const OPUS_FEC_LOSS_PERC: i32 = 10;

/// Largest Opus packet we accept (RFC 6716: 1275 bytes per frame).
pub const MAX_OPUS_PACKET: usize = 1275;

/// Byte length of the audio frame header (codec + flags + sample count).
pub const HEADER_LEN: usize = 6;

/// Audio frame flag bits (v0.21: none defined yet; reserved, must be 0).
pub const FLAG_NONE: u8 = 0;

/// Encode one audio tick into its wire payload.
///
/// `samples` are raw mono `i16` PCM at [`SAMPLE_RATE`]; the payload carries
/// them little-endian after the 6-byte header.
pub fn encode_audio_frame(samples: &[i16], flags: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + samples.len() * 2);
    out.push(CODEC_RAW_S16LE);
    out.push(flags);
    out.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Wrap one Opus packet (encoding `samples` PCM samples, i.e. 960 for a
/// 20 ms tick) as a codec-1 AUDIO payload.
pub fn encode_opus_frame(packet: &[u8], samples: u32, flags: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + packet.len());
    out.push(CODEC_OPUS);
    out.push(flags);
    out.extend_from_slice(&samples.to_be_bytes());
    out.extend_from_slice(packet);
    out
}

/// One parsed AUDIO payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPayload<'a> {
    /// Raw s16le PCM (codec 0), already decoded.
    Pcm(Vec<i16>),
    /// One Opus packet (codec 1) that decodes to `samples` samples.
    Opus { samples: u32, packet: &'a [u8] },
}

/// Parse an AUDIO payload of either codec. Validates the header (known
/// codec, no unknown flags, consistent length; an Opus packet must be
/// non-empty, at most [`MAX_OPUS_PACKET`] bytes and decode to at most one
/// 120 ms frame) — anything else is an `Err`, never a panic.
pub fn parse_audio_frame(payload: &[u8]) -> Result<(u8, AudioPayload<'_>), String> {
    if payload.len() < HEADER_LEN {
        return Err(format!(
            "AUDIO payload too short: {} bytes, want >= {HEADER_LEN}",
            payload.len()
        ));
    }
    let codec = payload[0];
    let flags = payload[1];
    if flags & !FLAG_NONE != 0 {
        return Err(format!("AUDIO has unknown flag bits {flags:#04x}"));
    }
    let count = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
    let body = &payload[HEADER_LEN..];
    match codec {
        CODEC_RAW_S16LE => {
            let want = HEADER_LEN as u64 + u64::from(count) * 2;
            if payload.len() as u64 != want {
                return Err(format!(
                    "AUDIO payload is {} bytes, want {want} for {count} samples",
                    payload.len()
                ));
            }
            let samples = body
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            Ok((flags, AudioPayload::Pcm(samples)))
        }
        CODEC_OPUS => {
            if body.is_empty() || body.len() > MAX_OPUS_PACKET {
                return Err(format!("AUDIO Opus packet has a bad length {}", body.len()));
            }
            // 120 ms is the longest Opus packet; a tick is 20 ms.
            if count == 0 || count > SAMPLE_RATE / 1000 * 120 {
                return Err(format!("AUDIO Opus frame claims {count} samples"));
            }
            Ok((
                flags,
                AudioPayload::Opus {
                    samples: count,
                    packet: body,
                },
            ))
        }
        _ => Err(format!("AUDIO has an unknown codec tag {codec:#04x}")),
    }
}

/// Decode one raw-PCM audio tick from its wire payload.
///
/// Returns the flags and the `i16` samples. The codec tag must be
/// [`CODEC_RAW_S16LE`] (an Opus payload needs an [`AudioDecoder`]), flags
/// must carry no unknown bits, and the payload must be exactly header +
/// `2 * samples` bytes — anything else is an `Err`, never a panic.
pub fn decode_audio_frame(payload: &[u8]) -> Result<(u8, Vec<i16>), String> {
    match parse_audio_frame(payload)? {
        (flags, AudioPayload::Pcm(samples)) => Ok((flags, samples)),
        (_, AudioPayload::Opus { .. }) => {
            Err("AUDIO is Opus; decode it with an AudioDecoder".into())
        }
    }
}

/// The CAPS bits this build's audio path can decode: [`crate::terminal::
/// CAP_OPUS`] with the `opus` feature, nothing otherwise.
pub fn audio_caps() -> u32 {
    if cfg!(feature = "opus") {
        // Wave 1: our decoder takes any Opus frame size (10 ms included).
        crate::terminal::CAP_OPUS | crate::terminal::CAP_AUDIO10
    } else {
        0
    }
}

/// Sender-side audio encoder: wraps each 20 ms PCM tick as an AUDIO payload,
/// as Opus when the build has the `opus` feature **and** the peer advertised
/// [`crate::terminal::CAP_OPUS`], raw PCM otherwise (old peers, iOS mic).
pub struct AudioEncoder {
    #[cfg(feature = "opus")]
    opus: Option<opus::Encoder>,
    #[cfg(feature = "opus")]
    out: Vec<u8>,
    /// AUDIO payload bytes produced (for the bitrate log line).
    pub bytes: u64,
    /// Ticks encoded.
    pub ticks: u64,
    /// Wave 1: PCM samples encoded (bitrate with any packet size).
    pub samples: u64,
    /// Wave 1: PCM waiting for a complete Opus frame ([`encode_ticks`](Self::encode_ticks)).
    pend: Vec<i16>,
}

impl Default for AudioEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEncoder {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "opus")]
            opus: None,
            #[cfg(feature = "opus")]
            out: vec![0u8; MAX_OPUS_PACKET],
            bytes: 0,
            ticks: 0,
            samples: 0,
            pend: Vec::new(),
        }
    }

    /// Wave 1: encode captured PCM of any tick size for a peer with
    /// `peer_caps`, as zero or more AUDIO payloads: 10 ms Opus packets for
    /// a [`crate::terminal::CAP_AUDIO10`] peer, 20 ms Opus packets for any
    /// other Opus peer (PCM is buffered until a frame is complete), and one
    /// raw-PCM payload per call otherwise.
    pub fn encode_ticks(&mut self, pcm: &[i16], peer_caps: u32) -> Vec<Vec<u8>> {
        use crate::terminal::{CAP_AUDIO10, CAP_OPUS};
        let mut out = Vec::new();
        if !(cfg!(feature = "opus") && peer_caps & CAP_OPUS != 0) {
            let mut all = std::mem::take(&mut self.pend);
            all.extend_from_slice(pcm);
            if !all.is_empty() {
                let p = encode_audio_frame(&all, FLAG_NONE);
                self.account(&p, all.len());
                out.push(p);
            }
            return out;
        }
        let frame = if peer_caps & CAP_AUDIO10 != 0 {
            OPUS10_SAMPLES
        } else {
            TICK_SAMPLES
        } as usize;
        self.pend.extend_from_slice(pcm);
        let mut at = 0;
        while self.pend.len() - at >= frame {
            let chunk = self.pend[at..at + frame].to_vec();
            at += frame;
            let p = self
                .try_opus(&chunk, peer_caps)
                .unwrap_or_else(|| encode_audio_frame(&chunk, FLAG_NONE));
            self.account(&p, frame);
            out.push(p);
        }
        self.pend.drain(..at);
        out
    }

    fn account(&mut self, payload: &[u8], samples: usize) {
        self.bytes += payload.len() as u64;
        self.ticks += 1;
        self.samples += samples as u64;
    }

    /// Encode one tick of `pcm` for a peer with `peer_caps`.
    pub fn encode(&mut self, pcm: &[i16], peer_caps: u32) -> Vec<u8> {
        let payload = self
            .try_opus(pcm, peer_caps)
            .unwrap_or_else(|| encode_audio_frame(pcm, FLAG_NONE));
        self.account(&payload, pcm.len());
        payload
    }

    /// Average AUDIO payload bitrate so far (bits/s).
    pub fn bitrate(&self) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        self.bytes as f64 * 8.0 / (self.samples as f64 / f64::from(SAMPLE_RATE))
    }

    #[cfg(feature = "opus")]
    fn try_opus(&mut self, pcm: &[i16], peer_caps: u32) -> Option<Vec<u8>> {
        let n = pcm.len() as u32;
        if peer_caps & crate::terminal::CAP_OPUS == 0 || (n != TICK_SAMPLES && n != OPUS10_SAMPLES)
        {
            return None;
        }
        if self.opus.is_none() {
            let mut enc =
                opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Audio)
                    .ok()?;
            let _ = enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE));
            let _ = enc.set_vbr(true);
            // Wave 1: in-band FEC (LBRR) for the next packet to carry a
            // low-rate copy of this one.
            let _ = enc.set_inband_fec(true);
            let _ = enc.set_packet_loss_perc(OPUS_FEC_LOSS_PERC);
            self.opus = Some(enc);
        }
        let enc = self.opus.as_mut()?;
        let n = enc.encode(pcm, &mut self.out).ok()?;
        (n > 0).then(|| encode_opus_frame(&self.out[..n], pcm.len() as u32, FLAG_NONE))
    }

    #[cfg(not(feature = "opus"))]
    fn try_opus(&mut self, _pcm: &[i16], _peer_caps: u32) -> Option<Vec<u8>> {
        None
    }
}

/// Receiver-side audio decoder: raw PCM always, Opus with the `opus`
/// feature (one decoder state per stream, created on the first packet).
#[derive(Default)]
pub struct AudioDecoder {
    #[cfg(feature = "opus")]
    opus: Option<opus::Decoder>,
}

impl AudioDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one AUDIO payload to PCM. Malformed input (or Opus without
    /// the `opus` feature) is an `Err`, never a panic.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, String> {
        match parse_audio_frame(payload)?.1 {
            AudioPayload::Pcm(samples) => Ok(samples),
            AudioPayload::Opus { samples, packet } => self.decode_opus(samples, packet),
        }
    }

    #[cfg(feature = "opus")]
    fn decode_opus(&mut self, samples: u32, packet: &[u8]) -> Result<Vec<i16>, String> {
        if self.opus.is_none() {
            self.opus = Some(
                opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
                    .map_err(|e| format!("opus decoder: {e}"))?,
            );
        }
        let dec = self.opus.as_mut().ok_or("opus decoder missing")?;
        let mut out = vec![0i16; samples as usize];
        let n = dec
            .decode(packet, &mut out, false)
            .map_err(|e| format!("opus decode: {e}"))?;
        out.truncate(n);
        Ok(out)
    }

    #[cfg(not(feature = "opus"))]
    fn decode_opus(&mut self, _samples: u32, _packet: &[u8]) -> Result<Vec<i16>, String> {
        Err("AUDIO is Opus but this build has no Opus decoder".into())
    }

    /// Wave 1: recover a lost packet of `lost_samples` samples from the
    /// in-band FEC of the packet that followed it (`next` = that packet's
    /// AUDIO payload; decode `next` itself normally afterwards). Without
    /// FEC data Opus conceals (PLC). Raw PCM / no `opus`: silence.
    pub fn decode_fec(&mut self, next: &[u8], lost_samples: u32) -> Result<Vec<i16>, String> {
        let lost = lost_samples.min(5760) as usize;
        match parse_audio_frame(next)?.1 {
            AudioPayload::Pcm(_) => Ok(vec![0; lost]),
            AudioPayload::Opus { packet, .. } => self.fec_opus(packet, lost),
        }
    }

    /// Wave 3: conceal one lost packet of `lost_samples` samples with no
    /// later packet to recover it from (Opus packet-loss concealment; raw
    /// PCM / no `opus`: silence).
    pub fn conceal(&mut self, lost_samples: u32) -> Result<Vec<i16>, String> {
        let lost = lost_samples.min(5760) as usize;
        self.plc_opus(lost)
    }

    #[cfg(feature = "opus")]
    fn plc_opus(&mut self, lost: usize) -> Result<Vec<i16>, String> {
        let Some(dec) = self.opus.as_mut() else {
            return Ok(vec![0; lost]); // nothing decoded yet: silence
        };
        let mut out = vec![0i16; lost];
        let n = dec
            .decode(&[], &mut out, false)
            .map_err(|e| format!("opus PLC: {e}"))?;
        out.truncate(n);
        Ok(out)
    }

    #[cfg(not(feature = "opus"))]
    fn plc_opus(&mut self, lost: usize) -> Result<Vec<i16>, String> {
        Ok(vec![0; lost])
    }

    #[cfg(feature = "opus")]
    fn fec_opus(&mut self, packet: &[u8], lost: usize) -> Result<Vec<i16>, String> {
        if self.opus.is_none() {
            self.opus = Some(
                opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
                    .map_err(|e| format!("opus decoder: {e}"))?,
            );
        }
        let dec = self.opus.as_mut().ok_or("opus decoder missing")?;
        let mut out = vec![0i16; lost];
        let n = dec
            .decode(packet, &mut out, true)
            .map_err(|e| format!("opus FEC decode: {e}"))?;
        out.truncate(n);
        Ok(out)
    }

    #[cfg(not(feature = "opus"))]
    fn fec_opus(&mut self, _packet: &[u8], lost: usize) -> Result<Vec<i16>, String> {
        Ok(vec![0; lost])
    }
}

/// A fixed-capacity FIFO of audio samples with **zero-fill on underrun**.
///
/// This is the viewer-side jitter buffer: the speaker callback pops a fixed
/// tick regardless of how much arrived, and a starved pop returns silence
/// and counts the underrun so the UI can show audio health. Pushes beyond
/// capacity drop the *oldest* samples (a live stream is worth more than a
/// buffered one).
pub struct AudioRingBuffer {
    buf: std::collections::VecDeque<i16>,
    capacity: usize,
    underruns: u64,
}

impl AudioRingBuffer {
    /// A buffer holding `capacity` samples (~`capacity / 48000` seconds).
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            underruns: 0,
        }
    }

    /// Push samples, dropping the oldest when full.
    pub fn push(&mut self, samples: &[i16]) {
        for &s in samples {
            if self.buf.len() == self.capacity {
                self.buf.pop_front();
            }
            self.buf.push_back(s);
        }
    }

    /// Pop exactly `out.len()` samples, zero-filling (and counting) any
    /// shortfall so playback timing never slips.
    pub fn pop(&mut self, out: &mut [i16]) {
        let have = self.buf.len().min(out.len());
        for (o, s) in out.iter_mut().zip(self.buf.drain(..have)) {
            *o = s;
        }
        if have < out.len() {
            out[have..].fill(0);
            self.underruns += 1;
        }
    }

    /// Samples currently buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the buffer holds no samples.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// How many pops have been starved since creation.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }
}

/// Playback jitter buffer (Phase 4): absorbs network jitter between the
/// sender's capture clock and the local device clock with bounded latency.
///
/// - **Prefill**: after start (and after every underrun) it outputs silence
///   until [`target`](Self::target) samples are buffered, so playback
///   starts with a cushion instead of stuttering tick by tick.
/// - **Bounded depth**: when a burst pushes the depth past
///   [`max`](Self::max), the oldest audio is dropped back down to the
///   target — latency can never ratchet up behind a stall.
/// - **Underrun**: a pop that cannot be fully served plays what is there,
///   zero-fills the rest, counts it, and re-enters prefill.
pub struct JitterBuffer {
    buf: std::collections::VecDeque<i16>,
    target: usize,
    max: usize,
    primed: bool,
    underruns: u64,
    dropped: u64,
}

/// Default jitter-buffer target depth (60 ms).
pub const JITTER_TARGET_MS: u32 = 60;
/// Default jitter-buffer ceiling (120 ms): beyond it the oldest is dropped.
pub const JITTER_MAX_MS: u32 = 120;

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::with_ms(JITTER_TARGET_MS, JITTER_MAX_MS)
    }
}

impl JitterBuffer {
    /// A buffer with `target_ms` prefill and a `max_ms` ceiling.
    pub fn with_ms(target_ms: u32, max_ms: u32) -> Self {
        let per_ms = (SAMPLE_RATE / 1000) as usize;
        let target = (target_ms as usize * per_ms).max(1);
        let max = (max_ms as usize * per_ms).max(target);
        Self {
            buf: std::collections::VecDeque::with_capacity(max + TICK_SAMPLES as usize),
            target,
            max,
            primed: false,
            underruns: 0,
            dropped: 0,
        }
    }

    /// Append received samples; trims back to the target past the ceiling.
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
        if self.buf.len() > self.max {
            let drop = self.buf.len() - self.target;
            self.buf.drain(..drop);
            self.dropped += drop as u64;
        }
    }

    /// Fill `out` with the next samples (silence while prefilling; partial
    /// + zero-fill on underrun). Returns `true` when real audio was played.
    pub fn pop(&mut self, out: &mut [i16]) -> bool {
        if !self.primed {
            if self.buf.len() < self.target {
                out.fill(0);
                return false;
            }
            self.primed = true;
        }
        let have = self.buf.len().min(out.len());
        for (o, s) in out.iter_mut().zip(self.buf.drain(..have)) {
            *o = s;
        }
        if have < out.len() {
            out[have..].fill(0);
            self.underruns += 1;
            self.primed = false;
        }
        have > 0
    }

    /// Discard everything and re-enter prefill (playback toggled off).
    pub fn clear(&mut self) {
        self.buf.clear();
        self.primed = false;
    }

    /// Samples buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Buffered depth in milliseconds.
    pub fn depth_ms(&self) -> u32 {
        (self.buf.len() as u64 * 1000 / u64::from(SAMPLE_RATE)) as u32
    }

    /// Prefill target (samples).
    pub fn target(&self) -> usize {
        self.target
    }

    /// Ceiling (samples).
    pub fn max(&self) -> usize {
        self.max
    }

    /// Pops that ran dry after playback had started.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Samples dropped by the ceiling.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Shared playback bus: the command hook pushes decoded ticks into the
/// [`JitterBuffer`], the device thread pops one tick per free device buffer.
/// Disabled (the default) it drops pushes, so a muted/off speaker never
/// accumulates stale audio.
#[derive(Clone)]
pub struct AudioBus {
    buf: std::sync::Arc<std::sync::Mutex<JitterBuffer>>,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for AudioBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBus {
    /// A disabled bus with the default 60 ms / 120 ms jitter buffer.
    pub fn new() -> Self {
        Self::with_ms(JITTER_TARGET_MS, JITTER_MAX_MS)
    }

    /// Wave 1: a disabled bus with a `target_ms` prefill / `max_ms`
    /// ceiling jitter buffer (the desktop viewer uses 30 / 90 ms).
    pub fn with_ms(target_ms: u32, max_ms: u32) -> Self {
        Self {
            buf: std::sync::Arc::new(std::sync::Mutex::new(JitterBuffer::with_ms(
                target_ms, max_ms,
            ))),
            enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Enable/disable playback; disabling discards buffered audio.
    pub fn set_enabled(&self, on: bool) {
        let was = self.enabled.swap(on, std::sync::atomic::Ordering::Relaxed);
        if was && !on {
            if let Ok(mut b) = self.buf.lock() {
                b.clear();
            }
        }
    }

    /// Whether playback is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Push decoded samples (called from the command hook); dropped while
    /// disabled.
    pub fn push(&self, samples: &[i16]) {
        if self.is_enabled() {
            if let Ok(mut b) = self.buf.lock() {
                b.push(samples);
            }
        }
    }

    /// Pop one device buffer's worth (see [`JitterBuffer::pop`]).
    pub fn pop_tick(&self, out: &mut [i16]) -> bool {
        match self.buf.lock() {
            Ok(mut b) => b.pop(out),
            Err(_) => {
                out.fill(0);
                false
            }
        }
    }

    /// `(depth_ms, underruns, dropped_samples)` for logs / the HUD.
    pub fn status(&self) -> (u32, u64, u64) {
        self.buf
            .lock()
            .map(|b| (b.depth_ms(), b.underruns(), b.dropped()))
            .unwrap_or_default()
    }
}

/// Drift-free tick clock for a clock-paced audio source (the synthetic
/// mic): deadlines advance by exactly one interval (`next += interval`), so
/// timer jitter never accumulates into a lower tick rate. Only after a stall
/// longer than ten intervals does it resynchronise to "now" (dropping the
/// stale backlog instead of bursting it).
#[derive(Debug, Clone)]
pub struct TickClock {
    interval: std::time::Duration,
    next: std::time::Instant,
    max_behind: std::time::Duration,
}

impl TickClock {
    /// A clock whose first tick is due at `now`.
    pub fn new(interval: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            interval,
            next: now,
            max_behind: interval * 10,
        }
    }

    /// Time until the next tick (zero when due).
    pub fn until_due(&self, now: std::time::Instant) -> std::time::Duration {
        self.next.saturating_duration_since(now)
    }

    /// Whether a tick is due at `now`; if so, schedules the next one exactly
    /// one interval later (catching up tick by tick after a short delay).
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        if now < self.next {
            return false;
        }
        if now.saturating_duration_since(self.next) > self.max_behind {
            self.next = now;
        }
        self.next += self.interval;
        true
    }
}

/// Where microphone ticks come from: fill `out` with the next 20 ms tick
/// ([`TICK_SAMPLES`] samples of mono s16le 48 kHz audio). A real device
/// blocks until the tick has been captured; a synthetic source returns
/// immediately and is paced by a [`TickClock`].
pub trait MicSource: Send {
    /// Read the next tick into `out` (always exactly [`TICK_SAMPLES`]).
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String>;
    /// `true` when `read_tick` paces itself (a capture device).
    fn self_paced(&self) -> bool {
        false
    }
    /// Wave 1: samples per tick this source delivers ([`TICK_SAMPLES`] =
    /// 20 ms by default; the system-audio loopback uses 10 ms).
    fn tick_samples(&self) -> usize {
        TICK_SAMPLES as usize
    }
}

/// A synthetic microphone: a 440 Hz sine at moderate amplitude. Lets the
/// full audio path run on machines with no mic (and gives tests a
/// deterministic signal).
pub struct SynthMic {
    gen: SineGen,
}

impl SynthMic {
    pub fn new() -> Self {
        Self {
            gen: SineGen::new(440.0, 8_000),
        }
    }
}

impl Default for SynthMic {
    fn default() -> Self {
        Self::new()
    }
}

impl MicSource for SynthMic {
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String> {
        self.gen.tick(out);
        Ok(())
    }
}

/// Run a mic capture loop on the current thread until `stop` (or until
/// `deliver` returns `false`): every captured tick is handed to `deliver`
/// in order. A self-paced device is read back to back (it blocks per tick,
/// and its buffer ring absorbs scheduling hiccups — nothing is dropped); a
/// synthetic source is paced by a drift-free [`TickClock`].
pub fn run_mic_loop(
    src: &mut dyn MicSource,
    stop: &std::sync::atomic::AtomicBool,
    mut deliver: impl FnMut(Vec<i16>) -> bool,
) {
    use std::sync::atomic::Ordering;
    let tick = std::time::Duration::from_micros(
        src.tick_samples() as u64 * 1_000_000 / u64::from(SAMPLE_RATE),
    );
    let mut clock = TickClock::new(tick, std::time::Instant::now());
    // A failing device fails every tick: warn once, then only at debug.
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        if !src.self_paced() {
            let now = std::time::Instant::now();
            if !clock.tick(now) {
                std::thread::sleep(clock.until_due(now));
                continue;
            }
        }
        let mut buf = vec![0i16; src.tick_samples()];
        match src.read_tick(&mut buf) {
            Ok(()) => {
                if !deliver(buf) {
                    return;
                }
            }
            Err(e) => {
                if warned {
                    tracing::debug!("mic read failed: {e}");
                } else {
                    tracing::warn!("mic read failed: {e}");
                    warned = true;
                }
                std::thread::sleep(tick);
            }
        }
    }
}

/// A pure sine-wave generator standing in for a microphone.
///
/// Used by the synthetic mic source (tests, `--mic-synth`) so the whole
/// audio path is exercisable without any audio hardware.
pub struct SineGen {
    freq_hz: f32,
    amplitude: i16,
    phase: f32,
}

impl SineGen {
    /// A generator at `freq_hz` with peak amplitude `amplitude`.
    pub fn new(freq_hz: f32, amplitude: i16) -> Self {
        Self {
            freq_hz,
            amplitude,
            phase: 0.0,
        }
    }

    /// Produce the next `out.len()` samples, advancing the phase.
    pub fn tick(&mut self, out: &mut [i16]) {
        let step = 2.0 * std::f32::consts::PI * self.freq_hz / SAMPLE_RATE as f32;
        for slot in out.iter_mut() {
            *slot = (self.phase.sin() * self.amplitude as f32) as i16;
            self.phase += step;
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert!(
            parse_audio_frame(&encode_opus_frame(&vec![0; MAX_OPUS_PACKET + 1], 960, 0)).is_err()
        );
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

    /// Wave 1: 10 ms Opus packets (with FEC) for a CAP_AUDIO10 peer, 20 ms
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
}
