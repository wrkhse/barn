//! The stall protocol's fixed parts: its version, its limits, its method names
//! and the message bodies the plan settles.
//!
//! Every message is one line of JSON, so each field's name here is its wire
//! name. A field added later is an addition inside a version, never a rename.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The protocol version barn speaks.
///
/// barn speaks this version and the one before it, and fields are only added
/// within a version. A stall states its own in `stall.start`, and a version
/// barn does not speak is refused with [`VERSION_REFUSED`].
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest line the protocol allows, in bytes, its newline included: 8 MiB.
///
/// A longer line is answered with `-32600` and an `id` of `null`, because the
/// line was never parsed, and then the connection is closed, as Chauffeur v2's
/// socket server does.
pub const MAX_LINE_BYTES: usize = 8_388_608;

/// The default cap on an inline body, counted in decoded bytes: 4 MiB.
///
/// The side that sends a body uses UTF-8 when the bytes are valid UTF-8 and the
/// line still fits `MAX_LINE_BYTES`, and base64 otherwise, with `body_encoding`
/// saying which. A body over the cap is refused.
pub const DEFAULT_MAX_BODY_BYTES: usize = 4_194_304;

/// The JSON-RPC error code for a request barn refused: `denied`.
pub const DENIED_CODE: i32 = -32_001;

/// The JSON-RPC error code for a `stall.start` whose version barn cannot
/// speak: `-32010`.
///
/// barn answers with it and no state folder, marks the stall failed, and does
/// not restart it, because a restart would be refused the same way.
pub const VERSION_REFUSED: i32 = -32_010;

/// The JSON-RPC error code for a method the receiver does not take: `-32601`.
///
/// A stall answers a `stall.start` that reaches it from barn with it, since a
/// stall is the side that starts and barn never sends that message.
pub const NO_SUCH_METHOD: i32 = -32_601;

/// Headers as the wire carries them: one entry per value, so a repeated name
/// keeps every value.
///
/// Either side may send a name in any case; barn lower-cases names on the way in
/// and on the way out, so a stall always sees lower-case names. A header whose
/// value is not valid UTF-8 is dropped, and barn logs it once per request with
/// its name; the request or response still goes through.
pub type Headers = Vec<(String, String)>;

/// The names of the protocol's methods.
pub mod method {
    /// stall to barn: start, the one message a stall sends first and the only
    /// one barn never sends.
    pub const STALL_START: &str = "stall.start";
    /// barn to stall: report health.
    pub const STALL_HEALTH: &str = "stall.health";
    /// barn to stall: finish in-flight work and exit within a deadline.
    pub const STALL_DRAIN: &str = "stall.drain";
    /// barn to stall: a client request from a listener.
    pub const REQUEST: &str = "request";
    /// barn to stall: a cancel, or a closed connection.
    pub const CANCEL: &str = "cancel";
    /// stall to barn: write to a response stream.
    pub const STREAM_SEND: &str = "stream.send";
    /// stall to barn: end a response stream.
    pub const STREAM_END: &str = "stream.end";
    /// barn to stall: a verified webhook delivery.
    pub const WEBHOOK: &str = "webhook";
    /// barn to stall: run a declared job.
    pub const JOB_RUN: &str = "job.run";
    /// stall to barn: one HTTP request through the broker.
    pub const NET_REQUEST: &str = "net.request";
    /// stall to barn: call a tool on an MCP server barn runs.
    pub const MCP_CALL: &str = "mcp.call";
    /// stall to barn: a structured log line.
    pub const LOG: &str = "log";
}

/// The token barn gives a stall in its environment, spent on the first
/// `stall.start` that carries it.
///
/// Its `Debug` prints `[redacted]` and never the value, so a parameter set that
/// carries one reaches a log without the token in it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token(String);

impl Token {
    /// Wraps a token a caller already holds.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Reads the token barn put in the environment under this name.
    ///
    /// # Errors
    ///
    /// The variable's name, when it is missing or empty.
    pub fn from_env(name: &str) -> Result<Self, String> {
        token_from(name, std::env::var(name).ok())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Builds a token from what a lookup found, refusing a missing or empty value.
fn token_from(name: &str, value: Option<String>) -> Result<Token, String> {
    value
        .filter(|value| !value.is_empty())
        .map(Token)
        .ok_or_else(|| format!("{name} is missing or empty"))
}

/// The `stall.start` parameters a stall sends barn, as its first message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallStartParams {
    /// The protocol version the stall speaks.
    pub protocol: u32,
    /// The stall's name, as its file in `~/.config/barn/stalls` names it.
    pub stall: String,
    /// The token barn put in the stall's environment, spent by this message.
    pub token: Token,
    /// The stall's own version.
    pub version: String,
    /// The jobs the stall declares, by name. A stall with no jobs sends none.
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
}

/// The answer to `stall.start`: where the stall may keep its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallStartAnswer {
    /// The state folder, absolute, as the stall's file gives it.
    pub state: String,
}

/// A job a stall declares and barn keeps the timer for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// How often barn runs the job: a whole number and one unit, `ms`, `s`,
    /// `m` or `h`, as `15s` writes it. [`parse_duration`] reads it.
    pub every: String,
}

/// Why a text is not a duration [`parse_duration`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationError {
    /// The text is empty.
    Empty,
    /// The digits carry no unit, as `15` does.
    MissingUnit,
    /// The unit is not one of `ms`, `s`, `m` or `h`.
    UnknownUnit,
    /// The text does not start with digits, as `fifteen` does not.
    NotNumeric,
    /// The value is zero, which schedules nothing.
    Zero,
    /// The value does not fit the unit's arithmetic, in milliseconds.
    TooLarge,
}

impl fmt::Display for DurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "the duration is empty",
            Self::MissingUnit => "the duration has no unit: ms, s, m or h",
            Self::UnknownUnit => "the unit is not one of ms, s, m or h",
            Self::NotNumeric => "the duration does not start with digits",
            Self::Zero => "the duration is zero",
            Self::TooLarge => "the duration is too large to count in milliseconds",
        };

        f.write_str(text)
    }
}

impl Error for DurationError {}

/// Reads a duration: a whole number and one unit, `ms`, `s`, `m` or `h`, as
/// `15s` and `250ms` write them.
///
/// # Errors
///
/// [`DurationError`] names what is wrong: empty text, no unit, an unknown
/// unit, no digits to start with, a zero value, or a value too large to count
/// in milliseconds.
pub fn parse_duration(text: &str) -> Result<Duration, DurationError> {
    if text.is_empty() {
        return Err(DurationError::Empty);
    }

    let (digits, unit) = split_duration(text);

    if digits.is_empty() {
        return Err(DurationError::NotNumeric);
    }

    if unit.is_empty() {
        return Err(DurationError::MissingUnit);
    }

    let value: u64 = digits.parse().map_err(|_| DurationError::TooLarge)?;
    let millis = match unit {
        "ms" => Ok(value),
        "s" => value.checked_mul(1_000).ok_or(DurationError::TooLarge),
        "m" => value.checked_mul(60_000).ok_or(DurationError::TooLarge),
        "h" => value.checked_mul(3_600_000).ok_or(DurationError::TooLarge),
        _ => Err(DurationError::UnknownUnit),
    }?;

    if millis == 0 {
        return Err(DurationError::Zero);
    }

    Ok(Duration::from_millis(millis))
}

/// The digits a duration starts with, and the unit after them.
fn split_duration(text: &str) -> (&str, &str) {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());

    text.split_at(end)
}

/// The answer to `stall.health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResult {
    /// Whether the stall can still take work.
    pub ok: bool,
    /// A short status for `barn status`, such as `ready` or `draining`.
    pub status: String,
}

/// The `stall.drain` parameters barn sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainParams {
    /// How long the stall has to finish in-flight work and exit.
    pub deadline_ms: u64,
}

/// The answer to `stall.drain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainResult {
    /// Whether the stall finished its work inside the deadline and is exiting.
    pub drained: bool,
}

/// The `job.run` parameters barn sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRunParams {
    /// The job's name, as the stall declared it in `stall.start`.
    pub job: String,
}

/// The answer to `job.run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRunResult {
    /// Whether the run finished.
    pub ran: bool,
}

/// How loud a log line is.
///
/// The set is closed within a protocol version: another level is a new
/// version, never a value added inside this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// What a person reads while chasing something.
    Debug,
    /// What happened, in the stall's own words.
    Info,
    /// Something went wrong and the stall carried on.
    Warn,
    /// Something the stall could not do.
    Error,
}

/// The `log` parameters a stall sends barn, which appends them to its log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogParams {
    /// How loud the line is.
    pub level: LogLevel,
    /// The line itself.
    pub message: String,
}

/// The secret a `net.request` names, and how barn adds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetAuth {
    /// The secret's name. The stall never reads its value.
    pub secret: String,
    /// How barn adds it.
    #[serde(rename = "as")]
    pub scheme: AuthScheme,
}

/// How barn adds a secret to a request.
///
/// The set is closed within a protocol version: another scheme is a new
/// version, never a value added inside this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthScheme {
    /// `Authorization: Bearer <secret>`.
    Bearer,
}

/// The `net.request` parameters a stall sends the broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetRequestParams {
    /// The plugin's name, when the request belongs to one of the stall's
    /// plugins. barn checks it against the plugin's grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// The HTTP method.
    pub method: String,
    /// The absolute URL.
    pub url: String,
    /// The headers to send, with lower-cased names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Headers,
    /// The request body, inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// How this request's body is encoded, as a response's is.
    #[serde(default)]
    pub body_encoding: BodyEncoding,
    /// The secret to add, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<NetAuth>,
    /// The budget to charge, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<String>,
}

/// How a body is encoded.
///
/// The set is closed within a protocol version: another encoding is a new
/// version, never a value added inside this one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyEncoding {
    /// The body is UTF-8 text.
    #[default]
    Utf8,
    /// The body is base64.
    Base64,
}

/// The `net.request` result the broker answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetResponse {
    /// The HTTP status.
    pub status: u16,
    /// The headers barn received, names lower-cased, one entry per value.
    #[serde(default)]
    pub headers: Headers,
    /// The body: UTF-8 text, or base64 when `body_encoding` says so.
    #[serde(default)]
    pub body: String,
    /// How `body` is encoded.
    #[serde(default)]
    pub body_encoding: BodyEncoding,
}

#[cfg(test)]
mod tests {
    use super::{Token, token_from};

    #[test]
    fn a_looked_up_token_becomes_a_token() {
        assert_eq!(
            token_from("BARN_TOKEN", Some("5f2c9a1e".to_owned())),
            Ok(Token::new("5f2c9a1e"))
        );
    }

    #[test]
    fn a_missing_token_is_refused() {
        assert_eq!(
            token_from("BARN_TOKEN", None),
            Err("BARN_TOKEN is missing or empty".to_owned())
        );
    }

    #[test]
    fn an_empty_token_is_refused() {
        assert_eq!(
            token_from("BARN_TOKEN", Some(String::new())),
            Err("BARN_TOKEN is missing or empty".to_owned())
        );
    }

    #[test]
    fn a_token_read_from_a_name_that_is_not_set_is_refused() {
        let read = Token::from_env("BARN_TOKEN_THAT_IS_NOT_SET_IN_THIS_PROCESS");

        assert!(read.is_err());
    }
}
