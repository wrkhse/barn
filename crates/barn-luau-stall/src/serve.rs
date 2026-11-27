//! The line-framed server a Luau stall runs on its authenticated connection.
//!
//! Each line is one JSON object: a JSON-RPC control message for health and
//! drain, or a framed hook and tool request from barn. Calls run on the
//! blocking pool so a health check is answered while a plugin is busy. Every
//! reply is one line of JSON ending in a newline, and every failure has a
//! name in [`ServeError`].

use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use barn_luau::Sandbox;
use barn_stall::{MAX_LINE_BYTES, Reply, Request, method, protocol::DRAIN_DEADLINE_MS};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::UnixStream;
use tokio::task::{self, JoinSet};
use tokio::time::{Duration, sleep, timeout};

use crate::bridge::handle_request;

/// How long a reply write may take before the connection is stalled.
///
/// The same bound as a drain: a peer that stops reading cannot hold the stall
/// past the time barn gives in-flight calls to finish.
const REPLY_TIMEOUT: Duration = Duration::from_millis(DRAIN_DEADLINE_MS);

/// The name on a call still running when the drain deadline passes.
const DRAIN_DENY: &str = "stall_draining";
const DRAIN_DENY_MESSAGE: &str = "in-flight call did not finish before the drain deadline";

/// The name on a call whose worker failed before it could answer.
const LOST_DENY: &str = "stall_unavailable";
const LOST_DENY_MESSAGE: &str = "the stall did not answer";

/// Why a connection could not finish the exchange.
#[derive(Debug)]
pub enum ServeError {
    /// A line reached the 8 MiB frame limit without a newline.
    Oversize,
    /// A line was not JSON, or was not a known control or framed request.
    Malformed(serde_json::Error),
    /// The peer closed or its read failed before a complete line arrived.
    Closed,
    /// A reply could not be written within [`REPLY_TIMEOUT`].
    StalledReply,
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize => formatter.write_str("a request line exceeded MAX_LINE_BYTES"),
            Self::Malformed(error) => {
                write!(formatter, "a request line is malformed: {error}")
            }
            Self::Closed => formatter.write_str("the peer closed before the exchange finished"),
            Self::StalledReply => formatter.write_str("a reply stalled before it was written"),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(error) => Some(error),
            _ => None,
        }
    }
}

/// One line decoded as either a control message or a framed request.
enum Frame {
    Health(Value),
    Drain(Value),
    Request(Request),
}

/// An event the main loop can act on without holding a read borrow to write.
enum Event {
    Line(Vec<u8>),
    Call(Result<(task::Id, Reply), task::JoinError>),
}

/// Serve frames on the stream until the peer closes or asks to drain.
///
/// # Errors
///
/// Returns [`ServeError::Oversize`] for a line at the frame limit without a
/// newline, [`ServeError::Malformed`] for an unknown or unparseable line,
/// [`ServeError::Closed`] when the peer goes away, and
/// [`ServeError::StalledReply`] when a reply cannot be written in time. A drain
/// ends the loop with ok after its in-flight calls finish or are denied.
pub async fn serve(stream: UnixStream, sandbox: Sandbox) -> Result<(), ServeError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let sandbox = Arc::new(sandbox);
    let mut calls = JoinSet::new();
    let mut pending = BTreeMap::new();

    loop {
        let event = if calls.is_empty() {
            Event::Line(read_line(&mut reader).await?)
        } else {
            tokio::select! {
                line = read_line(&mut reader) => Event::Line(line?),
                Some(joined) = calls.join_next_with_id() => Event::Call(joined),
            }
        };

        match event {
            Event::Line(line) => match decode(&line)? {
                Frame::Health(id) => {
                    write_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": id, "result": null,
                        }),
                    )
                    .await?;
                }
                Frame::Drain(id) => {
                    write_json(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": id, "result": null,
                        }),
                    )
                    .await?;

                    return drain(&mut writer, &mut calls, &mut pending).await;
                }
                Frame::Request(request) => {
                    let id = request_id(&request);
                    let worker = Arc::clone(&sandbox);
                    let call = calls.spawn_blocking(move || handle_request(&worker, request));
                    pending.insert(call.id(), id);
                }
            },
            Event::Call(joined) => match joined {
                Ok((task, reply)) => {
                    pending.remove(&task);
                    write_reply(&mut writer, reply).await?;
                }
                Err(error) => {
                    if let Some(id) = pending.remove(&error.id()) {
                        write_reply(&mut writer, lost(id)).await?;
                    }
                }
            },
        }
    }
}

/// Read one newline-terminated line, bounded at [`MAX_LINE_BYTES`].
async fn read_line(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>, ServeError> {
    let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX);
    let mut line = Vec::new();

    match reader.take(limit).read_until(b'\n', &mut line).await {
        Ok(_) => {}
        Err(_) => return Err(ServeError::Closed),
    }

    if line.last() == Some(&b'\n') {
        Ok(line)
    } else if line.len() == MAX_LINE_BYTES {
        Err(ServeError::Oversize)
    } else {
        Err(ServeError::Closed)
    }
}

/// Decode a control message by its method, or the line as a framed request.
fn decode(line: &[u8]) -> Result<Frame, ServeError> {
    let envelope: Value = serde_json::from_slice(line).map_err(ServeError::Malformed)?;

    match envelope.get("method").and_then(Value::as_str) {
        Some(method::STALL_HEALTH) => Ok(Frame::Health(id(&envelope))),
        Some(method::STALL_DRAIN) => Ok(Frame::Drain(id(&envelope))),
        _ => serde_json::from_slice(line)
            .map(Frame::Request)
            .map_err(ServeError::Malformed),
    }
}

/// The id a control reply repeats, or null when the request carried none.
fn id(envelope: &Value) -> Value {
    envelope.get("id").cloned().unwrap_or(Value::Null)
}

/// The id a framed request needs repeated in its reply.
const fn request_id(request: &Request) -> u64 {
    match request {
        Request::Hook { id, .. } | Request::Tool { id, .. } => *id,
    }
}

/// Write a framed reply as one bounded line.
async fn write_reply(
    writer: &mut (impl AsyncWrite + Unpin),
    reply: Reply,
) -> Result<(), ServeError> {
    let value = serde_json::to_value(reply).map_err(ServeError::Malformed)?;
    write_json(writer, &value).await
}

/// Write one line within [`REPLY_TIMEOUT`], refusing one too long to frame.
async fn write_json(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &Value,
) -> Result<(), ServeError> {
    let mut line = serde_json::to_vec(value).map_err(ServeError::Malformed)?;
    line.push(b'\n');

    if line.len() > MAX_LINE_BYTES {
        return Err(ServeError::Oversize);
    }

    timeout(REPLY_TIMEOUT, writer.write_all(&line))
        .await
        .map_err(|_| ServeError::StalledReply)?
        .map_err(|_| ServeError::StalledReply)
}

/// A reply naming a call whose worker failed before it answered.
fn lost(id: u64) -> Reply {
    Reply::Error {
        id,
        error: LOST_DENY.into(),
        message: LOST_DENY_MESSAGE.into(),
    }
}

/// Answer the calls already in flight, then stop before the deadline passes.
///
/// Calls that finish in time keep their own replies. On the deadline the rest
/// get a named deny and are abandoned; barn never waits on a stalled plugin.
async fn drain(
    writer: &mut (impl AsyncWrite + Unpin),
    calls: &mut JoinSet<Reply>,
    pending: &mut BTreeMap<task::Id, u64>,
) -> Result<(), ServeError> {
    let mut deadline = Box::pin(sleep(Duration::from_millis(DRAIN_DEADLINE_MS)));

    loop {
        tokio::select! {
            () = &mut deadline => {
                for (_, id) in std::mem::take(pending) {
                    write_reply(writer, Reply::Error {
                        id,
                        error: DRAIN_DENY.into(),
                        message: DRAIN_DENY_MESSAGE.into(),
                    })
                    .await?;
                }
                calls.abort_all();
                return Ok(());
            }
            Some(joined) = calls.join_next_with_id(), if !calls.is_empty() => {
                match joined {
                    Ok((task, reply)) => {
                        pending.remove(&task);
                        write_reply(writer, reply).await?;
                    }
                    Err(error) => {
                        if let Some(id) = pending.remove(&error.id()) {
                            write_reply(writer, lost(id)).await?;
                        }
                    }
                }
            }
        }
    }
}
