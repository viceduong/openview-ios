//! `ovapp::file_transfer` — bidirectional, file-id-keyed file transfer.
//!
//! First-class, file-id-keyed transfers over the established command channel.
//! Each chunk carries a header (file id / length / offset), and a completion
//! command ends the transfer. It rides OpenView's existing command opcodes instead of a dedicated
//! length-prefixed record type:
//!
//! - [`Command::FileXferData`] (`0x14`): `4B file_id u32-BE` + `8B offset
//!   u64-BE` + raw chunk bytes — the data chunk at `offset` of file
//!   `file_id`.
//! - [`Command::FileXferComplete`] (`0x18`): `4B file_id u32-BE` + `8B total
//!   u64-BE` + `1B ok` + optional UTF-8 name — closes the transfer (`ok = 1`
//!   success, `0` abort).
//!
//! Unlike the older `FILE_BEGIN`/`FILE_DATA`/`FILE_END` path (one
//! transfer at a time, positionally framed), the file id in every chunk means
//! **multiple transfers can interleave** on the same channel and the receiver
//! reassembles each stream independently. The declared total rides the
//! completion command, so the receiver validates the reassembled length
//! instead of trusting a begin header.
//!
//! # Resource limits (hostile peers)
//!
//! Everything a peer sends is buffered in memory until the completion, so the
//! receiver enforces [`TransferLimits`]: at most `max_concurrent` transfers in
//! flight, at most `max_in_flight_bytes` buffered across all of them
//! (completed-but-untaken files included), at most `max_file_bytes` per file,
//! and at most [`MAX_FRAGMENTS`] non-contiguous pieces per file (contiguous
//! chunks are coalesced, so an in-order sender always has one). Overlapping
//! chunks are rejected; an exact retransmission of bytes already held is
//! ignored (never double-counted). A transfer that breaks a rule is dropped
//! and its remaining chunks are discarded silently (one `Err` per transfer,
//! not one per chunk).
//!
//! Received names go through [`FileTransfer::sanitize_name`] (and
//! [`unique_name`] against existing files) before touching a filesystem.
//!
//! Everything here is pure: no platform calls, no panics — malformed or
//! inconsistent input is an `Err` (or a silent drop), never a crash, so a
//! hostile peer cannot take the endpoint down.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::error::AppError;
use crate::terminal::Command;

/// Default chunk size for [`FileTransfer::send`] when `chunk_size` is 0:
/// 16 KiB — the same default the legacy `send_file` path uses, small enough
/// that a chunk's record stays far under the tightest carrier's frame limit.
pub const DEFAULT_CHUNK_SIZE: usize = 16 * 1024;

/// Most non-contiguous fragments one inbound transfer may hold (reordering
/// headroom; an in-order stream is always a single fragment).
pub const MAX_FRAGMENTS: usize = 1024;

/// Most transfer ids remembered as "rejected" (their trailing chunks are
/// dropped silently); beyond this every stray chunk is an `Err` again.
const MAX_REJECTED_IDS: usize = 256;

/// Longest sanitized file name, in bytes (NTFS / APFS component limit).
const MAX_NAME_BYTES: usize = 255;

/// Receive-side resource caps (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferLimits {
    /// Inbound transfers reassembling at the same time.
    pub max_concurrent: usize,
    /// Bytes buffered across all inbound transfers (incl. completed, untaken).
    pub max_in_flight_bytes: u64,
    /// Largest single inbound file.
    pub max_file_bytes: u64,
}

impl Default for TransferLimits {
    /// 4 concurrent transfers, 256 MiB buffered, 256 MiB per file.
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_in_flight_bytes: 256 * 1024 * 1024,
            max_file_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Metadata for one transfer, sent with the completion (and tracked while it
/// is in flight).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferJob {
    /// The transfer's wire identifier (unique per direction while in flight).
    pub file_id: u32,
    /// File name (informational; the wire carries it out of band — the
    /// sender's UI picks the name, the receiver's sink uses it).
    pub name: String,
    /// Declared total byte length (validated against the reassembled bytes).
    pub total_len: u64,
}

/// One completed inbound transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFile {
    /// The transfer's wire identifier.
    pub file_id: u32,
    /// File name as the peer sent it — **unsanitized**; pass it through
    /// [`FileTransfer::sanitize_name`] before using it as a path.
    pub name: String,
    /// The reassembled, length-verified bytes.
    pub data: Vec<u8>,
}

/// What [`InFlight::insert`] did with a chunk.
#[derive(Debug, PartialEq, Eq)]
enum Inserted {
    /// New bytes, now buffered.
    New,
    /// An exact copy of bytes already held (retransmission): ignored.
    Duplicate,
}

/// Reassembly state for one in-flight inbound transfer.
#[derive(Debug, Default)]
struct InFlight {
    /// Fragments keyed by their starting offset; adjacent chunks are
    /// coalesced, so fragments never touch or overlap.
    chunks: BTreeMap<u64, Vec<u8>>,
    /// Bytes buffered (sum of fragment lengths).
    got: u64,
}

impl InFlight {
    /// Place `chunk` at `offset`, rejecting overlaps and oversized fragments.
    fn insert(&mut self, offset: u64, chunk: &[u8]) -> Result<Inserted, String> {
        if chunk.is_empty() {
            // The sender's empty-file marker (offset 0); anything else is noise.
            return if offset == 0 {
                Ok(Inserted::Duplicate)
            } else {
                Err(format!("empty chunk at offset {offset}"))
            };
        }
        let end = offset
            .checked_add(chunk.len() as u64)
            .ok_or("chunk offset overflow")?;
        // The fragment starting at or before `offset`.
        if let Some((&ps, prev)) = self.chunks.range(..=offset).next_back() {
            let pe = ps + prev.len() as u64;
            if offset < pe {
                // Exact retransmission of bytes we hold: harmless, ignore.
                let within = end <= pe;
                let from = (offset - ps) as usize;
                if within && prev[from..from + chunk.len()] == *chunk {
                    return Ok(Inserted::Duplicate);
                }
                return Err(format!(
                    "chunk {offset}..{end} overlaps buffered bytes {ps}..{pe}"
                ));
            }
        }
        // The first fragment after `offset`.
        let next = self.chunks.range(offset + 1..).next().map(|(&ns, _)| ns);
        if let Some(ns) = next {
            if ns < end {
                return Err(format!(
                    "chunk {offset}..{end} overlaps buffered bytes at {ns}"
                ));
            }
        }
        // Coalesce with the previous fragment if it ends exactly here.
        let start = match self.chunks.range_mut(..offset).next_back() {
            Some((&ps, prev)) if ps + prev.len() as u64 == offset => {
                prev.extend_from_slice(chunk);
                ps
            }
            _ => {
                if self.chunks.len() >= MAX_FRAGMENTS {
                    return Err(format!("more than {MAX_FRAGMENTS} out-of-order fragments"));
                }
                self.chunks.insert(offset, chunk.to_vec());
                offset
            }
        };
        // ... and with the next one if it starts exactly at our end.
        if next == Some(end) {
            if let Some(tail) = self.chunks.remove(&end) {
                if let Some(head) = self.chunks.get_mut(&start) {
                    head.extend_from_slice(&tail);
                }
            }
        }
        self.got += chunk.len() as u64;
        Ok(Inserted::New)
    }
}

/// Bidirectional file-transfer endpoint.
///
/// One instance per session direction of use: `send` produces the wire
/// commands (caller ships them through the transport), `poll` consumes the
/// peer's decoded commands and reassembles the files. The same struct can
/// send and receive concurrently — send and receive state are disjoint.
#[derive(Debug, Default)]
pub struct FileTransfer {
    /// Receive-side caps.
    limits: TransferLimits,
    /// In-flight inbound transfers keyed by `file_id`.
    rx: BTreeMap<u32, InFlight>,
    /// Transfers dropped for breaking a limit: their stray chunks are
    /// discarded silently.
    rejected: BTreeSet<u32>,
    /// Completed inbound transfers, oldest first (drained by [`take`]).
    finished: VecDeque<ReceivedFile>,
    /// Transfers this side has sent, keyed by `file_id` (name bookkeeping so
    /// a completed transfer can be reported with its name).
    tx_names: BTreeMap<u32, String>,
    /// Next file id to allocate on the send side.
    next_id: u32,
    /// Chunks received (diagnostics).
    chunks_rx: u64,
    /// Transfers completed inbound (diagnostics).
    transfers_rx: u64,
    /// Transfers rejected inbound (length mismatch / abort / limits) (diagnostics).
    transfers_failed: u64,
}

impl FileTransfer {
    /// A fresh endpoint with the default [`TransferLimits`]; send-side file
    /// ids start at 1 (0 is reserved so a zeroed wire buffer can never alias
    /// a live transfer).
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(TransferLimits::default())
    }

    /// A fresh endpoint with explicit receive-side limits.
    #[must_use]
    pub fn with_limits(limits: TransferLimits) -> Self {
        Self {
            limits,
            next_id: 1,
            ..Self::default()
        }
    }

    /// The receive-side limits in force.
    #[must_use]
    pub fn limits(&self) -> TransferLimits {
        self.limits
    }

    /// Split `data` into chunk-sized [`Command::FileXferData`] commands
    /// followed by one [`Command::FileXferComplete`]. The caller ships the
    /// returned commands through the transport in order (each command is one
    /// AEAD record on channel 0, exactly like `TerminalSession::send`).
    ///
    /// An empty file produces one zero-length chunk and the completion.
    /// `chunk_size` 0 selects [`DEFAULT_CHUNK_SIZE`].
    pub fn send_commands(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
    ) -> Result<Vec<Command>, AppError> {
        let chunk = if chunk_size == 0 {
            DEFAULT_CHUNK_SIZE
        } else {
            chunk_size
        };
        let file_id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| AppError::invalid("file ids exhausted"))?;
        self.tx_names.insert(file_id, name.to_owned());
        let mut out = Vec::with_capacity(data.len() / chunk.max(1) + 2);
        let mut offset = 0u64;
        // An empty file still emits one zero-length chunk so the receiver
        // registers the in-flight transfer before the completion arrives.
        for c in std::iter::once(data)
            .filter(|_| data.is_empty())
            .chain(data.chunks(chunk.max(1)))
        {
            out.push(Command::FileXferData {
                file_id,
                offset,
                chunk: c.to_vec(),
            });
            offset += c.len() as u64;
        }
        out.push(Command::FileXferComplete {
            file_id,
            total: data.len() as u64,
            ok: 1,
            name: name.to_owned(),
        });
        Ok(out)
    }

    /// Convenience mirroring `TerminalSession::send_file`: run `send_commands`
    /// and push each command through `ship` (typically a closure calling
    /// `TerminalSession::send`). Stops at the first send error; `ship`'s
    /// error type only needs to absorb an [`AppError`] (e.g. `String`).
    pub fn send<E: From<AppError>>(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
        mut ship: impl FnMut(&Command) -> Result<(), E>,
    ) -> Result<(), E> {
        for cmd in self.send_commands(name, data, chunk_size)? {
            ship(&cmd)?;
        }
        Ok(())
    }

    /// Bytes currently buffered: in-flight fragments + completed, untaken files.
    #[must_use]
    pub fn buffered_bytes(&self) -> u64 {
        let rx: u64 = self.rx.values().map(|f| f.got).sum();
        let done: u64 = self.finished.iter().map(|f| f.data.len() as u64).sum();
        rx + done
    }

    /// Drop transfer `file_id` for breaking a rule; its later chunks are
    /// discarded silently.
    fn reject(&mut self, file_id: u32, why: String) -> Result<(), AppError> {
        self.rx.remove(&file_id);
        self.transfers_failed += 1;
        if self.rejected.len() < MAX_REJECTED_IDS {
            self.rejected.insert(file_id);
        }
        Err(AppError::malformed(format!("file {file_id}: {why}")))
    }

    fn on_data(&mut self, file_id: u32, offset: u64, chunk: &[u8]) -> Result<(), AppError> {
        self.chunks_rx += 1;
        if self.rejected.contains(&file_id) {
            return Ok(()); // already reported once
        }
        let end = offset.saturating_add(chunk.len() as u64);
        if end > self.limits.max_file_bytes {
            return self.reject(
                file_id,
                format!(
                    "exceeds the {} byte per-file limit",
                    self.limits.max_file_bytes
                ),
            );
        }
        if !self.rx.contains_key(&file_id) && self.rx.len() >= self.limits.max_concurrent {
            return self.reject(
                file_id,
                format!(
                    "more than {} concurrent transfers",
                    self.limits.max_concurrent
                ),
            );
        }
        if self.buffered_bytes().saturating_add(chunk.len() as u64)
            > self.limits.max_in_flight_bytes
        {
            return self.reject(
                file_id,
                format!(
                    "in-flight budget of {} bytes exhausted",
                    self.limits.max_in_flight_bytes
                ),
            );
        }
        let inflight = self.rx.entry(file_id).or_default();
        match inflight.insert(offset, chunk) {
            Ok(_) => Ok(()),
            Err(e) => self.reject(file_id, e),
        }
    }

    fn on_complete(&mut self, file_id: u32, total: u64, name: &str) -> Result<(), AppError> {
        if self.rejected.remove(&file_id) {
            return Ok(()); // the rejection was already reported
        }
        let Some(mut inflight) = self.rx.remove(&file_id) else {
            return Err(AppError::malformed(format!(
                "FILE_XFER_COMPLETE for unknown file_id {file_id}"
            )));
        };
        if inflight.got != total {
            self.transfers_failed += 1;
            return Err(AppError::malformed(format!(
                "file {file_id}: reassembled {} bytes, declared {total}",
                inflight.got
            )));
        }
        // Coalescing leaves exactly one fragment at offset 0 when the
        // offsets tile 0..total; anything else has a hole.
        let data = match (inflight.chunks.len(), inflight.chunks.pop_first()) {
            (0, _) => Vec::new(),
            (1, Some((0, data))) => data,
            (_, first) => {
                self.transfers_failed += 1;
                let at = first.map_or(0, |(off, _)| off);
                return Err(AppError::malformed(format!(
                    "file {file_id}: chunks do not tile 0..{total} (fragment at {at})"
                )));
            }
        };
        self.transfers_rx += 1;
        let name = if name.is_empty() {
            format!("file-{file_id}")
        } else {
            name.to_owned()
        };
        self.finished.push_back(ReceivedFile {
            file_id,
            name,
            data,
        });
        Ok(())
    }

    /// Feed one decoded command from the peer. `FileXferData` accumulates;
    /// `FileXferComplete` finalizes (or aborts) the transfer. Any other
    /// command is ignored (the caller's pump routes the rest elsewhere).
    /// Malformed or over-limit input is dropped or errors — never a panic.
    pub fn poll(&mut self, cmd: &Command) -> Result<(), AppError> {
        match cmd {
            Command::FileXferData {
                file_id,
                offset,
                chunk,
            } => self.on_data(*file_id, *offset, chunk),
            Command::FileXferComplete {
                file_id,
                total,
                ok: 1,
                name,
            } => self.on_complete(*file_id, *total, name),
            Command::FileXferComplete { file_id, ok: 0, .. } => {
                // An explicit abort: drop the in-flight state, do not error.
                self.rejected.remove(file_id);
                self.rx.remove(file_id);
                self.transfers_failed += 1;
                Ok(())
            }
            Command::FileXferComplete { ok, .. } => Err(AppError::malformed(format!(
                "FILE_XFER_COMPLETE with invalid ok byte {ok}"
            ))),
            _ => Ok(()),
        }
    }

    /// Take the next completed inbound transfer (oldest first). `None` when
    /// no transfer finished since the last call.
    pub fn take(&mut self) -> Option<ReceivedFile> {
        self.finished.pop_front()
    }

    /// Reduce a peer-supplied file name to a safe single path component
    /// so a received file can be written into a fixed directory
    /// without escaping it or aliasing a device:
    ///
    /// - only the final segment after any `/` or `\` survives;
    /// - `:` (drive letters `D:x`, NTFS alternate data streams `x:ads`),
    ///   `< > " | ? *` and control characters become `_`;
    /// - leading/trailing spaces and trailing dots are trimmed (Windows
    ///   strips them, which could otherwise turn `x.exe.` into `x.exe`);
    /// - empty, `.` and `..` become `_`;
    /// - reserved Windows device names (`CON`, `PRN`, `AUX`, `NUL`,
    ///   `COM1`-`COM9`, `LPT1`-`LPT9`, with any extension) get a `_` prefix;
    /// - the result is at most 255 bytes (the extension is kept).
    ///
    /// Pure — the caller owns the filesystem (see [`unique_name`] for
    /// collisions).
    #[must_use]
    pub fn sanitize_name(name: &str) -> String {
        let base = name.rsplit(['/', '\\']).next().unwrap_or("");
        let cleaned: String = base
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let trimmed = cleaned.trim_start().trim_end_matches([' ', '.']);
        if trimmed.is_empty() {
            return "_".to_owned();
        }
        let mut out = if is_reserved_windows_name(trimmed) {
            format!("_{trimmed}")
        } else {
            trimmed.to_owned()
        };
        if out.len() > MAX_NAME_BYTES {
            out = truncate_keeping_extension(&out, MAX_NAME_BYTES);
        }
        out
    }

    /// The number of transfers still being reassembled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.rx.len()
    }

    /// Chunks received so far (diagnostics).
    #[must_use]
    pub fn chunks_rx(&self) -> u64 {
        self.chunks_rx
    }

    /// Transfers completed inbound (diagnostics).
    #[must_use]
    pub fn transfers_rx(&self) -> u64 {
        self.transfers_rx
    }

    /// Transfers aborted, length-rejected or limit-rejected inbound (diagnostics).
    #[must_use]
    pub fn transfers_failed(&self) -> u64 {
        self.transfers_failed
    }
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9` (plus the superscript
/// digit forms Windows also reserves), case-insensitive, ignoring any
/// extension and trailing spaces before it.
fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut chars = upper.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let rest: String = chars.collect();
    (prefix == "COM" || prefix == "LPT")
        && rest.chars().count() == 1
        && rest
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
}

/// Cut `name` to at most `max` bytes on a char boundary, keeping a short
/// extension (≤ 16 bytes) intact.
fn truncate_keeping_extension(name: &str, max: usize) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => name.split_at(i),
        _ => (name, ""),
    };
    let budget = max.saturating_sub(ext.len());
    let mut cut = budget.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{ext}", &stem[..cut])
}

/// Pick a non-colliding variant of an (already sanitized) file name:
/// `name` itself when `exists(name)` is false, else `stem (1).ext`,
/// `stem (2).ext`, … Returns `None` after 9999 collisions. Pure — `exists`
/// is the caller's view of the target directory.
#[must_use]
pub fn unique_name(name: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
    if !exists(name) {
        return Some(name.to_owned());
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    (1..10_000)
        .map(|n| {
            let suffix = format!(" ({n}){ext}");
            let stem = truncate_keeping_extension(stem, MAX_NAME_BYTES - suffix.len());
            format!("{stem}{suffix}")
        })
        .find(|cand| !exists(cand))
}

#[cfg(test)]
mod tests;
