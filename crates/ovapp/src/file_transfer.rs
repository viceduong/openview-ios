//! `ovapp::file_transfer` — bidirectional file transfer (v0.24).
//!
//! First-class, file-id-keyed transfers over the established command channel.
//! Each chunk carries a header (file id / length / offset), and a completion
//! command ends the transfer. It rides OpenView's existing command opcodes instead of a dedicated
//! length-prefixed record type:
//!
//! - [`Command::FileXferData`] (`0x14`): `4B file_id u32-BE` + raw chunk
//!   bytes — the data chunk for file `file_id`.
//! - [`Command::FileXferComplete`] (`0x18`): `4B file_id u32-BE` + `8B total
//!   u64-BE` + `1B ok` — closes the transfer (`ok = 1` success, `0` abort).
//!
//! Unlike the legacy v0.6 `FILE_BEGIN`/`FILE_DATA`/`FILE_END` path (one
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
    ) -> Result<Vec<Command>, String> {
        let chunk = if chunk_size == 0 {
            DEFAULT_CHUNK_SIZE
        } else {
            chunk_size
        };
        let file_id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or("file ids exhausted")?;
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
    /// `TerminalSession::send`). Stops at the first send error.
    pub fn send(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
        mut ship: impl FnMut(&Command) -> Result<(), String>,
    ) -> Result<(), String> {
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
    fn reject(&mut self, file_id: u32, why: String) -> Result<(), String> {
        self.rx.remove(&file_id);
        self.transfers_failed += 1;
        if self.rejected.len() < MAX_REJECTED_IDS {
            self.rejected.insert(file_id);
        }
        Err(format!("file {file_id}: {why}"))
    }

    fn on_data(&mut self, file_id: u32, offset: u64, chunk: &[u8]) -> Result<(), String> {
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

    fn on_complete(&mut self, file_id: u32, total: u64, name: &str) -> Result<(), String> {
        if self.rejected.remove(&file_id) {
            return Ok(()); // the rejection was already reported
        }
        let Some(mut inflight) = self.rx.remove(&file_id) else {
            return Err(format!("FILE_XFER_COMPLETE for unknown file_id {file_id}"));
        };
        if inflight.got != total {
            self.transfers_failed += 1;
            return Err(format!(
                "file {file_id}: reassembled {} bytes, declared {total}",
                inflight.got
            ));
        }
        // Coalescing leaves exactly one fragment at offset 0 when the
        // offsets tile 0..total; anything else has a hole.
        let data = match (inflight.chunks.len(), inflight.chunks.pop_first()) {
            (0, _) => Vec::new(),
            (1, Some((0, data))) => data,
            (_, first) => {
                self.transfers_failed += 1;
                let at = first.map_or(0, |(off, _)| off);
                return Err(format!(
                    "file {file_id}: chunks do not tile 0..{total} (fragment at {at})"
                ));
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
    pub fn poll(&mut self, cmd: &Command) -> Result<(), String> {
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
            Command::FileXferComplete { ok, .. } => {
                Err(format!("FILE_XFER_COMPLETE with invalid ok byte {ok}"))
            }
            _ => Ok(()),
        }
    }

    /// Take the next completed inbound transfer (oldest first). `None` when
    /// no transfer finished since the last call.
    pub fn take(&mut self) -> Option<ReceivedFile> {
        self.finished.pop_front()
    }

    /// Reduce a peer-supplied file name to a safe single path component
    /// (SEC-1), so a received file can be written into a fixed directory
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
mod tests {
    use super::*;

    fn data_cmds(file_id: u32, data: &[u8], chunk: usize) -> Vec<Command> {
        data_cmds_named(file_id, "test.bin", data, chunk)
    }

    fn data_cmds_named(file_id: u32, name: &str, data: &[u8], chunk: usize) -> Vec<Command> {
        let mut out = Vec::new();
        let mut offset = 0u64;
        for c in data.chunks(chunk.max(1)) {
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
        out
    }

    fn chunk(file_id: u32, offset: u64, bytes: &[u8]) -> Command {
        Command::FileXferData {
            file_id,
            offset,
            chunk: bytes.to_vec(),
        }
    }

    fn complete(file_id: u32, total: u64) -> Command {
        Command::FileXferComplete {
            file_id,
            total,
            ok: 1,
            name: String::new(),
        }
    }

    #[test]
    fn roundtrip_small_file() {
        let mut tx = FileTransfer::new();
        let mut rx = FileTransfer::new();
        let payload = b"hello openview".to_vec();
        for cmd in tx.send_commands("small.bin", &payload, 0).expect("send") {
            rx.poll(&cmd).expect("poll");
        }
        let got = rx.take().expect("completed");
        assert_eq!(got.name, "small.bin");
        assert_eq!(got.data, payload);
        assert_eq!(rx.in_flight(), 0);
        assert_eq!(rx.transfers_rx(), 1);
    }

    #[test]
    fn multi_chunk_file_larger_than_chunk_size() {
        let mut tx = FileTransfer::new();
        let mut rx = FileTransfer::new();
        let payload: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        let cmds = tx.send_commands("big.bin", &payload, 4096).expect("send");
        // 50_000 / 4096 = 13 chunks (12 full + 1 partial) + completion.
        assert_eq!(cmds.len(), 14);
        assert!(matches!(cmds[0], Command::FileXferData { file_id: 1, .. }));
        for cmd in &cmds {
            rx.poll(cmd).expect("poll");
        }
        let got = rx.take().expect("completed");
        assert_eq!(got.data, payload);
        assert_eq!(rx.chunks_rx(), 13);
    }

    #[test]
    fn two_concurrent_file_ids_interleaved() {
        let mut rx = FileTransfer::new();
        let a: Vec<u8> = (0..100u8).collect();
        let b: Vec<u8> = (200..250u8).collect();
        let cmds_a = data_cmds(7, &a, 30);
        let cmds_b = data_cmds(9, &b, 30);
        // Interleave the two streams chunk-by-chunk.
        let mut i = 0;
        while i < cmds_a.len().max(cmds_b.len()) {
            if let Some(c) = cmds_a.get(i) {
                rx.poll(c).expect("poll a");
            }
            if let Some(c) = cmds_b.get(i) {
                rx.poll(c).expect("poll b");
            }
            i += 1;
        }
        assert_eq!(rx.in_flight(), 0);
        let first = rx.take().expect("a completed");
        let second = rx.take().expect("b completed");
        // Completion order follows arrival order: b is shorter, so its
        // completion lands first even though a started first.
        assert_eq!((first.file_id, first.data), (9, b));
        assert_eq!((second.file_id, second.data), (7, a));
        assert_eq!(rx.transfers_rx(), 2);
    }

    #[test]
    fn out_of_order_chunks_reassemble_by_offset() {
        let mut rx = FileTransfer::new();
        let payload: Vec<u8> = (0..60u8).collect();
        let mut cmds = data_cmds(3, &payload, 20);
        // Swap the first two data chunks (completion stays last).
        cmds.swap(0, 1);
        for cmd in &cmds {
            rx.poll(cmd).expect("poll");
        }
        assert_eq!(rx.take().expect("completed").data, payload);
    }

    #[test]
    fn reversed_chunks_coalesce_into_one_fragment() {
        let mut rx = FileTransfer::new();
        let payload: Vec<u8> = (0..=255u8).collect();
        let mut cmds = data_cmds(3, &payload, 16);
        let done = cmds.pop().unwrap();
        for cmd in cmds.iter().rev() {
            rx.poll(cmd).expect("poll");
        }
        assert_eq!(rx.rx[&3].chunks.len(), 1, "fully coalesced");
        rx.poll(&done).expect("complete");
        assert_eq!(rx.take().unwrap().data, payload);
    }

    #[test]
    fn truncated_transfer_accumulates_until_complete() {
        let mut rx = FileTransfer::new();
        let payload: Vec<u8> = (0..40u8).collect();
        let cmds = data_cmds(5, &payload, 10);
        // Feed only the data chunks — no completion yet.
        for cmd in cmds.iter().take(cmds.len() - 1) {
            rx.poll(cmd).expect("poll");
        }
        assert_eq!(rx.in_flight(), 1, "still accumulating");
        assert!(rx.take().is_none(), "not finalized without completion");
        // Now the completion arrives.
        rx.poll(cmds.last().expect("completion")).expect("poll");
        let got = rx.take().expect("completed");
        assert_eq!(got.data, payload);
    }

    #[test]
    fn length_mismatch_is_an_error_not_a_panic() {
        let mut rx = FileTransfer::new();
        for c in data_cmds(2, b"0123456789", 5).iter().take(2) {
            rx.poll(c).expect("poll");
        }
        // Completion declares 99 bytes but only 10 were received.
        let err = rx
            .poll(&complete(2, 99))
            .expect_err("must reject the mismatch");
        assert!(err.contains("declared"), "message: {err}");
        assert_eq!(rx.transfers_failed(), 1);
        assert!(rx.take().is_none());
    }

    #[test]
    fn hole_is_rejected_even_when_the_length_matches() {
        let mut rx = FileTransfer::new();
        rx.poll(&chunk(1, 0, b"abc")).unwrap();
        rx.poll(&chunk(1, 10, b"xyz")).unwrap();
        let err = rx.poll(&complete(1, 6)).unwrap_err();
        assert!(err.contains("tile"), "{err}");
    }

    #[test]
    fn duplicate_chunk_is_not_double_counted() {
        let mut rx = FileTransfer::new();
        rx.poll(&chunk(1, 0, b"hello")).unwrap();
        rx.poll(&chunk(1, 0, b"hello"))
            .expect("exact retransmit is ignored");
        rx.poll(&chunk(1, 2, b"ll"))
            .expect("retransmit inside a fragment");
        rx.poll(&chunk(1, 5, b" world")).unwrap();
        rx.poll(&complete(1, 11)).expect("11 bytes, not 18");
        assert_eq!(rx.take().unwrap().data, b"hello world");
    }

    #[test]
    fn overlapping_chunk_rejects_the_transfer_once() {
        let mut rx = FileTransfer::new();
        rx.poll(&chunk(1, 0, b"hello")).unwrap();
        let err = rx.poll(&chunk(1, 3, b"XXXX")).unwrap_err();
        assert!(err.contains("overlaps"), "{err}");
        // Later chunks + completion of the dropped transfer: silent.
        rx.poll(&chunk(1, 9, b"zz")).expect("silently dropped");
        rx.poll(&complete(1, 11)).expect("silently dropped");
        assert!(rx.take().is_none());
        assert_eq!(rx.in_flight(), 0);
        // Overlap with the *next* fragment is caught too.
        rx.poll(&chunk(2, 10, b"abc")).unwrap();
        assert!(rx.poll(&chunk(2, 8, b"0123")).is_err());
        // A differing chunk at a known offset is an overlap, not a duplicate.
        rx.poll(&chunk(3, 0, b"abc")).unwrap();
        assert!(rx.poll(&chunk(3, 0, b"abd")).is_err());
    }

    #[test]
    fn concurrent_transfer_cap() {
        let mut rx = FileTransfer::with_limits(TransferLimits {
            max_concurrent: 2,
            ..TransferLimits::default()
        });
        rx.poll(&chunk(1, 0, b"a")).unwrap();
        rx.poll(&chunk(2, 0, b"b")).unwrap();
        let err = rx.poll(&chunk(3, 0, b"c")).unwrap_err();
        assert!(err.contains("concurrent"), "{err}");
        rx.poll(&complete(1, 1)).unwrap();
        rx.poll(&chunk(4, 0, b"d")).expect("a slot freed up");
    }

    #[test]
    fn in_flight_byte_budget() {
        let mut rx = FileTransfer::with_limits(TransferLimits {
            max_in_flight_bytes: 10,
            ..TransferLimits::default()
        });
        rx.poll(&chunk(1, 0, &[0; 6])).unwrap();
        let err = rx.poll(&chunk(2, 0, &[0; 6])).unwrap_err();
        assert!(err.contains("budget"), "{err}");
        // Completed-but-untaken files still count.
        rx.poll(&complete(1, 6)).unwrap();
        assert!(rx.poll(&chunk(3, 0, &[0; 6])).is_err());
        let _ = rx.take();
        rx.poll(&chunk(4, 0, &[0; 6]))
            .expect("budget freed by take()");
    }

    #[test]
    fn per_file_cap_and_offset_overflow() {
        let mut rx = FileTransfer::with_limits(TransferLimits {
            max_file_bytes: 100,
            ..TransferLimits::default()
        });
        let err = rx.poll(&chunk(1, 99, b"ab")).unwrap_err();
        assert!(err.contains("per-file"), "{err}");
        assert!(rx.poll(&chunk(2, u64::MAX, b"x")).is_err());
        assert!(rx.poll(&chunk(3, 50, b"")).is_err(), "empty chunk mid-file");
    }

    #[test]
    fn fragment_cap() {
        let mut rx = FileTransfer::new();
        for i in 0..MAX_FRAGMENTS as u64 {
            rx.poll(&chunk(1, i * 2, b"x")).unwrap();
        }
        let err = rx
            .poll(&chunk(1, MAX_FRAGMENTS as u64 * 2, b"x"))
            .unwrap_err();
        assert!(err.contains("fragments"), "{err}");
    }

    #[test]
    fn complete_for_unknown_file_id_is_an_error() {
        let mut rx = FileTransfer::new();
        let err = rx
            .poll(&complete(42, 0))
            .expect_err("unknown file id must error");
        assert!(err.contains("unknown"), "message: {err}");
    }

    #[test]
    fn abort_drops_the_in_flight_transfer() {
        let mut rx = FileTransfer::new();
        for c in data_cmds(4, b"partial data here", 6).iter().take(2) {
            rx.poll(c).expect("poll");
        }
        assert_eq!(rx.in_flight(), 1);
        rx.poll(&Command::FileXferComplete {
            file_id: 4,
            total: 17,
            ok: 0,
            name: String::new(),
        })
        .expect("abort is not an error");
        assert_eq!(rx.in_flight(), 0);
        assert!(rx.take().is_none());
        assert_eq!(rx.transfers_failed(), 1);
    }

    #[test]
    fn empty_file_transfers_as_completion_only() {
        let mut tx = FileTransfer::new();
        let mut rx = FileTransfer::new();
        let cmds = tx.send_commands("empty.bin", b"", 0).expect("send");
        assert_eq!(cmds.len(), 2, "one zero-length chunk + completion");
        for cmd in &cmds {
            rx.poll(cmd).expect("poll");
        }
        let got = rx.take().expect("completed");
        assert_eq!(got.name, "empty.bin");
        assert!(got.data.is_empty());
    }

    #[test]
    fn send_allocates_distinct_file_ids() {
        let mut tx = FileTransfer::new();
        let c1 = tx.send_commands("a", b"x", 0).expect("send");
        let c2 = tx.send_commands("b", b"y", 0).expect("send");
        let id1 = match &c1[0] {
            Command::FileXferData { file_id, .. } => *file_id,
            _ => panic!("first command must be a data chunk"),
        };
        let id2 = match &c2[0] {
            Command::FileXferData { file_id, .. } => *file_id,
            _ => panic!("first command must be a data chunk"),
        };
        assert_ne!(id1, id2, "concurrent transfers need distinct ids");
    }

    #[test]
    fn non_file_commands_are_ignored() {
        let mut rx = FileTransfer::new();
        rx.poll(&Command::Ping(7)).expect("ignored");
        rx.poll(&Command::Echo(b"hi".to_vec())).expect("ignored");
        assert_eq!(rx.in_flight(), 0);
        assert_eq!(rx.chunks_rx(), 0);
    }

    #[test]
    fn sanitize_name_strips_paths_and_hostile_names() {
        let s = FileTransfer::sanitize_name;
        assert_eq!(s("a/b/c.txt"), "c.txt");
        assert_eq!(s("..\\evil.exe"), "evil.exe");
        assert_eq!(s("C:/tmp/x"), "x");
        assert_eq!(s(".."), "_");
        assert_eq!(s("."), "_");
        assert_eq!(s(""), "_");
        assert_eq!(s("ok\u{1}name"), "ok_name");
        // SEC-1: drive-relative paths and alternate data streams.
        assert_eq!(s("D:evil.exe"), "D_evil.exe");
        assert_eq!(s("x.txt:ads"), "x.txt_ads");
        assert_eq!(s("C:"), "C_");
        assert_eq!(s("\\\\server\\share\\f.txt"), "f.txt");
        assert_eq!(s("\\\\?\\C:\\Windows\\x.dll"), "x.dll");
        // Windows-invalid characters.
        assert_eq!(s("a<b>c|d?e*f\"g"), "a_b_c_d_e_f_g");
        // Trailing dots / spaces are stripped by Windows; strip them first.
        assert_eq!(s("evil.exe. . "), "evil.exe");
        assert_eq!(s("..."), "_");
        assert_eq!(s("  spaced.txt"), "spaced.txt");
        // Reserved device names, any case / extension.
        assert_eq!(s("CON"), "_CON");
        assert_eq!(s("nul.txt"), "_nul.txt");
        assert_eq!(s("Com1.log"), "_Com1.log");
        assert_eq!(s("lpt9"), "_lpt9");
        assert_eq!(s("COM¹"), "_COM¹");
        assert_eq!(s("console.txt"), "console.txt");
        assert_eq!(s("com10"), "com10");
        // Unicode survives; length is capped with the extension kept.
        assert_eq!(s("résumé.pdf"), "résumé.pdf");
        let long = format!("{}.txt", "é".repeat(300));
        let out = s(&long);
        assert!(out.len() <= 255 && out.ends_with(".txt"), "{}", out.len());
    }

    #[test]
    fn sanitized_names_are_always_a_single_normal_component() {
        use std::path::{Component, Path};
        for name in [
            "a/b",
            "..",
            "../../etc/passwd",
            "C:\\x",
            "D:y",
            "\\\\srv\\s\\z",
            "/abs",
            "x:y:z",
            "",
            " . ",
        ] {
            let out = FileTransfer::sanitize_name(name);
            let comps: Vec<_> = Path::new(&out).components().collect();
            assert_eq!(comps.len(), 1, "{name:?} -> {out:?}");
            assert!(
                matches!(comps[0], Component::Normal(_)),
                "{name:?} -> {out:?}"
            );
            assert!(!out.contains(':'));
        }
    }

    #[test]
    fn unique_name_appends_counters() {
        use std::collections::HashSet;
        let taken: HashSet<&str> = ["a.txt", "a (1).txt", "README"].into();
        let exists = |n: &str| taken.contains(n);
        assert_eq!(unique_name("b.txt", exists).unwrap(), "b.txt");
        assert_eq!(unique_name("a.txt", exists).unwrap(), "a (2).txt");
        assert_eq!(unique_name("README", exists).unwrap(), "README (1)");
        assert_eq!(unique_name("x", |_| true), None);
        let long = "x".repeat(255);
        let u = unique_name(&long, |n| n == long).unwrap();
        assert!(u.len() <= 255 && u.ends_with(" (1)"), "{u}");
    }

    #[test]
    fn wire_roundtrip_through_encode_decode() {
        // The commands survive the terminal layer's encode/decode intact.
        let mut tx = FileTransfer::new();
        let mut rx = FileTransfer::new();
        let payload: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        for cmd in tx.send_commands("wire.bin", &payload, 1000).expect("send") {
            let wire = crate::terminal::encode(&cmd);
            let decoded = crate::terminal::decode(&wire).expect("decode");
            rx.poll(&decoded).expect("poll");
        }
        assert_eq!(rx.take().expect("completed").data, payload);
    }
}
