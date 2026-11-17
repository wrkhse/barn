//! The stall protocol on fd 3: JSON-RPC 2.0, one JSON object per line, <= 1 MiB.
//!
//! Reading is framed by a bounded splitter so an over-long line is discarded
//! with an error instead of growing the buffer or panicking. Requests we send
//! to the broker are correlated by numeric id through a pending map; every
//! call has a timeout and turns into a plugin-visible error.

use std::collections::HashMap;
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

pub const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcFailure {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// One framed inbound message, classified.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: u64,
        result: Result<Value, RpcFailure>,
    },
    Malformed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Text,
    Overlong,
    NonUtf8,
}

/// Bounded line splitter. Bytes are buffered only up to `max` per line; once a
/// line exceeds that, the rest of it is discarded until the next newline so
/// framing survives.
pub struct LineSplitter {
    max: usize,
    buf: Vec<u8>,
    discarding: bool,
}

impl LineSplitter {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            buf: Vec::new(),
            discarding: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<(Line, String)>) {
        for &byte in chunk {
            if byte == b'\n' {
                if self.discarding {
                    self.discarding = false;
                    out.push((Line::Overlong, String::new()));
                    continue;
                }
                let mut line = std::mem::take(&mut self.buf);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.len() > self.max {
                    out.push((Line::Overlong, String::new()));
                } else if line.is_empty() {
                    // Blank lines are not JSON-RPC messages; ignore them.
                } else {
                    match String::from_utf8(line) {
                        Ok(text) => out.push((Line::Text, text)),
                        Err(_) => out.push((Line::NonUtf8, String::new())),
                    }
                }
                continue;
            }
            if self.discarding {
                continue;
            }
            if self.buf.len() >= self.max {
                self.buf.clear();
                self.discarding = true;
                continue;
            }
            self.buf.push(byte);
        }
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Classify one JSON-RPC line. `jsonrpc` is not required on inbound messages:
/// the harness is the only peer, but a missing version field must not break us.
pub fn parse_inbound(text: &str) -> Inbound {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return Inbound::Malformed(format!("invalid JSON: {error}")),
    };
    let Some(object) = value.as_object() else {
        return Inbound::Malformed("message is not a JSON object".to_string());
    };

    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        return match object.get("id") {
            Some(id) if !id.is_null() => Inbound::Request {
                id: id.clone(),
                method: method.to_string(),
                params,
            },
            _ => Inbound::Notification {
                method: method.to_string(),
                params,
            },
        };
    }

    let Some(id) = object.get("id") else {
        return Inbound::Malformed("message has neither method nor id".to_string());
    };
    let Some(id) = id.as_u64() else {
        return Inbound::Malformed(format!("response id is not a number: {id}"));
    };
    if let Some(error) = object.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("broker error")
            .to_string();
        return Inbound::Response {
            id,
            result: Err(RpcFailure { code, message }),
        };
    }
    Inbound::Response {
        id,
        result: Ok(object.get("result").cloned().unwrap_or(Value::Null)),
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(String),
    Closed,
    Timeout,
    TooLarge(usize),
    Rpc(RpcFailure),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(message) => write!(f, "stall protocol io: {message}"),
            ProtocolError::Closed => write!(f, "stall protocol closed"),
            ProtocolError::Timeout => write!(f, "stall protocol request timed out"),
            ProtocolError::TooLarge(size) => {
                write!(f, "stall protocol line of {size} bytes exceeds {MAX_LINE}")
            }
            ProtocolError::Rpc(failure) => write!(f, "broker error: {failure}"),
        }
    }
}

pub struct Protocol {
    writer: AsyncMutex<OwnedWriteHalf>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcFailure>>>>,
    next_id: AtomicU64,
}

/// Take ownership of fd 3, the stall's end of the protocol socket.
pub fn open_fd3() -> Result<(Protocol, OwnedReadHalf), String> {
    // SAFETY: the harness spawns this process with fd 3 as one end of a Unix
    // socketpair, and this process takes ownership of that descriptor for the
    // rest of its life. Production barn passes a duplicated descriptor instead;
    // this spike takes fd 3 directly.
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    if let Err(error) = std_stream.peer_addr() {
        return Err(format!("fd 3 is not a connected Unix socket: {error}"));
    }
    std_stream
        .set_nonblocking(true)
        .map_err(|error| format!("fd 3 nonblocking: {error}"))?;
    let stream = tokio::net::UnixStream::from_std(std_stream)
        .map_err(|error| format!("fd 3 into tokio: {error}"))?;
    let (read_half, write_half) = stream.into_split();
    let protocol = Protocol {
        writer: AsyncMutex::new(write_half),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
    };
    Ok((protocol, read_half))
}

/// Read framed lines forever, classifying them for the dispatch loop.
pub async fn read_loop(read_half: OwnedReadHalf, sender: mpsc::UnboundedSender<Inbound>) {
    let mut reader = BufReader::new(read_half);
    let mut splitter = LineSplitter::new(MAX_LINE);
    let mut lines: Vec<(Line, String)> = Vec::new();

    loop {
        let consumed = {
            let chunk = match reader.fill_buf().await {
                Ok(chunk) => chunk,
                Err(error) => {
                    crate::log_line("error", &format!("stall protocol read: {error}"));
                    return;
                }
            };
            if chunk.is_empty() {
                crate::log_line(
                    "warn",
                    &format!(
                        "stall protocol closed at EOF; {} bytes of an unfinished line dropped",
                        splitter.buffered()
                    ),
                );
                return;
            }
            splitter.push(chunk, &mut lines);
            chunk.len()
        };
        reader.consume(consumed);

        for (kind, text) in lines.drain(..) {
            match kind {
                Line::Text => {
                    if sender.send(parse_inbound(&text)).is_err() {
                        return;
                    }
                }
                Line::Overlong => crate::log_line(
                    "error",
                    &format!("stall protocol line over {MAX_LINE} bytes discarded"),
                ),
                Line::NonUtf8 => {
                    crate::log_line("error", "stall protocol line was not valid UTF-8")
                }
            }
        }
    }
}

impl Protocol {
    /// Send a request to the broker and wait for its response.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProtocolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, wait) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id, reply);

        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.write_value(&message).await {
            self.forget(id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(failure))) => Err(ProtocolError::Rpc(failure)),
            Ok(Err(_)) => Err(ProtocolError::Closed),
            Err(_) => {
                self.forget(id);
                Err(ProtocolError::Timeout)
            }
        }
    }

    /// Reply to an inbound stall protocol request.
    pub async fn respond(&self, id: Value, result: Value) {
        let message = json!({"jsonrpc": "2.0", "id": id, "result": result});
        if let Err(error) = self.write_value(&message).await {
            crate::log_line("error", &format!("stall protocol respond: {error}"));
        }
    }

    pub async fn respond_error(&self, id: Value, code: i64, message: &str) {
        let body = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        if let Err(error) = self.write_value(&body).await {
            crate::log_line("error", &format!("stall protocol respond: {error}"));
        }
    }

    /// Deliver a broker response to the waiting request.
    pub fn deliver(&self, id: u64, result: Result<Value, RpcFailure>) -> bool {
        let waiting = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        match waiting {
            Some(reply) => reply.send(result).is_ok(),
            None => false,
        }
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
    }

    async fn write_value(&self, value: &Value) -> Result<(), ProtocolError> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|error| ProtocolError::Io(format!("serialize: {error}")))?;
        if bytes.len() + 1 > MAX_LINE {
            return Err(ProtocolError::TooLarge(bytes.len() + 1));
        }
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer
            .write_all(&bytes)
            .await
            .map_err(|error| ProtocolError::Io(error.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|error| ProtocolError::Io(error.to_string()))
    }
}
