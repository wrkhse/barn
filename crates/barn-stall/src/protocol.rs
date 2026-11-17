//! The stall protocol's fixed parts: its version, its limits, its method names
//! and the message bodies the plan settles.
//!
//! Every message is one line of JSON, so each field's name here is its wire
//! name. A field added later is an addition inside a version, never a rename.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The protocol version barn speaks.
///
/// barn speaks this version and the one before it, and fields are only added
/// within a version.
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
    /// barn to stall: start, which returns the stall's jobs.
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

/// The `stall.start` parameters barn sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallStartParams {
    /// The protocol version barn speaks.
    pub protocol: u32,
    /// The stall's name, as its file in `~/.config/barn/stalls` names it.
    pub stall: String,
    /// The stall's state folder.
    pub state: String,
}

/// A job a stall declares and barn keeps the timer for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// How often barn runs the job, such as `15s`.
    pub every: String,
}

/// The `stall.start` result the stall answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallStartResult {
    /// Whether the stall is ready to take requests.
    pub ready: bool,
    /// The stall's own version.
    pub version: String,
    /// The jobs the stall declares, by name. A stall with no jobs sends none.
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
}

/// The secret a `net.request` names, and how barn adds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetAuth {
    /// The secret's name. The stall never reads its value.
    pub secret: String,
    /// How barn adds it, such as `bearer`.
    #[serde(rename = "as")]
    pub scheme: String,
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
