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
/// within a version. A stall answers `stall.start` with the version it speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest line the protocol allows, in bytes: 1 MiB.
pub const MAX_LINE_BYTES: usize = 1_048_576;

/// The default cap on a body a stall carries inline: 4 MiB.
pub const DEFAULT_MAX_BODY_BYTES: usize = 4_194_304;

/// The JSON-RPC error code for a request barn refused: `denied`.
pub const DENIED_CODE: i32 = -32_001;

/// Whether a line, without its terminator, fits the protocol's limit.
///
/// [`MAX_LINE_BYTES`] is the longest line barn and a stall accept.
#[must_use]
pub const fn fits_line(len: usize) -> bool {
    len <= MAX_LINE_BYTES
}

/// Whether a body fits the default cap a grant sets.
///
/// [`DEFAULT_MAX_BODY_BYTES`] is the cap barn applies when a grant sets none.
#[must_use]
pub const fn fits_default_body(len: usize) -> bool {
    len <= DEFAULT_MAX_BODY_BYTES
}

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
    /// The jobs the stall declares, by name.
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
    /// The plugin's name. barn checks it against the plugin's grant.
    pub plugin: String,
    /// The HTTP method.
    pub method: String,
    /// The absolute URL.
    pub url: String,
    /// The secret to add, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<NetAuth>,
    /// The budget to charge, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<String>,
}

/// The `net.request` result the broker answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetResponse {
    /// The HTTP status.
    pub status: u16,
    /// The response's headers.
    pub headers: BTreeMap<String, String>,
    /// The response's body.
    pub body: String,
}
