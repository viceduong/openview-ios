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
//! [`run_mic_loop`], [`SineGen`]); both are re-exported here.

mod buffer;
mod source;
#[cfg(test)]
mod tests;

pub use buffer::*;
pub use source::*;

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
        // Our decoder takes any Opus frame size (10 ms included).
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
    /// PCM samples encoded (bitrate with any packet size).
    pub samples: u64,
    /// PCM waiting for a complete Opus frame ([`encode_ticks`](Self::encode_ticks)).
    pend: Vec<i16>,
}

impl Default for AudioEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEncoder {
    /// An encoder that has sent nothing yet (Opus state created lazily).
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

    /// Encode captured PCM of any tick size for a peer with
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
            // In-band FEC (LBRR) for the next packet to carry a
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
    /// A decoder with no stream state yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one AUDIO payload to PCM. Malformed input (or Opus without
    /// the `opus` feature) is an `Err`, never a panic.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, AppError> {
        match parse_audio_frame(payload)?.1 {
            AudioPayload::Pcm(samples) => Ok(samples),
            AudioPayload::Opus { samples, packet } => self.decode_opus(samples, packet),
        }
    }

    #[cfg(feature = "opus")]
    fn decode_opus(&mut self, samples: u32, packet: &[u8]) -> Result<Vec<i16>, AppError> {
        if self.opus.is_none() {
            self.opus = Some(
                opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
                    .map_err(|e| AppError::codec(format!("opus decoder: {e}")))?,
            );
        }
        let dec = self
            .opus
            .as_mut()
            .ok_or_else(|| AppError::codec("opus decoder missing"))?;
        let mut out = vec![0i16; samples as usize];
        let n = dec
            .decode(packet, &mut out, false)
            .map_err(|e| AppError::codec(format!("opus decode: {e}")))?;
        out.truncate(n);
        Ok(out)
    }

    #[cfg(not(feature = "opus"))]
    fn decode_opus(&mut self, _samples: u32, _packet: &[u8]) -> Result<Vec<i16>, AppError> {
        Err(AppError::invalid(
            "AUDIO is Opus but this build has no Opus decoder",
        ))
    }

    /// Recover a lost packet of `lost_samples` samples from the
    /// in-band FEC of the packet that followed it (`next` = that packet's
    /// AUDIO payload; decode `next` itself normally afterwards). Without
    /// FEC data Opus conceals (PLC). Raw PCM / no `opus`: silence.
    pub fn decode_fec(&mut self, next: &[u8], lost_samples: u32) -> Result<Vec<i16>, AppError> {
        let lost = lost_samples.min(5760) as usize;
        match parse_audio_frame(next)?.1 {
            AudioPayload::Pcm(_) => Ok(vec![0; lost]),
            AudioPayload::Opus { packet, .. } => self.fec_opus(packet, lost),
        }
    }

    /// Conceal one lost packet of `lost_samples` samples with no
    /// later packet to recover it from (Opus packet-loss concealment; raw
    /// PCM / no `opus`: silence).
    pub fn conceal(&mut self, lost_samples: u32) -> Result<Vec<i16>, AppError> {
        let lost = lost_samples.min(5760) as usize;
        self.plc_opus(lost)
    }

    #[cfg(feature = "opus")]
    fn plc_opus(&mut self, lost: usize) -> Result<Vec<i16>, AppError> {
        let Some(dec) = self.opus.as_mut() else {
            return Ok(vec![0; lost]); // nothing decoded yet: silence
        };
        let mut out = vec![0i16; lost];
        let n = dec
            .decode(&[], &mut out, false)
            .map_err(|e| AppError::codec(format!("opus PLC: {e}")))?;
        out.truncate(n);
        Ok(out)
    }

    #[cfg(not(feature = "opus"))]
    fn plc_opus(&mut self, lost: usize) -> Result<Vec<i16>, AppError> {
        Ok(vec![0; lost])
    }

    #[cfg(feature = "opus")]
    fn fec_opus(&mut self, packet: &[u8], lost: usize) -> Result<Vec<i16>, AppError> {
        if self.opus.is_none() {
            self.opus = Some(
                opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
                    .map_err(|e| AppError::codec(format!("opus decoder: {e}")))?,
            );
        }
        let dec = self
            .opus
            .as_mut()
            .ok_or_else(|| AppError::codec("opus decoder missing"))?;
        let mut out = vec![0i16; lost];
        let n = dec
            .decode(packet, &mut out, true)
            .map_err(|e| AppError::codec(format!("opus FEC decode: {e}")))?;
        out.truncate(n);
        Ok(out)
    }

    #[cfg(not(feature = "opus"))]
    fn fec_opus(&mut self, _packet: &[u8], lost: usize) -> Result<Vec<i16>, AppError> {
        Ok(vec![0; lost])
    }
}
