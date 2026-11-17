//! The JSON-RPC 2.0 envelope: the messages one line may carry, and the rules
//! that line must keep.
//!
//! A line is read through [`Message::parse`], which refuses what JSON-RPC 2.0
//! does not allow rather than guessing at it: a peer that cannot say what it
//! meant has broken the protocol, and the connection closes.

use std::error::Error;
use std::fmt;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The `jsonrpc` member: the version every message states, `2.0` and nothing
/// else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum JsonRpc {
    /// JSON-RPC 2.0, the one version the protocol speaks.
    #[serde(rename = "2.0")]
    V2,
}

/// One line of the protocol, read as a JSON-RPC 2.0 message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A call that wants an answer.
    Request(Request),
    /// A call that wants none, such as a log line.
    Notification(Notification),
    /// An answer to a request.
    Answer(Answer),
}

/// A call that wants an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The `id` its answer carries.
    pub id: u64,
    /// The method's name.
    pub method: String,
    /// The parameters, an object, or none when the method takes none.
    pub params: Option<Map<String, Value>>,
}

/// A call that wants no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The method's name.
    pub method: String,
    /// The parameters, an object, or none when the method takes none.
    pub params: Option<Map<String, Value>>,
}

/// An answer to a request: what the method returned, or why it failed.
///
/// An answer carries one of the two, never both, so a peer cannot answer with
/// a result and an error at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// What the method returned.
    Result {
        /// The `id` of the request this answers.
        id: u64,
        /// The method's result.
        result: Value,
    },
    /// Why the method failed.
    Error {
        /// The `id` of the request this answers.
        id: u64,
        /// The error the answer carries.
        error: ErrorObject,
    },
}

/// The error an answer carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// The code, such as `-32600`.
    pub code: i32,
    /// A short message for a person.
    pub message: String,
    /// What the code needs, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Why a line was refused as not acceptable.
///
/// Each rule the reader enforces has one reason, so a refusal names the rule an
/// operator has to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrictReason {
    /// The line is not a JSON object, or its `method` is not a string.
    NotAnObject,
    /// The `jsonrpc` member is missing.
    MissingVersion,
    /// The `jsonrpc` member is not `2.0`.
    WrongVersion,
    /// The `id` is not a positive integer.
    IdShape,
    /// A method sits beside a result or an error.
    MethodWithAnswer,
    /// The `params` member is not an object.
    ParamsShape,
    /// An answer carries both a result and an error, or neither.
    AnswerShape,
    /// An answer carries no `id`.
    AnswerWithoutId,
    /// The line is not UTF-8.
    NotUtf8,
    /// The line ended with a carriage return.
    CarriageReturn,
    /// More blank lines arrived in a row than the protocol allows.
    TooManyBlankLines,
}

impl fmt::Display for StrictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NotAnObject => "the line is not a JSON-RPC object",
            Self::MissingVersion => "jsonrpc is missing",
            Self::WrongVersion => "jsonrpc is not 2.0",
            Self::IdShape => "the id is not a positive integer",
            Self::MethodWithAnswer => "a method cannot sit beside a result or an error",
            Self::ParamsShape => "params is not an object",
            Self::AnswerShape => {
                "an answer carries a result or an error, never both and never neither"
            }
            Self::AnswerWithoutId => "an answer carries no id",
            Self::NotUtf8 => "the line is not UTF-8",
            Self::CarriageReturn => "a line ends with a newline, never a carriage return",
            Self::TooManyBlankLines => "too many blank lines arrived in a row",
        };

        f.write_str(text)
    }
}

impl Error for StrictReason {}

/// Why a line could not be read, and why a message was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The line is longer than the cap, so it is refused.
    OverLimit,
    /// The stream ended in the middle of a line.
    PartialLine,
    /// A read after a line that was refused: the connection must close.
    Poisoned,
    /// The message is not one JSON-RPC 2.0 allows, for this reason.
    Strict(StrictReason),
    /// The stream itself failed, in its own words.
    Io(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::OverLimit => "the line is over the cap",
            Self::PartialLine => "the stream ended in the middle of a line",
            Self::Poisoned => "the connection is closed after a refused line",
            Self::Strict(reason) => return write!(f, "the line is not acceptable: {reason}"),
            Self::Io(error) => error.as_str(),
        };

        f.write_str(text)
    }
}

impl Error for RpcError {}

impl Message {
    /// Reads one line as a message, refusing what JSON-RPC 2.0 does not allow.
    ///
    /// The `jsonrpc` member is required and must be `2.0`. A member the
    /// protocol does not name, such as `_meta`, is carried and ignored.
    ///
    /// # Errors
    ///
    /// [`RpcError::Strict`] with the [`StrictReason`] that names the rule: a
    /// line that is not a JSON object, a `jsonrpc` that is missing or not
    /// `2.0`, an `id` that is not a positive integer, a `null` `id` beside
    /// a method, a method beside a result or an error, params that are not an
    /// object, and an answer with both or neither of result and error.
    pub fn parse(line: &str) -> Result<Self, RpcError> {
        let value: Value =
            serde_json::from_str(line).map_err(|_| RpcError::Strict(StrictReason::NotAnObject))?;
        let object = value
            .as_object()
            .ok_or(RpcError::Strict(StrictReason::NotAnObject))?;

        version(object)?;

        if object.contains_key("method") {
            return call(object);
        }

        answer(object)
    }
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Request(request) => request.serialize(serializer),
            Self::Notification(notification) => notification.serialize(serializer),
            Self::Answer(answer) => answer.serialize(serializer),
        }
    }
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", &JsonRpc::V2)?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("method", &self.method)?;

        if let Some(params) = &self.params {
            map.serialize_entry("params", params)?;
        }

        map.end()
    }
}

impl Serialize for Notification {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", &JsonRpc::V2)?;
        map.serialize_entry("method", &self.method)?;

        if let Some(params) = &self.params {
            map.serialize_entry("params", params)?;
        }

        map.end()
    }
}

impl Serialize for Answer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", &JsonRpc::V2)?;

        match self {
            Self::Result { id, result } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("result", result)?;
            }
            Self::Error { id, error } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("error", error)?;
            }
        }

        map.end()
    }
}

/// A message that carries a method: a request, or a notification.
fn call(object: &Map<String, Value>) -> Result<Message, RpcError> {
    if object.contains_key("result") || object.contains_key("error") {
        return Err(RpcError::Strict(StrictReason::MethodWithAnswer));
    }

    let method = match object.get("method") {
        Some(Value::String(method)) => method.clone(),
        _ => return Err(RpcError::Strict(StrictReason::NotAnObject)),
    };
    let params = params(object)?;

    match id(object)? {
        Some(id) => Ok(Message::Request(Request { id, method, params })),
        None => Ok(Message::Notification(Notification { method, params })),
    }
}

/// A message that carries no method: an answer.
fn answer(object: &Map<String, Value>) -> Result<Message, RpcError> {
    let id = id(object)?.ok_or(RpcError::Strict(StrictReason::AnswerWithoutId))?;

    // An answer carries no parameters; one that is an object is ignored.
    let _ = params(object)?;

    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(Message::Answer(Answer::Result {
            id,
            result: result.clone(),
        })),
        (None, Some(error)) => {
            let error: ErrorObject = serde_json::from_value(error.clone())
                .map_err(|_| RpcError::Strict(StrictReason::AnswerShape))?;

            Ok(Message::Answer(Answer::Error { id, error }))
        }
        _ => Err(RpcError::Strict(StrictReason::AnswerShape)),
    }
}

/// The `jsonrpc` member every message carries: `2.0`, and nothing else.
fn version(object: &Map<String, Value>) -> Result<(), RpcError> {
    match object.get("jsonrpc") {
        None => Err(RpcError::Strict(StrictReason::MissingVersion)),
        Some(Value::String(version)) if version == "2.0" => Ok(()),
        Some(_) => Err(RpcError::Strict(StrictReason::WrongVersion)),
    }
}

/// The `id` a message carries: a positive integer, or none for a notification.
fn id(object: &Map<String, Value>) -> Result<Option<u64>, RpcError> {
    let Some(value) = object.get("id") else {
        return Ok(None);
    };

    value
        .as_u64()
        .filter(|id| *id != 0)
        .map(Some)
        .ok_or(RpcError::Strict(StrictReason::IdShape))
}

/// The `params` a message carries: an object, or none.
fn params(object: &Map<String, Value>) -> Result<Option<Map<String, Value>>, RpcError> {
    match object.get("params") {
        None => Ok(None),
        Some(Value::Object(params)) => Ok(Some(params.clone())),
        Some(_) => Err(RpcError::Strict(StrictReason::ParamsShape)),
    }
}
