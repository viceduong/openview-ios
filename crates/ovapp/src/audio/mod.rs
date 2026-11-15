//! `ovapp::audio` — the audio media payload.
//!
//! Audio is a **media channel parallel to video**: one AEAD record per
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
//! Codec `0` is raw little-endian `i16` PCM at 48 kHz mono. Codec `1` is
//! one Opus packet (20 ms, or 10 ms for a [`crate::terminal::CAP_AUDIO10`]
//! peer; 48 kHz mono) in place of the PCM tail; `samples` then carries the
//! decoded sample count (960 / 480). A sender only emits Opus to a peer that
//! advertised [`crate::terminal::CAP_OPUS`], so an older peer keeps
//! receiving raw PCM (768 kbps; Opus is ~48 kbps).
//!
//! **Stereo** ([`crate::terminal::CAP_STEREO`]): a stereo source (the host's
//! system-audio loopback) is sent as 2-channel Opus (128 kbps, still 10 ms
//! frames with FEC) to a peer that set `CAP_STEREO`; `samples` stays the
//! per-channel count and the packet's TOC carries the channel count, so the
//! header is unchanged. Every other peer gets the down-mix in mono, and a
//! thin link squeezes the stereo stream to mono / 64 kbps ([`StereoGate`]).
//! Raw PCM is always mono.
//!
//! Everything in the wire/jitter part is pure: no platform calls, no panics
//! — malformed input is an `Err`, never a crash, so a hostile relay cannot
//! take the viewer down. The Opus encoder/decoder itself is the C libopus
//! (via the `opus` crate) behind the non-default `opus` feature, which only
//! the desktop binaries enable: the iOS staticlib stays pure Rust and hands
//! Opus packets to Swift (AVAudioConverter) instead.
//!
//! Layout: the wire format and the codec wrappers live here; `buffer` holds
//! the playback side ([`AudioRingBuffer`], [`JitterBuffer`], [`AudioBus`]) and
//! `source` the capture side ([`TickClock`], [`MicSource`], [`SynthMic`],
//! [`run_mic_loop`], [`SineGen`]) and `stereo` the channel helpers and the
//! [`StereoGate`]; all are re-exported here.

mod buffer;
mod source;
mod stereo;
#[cfg(test)]
mod tests;

pub use buffer::*;
pub use source::*;
pub use stereo::*;

use crate::error::AppError;

/// Capture/playback sample rate (Hz).
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per audio tick (20 ms at 48 kHz).
pub const TICK_SAMPLES: u32 = 960;
/// Tick duration in milliseconds.
pub const TICK_MS: u32 = 20;

/// Payload codec tag: raw s16le PCM at [`SAMPLE_RATE`], mono.
pub const CODEC_RAW_S16LE: u8 = 0;

/// Payload codec tag: one Opus packet ([`crate::terminal::CAP_OPUS`]).
pub const CODEC_OPUS: u8 = 1;

/// Opus target bitrate (bits/s, VBR): transparent for speech, fine for
/// system sounds — 16x less than raw s16le (768 kbps).
pub const OPUS_BITRATE: i32 = 48_000;

/// Samples of a 10 ms Opus packet (sent to a peer with
/// [`crate::terminal::CAP_AUDIO10`]; 20 ms packets otherwise).
pub const OPUS10_SAMPLES: u32 = 480;

/// Expected packet loss the Opus encoder provisions in-band FEC
/// for (LBRR). On the reliable record path FEC only costs a few kbit/s; it
/// pays off when audio rides the unreliable media channel of the direct
/// path (the receiver then decodes a lost frame from the next packet with
/// [`AudioDecoder::decode_fec`]).
pub const OPUS_FEC_LOSS_PERC: i32 = 10;

/// Largest Opus packet we accept (RFC 6716: 1275 bytes per frame).
pub(crate) const MAX_OPUS_PACKET: usize = 1275;

/// Byte length of the audio frame header (codec + flags + sample count).
pub const HEADER_LEN: usize = 6;

/// Audio frame flag bits (none defined yet; reserved, must be 0).
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
    Opus {
        /// The decoded sample count the header claims.
        samples: u32,
        /// The Opus packet bytes (borrowed from the payload).
        packet: &'a [u8],
    },
}

/// Parse an AUDIO payload of either codec. Validates the header (known
/// codec, no unknown flags, consistent length; an Opus packet must be
/// non-empty, at most `MAX_OPUS_PACKET` (1275) bytes and decode to at most one
/// 120 ms frame) — anything else is an `Err`, never a panic.
pub fn parse_audio_frame(payload: &[u8]) -> Result<(u8, AudioPayload<'_>), AppError> {
    if payload.len() < HEADER_LEN {
        return Err(AppError::malformed(format!(
            "AUDIO payload too short: {} bytes, want >= {HEADER_LEN}",
            payload.len()
        )));
    }
    let codec = payload[0];
    let flags = payload[1];
    if flags & !FLAG_NONE != 0 {
        return Err(AppError::malformed(format!(
            "AUDIO has unknown flag bits {flags:#04x}"
        )));
    }
    let count = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
    let body = &payload[HEADER_LEN..];
    match codec {
        CODEC_RAW_S16LE => {
            let want = HEADER_LEN as u64 + u64::from(count) * 2;
            if payload.len() as u64 != want {
                return Err(AppError::malformed(format!(
                    "AUDIO payload is {} bytes, want {want} for {count} samples",
                    payload.len()
                )));
            }
            // The length check above makes `body` exactly `2 * count` bytes.
            let (pairs, _) = body.as_chunks::<2>();
            let samples = pairs.iter().map(|&c| i16::from_le_bytes(c)).collect();
            Ok((flags, AudioPayload::Pcm(samples)))
        }
        CODEC_OPUS => {
            if body.is_empty() || body.len() > MAX_OPUS_PACKET {
                return Err(AppError::malformed(format!(
                    "AUDIO Opus packet has a bad length {}",
                    body.len()
                )));
            }
            // 120 ms is the longest Opus packet; a tick is 20 ms.
            if count == 0 || count > SAMPLE_RATE / 1000 * 120 {
                return Err(AppError::malformed(format!(
                    "AUDIO Opus frame claims {count} samples"
                )));
            }
            Ok((
                flags,
                AudioPayload::Opus {
                    samples: count,
                    packet: body,
                },
            ))
        }
        _ => Err(AppError::malformed(format!(
            "AUDIO has an unknown codec tag {codec:#04x}"
        ))),
    }
}

/// Decode one raw-PCM audio tick from its wire payload.
///
/// Returns the flags and the `i16` samples. The codec tag must be
/// [`CODEC_RAW_S16LE`] (an Opus payload needs an [`AudioDecoder`]), flags
/// must carry no unknown bits, and the payload must be exactly header +
/// `2 * samples` bytes — anything else is an `Err`, never a panic.
pub fn decode_audio_frame(payload: &[u8]) -> Result<(u8, Vec<i16>), AppError> {
    match parse_audio_frame(payload)? {
        (flags, AudioPayload::Pcm(samples)) => Ok((flags, samples)),
        (_, AudioPayload::Opus { .. }) => Err(AppError::invalid(
            "AUDIO is Opus; decode it with an AudioDecoder",
        )),
    }
}

/// The CAPS bits this build's audio path can decode: [`crate::terminal::
/// CAP_OPUS`] with the `opus` feature, nothing otherwise.
pub(crate) fn audio_caps() -> u32 {
    if cfg!(feature = "opus") {
        // Our decoder takes any Opus frame size (10 ms included) and any
        // channel count (libopus down-/up-mixes to the decoder's layout).
        crate::terminal::CAP_OPUS | crate::terminal::CAP_AUDIO10 | crate::terminal::CAP_STEREO
    } else {
        0
    }
}

/// What the encoder sent last — for the bitrate log line / HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioMode {
    /// Nothing encoded yet.
    #[default]
    Idle,
    /// Raw s16le PCM, mono (a peer without `CAP_OPUS`).
    Pcm,
    /// Mono Opus (a mono source, or a peer without `CAP_STEREO`).
    OpusMono,
    /// Stereo Opus at [`OPUS_STEREO_BITRATE`].
    OpusStereo,
    /// A stereo stream squeezed to mono at [`OPUS_SQUEEZED_BITRATE`] by the
    /// [`StereoGate`] (thin link).
    OpusSqueezed,
}

impl AudioMode {
    /// A short human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pcm => "raw s16le",
            Self::OpusMono => "opus mono",
            Self::OpusStereo => "opus stereo",
            Self::OpusSqueezed => "opus stereo squeezed to mono",
        }
    }
}

impl std::fmt::Display for AudioMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Sender-side audio encoder: wraps captured PCM as AUDIO payloads, as Opus
/// when the build has the `opus` feature **and** the peer advertised
/// [`crate::terminal::CAP_OPUS`], raw mono PCM otherwise (old peers, iOS
/// mic). A stereo source ([`with_channels`](Self::with_channels)`(2)`) is
/// sent as stereo Opus only to a [`crate::terminal::CAP_STEREO`] peer, and
/// down-mixed to mono for everyone else.
pub struct AudioEncoder {
    /// Channels of the PCM handed in (1 = mono, 2 = interleaved stereo).
    channels: usize,
    #[cfg(feature = "opus")]
    opus: Option<OpusEnc>,
    #[cfg(feature = "opus")]
    out: Vec<u8>,
    /// Squeezes a stereo stream to mono on a thin link.
    gate: StereoGate,
    /// What the last payload was.
    mode: AudioMode,
    /// AUDIO payload bytes produced (for the bitrate log line).
    pub bytes: u64,
    /// Ticks encoded.
    pub ticks: u64,
    /// PCM samples (per channel) encoded (bitrate with any packet size).
    pub samples: u64,
    /// PCM (input layout) waiting for a complete Opus frame
    /// ([`encode_ticks`](Self::encode_ticks)).
    pend: Vec<i16>,
}

/// One libopus encoder state and how it is configured.
#[cfg(feature = "opus")]
struct OpusEnc {
    enc: opus::Encoder,
    channels: usize,
    squeezed: bool,
}

#[cfg(feature = "opus")]
impl OpusEnc {
    /// A VBR encoder with in-band FEC: mono at [`OPUS_BITRATE`], or stereo
    /// (forced, so every packet is 2-channel) at [`OPUS_STEREO_BITRATE`].
    fn new(channels: usize) -> Option<Self> {
        let layout = if channels == 2 {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };
        let mut enc = opus::Encoder::new(SAMPLE_RATE, layout, opus::Application::Audio).ok()?;
        let rate = if channels == 2 {
            let _ = enc.set_force_channels(Some(opus::Channels::Stereo));
            OPUS_STEREO_BITRATE
        } else {
            OPUS_BITRATE
        };
        let _ = enc.set_bitrate(opus::Bitrate::Bits(rate));
        let _ = enc.set_vbr(true);
        // In-band FEC (LBRR) for the next packet to carry a
        // low-rate copy of this one.
        let _ = enc.set_inband_fec(true);
        let _ = enc.set_packet_loss_perc(OPUS_FEC_LOSS_PERC);
        Some(Self {
            enc,
            channels,
            squeezed: false,
        })
    }

    /// Stereo only: code mono at [`OPUS_SQUEEZED_BITRATE`] (`true`) or
    /// stereo at [`OPUS_STEREO_BITRATE`]. The stream stays 2-channel (a
    /// mono-coded packet decodes to L = R), so receivers never reconfigure.
    fn set_squeezed(&mut self, squeezed: bool) {
        if self.channels != 2 || self.squeezed == squeezed {
            return;
        }
        self.squeezed = squeezed;
        let (force, rate) = if squeezed {
            (opus::Channels::Mono, OPUS_SQUEEZED_BITRATE)
        } else {
            (opus::Channels::Stereo, OPUS_STEREO_BITRATE)
        };
        let _ = self.enc.set_force_channels(Some(force));
        let _ = self.enc.set_bitrate(opus::Bitrate::Bits(rate));
    }
}

impl Default for AudioEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEncoder {
    /// An encoder for mono PCM that has sent nothing yet (Opus state
    /// created lazily).
    pub fn new() -> Self {
        Self::with_channels(1)
    }

    /// An encoder for `channels`-channel PCM (1 = mono, 2 = interleaved
    /// stereo; anything else is clamped into that range).
    pub fn with_channels(channels: usize) -> Self {
        Self {
            channels: channels.clamp(1, 2),
            #[cfg(feature = "opus")]
            opus: None,
            #[cfg(feature = "opus")]
            out: vec![0u8; MAX_OPUS_PACKET],
            gate: StereoGate::new(),
            mode: AudioMode::Idle,
            bytes: 0,
            ticks: 0,
            samples: 0,
            pend: Vec::new(),
        }
    }

    /// Channels of the PCM this encoder takes.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// What the last payload was.
    pub fn mode(&self) -> AudioMode {
        self.mode
    }

    /// Feed the transport's rate estimate (bytes/s, `0` = unknown): a
    /// stereo stream is squeezed to mono / 64 kbps on a thin link (see
    /// [`StereoGate`]). Returns whether it is squeezed now.
    pub fn set_link_rate(&mut self, bytes_per_sec: u64, now: std::time::Instant) -> bool {
        self.gate.update(bytes_per_sec, now)
    }

    /// Encode captured PCM of any tick size (in this encoder's channel
    /// layout) for a peer with `peer_caps`, as zero or more AUDIO payloads:
    /// 10 ms Opus packets for a [`crate::terminal::CAP_AUDIO10`] peer, 20 ms
    /// Opus packets for any other Opus peer (PCM is buffered until a frame
    /// is complete) — stereo for a [`crate::terminal::CAP_STEREO`] peer when
    /// the source is stereo, mono otherwise — and one raw mono PCM payload
    /// per call for a peer without Opus.
    pub fn encode_ticks(&mut self, pcm: &[i16], peer_caps: u32) -> Vec<Vec<u8>> {
        use crate::terminal::{CAP_AUDIO10, CAP_OPUS};
        let mut out = Vec::new();
        if !(cfg!(feature = "opus") && peer_caps & CAP_OPUS != 0) {
            let mut all = std::mem::take(&mut self.pend);
            all.extend_from_slice(pcm);
            let mono = self.to_mono(all);
            if !mono.is_empty() {
                let p = encode_audio_frame(&mono, FLAG_NONE);
                self.mode = AudioMode::Pcm;
                self.account(&p, mono.len());
                out.push(p);
            }
            return out;
        }
        let frames = if peer_caps & CAP_AUDIO10 != 0 {
            OPUS10_SAMPLES
        } else {
            TICK_SAMPLES
        } as usize;
        let frame = frames * self.channels;
        self.pend.extend_from_slice(pcm);
        let mut at = 0;
        while self.pend.len() - at >= frame {
            let chunk = self.pend[at..at + frame].to_vec();
            at += frame;
            let p = self.encode_frame(chunk, peer_caps);
            self.account(&p, frames);
            out.push(p);
        }
        self.pend.drain(..at);
        out
    }

    /// Encode one tick of `pcm` (this encoder's channel layout) for a peer
    /// with `peer_caps`.
    pub fn encode(&mut self, pcm: &[i16], peer_caps: u32) -> Vec<u8> {
        let frames = pcm.len() / self.channels;
        let payload = if peer_caps & crate::terminal::CAP_OPUS != 0 {
            self.encode_frame(pcm.to_vec(), peer_caps)
        } else {
            self.mode = AudioMode::Pcm;
            encode_audio_frame(&self.to_mono(pcm.to_vec()), FLAG_NONE)
        };
        self.account(&payload, frames);
        payload
    }

    /// One Opus frame of `chunk` (input layout) for an Opus peer: stereo to a
    /// `CAP_STEREO` peer from a stereo source, the mono down-mix otherwise;
    /// raw mono PCM if Opus is unavailable.
    fn encode_frame(&mut self, chunk: Vec<i16>, peer_caps: u32) -> Vec<u8> {
        let stereo = self.channels == 2 && peer_caps & crate::terminal::CAP_STEREO != 0;
        let input = if stereo { chunk } else { self.to_mono(chunk) };
        let ch = if stereo { 2 } else { 1 };
        if let Some(p) = self.try_opus(&input, ch) {
            return p;
        }
        self.mode = AudioMode::Pcm;
        let mono = if stereo {
            downmix_stereo(&input)
        } else {
            input
        };
        encode_audio_frame(&mono, FLAG_NONE)
    }

    /// `pcm` (input layout) as mono.
    fn to_mono(&self, pcm: Vec<i16>) -> Vec<i16> {
        if self.channels == 2 {
            downmix_stereo(&pcm)
        } else {
            pcm
        }
    }

    fn account(&mut self, payload: &[u8], samples: usize) {
        self.bytes += payload.len() as u64;
        self.ticks += 1;
        self.samples += samples as u64;
    }

    /// Average AUDIO payload bitrate so far (bits/s).
    pub fn bitrate(&self) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        self.bytes as f64 * 8.0 / (self.samples as f64 / f64::from(SAMPLE_RATE))
    }

    /// One `ch`-channel Opus packet of `pcm` (a 10 or 20 ms frame), or
    /// `None` when the size is wrong or libopus fails.
    #[cfg(feature = "opus")]
    fn try_opus(&mut self, pcm: &[i16], ch: usize) -> Option<Vec<u8>> {
        let n = (pcm.len() / ch) as u32;
        if !pcm.len().is_multiple_of(ch) || (n != TICK_SAMPLES && n != OPUS10_SAMPLES) {
            return None;
        }
        if !matches!(&self.opus, Some(o) if o.channels == ch) {
            self.opus = Some(OpusEnc::new(ch)?);
        }
        let squeezed = ch == 2 && self.gate.squeezed();
        let st = self.opus.as_mut()?;
        st.set_squeezed(squeezed);
        let len = st.enc.encode(pcm, &mut self.out).ok()?;
        self.mode = match (ch, squeezed) {
            (1, _) => AudioMode::OpusMono,
            (_, true) => AudioMode::OpusSqueezed,
            _ => AudioMode::OpusStereo,
        };
        (len > 0).then(|| encode_opus_frame(&self.out[..len], n, FLAG_NONE))
    }

    #[cfg(not(feature = "opus"))]
    fn try_opus(&mut self, _pcm: &[i16], _ch: usize) -> Option<Vec<u8>> {
        None
    }
}

/// Receiver-side audio decoder: raw PCM always, Opus with the `opus`
/// feature (one decoder state per stream, created on the first packet).
///
/// The output layout is fixed per decoder
/// ([`with_channels`](Self::with_channels)): mono by default, or
/// interleaved stereo for a stereo playback device. Any payload decodes
/// into it — libopus down-/up-mixes Opus packets, raw (mono) PCM is
/// duplicated into both channels.
pub struct AudioDecoder {
    channels: usize,
    #[cfg(feature = "opus")]
    opus: Option<opus::Decoder>,
}

impl Default for AudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDecoder {
    /// A mono-output decoder with no stream state yet.
    pub fn new() -> Self {
        Self::with_channels(1)
    }

    /// A decoder producing `channels`-channel output (1 = mono, 2 =
    /// interleaved stereo; clamped into that range).
    pub fn with_channels(channels: usize) -> Self {
        Self {
            channels: channels.clamp(1, 2),
            #[cfg(feature = "opus")]
            opus: None,
        }
    }

    /// Output channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode one AUDIO payload to PCM in the output layout. Malformed
    /// input (or Opus without the `opus` feature) is an `Err`, never a panic.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, AppError> {
        match parse_audio_frame(payload)?.1 {
            AudioPayload::Pcm(samples) if self.channels == 2 => Ok(upmix_mono(&samples)),
            AudioPayload::Pcm(samples) => Ok(samples),
            AudioPayload::Opus { samples, packet } => self.decode_opus(samples, packet),
        }
    }

    /// The libopus state, created on first use in the output layout.
    #[cfg(feature = "opus")]
    fn opus_state(&mut self) -> Result<&mut opus::Decoder, AppError> {
        if self.opus.is_none() {
            let layout = if self.channels == 2 {
                opus::Channels::Stereo
            } else {
                opus::Channels::Mono
            };
            self.opus = Some(
                opus::Decoder::new(SAMPLE_RATE, layout)
                    .map_err(|e| AppError::codec(format!("opus decoder: {e}")))?,
            );
        }
        self.opus
            .as_mut()
            .ok_or_else(|| AppError::codec("opus decoder missing"))
    }

    /// Run libopus over `packet` (`[]` = PLC) into `frames` frames.
    #[cfg(feature = "opus")]
    fn run_opus(&mut self, packet: &[u8], frames: usize, fec: bool) -> Result<Vec<i16>, AppError> {
        let ch = self.channels;
        let dec = self.opus_state()?;
        let mut out = vec![0i16; frames * ch];
        let n = dec
            .decode(packet, &mut out, fec)
            .map_err(|e| AppError::codec(format!("opus decode: {e}")))?;
        out.truncate(n * ch);
        Ok(out)
    }

    #[cfg(feature = "opus")]
    fn decode_opus(&mut self, samples: u32, packet: &[u8]) -> Result<Vec<i16>, AppError> {
        self.run_opus(packet, samples as usize, false)
    }

    #[cfg(not(feature = "opus"))]
    fn decode_opus(&mut self, _samples: u32, _packet: &[u8]) -> Result<Vec<i16>, AppError> {
        Err(AppError::invalid(
            "AUDIO is Opus but this build has no Opus decoder",
        ))
    }

    /// Recover a lost packet of `lost_samples` samples (per channel) from
    /// the in-band FEC of the packet that followed it (`next` = that
    /// packet's AUDIO payload; decode `next` itself normally afterwards).
    /// Without FEC data Opus conceals (PLC). Raw PCM / no `opus`: silence.
    pub fn decode_fec(&mut self, next: &[u8], lost_samples: u32) -> Result<Vec<i16>, AppError> {
        let lost = lost_samples.min(5760) as usize;
        match parse_audio_frame(next)?.1 {
            AudioPayload::Pcm(_) => Ok(vec![0; lost * self.channels]),
            AudioPayload::Opus { packet, .. } => self.fec_opus(packet, lost),
        }
    }

    /// Conceal one lost packet of `lost_samples` samples (per channel) with
    /// no later packet to recover it from (Opus packet-loss concealment;
    /// raw PCM / no `opus`: silence).
    pub fn conceal(&mut self, lost_samples: u32) -> Result<Vec<i16>, AppError> {
        let lost = lost_samples.min(5760) as usize;
        self.plc_opus(lost)
    }

    #[cfg(feature = "opus")]
    fn plc_opus(&mut self, lost: usize) -> Result<Vec<i16>, AppError> {
        if self.opus.is_none() {
            // Nothing decoded yet: silence.
            return Ok(vec![0; lost * self.channels]);
        }
        self.run_opus(&[], lost, false)
    }

    #[cfg(not(feature = "opus"))]
    fn plc_opus(&mut self, lost: usize) -> Result<Vec<i16>, AppError> {
        Ok(vec![0; lost * self.channels])
    }

    #[cfg(feature = "opus")]
    fn fec_opus(&mut self, packet: &[u8], lost: usize) -> Result<Vec<i16>, AppError> {
        self.run_opus(packet, lost, true)
    }

    #[cfg(not(feature = "opus"))]
    fn fec_opus(&mut self, _packet: &[u8], lost: usize) -> Result<Vec<i16>, AppError> {
        Ok(vec![0; lost * self.channels])
    }
}
