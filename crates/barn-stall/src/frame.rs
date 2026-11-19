//! Lines: the framing every message travels in.
//!
//! One message per line, ended by `\n` and never by `\r\n`. A read keeps the
//! bytes of a line it has not finished, so a read that is dropped before the
//! line is complete loses nothing: the next read starts where the last one
//! stopped.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::MAX_LINE_BYTES;
use crate::rpc::{Message, RpcError, StrictReason};

/// The most one read asks for, so a line never needs a buffer the size of the
/// cap. The cap itself is still the limit; this only bounds each read call.
const CHUNK: usize = 65_536;

/// The most blank lines a peer may send in a row between two messages: 1024.
///
/// One more is refused with [`RpcError::Strict`] carrying
/// [`StrictReason::TooManyBlankLines`], so a peer cannot keep the reader busy
/// on nothing.
pub const MAX_BLANK_LINES: usize = 1_024;

/// A peer read and written as lines: the two halves of its socket.
pub struct Lines<R, W> {
    reader: R,
    writer: W,
    buffer: Vec<u8>,
    scratch: Vec<u8>,
    max_line: usize,
    poisoned: bool,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Lines<R, W> {
    /// Reads and writes lines on this peer, under the protocol's cap.
    pub fn new(reader: R, writer: W) -> Self {
        Self::with_max_line(reader, writer, MAX_LINE_BYTES)
    }

    /// Reads and writes lines on this peer, under a cap of this many bytes, a
    /// line's newline included.
    pub fn with_max_line(reader: R, writer: W, max_line: usize) -> Self {
        Self {
            reader,
            writer,
            buffer: Vec::new(),
            scratch: vec![0; CHUNK.min(max_line)],
            max_line,
            poisoned: false,
        }
    }

    /// Reads the next message.
    ///
    /// A blank line between messages is skipped, but no more than
    /// [`MAX_BLANK_LINES`] in a row: one more is refused with
    /// [`RpcError::Strict`], for too many blank lines, so a peer cannot keep
    /// the reader busy on nothing. The bytes of a line that is not finished
    /// stay in the buffer across calls, so a read dropped part way through one
    /// leaves them for the next.
    ///
    /// # Errors
    ///
    /// [`RpcError::OverLimit`] for a line over the cap, and
    /// [`RpcError::Strict`] with the [`StrictReason`] that names the rule:
    /// `\r\n`, a line that is not UTF-8, too many blank lines, or a message
    /// JSON-RPC 2.0 does not allow. Each of those poisons the lines, so every
    /// later read returns [`RpcError::Poisoned`]: the caller answers `-32600`
    /// once and closes. [`RpcError::PartialLine`] when the stream ends inside
    /// a line, and [`RpcError::Io`] when it fails.
    pub async fn read(&mut self) -> Result<Message, RpcError> {
        if self.poisoned {
            return Err(RpcError::Poisoned);
        }

        let mut blank_lines = 0_usize;
        let result = loop {
            match self.next_line().await {
                Ok(line) if line.is_empty() => {
                    blank_lines = blank_lines.saturating_add(1);

                    if blank_lines > MAX_BLANK_LINES {
                        break Err(RpcError::Strict(StrictReason::TooManyBlankLines));
                    }
                }
                Ok(line) => break Message::parse(&line),
                Err(error) => break Err(error),
            }
        };

        match result {
            Ok(message) => Ok(message),
            Err(error) => {
                if matches!(error, RpcError::Strict(_) | RpcError::OverLimit) {
                    self.poisoned = true;
                }

                Err(error)
            }
        }
    }

    /// Writes one message as a line.
    ///
    /// # Errors
    ///
    /// [`RpcError::OverLimit`] when the line would be longer than the cap; the
    /// stream is left alone, so the caller can answer `-32603` and close.
    /// [`RpcError::Io`] when the stream fails.
    pub async fn write(&mut self, message: &Message) -> Result<(), RpcError> {
        let mut line =
            serde_json::to_vec(message).map_err(|error| RpcError::Io(error.to_string()))?;
        line.push(b'\n');

        if line.len() > self.max_line {
            return Err(RpcError::OverLimit);
        }

        self.writer
            .write_all(&line)
            .await
            .map_err(|error| io_error(&error))?;

        Ok(())
    }

    /// The next line, without its newline.
    async fn next_line(&mut self) -> Result<String, RpcError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                return self.take(end);
            }

            self.fill().await?;
        }
    }

    /// Takes the line that ends at this newline, and checks it.
    fn take(&mut self, end: usize) -> Result<String, RpcError> {
        // The drain's drop removes the newline the content stops before.
        let content: Vec<u8> = self.buffer.drain(..=end).take(end).collect();

        if content.last() == Some(&b'\r') {
            return Err(RpcError::Strict(StrictReason::CarriageReturn));
        }

        std::str::from_utf8(&content)
            .map(str::to_owned)
            .map_err(|_| RpcError::Strict(StrictReason::NotUtf8))
    }

    /// Reads what the stream has, at most as far as the cap allows.
    async fn fill(&mut self) -> Result<(), RpcError> {
        let room = self.room();

        if room == 0 {
            return Err(RpcError::OverLimit);
        }

        let limit = u64::try_from(room).map_err(|_| RpcError::OverLimit)?;
        let count = (&mut self.reader)
            .take(limit)
            .read(&mut self.scratch)
            .await
            .map_err(|error| io_error(&error))?;

        if count == 0 {
            return self.at_end();
        }

        self.buffer.extend(self.scratch.iter().take(count).copied());

        Ok(())
    }

    /// The bytes left before the cap is reached.
    const fn room(&self) -> usize {
        self.max_line.saturating_sub(self.buffer.len())
    }

    /// What a stream that ended says.
    fn at_end(&self) -> Result<(), RpcError> {
        if self.buffer.is_empty() {
            Err(RpcError::Io("the stream closed".to_owned()))
        } else {
            Err(RpcError::PartialLine)
        }
    }
}

/// The protocol's own words for a stream that failed.
fn io_error(error: &std::io::Error) -> RpcError {
    RpcError::Io(error.to_string())
}
