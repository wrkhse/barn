//! The line-framed server a Luau stall runs on its authenticated connection.
//!
//! Each line is one JSON object: a JSON-RPC control message for health and
//! drain, or a framed hook and tool request from barn. Calls run on the
//! blocking pool so a health check is answered while a plugin is busy. Every
//! reply is one line of JSON ending in a newline, and every failure has a
//! name in [`ServeError`]. Calls are bounded: a request past the cap gets a
//! named busy deny instead of a worker. Control calls made by a plugin arrive
//! on a separate channel, get one framed request written to the peer, and are
//! answered by the next matching control or error line.

use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use barn_luau::Sandbox;
use barn_stall::protocol::deny::{OVERSIZE_REPLY, STALL_BUSY, STALL_DRAINING, STALL_UNAVAILABLE};
use barn_stall::{
    DENIED_CODE, MAX_LINE_BYTES, Reply, Request, deny, method, protocol::DRAIN_DEADLINE_MS,
};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{self, JoinSet};
use tokio::time::{Duration, Instant, sleep_until, timeout, timeout_at};

use crate::bridge::handle_request;
use crate::control::{self, ControlCall};

/// Emit a timestamped diagnostic line when `BARN_TEST_TRACE` is set.
pub fn trace(message: impl FnOnce() -> String) {
    if std::env::var_os("BARN_TEST_TRACE").is_some() {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(std::time::Instant::now);
        eprintln!("STALL TRACE {:?} {}", start.elapsed(), message());
    }
}

/// A short diagnostic tag for a joined call: its task id and outcome.
fn joined_tag(kind: &str, id: task::Id) -> String {
    format!("task={id:?} {kind}")
}

/// The cap on calls a stall runs at once.
///
/// A request that arrives with this many calls in flight is answered with
/// [`STALL_BUSY`] and never reaches the sandbox.
pub const MAX_IN_FLIGHT: usize = 8;

/// How long a reply write may take before the connection is stalled.
///
/// The same bound as a drain: a peer that stops reading cannot hold the stall
/// past the time barn gives in-flight calls to finish.
const REPLY_TIMEOUT: Duration = Duration::from_millis(DRAIN_DEADLINE_MS);

/// How long a control call waits for its framed reply.
const CONTROL_TIMEOUT: Duration = REPLY_TIMEOUT;

/// The message on a control frame too large to write to the peer.
const CONTROL_FRAME_ERROR: &str = "control call frame exceeds the size cap";

/// The message on a control call whose reply never arrived.
const CONTROL_TIMEOUT_ERROR: &str = "control call timed out";

/// The message on a call still running when the drain deadline passes.
const DRAIN_DENY_MESSAGE: &str = "in-flight call did not finish before the drain deadline";

/// The message on a call whose worker failed before it could answer.
const LOST_DENY_MESSAGE: &str = "the stall did not answer";

/// The message on a request that arrives with no free call slot.
const BUSY_DENY_MESSAGE: &str = "too many in-flight calls";

/// The message on a reply too long to frame.
const OVERSIZE_REPLY_MESSAGE: &str = "the reply exceeds MAX_LINE_BYTES";

/// A control call waiting for its reply: id, reply channel and deadline.
type PendingControl = (
    u64,
    oneshot::Sender<Result<serde_json::Value, String>>,
    Instant,
);

/// Why a connection could not finish the exchange.
#[derive(Debug)]
pub enum ServeError {
    /// A line reached the frame limit without a newline.
    Oversize,
    /// A line was not JSON, or was not a known control or framed request.
    Malformed(serde_json::Error),
    /// A reply this stall built could not be serialized.
    Encode(serde_json::Error),
    /// The peer closed or its read failed before a complete line arrived.
    Closed,
    /// A reply could not be written within its bound.
    StalledReply,
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize => formatter.write_str("a request line exceeded MAX_LINE_BYTES"),
            Self::Malformed(error) => {
                write!(formatter, "a request line is malformed: {error}")
            }
            Self::Encode(error) => {
                write!(formatter, "a reply could not be encoded: {error}")
            }
            Self::Closed => formatter.write_str("the peer closed before the exchange finished"),
            Self::StalledReply => formatter.write_str("a reply stalled before it was written"),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(error) | Self::Encode(error) => Some(error),
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
    Line,
    Call(Result<(task::Id, Reply), task::JoinError>),
}

/// Serve frames on the stream until the peer closes or asks to drain.
///
/// At most [`MAX_IN_FLIGHT`] calls run at once; a request that arrives at the
/// cap is answered with stall busy and never reaches the sandbox. After a
/// busy deny the loop awaits a call completion before reading another line.
/// Control calls arrive on a separate channel: one is written to the peer at
/// a time and its reply is matched by id before the line reaches normal
/// decoding. For a later slice to confirm: the exact cap value.
///
/// # Errors
///
/// Returns [`ServeError::Oversize`] for a line at the frame limit without a
/// newline, [`ServeError::Malformed`] for an unknown or unparseable line,
/// [`ServeError::Encode`] when one of our replies cannot be serialized,
/// [`ServeError::Closed`] when the peer goes away or a write fails, and
/// [`ServeError::StalledReply`] when a reply cannot be written in time. A
/// drain ends the loop with ok after its in-flight calls finish or are denied.
pub async fn serve(
    stream: UnixStream,
    sandbox: Sandbox,
    mut control_rx: mpsc::UnboundedReceiver<ControlCall>,
) -> Result<(), ServeError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let sandbox = Arc::new(sandbox);
    let mut calls = JoinSet::new();
    let mut pending = BTreeMap::new();
    let mut pending_control: Option<PendingControl> = None;
    let mut line = Vec::new();
    let mut need_slot = false;

    loop {
        let control_deadline = pending_control.as_ref().map(|&(_, _, deadline)| deadline);

        let event = tokio::select! {
            event = next_event(
                &mut reader,
                &mut writer,
                &mut line,
                &mut calls,
                &mut need_slot,
                pending_control.is_some(),
            ) => event?,

            Some(call) = control_rx.recv(), if control_deadline.is_none() => {
                queue_control(&mut writer, &mut pending_control, call).await?;

                continue;
            }

            () = control_wait(control_deadline), if control_deadline.is_some() => {
                expire_control(&mut pending_control);

                continue;
            }
        };
        trace(|| match &event {
            Event::Line => "event: line".to_owned(),
            Event::Call(_) => "event: call".to_owned(),
        });

        match event {
            Event::Line => {
                let drained = handle_line_event(
                    &mut writer,
                    &mut line,
                    &mut calls,
                    &mut pending,
                    &mut need_slot,
                    &mut pending_control,
                    &sandbox,
                )
                .await?;
                if drained {
                    return Ok(());
                }
            }
            Event::Call(joined) => {
                trace(|| "finish_call start".to_owned());
                let result = finish_call(&mut writer, &mut pending, joined).await;
                match &result {
                    Ok(()) => trace(|| "finish_call done".to_owned()),
                    Err(error) => trace(|| format!("finish_call error {error:?}")),
                }
                result?;
            }
        }
    }
}

/// Handle one decoded line: route a matching control reply, skip a control
/// envelope that answers no pending call, then decode and dispatch the frame.
///
/// Returns whether the frame was a drain, which ends the connection.
async fn handle_line_event(
    writer: &mut (impl AsyncWrite + Unpin),
    line: &mut Vec<u8>,
    calls: &mut JoinSet<Reply>,
    pending: &mut BTreeMap<task::Id, u64>,
    need_slot: &mut bool,
    pending_control: &mut Option<PendingControl>,
    sandbox: &Arc<Sandbox>,
) -> Result<bool, ServeError> {
    if take_control_reply(line, pending_control) {
        return Ok(false);
    }

    if is_foreign_control_envelope(line) {
        trace(|| "skip foreign control envelope".to_owned());
        line.clear();

        return Ok(false);
    }

    let frame = decode_frame(writer, line).await?;
    trace(|| match &frame {
        Frame::Health(_) => "frame: health".to_owned(),
        Frame::Drain(_) => "frame: drain".to_owned(),
        Frame::Request(_) => "frame: request".to_owned(),
    });

    let drained = matches!(frame, Frame::Drain(_));
    handle_frame(writer, calls, pending, need_slot, frame, sandbox).await?;

    Ok(drained)
}

/// Whether a line is a control envelope that answers no pending call.
///
/// A late reply after a timeout, or a duplicate reply, carries `kind` set to
/// "control" or "error" and a numeric `version`, whatever its id. Barn's own
/// requests carry a `type` tag instead, so they keep the normal decode path.
fn is_foreign_control_envelope(line: &[u8]) -> bool {
    let Ok(envelope) = serde_json::from_slice::<Value>(line) else {
        return false;
    };

    let control_kind = matches!(
        envelope.get("kind").and_then(Value::as_str),
        Some("control" | "error")
    );

    control_kind && envelope.get("version").is_some_and(Value::is_number)
}

/// Write one queued control request and remember its reply channel.
///
/// An oversize frame is refused to the caller without being written. The
/// encode step already caps frames, so this is a second, explicit bound.
async fn queue_control(
    writer: &mut (impl AsyncWrite + Unpin),
    pending_control: &mut Option<PendingControl>,
    call: ControlCall,
) -> Result<(), ServeError> {
    if call.frame.len() > tack_protocol::MAX_FRAME_BYTES {
        let _ = call.reply.send(Err(CONTROL_FRAME_ERROR.to_owned()));

        return Ok(());
    }

    write_line(writer, &call.frame).await?;
    *pending_control = Some((call.id, call.reply, deadline_after(CONTROL_TIMEOUT)));

    Ok(())
}

/// Wait for the pending control deadline, or forever while none is pending.
async fn control_wait(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Answer a timed-out control call with the named timeout error.
fn expire_control(pending_control: &mut Option<PendingControl>) {
    if let Some((id, reply, _)) = pending_control.take() {
        trace(|| format!("control call timed out id={id}"));
        let _ = reply.send(Err(CONTROL_TIMEOUT_ERROR.to_owned()));
    }
}

/// Route a control reply to its waiting caller and clear its line.
///
/// Returns whether the line answered the pending control call; an unrelated
/// line stays in the buffer for the normal decode path.
fn take_control_reply(line: &mut Vec<u8>, pending_control: &mut Option<PendingControl>) -> bool {
    let Some((id, _, _)) = pending_control.as_ref() else {
        return false;
    };
    let id = *id;

    if !control::is_control_reply(line, id) {
        return false;
    }

    let result = control::decode_control_reply(line, id);
    line.clear();

    if let Some((_, reply, _)) = pending_control.take() {
        let _ = reply.send(result);
    }

    true
}

/// Handle one decoded frame under the same cap and write bounds as before.
///
/// A drain ends the connection after its calls, so the caller watches for
/// that frame and stops its loop once the reply has been written. A request
/// at the cap denies once and asks the caller for a call slot before reading.
async fn handle_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    calls: &mut JoinSet<Reply>,
    pending: &mut BTreeMap<task::Id, u64>,
    need_slot: &mut bool,
    frame: Frame,
    sandbox: &Arc<Sandbox>,
) -> Result<(), ServeError> {
    match frame {
        Frame::Health(id) => {
            let id_tag = format!("id={id:?}");
            trace(|| format!("health write start {id_tag}"));
            let result = write_health(writer, id).await;
            match &result {
                Ok(()) => trace(|| format!("health write done {id_tag}")),
                Err(error) => {
                    trace(|| format!("health write error {id_tag} error={error:?}"));
                }
            }
            result?;
        }
        Frame::Drain(id) => {
            return drain_and_reply(writer, calls, pending, id).await;
        }
        Frame::Request(request) => {
            let id = request_id(&request);

            if calls.len() >= MAX_IN_FLIGHT {
                let result = write_reply(writer, busy(id)).await;
                if let Err(error) = &result {
                    trace(|| format!("busy deny write error id={id} {error:?}"));
                }
                result?;
                *need_slot = true;

                return Ok(());
            }

            let worker = Arc::clone(sandbox);
            let call = calls.spawn_blocking(move || handle_request(&worker, request));
            let task = call.id();
            trace(|| format!("request worker spawned task={task:?} id={id}"));
            pending.insert(task, id);
        }
    }
    Ok(())
}

/// An event for a finished call, traced with the branch that observed it.
fn call_event(branch: &str, joined: Result<(task::Id, Reply), task::JoinError>) -> Event {
    trace(|| {
        let (kind, id) = match &joined {
            Ok((task, _)) => ("answered", *task),
            Err(error) => ("join_error", error.id()),
        };
        format!(
            "next_event call complete ({branch}) {}",
            joined_tag(kind, id)
        )
    });
    Event::Call(joined)
}

/// Read one line into an event, naming a read error on the wire first.
async fn read_event(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    line: &mut Vec<u8>,
) -> Result<Event, ServeError> {
    if let Err(error) = read_or_report(reader, writer, line).await {
        trace(|| format!("next_event read error {error:?}"));

        return Err(error);
    }

    Ok(Event::Line)
}

/// Choose the next event, giving a busy stall a call slot before another read.
///
/// While a control call is pending the read stays live even in the need-slot
/// branch, so a late or duplicate control reply can be read and skipped.
async fn next_event(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    line: &mut Vec<u8>,
    calls: &mut JoinSet<Reply>,
    need_slot: &mut bool,
    control_pending: bool,
) -> Result<Event, ServeError> {
    if *need_slot && !calls.is_empty() {
        if control_pending {
            let event = tokio::select! {
                read = read_or_report(reader, writer, line) => match read {
                    Ok(()) => Event::Line,
                    Err(error) => {
                        trace(|| format!("next_event read error (need_slot) {error:?}"));
                        return Err(error);
                    }
                },
                Some(joined) = calls.join_next_with_id() => call_event("need_slot", joined),
            };
            *need_slot = false;

            return Ok(event);
        }
        if let Some(joined) = calls.join_next_with_id().await {
            *need_slot = false;

            return Ok(call_event("need_slot", joined));
        }
    }

    *need_slot = false;

    if calls.is_empty() {
        return read_event(reader, writer, line).await;
    }

    tokio::select! {
        read = read_or_report(reader, writer, line) => match read {
            Ok(()) => Ok(Event::Line),
            Err(error) => {
                trace(|| format!("next_event read error (select) {error:?}"));
                Err(error)
            }
        },
        Some(joined) = calls.join_next_with_id() => Ok(call_event("select", joined)),
    }
}

/// Decode a complete line, putting its named error on the wire first.
async fn decode_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    line: &mut Vec<u8>,
) -> Result<Frame, ServeError> {
    let decoded = decode(line);
    line.clear();

    match decoded {
        Ok(frame) => Ok(frame),
        Err(error) => {
            report_decode_error(writer, &error).await?;

            Err(error)
        }
    }
}

/// Write a control reply with the matching id.
async fn write_health(writer: &mut (impl AsyncWrite + Unpin), id: Value) -> Result<(), ServeError> {
    let value = json!({
        "jsonrpc": "2.0", "id": id, "result": null,
    });
    trace(|| {
        format!(
            "health json bytes={:?}",
            serde_json::to_vec(&value).map(|bytes| bytes.len())
        )
    });
    let result = write_json(writer, &value).await;
    match &result {
        Ok(()) => trace(|| "health write_json ok".to_owned()),
        Err(error) => trace(|| format!("health write_json error={error:?}")),
    }

    result
}

/// Drain first, then answer the drain itself with null.
async fn drain_and_reply(
    writer: &mut (impl AsyncWrite + Unpin),
    calls: &mut JoinSet<Reply>,
    pending: &mut BTreeMap<task::Id, u64>,
    id: Value,
) -> Result<(), ServeError> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(DRAIN_DEADLINE_MS))
        .unwrap_or_else(Instant::now);

    drain(writer, calls, pending, deadline).await?;
    write_health(writer, id).await
}

/// Deliver a finished call, or a named deny for a worker that did not answer.
async fn finish_call(
    writer: &mut (impl AsyncWrite + Unpin),
    pending: &mut BTreeMap<task::Id, u64>,
    joined: Result<(task::Id, Reply), task::JoinError>,
) -> Result<(), ServeError> {
    match joined {
        Ok((task, reply)) => {
            pending.remove(&task);
            write_reply(writer, reply).await
        }
        Err(error) => match pending.remove(&error.id()) {
            Some(id) => write_reply(writer, lost(id)).await,
            None => Ok(()),
        },
    }
}

/// Append one newline-terminated line into a buffer that survives cancellation.
///
/// The limit is what is left of [`MAX_LINE_BYTES`], so a read cancelled by a
/// call completion keeps the bytes it already appended. A complete line is
/// consumed by the caller, which clears the buffer only after decoding it.
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> Result<(), ServeError> {
    let room = MAX_LINE_BYTES.saturating_sub(line.len());
    let limit = u64::try_from(room).unwrap_or(u64::MAX);

    match reader.take(limit).read_until(b'\n', line).await {
        Ok(_) => {}
        Err(_) => return Err(ServeError::Closed),
    }

    if line.last() == Some(&b'\n') {
        Ok(())
    } else if line.len() == MAX_LINE_BYTES {
        Err(ServeError::Oversize)
    } else {
        Err(ServeError::Closed)
    }
}

/// Read a line, putting its named error on the wire before returning it.
async fn read_or_report(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    line: &mut Vec<u8>,
) -> Result<(), ServeError> {
    match read_line(reader, line).await {
        Ok(()) => Ok(()),
        Err(error) => {
            report_decode_error(writer, &error).await?;

            Err(error)
        }
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

/// The id a framed reply repeats.
const fn reply_id(reply: &Reply) -> u64 {
    match reply {
        Reply::Hook { id, .. } | Reply::Tool { id, .. } | Reply::Error { id, .. } => *id,
    }
}

/// Write one bounded line, mapping a failed write to a closed peer.
async fn write_within(
    writer: &mut (impl AsyncWrite + Unpin),
    line: &[u8],
    deadline: Instant,
) -> Result<(), ServeError> {
    match timeout_at(deadline, writer.write_all(line)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(ServeError::Closed),
        Err(_) => Err(ServeError::StalledReply),
    }
}

/// An instant a duration from now, falling back to now if it overflows.
fn deadline_after(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now)
}

/// Write one line within `REPLY_TIMEOUT`.
async fn write_line(writer: &mut (impl AsyncWrite + Unpin), line: &[u8]) -> Result<(), ServeError> {
    write_within(writer, line, deadline_after(REPLY_TIMEOUT)).await
}

/// Write a control reply as one bounded line.
async fn write_json(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &Value,
) -> Result<(), ServeError> {
    let mut line = serde_json::to_vec(value).map_err(ServeError::Encode)?;
    line.push(b'\n');

    if line.len() > MAX_LINE_BYTES {
        return Err(ServeError::Oversize);
    }

    write_line(writer, &line).await
}

/// Write a framed reply, replacing one too long to frame with a named deny.
async fn write_reply(
    writer: &mut (impl AsyncWrite + Unpin),
    reply: Reply,
) -> Result<(), ServeError> {
    write_reply_until(writer, reply, deadline_after(REPLY_TIMEOUT)).await
}

/// Write a framed reply, bounding its deny write by a caller's deadline.
async fn write_reply_until(
    writer: &mut (impl AsyncWrite + Unpin),
    reply: Reply,
    deadline: Instant,
) -> Result<(), ServeError> {
    let id = reply_id(&reply);
    let mut line = serde_json::to_vec(&reply).map_err(ServeError::Encode)?;
    line.push(b'\n');

    if line.len() > MAX_LINE_BYTES {
        let error = Reply::Error {
            id,
            error: OVERSIZE_REPLY.into(),
            message: OVERSIZE_REPLY_MESSAGE.into(),
        };
        let mut error_line = serde_json::to_vec(&error).map_err(ServeError::Encode)?;
        error_line.push(b'\n');

        return write_within(writer, &error_line, deadline).await;
    }

    write_within(writer, &line, deadline).await
}

/// Put a named error on the wire for a line that could not be decoded.
async fn report_decode_error(
    writer: &mut (impl AsyncWrite + Unpin),
    error: &ServeError,
) -> Result<(), ServeError> {
    match error {
        ServeError::Oversize => {
            write_json(
                writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": DENIED_CODE,
                        "message": "denied",
                        "data": {"reason": deny::OVERSIZE_FRAME},
                    },
                }),
            )
            .await
        }
        ServeError::Malformed(_) => {
            write_json(
                writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": DENIED_CODE,
                        "message": "denied",
                        "data": {"reason": deny::BAD_FRAME},
                    },
                }),
            )
            .await
        }
        _ => Ok(()),
    }
}

/// A reply naming a call whose worker failed before it answered.
fn lost(id: u64) -> Reply {
    Reply::Error {
        id,
        error: STALL_UNAVAILABLE.into(),
        message: LOST_DENY_MESSAGE.into(),
    }
}

/// A reply naming a call that arrived with no free slot.
fn busy(id: u64) -> Reply {
    Reply::Error {
        id,
        error: STALL_BUSY.into(),
        message: BUSY_DENY_MESSAGE.into(),
    }
}

/// Answer the calls already in flight, then stop before the deadline passes.
///
/// Calls that finish in time keep their own replies. Denies made before the
/// deadline share it, so writing them cannot extend the drain. At the
/// deadline the rest get a named deny with a fresh bound — the shared deadline
/// has passed by then — and are aborted; barn never waits on a stalled plugin.
async fn drain(
    writer: &mut (impl AsyncWrite + Unpin),
    calls: &mut JoinSet<Reply>,
    pending: &mut BTreeMap<task::Id, u64>,
    deadline: Instant,
) -> Result<(), ServeError> {
    if calls.is_empty() {
        return Ok(());
    }

    loop {
        match timeout_at(deadline, calls.join_next_with_id()).await {
            Ok(Some(joined)) => match joined {
                Ok((task, reply)) => {
                    pending.remove(&task);
                    write_reply_until(writer, reply, deadline).await?;
                }
                Err(error) => {
                    if let Some(id) = pending.remove(&error.id()) {
                        write_reply_until(writer, lost(id), deadline).await?;
                    }
                }
            },
            Ok(None) => return Ok(()),
            Err(_) => {
                timeout(REPLY_TIMEOUT, async {
                    for (_, id) in std::mem::take(pending) {
                        write_reply(
                            writer,
                            Reply::Error {
                                id,
                                error: STALL_DRAINING.into(),
                                message: DRAIN_DENY_MESSAGE.into(),
                            },
                        )
                        .await?;
                    }

                    Ok::<(), ServeError>(())
                })
                .await
                .map_err(|_| ServeError::StalledReply)??;

                calls.abort_all();

                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barn_luau::{Capabilities, Limits};
    use barn_stall::Content;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::bridge::real_clock;

    /// A tool reply carrying len bytes of text. Plain ASCII needs no JSON
    /// escaping, so the serialized length grows exactly with len.
    fn tool_reply(len: usize) -> Reply {
        Reply::Tool {
            id: 7,
            content: Content {
                kind: "text".into(),
                text: "x".repeat(len),
            },
        }
    }

    /// The exact wire bytes for a reply: its JSON followed by one newline.
    fn line_for(reply: &Reply) -> Vec<u8> {
        let mut line = serde_json::to_vec(reply).expect("the reply serializes");
        line.push(b'\n');

        line
    }

    /// Bytes left for the text after the reply's fixed JSON envelope.
    fn text_room() -> usize {
        MAX_LINE_BYTES
            .checked_sub(line_for(&tool_reply(0)).len())
            .expect("the cap fits the reply envelope")
    }

    #[tokio::test]
    async fn write_reply_until_writes_one_exact_newline_terminated_line() {
        let reply = tool_reply(2);
        let expected = line_for(&reply);

        let mut sink = Vec::new();
        write_reply_until(&mut sink, reply, deadline_after(REPLY_TIMEOUT))
            .await
            .expect("the reply is written");

        assert_eq!(sink.last(), Some(&b'\n'));
        assert_eq!(sink, expected);
    }

    #[tokio::test]
    async fn write_reply_until_frames_at_the_cap_and_denies_one_byte_over() {
        let at_cap = tool_reply(text_room());
        let cap_line = line_for(&at_cap);
        assert_eq!(cap_line.len(), MAX_LINE_BYTES);

        let mut sink = Vec::new();
        write_reply_until(&mut sink, at_cap, deadline_after(REPLY_TIMEOUT))
            .await
            .expect("a reply at the cap is written");
        assert_eq!(sink, cap_line);

        let over_cap = tool_reply(
            text_room()
                .checked_add(1)
                .expect("one more byte fits in usize"),
        );
        assert_eq!(
            line_for(&over_cap).len(),
            MAX_LINE_BYTES.checked_add(1).expect("one more byte")
        );

        let mut deny = serde_json::to_vec(&Reply::Error {
            id: 7,
            error: OVERSIZE_REPLY.into(),
            message: OVERSIZE_REPLY_MESSAGE.into(),
        })
        .expect("the deny serializes");
        deny.push(b'\n');

        let mut sink = Vec::new();
        write_reply_until(&mut sink, over_cap, deadline_after(REPLY_TIMEOUT))
            .await
            .expect("the oversize reply writes a deny");
        assert_eq!(sink, deny);
    }

    /// The exact wire bytes for a string value: its JSON followed by one
    /// newline. Plain ASCII needs no escaping, so the serialized length grows
    /// exactly with len.
    fn string_line(len: usize) -> Vec<u8> {
        let value = Value::String("x".repeat(len));
        let mut line = serde_json::to_vec(&value).expect("the value serializes");
        line.push(b'\n');

        line
    }

    /// Bytes left for the string after its quotes and the trailing newline.
    fn string_room() -> usize {
        MAX_LINE_BYTES
            .checked_sub(string_line(0).len())
            .expect("the cap fits the string envelope")
    }

    #[tokio::test]
    async fn write_json_frames_at_the_cap_and_denies_one_byte_over() {
        let at_cap = Value::String("x".repeat(string_room()));
        let cap_line = string_line(string_room());
        assert_eq!(cap_line.len(), MAX_LINE_BYTES);

        let mut sink = Vec::new();
        write_json(&mut sink, &at_cap)
            .await
            .expect("a value at the cap is written");
        assert_eq!(sink, cap_line);

        let over_len = string_room()
            .checked_add(1)
            .expect("one more byte fits in usize");
        let over_cap = Value::String("x".repeat(over_len));
        assert_eq!(
            string_line(over_len).len(),
            MAX_LINE_BYTES.checked_add(1).expect("one more byte")
        );

        let mut sink = Vec::new();
        let result = write_json(&mut sink, &over_cap).await;
        assert!(matches!(result, Err(ServeError::Oversize)));
        assert_eq!(sink, Vec::<u8>::new());
    }

    #[tokio::test]
    async fn write_reply_writes_one_exact_newline_terminated_line() {
        let reply = tool_reply(2);
        let expected = line_for(&reply);

        let mut sink = Vec::new();
        write_reply(&mut sink, reply)
            .await
            .expect("the reply is written");

        assert_eq!(sink, expected);
    }

    #[tokio::test]
    async fn next_event_waits_for_a_running_call_before_reading_another_line() {
        let (release, hold) = std::sync::mpsc::channel::<()>();

        let mut calls = JoinSet::new();
        calls.spawn_blocking(move || {
            hold.recv().expect("the release arrives");
            tool_reply(1)
        });

        let ready_line: &[u8] = b"{}\n";
        let mut reader = BufReader::new(ready_line);
        let mut writer = Vec::new();
        let mut line = Vec::new();
        let mut need_slot = true;

        // With a call still running, waiting for its slot outranks the ready
        // line, so the read must not complete while the call is held.
        let early_event = timeout(
            Duration::from_millis(50),
            next_event(
                &mut reader,
                &mut writer,
                &mut line,
                &mut calls,
                &mut need_slot,
                false,
            ),
        )
        .await;
        assert!(
            early_event.is_err(),
            "next_event must wait for the running call, not read the line"
        );

        release.send(()).expect("the worker is still held");

        let event = timeout(
            Duration::from_secs(1),
            next_event(
                &mut reader,
                &mut writer,
                &mut line,
                &mut calls,
                &mut need_slot,
                false,
            ),
        )
        .await
        .expect("the released call completes")
        .expect("the event is readable");
        assert!(matches!(event, Event::Call(Ok(_))));
    }

    #[tokio::test]
    async fn finish_call_writes_one_exact_line_for_a_joined_reply() {
        let reply = tool_reply(3);
        let expected = line_for(&reply);

        let mut calls = JoinSet::new();
        calls.spawn(async move { reply });
        let joined = calls
            .join_next_with_id()
            .await
            .expect("the task completes")
            .expect("the task did not fail");

        let mut pending = BTreeMap::new();
        let mut sink = Vec::new();
        finish_call(&mut sink, &mut pending, Ok(joined))
            .await
            .expect("the reply is written");

        assert_eq!(sink, expected);
    }

    #[tokio::test]
    async fn finish_call_denies_an_aborted_worker_with_stall_unavailable() {
        let mut calls = JoinSet::new();
        let task = calls.spawn(async { tool_reply(0) });
        let id = task.id();
        task.abort();

        let error = calls
            .join_next_with_id()
            .await
            .expect("the task completes")
            .expect_err("the aborted task fails");

        let mut pending = BTreeMap::new();
        pending.insert(id, 42);

        let mut sink = Vec::new();
        finish_call(&mut sink, &mut pending, Err(error))
            .await
            .expect("the deny is written");

        assert_eq!(sink, line_for(&lost(42)));
    }

    #[tokio::test]
    async fn joined_tag_renders_an_answered_call_with_its_task_id() {
        let task = tokio::spawn(async {});
        let id = task.id();
        assert_eq!(joined_tag("answered", id), format!("task={id:?} answered"));
    }

    #[tokio::test]
    async fn joined_tag_renders_a_join_error_with_its_task_id() {
        let task = tokio::spawn(async {});
        let id = task.id();
        assert_eq!(
            joined_tag("join_error", id),
            format!("task={id:?} join_error")
        );
    }

    #[tokio::test]
    async fn queue_control_writes_the_exact_frame_and_holds_the_reply() {
        let (reply, receiver) = oneshot::channel();
        let frame = b"{\"kind\":\"control\"}\n".to_vec();
        let call = ControlCall {
            id: 9,
            frame: frame.clone(),
            reply,
        };
        let mut pending_control = None;
        let mut sink = Vec::new();

        queue_control(&mut sink, &mut pending_control, call)
            .await
            .expect("the control frame is written");

        assert_eq!(sink, frame);

        let (id, sender, deadline) = pending_control.expect("a pending control call");
        assert_eq!(id, 9);
        assert!(deadline > Instant::now());

        sender
            .send(Ok(json!({ "ok": true })))
            .expect("the caller still waits");
        assert_eq!(
            receiver.await.expect("the reply arrives"),
            Ok(json!({ "ok": true }))
        );
    }

    #[tokio::test]
    async fn a_frame_exactly_at_the_cap_is_written() {
        let (reply, receiver) = oneshot::channel();
        let frame = vec![b'x'; tack_protocol::MAX_FRAME_BYTES];
        let call = ControlCall {
            id: 10,
            frame: frame.clone(),
            reply,
        };
        let mut pending_control = None;
        let mut sink = Vec::new();

        queue_control(&mut sink, &mut pending_control, call)
            .await
            .expect("the control frame is written");

        assert_eq!(sink, frame);

        let (id, sender, deadline) = pending_control.expect("a pending control call");
        assert_eq!(id, 10);
        assert!(deadline > Instant::now());

        sender
            .send(Ok(json!({ "ok": true })))
            .expect("the caller still waits");
        assert_eq!(
            receiver.await.expect("the reply arrives"),
            Ok(json!({ "ok": true }))
        );
    }

    #[tokio::test]
    async fn queue_control_refuses_an_oversize_frame_without_writing_it() {
        let (reply, receiver) = oneshot::channel();
        let call = ControlCall {
            id: 1,
            frame: vec![b'x'; tack_protocol::MAX_FRAME_BYTES.saturating_add(1)],
            reply,
        };
        let mut pending_control = None;
        let mut sink = Vec::new();

        queue_control(&mut sink, &mut pending_control, call)
            .await
            .expect("an oversize frame is refused");

        assert_eq!(sink, Vec::<u8>::new());
        assert!(pending_control.is_none());
        assert_eq!(
            receiver.await.expect("the refusal arrives"),
            Err(CONTROL_FRAME_ERROR.to_owned())
        );
    }

    /// A complete control reply line for the call id, as the peer writes it.
    fn control_reply_line(id: u64, result: Value) -> Vec<u8> {
        let reply = tack_protocol::wire::Reply::Control {
            id,
            version: tack_protocol::wire::PROTOCOL_VERSION,
            result,
        };
        let mut line = serde_json::to_vec(&reply).expect("the reply serializes");
        line.push(b'\n');

        line
    }

    #[test]
    fn take_control_reply_routes_the_result_and_clears_the_line() {
        let (reply, receiver) = oneshot::channel();
        let mut pending_control = Some((7, reply, deadline_after(CONTROL_TIMEOUT)));
        let mut line = control_reply_line(7, json!({ "ok": true }));

        assert!(take_control_reply(&mut line, &mut pending_control));
        assert_eq!(line, Vec::<u8>::new());
        assert!(pending_control.is_none());
        assert_eq!(
            receiver.blocking_recv().expect("the result arrives"),
            Ok(json!({ "ok": true }))
        );
    }

    #[test]
    fn take_control_reply_routes_a_named_error() {
        let (reply, receiver) = oneshot::channel();
        let mut pending_control = Some((8, reply, deadline_after(CONTROL_TIMEOUT)));
        let error = tack_protocol::wire::Reply::Error {
            id: 8,
            version: tack_protocol::wire::PROTOCOL_VERSION,
            error: tack_protocol::wire::ReplyError::DaemonError,
            message: "navigate refused".to_owned(),
        };
        let mut line = serde_json::to_vec(&error).expect("the error serializes");
        line.push(b'\n');

        assert!(take_control_reply(&mut line, &mut pending_control));
        assert_eq!(line, Vec::<u8>::new());
        assert!(pending_control.is_none());
        assert_eq!(
            receiver.blocking_recv().expect("the refusal arrives"),
            Err("daemon_error: navigate refused".to_owned())
        );
    }

    #[test]
    fn take_control_reply_leaves_an_unrelated_line_for_the_decode_path() {
        let (reply, _receiver) = oneshot::channel();
        let mut pending_control = Some((7, reply, deadline_after(CONTROL_TIMEOUT)));
        let mut line = b"{}\n".to_vec();

        assert!(!take_control_reply(&mut line, &mut pending_control));
        assert_eq!(line, b"{}\n");
        assert!(pending_control.is_some());
    }

    #[test]
    fn is_foreign_control_envelope_ignores_a_normal_barn_line() {
        let line = b"{\"jsonrpc\":\"2.0\",\"method\":\"stall.health\",\"id\":5}\n";

        assert!(!is_foreign_control_envelope(line));
    }

    #[test]
    fn is_foreign_control_envelope_ignores_a_framed_hook_line() {
        let line =
            b"{\"type\":\"hook\",\"id\":5,\"version\":1,\"hook\":\"PreToolUse\",\"call\":{}}\n";

        assert!(!is_foreign_control_envelope(line));
    }

    #[test]
    fn is_foreign_control_envelope_matches_a_control_reply() {
        let line = b"{\"kind\":\"control\",\"id\":7,\"version\":1,\"result\":{}}\n";

        assert!(is_foreign_control_envelope(line));
    }

    #[test]
    fn is_foreign_control_envelope_matches_an_error_reply() {
        let line = b"{\"kind\":\"error\",\"id\":8,\"version\":1}\n";

        assert!(is_foreign_control_envelope(line));
    }

    /// A plugin folder removed when the guard drops, so tests leave no files.
    struct TempPluginFolder(PathBuf);

    impl Drop for TempPluginFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn handle_line_event_answers_a_health_line_without_draining() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "barn-luau-serve-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the plugin folder");
        let folder = TempPluginFolder(root);

        std::fs::write(
            folder.0.join("plugin.luau"),
            "return { name = 'serve-health', tools = {} }",
        )
        .expect("write the plugin fixture");

        let limits = Limits::new(16 * 1024 * 1024, 100).expect("valid limits");
        let sandbox = Arc::new(
            Sandbox::new(
                folder.0.as_path(),
                limits,
                Capabilities::default(),
                real_clock(),
            )
            .expect("fixture loads"),
        );

        let mut line = b"{\"jsonrpc\":\"2.0\",\"method\":\"stall.health\",\"id\":5}\n".to_vec();
        let mut sink = Vec::new();
        let mut calls = JoinSet::new();
        let mut pending = BTreeMap::new();
        let mut need_slot = false;
        let mut pending_control = None;

        let drained = handle_line_event(
            &mut sink,
            &mut line,
            &mut calls,
            &mut pending,
            &mut need_slot,
            &mut pending_control,
            &sandbox,
        )
        .await
        .expect("the health line is handled");

        assert!(!drained);
        assert_eq!(sink, b"{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":null}\n");
    }
}
