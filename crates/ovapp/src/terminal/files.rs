//! File transfers on [`TerminalSession`]: the single-transfer FILE_BEGIN /
//! FILE_DATA / FILE_END path and the file-id-keyed FILE_XFER path.

use ovtransport::TransportError;

use super::{Command, TerminalSession};

impl TerminalSession {
    /// Send a file: FILE_BEGIN + FILE_DATA chunks + FILE_END.
    /// `chunk_size` defaults to 16 KiB when zero.
    pub fn send_file(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
    ) -> Result<(), TransportError> {
        let chunk = if chunk_size == 0 {
            16 * 1024
        } else {
            chunk_size
        };
        self.send(&Command::FileBegin {
            size: data.len() as u32,
            name: name.to_owned(),
        })?;
        for c in data.chunks(chunk.max(1)) {
            self.send(&Command::FileData(c.to_vec()))?;
        }
        self.send(&Command::FileEnd)
    }

    /// Take a completed inbound transfer: `(name, bytes)`. `None` when no
    /// transfer finished since the last call.
    pub fn take_file(&mut self) -> Option<(String, Vec<u8>)> {
        self.finished_files.pop_front()
    }

    /// Send a file with the file-id-keyed protocol: `FILE_XFER_DATA`
    /// chunks (16 KiB default) + one `FILE_XFER_COMPLETE`. Unlike
    /// [`send_file`](Self::send_file), multiple transfers can interleave and
    /// the receiver reassembles each independently.
    pub fn send_file_xfer(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
    ) -> Result<(), TransportError> {
        for cmd in self
            .xfer
            .send_commands(name, data, chunk_size)
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?
        {
            self.send(&cmd)?;
        }
        Ok(())
    }

    /// Take a completed inbound file-id-keyed transfer. `None` when no
    /// transfer finished since the last call.
    pub fn take_xfer_file(&mut self) -> Option<crate::file_transfer::ReceivedFile> {
        self.xfer.take()
    }
}
