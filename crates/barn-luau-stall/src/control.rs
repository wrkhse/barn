//! The host-side bridge for a plugin's `ctx.control.call(name, input)`.
//!
//! A capability callback is synchronous: it runs inside the Luau call on a
//! blocking worker. It sends one framed `control` request to the serve loop
//! over an unbounded channel, then waits on a oneshot for the matching reply.
//! The channel is unbounded on purpose — the callback cannot yield, so it
//! must never block the sender while the serve loop is the only reader.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tack_protocol::wire::{PROTOCOL_VERSION, Request, encode_line};
use tokio::sync::{mpsc, oneshot};

/// The message on a call with the wrong number or kinds of arguments.
pub const BAD_ARGS: &str = "control.call expects a name and an input";

/// The message when the serve loop is gone before it can answer.
const LOST_REPLY: &str = "control reply lost";

/// The prefix of the message when the request cannot be framed.
const ENCODE_ERROR: &str = "control request could not be encoded";

/// The message on a line that is not a reply to this call.
const REJECTED_REPLY: &str = "control reply rejected";

/// One control request handed to the serve loop, with its reply channel.
pub struct ControlCall {
    /// The request id the matching reply must repeat.
    pub id: u64,
    /// The newline-terminated wire frame to write to the peer.
    pub frame: Vec<u8>,
    /// The channel the serve loop answers once the framed reply arrives.
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// The sending half of the control bridge, held by the capability callback.
pub struct ControlBridge {
    tx: mpsc::UnboundedSender<ControlCall>,
    next: AtomicU64,
}

/// The shape of a framed reply this bridge accepts.
enum ReplyShape {
    Control(Value),
    Error(String, String),
}

impl ControlBridge {
    /// Create a bridge and the receiver its serve loop must drain.
    ///
    /// The channel is unbounded because the caller blocks on a oneshot while
    /// the serve loop is the only task that can receive and answer it.
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<ControlCall>) {
        let (tx, rx) = mpsc::unbounded_channel();

        (
            Self {
                tx,
                next: AtomicU64::new(0),
            },
            rx,
        )
    }

    /// Call a host control method and wait for its framed reply.
    ///
    /// Takes exactly two arguments: the call name and any JSON input.
    ///
    /// # Errors
    ///
    /// Returns the bad-arguments message for the wrong arguments, a named
    /// encode error when the request will not fit one frame, the lost-reply
    /// message when the serve loop cannot take or answer the call, or the
    /// named error the peer sent.
    pub fn call(&self, args: Vec<Value>) -> Result<Value, String> {
        let mut args = args.into_iter();
        let (Some(Value::String(call)), Some(input), None) =
            (args.next(), args.next(), args.next())
        else {
            return Err(BAD_ARGS.to_owned());
        };

        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let request = Request::Control {
            id,
            version: PROTOCOL_VERSION,
            call,
            input,
        };
        let frame = encode_line(&request).map_err(|error| format!("{ENCODE_ERROR}: {error}"))?;
        let (reply, receiver) = oneshot::channel();

        self.tx
            .send(ControlCall { id, frame, reply })
            .map_err(|_| LOST_REPLY.to_owned())?;

        receiver
            .blocking_recv()
            .map_err(|_| LOST_REPLY.to_owned())?
    }
}

/// Read a reply shape only when it answers `id` at this protocol version.
fn reply_shape(frame: &[u8], id: u64) -> Option<ReplyShape> {
    let value: Value = serde_json::from_slice(frame).ok()?;

    if value.get("id").and_then(Value::as_u64) != Some(id) {
        return None;
    }
    if value.get("version").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
        return None;
    }

    match value.get("kind").and_then(Value::as_str)? {
        "control" => Some(ReplyShape::Control(value.get("result")?.clone())),
        "error" => Some(ReplyShape::Error(
            value.get("error").and_then(Value::as_str)?.to_owned(),
            value.get("message").and_then(Value::as_str)?.to_owned(),
        )),
        _ => None,
    }
}

/// Whether `frame` is a control reply, or a named error, for `id`.
#[must_use]
pub fn is_control_reply(frame: &[u8], id: u64) -> bool {
    reply_shape(frame, id).is_some()
}

/// Decode the result, or the named error, of a reply to `id`.
///
/// # Errors
///
/// Returns the rejected-reply message when the frame is not a reply to the
/// call, or the peer's error and message joined with a colon for a named error.
pub fn decode_control_reply(frame: &[u8], id: u64) -> Result<Value, String> {
    match reply_shape(frame, id) {
        Some(ReplyShape::Control(result)) => Ok(result),
        Some(ReplyShape::Error(error, message)) => Err(format!("{error}: {message}")),
        None => Err(REJECTED_REPLY.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn control_line(id: u64) -> Vec<u8> {
        let mut line = serde_json::to_vec(&json!({
            "kind": "control",
            "id": id,
            "version": PROTOCOL_VERSION,
            "result": { "url": "https://example.test/x" },
        }))
        .expect("the reply serializes");
        line.push(b'\n');

        line
    }

    fn error_line(id: u64) -> Vec<u8> {
        let mut line = serde_json::to_vec(&json!({
            "kind": "error",
            "id": id,
            "version": PROTOCOL_VERSION,
            "error": "refused",
            "message": "navigate denied",
        }))
        .expect("the reply serializes");
        line.push(b'\n');

        line
    }

    #[test]
    fn call_rejects_wrong_argument_counts() {
        let (bridge, _rx) = ControlBridge::new();

        assert_eq!(bridge.call(vec![]), Err(BAD_ARGS.to_owned()));
        assert_eq!(
            bridge.call(vec![json!("navigate")]),
            Err(BAD_ARGS.to_owned())
        );
        assert_eq!(
            bridge.call(vec![json!("navigate"), json!({}), json!(1)]),
            Err(BAD_ARGS.to_owned())
        );
    }

    #[test]
    fn call_rejects_a_non_string_name() {
        let (bridge, _rx) = ControlBridge::new();

        assert_eq!(
            bridge.call(vec![json!(7), json!({})]),
            Err(BAD_ARGS.to_owned())
        );
    }

    #[test]
    fn is_control_reply_accepts_a_result_and_a_named_error() {
        assert!(is_control_reply(&control_line(3), 3));
        assert!(is_control_reply(&error_line(4), 4));
    }

    #[test]
    fn is_control_reply_rejects_a_wrong_id_version_kind_or_missing_result() {
        assert!(!is_control_reply(&control_line(3), 4));

        let wrong_version = serde_json::to_vec(&json!({
            "kind": "control",
            "id": 3,
            "version": PROTOCOL_VERSION.wrapping_add(1),
            "result": null,
        }))
        .expect("the reply serializes");
        assert!(!is_control_reply(&wrong_version, 3));

        let unknown_kind = serde_json::to_vec(&json!({
            "kind": "hook", "id": 3, "version": PROTOCOL_VERSION, "result": null,
        }))
        .expect("the reply serializes");
        assert!(!is_control_reply(&unknown_kind, 3));

        let missing_result = serde_json::to_vec(&json!({
            "kind": "control", "id": 3, "version": PROTOCOL_VERSION,
        }))
        .expect("the reply serializes");
        assert!(!is_control_reply(&missing_result, 3));
    }

    #[test]
    fn decode_control_reply_returns_the_exact_result() {
        assert_eq!(
            decode_control_reply(&control_line(3), 3),
            Ok(json!({ "url": "https://example.test/x" }))
        );
    }

    #[test]
    fn decode_control_reply_names_the_error() {
        assert_eq!(
            decode_control_reply(&error_line(4), 4),
            Err("refused: navigate denied".to_owned())
        );
    }

    #[test]
    fn decode_control_reply_rejects_a_foreign_frame() {
        assert_eq!(
            decode_control_reply(&control_line(3), 4),
            Err(REJECTED_REPLY.to_owned())
        );
    }
}
