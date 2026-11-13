//! `ovapp` — the OpenView application layer.
//!
//! Everything here sits on top of an established [`ovcore::session::Session`]
//! (the post-handshake AEAD record channel) and moves **command-channel**
//! traffic: one record = one command. The main building blocks:
//!
//! - [`terminal::TerminalSession`] — the command channel itself (echo, remote
//!   keystrokes, file transfer, screen-frame streaming, ping/pong) plus the
//!   carrier pump that keeps a session and its transport in lock-step.
//! - [`screen`] — the remote-display payload: pixel frames (`Frame`,
//!   `PixelFormat`) streamed over the command channel and reassembled
//!   byte-exactly.
//! - [`sendq`], [`tiles`], [`bands`], [`lossy`], [`video`] — the host-side
//!   screen encoders and their viewer-side decoders.
//! - [`audio`] — audio framing, jitter buffering and (optionally) Opus.
//! - [`chat::ChatSession`] — a thin line-based chat wrapper over TYPE commands.
//!
//! Fallible APIs return [`AppError`].

#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod adapt;
pub mod audio;
pub mod bands;
pub mod chat;
pub mod clipboard;
pub mod codec;
pub mod error;
pub mod file_transfer;
pub mod gdisrc;
pub mod image;
pub mod liveness;
pub mod lossy;
#[cfg(all(windows, feature = "mf"))]
pub mod mfcodec;
pub mod monitors;
pub mod netwait;
pub mod pacing;
pub mod record;
pub mod scale;
pub mod screen;
pub mod scroll;
pub mod sendq;
pub mod terminal;
pub mod tiles;
pub mod video;
#[cfg(windows)]
pub mod winaudio;
pub mod zstd_par;

pub use adapt::{
    frame_changed, frame_changed_fast, qos, should_send, AutoQuality, QualityDecision,
    RateGovernor, RateStats, SAMPLE_STRIDE,
};
pub use audio::{
    decode_audio_frame, encode_audio_frame, encode_opus_frame, parse_audio_frame, AudioBus,
    AudioDecoder, AudioEncoder, AudioPayload, AudioRingBuffer, JitterBuffer, MicSource, SineGen,
    SynthMic, TickClock, CODEC_OPUS, CODEC_RAW_S16LE, FLAG_NONE, HEADER_LEN, SAMPLE_RATE, TICK_MS,
    TICK_SAMPLES,
};
pub use chat::{ChatSession, LineHook};
pub use clipboard::ClipboardSync;
pub use error::AppError;
pub use gdisrc::GdiScreen;
pub use image::{bgra_to_ppm, frame_pacing};
pub use liveness::{Keepalive, LinkStats, Liveness, DEAD_AFTER, KEEPALIVE_INTERVAL, RTT_SMOOTHING};
pub use monitors::{enumerate_monitors, region, validate_region, Monitor};
pub use record::{RecordHeader, RecordSummary, RecordWriter, RecordedFrame};
pub use screen::{synthetic_frame, Frame, FrameHook, PixelFormat};
pub use terminal::{CmdHook, OpCode, PointerKind, TerminalSession};
