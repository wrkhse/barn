//! The stall protocol's fixed parts: its version, its limits, its method names
//! and the message bodies the plan settles.
//!
//! Every message is one line of JSON, so each field's name here is its wire
//! name. A field added later is an addition inside a version, never a rename.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};

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

/// Seconds after launch before the single-use `BARN_TOKEN` expires.
pub const TOKEN_TTL_SECS: u64 = 30;

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
    /// Stall to barn: authenticate and get the stall's state folder.
    pub const STALL_START: &str = "stall.start";
    /// Host to barn: authenticate and get the stall's name on `host.sock`.
    pub const HOST_START: &str = "host.start";
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

/// The named reasons barn puts in a `denied` error's `data.reason`.
///
/// Every one of them uses [`DENIED_CODE`] as its error code.
pub mod deny {
    /// The host's peer pid is not the pid barn supplied at stall start.
    pub const WRONG_PEER_PID: &str = "wrong peer pid";
    /// The host's peer uid does not own the socket.
    pub const WRONG_PEER_UID: &str = "wrong peer uid";
    /// The token has expired or was already spent.
    pub const TOKEN_EXPIRED_OR_SPENT: &str = "token expired or spent";
    /// The token does not match the one barn issued.
    pub const TOKEN_MISMATCH: &str = "token mismatch";
    /// The protocol version does not match the one barn speaks.
    pub const PROTOCOL_MISMATCH: &str = "protocol mismatch";
    /// A line is longer than [`MAX_LINE_BYTES`].
    pub const OVERSIZE_FRAME: &str = "oversize frame";
    /// The socket closed before the peer completed the exchange.
    pub const CLOSED_SOCKET: &str = "closed socket";
}

/// The `stall.start` parameters a stall sends when it connects.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallStartParams {
    /// The protocol version the stall speaks.
    pub protocol: u32,
    /// The single-use token from `BARN_TOKEN`.
    pub token: String,
    /// The jobs the stall declares for barn to time, keyed by name; none
    /// when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, Job>,
}

impl Debug for StallStartParams {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StallStartParams")
            .field("protocol", &self.protocol)
            .field("token", &"[redacted]")
            .field("jobs", &self.jobs)
            .finish()
    }
}

/// A job a stall declares and barn keeps the timer for.
///
/// Its name is the key it carries in the start message's `jobs` map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// How often barn runs the job, such as `15s`.
    pub every: String,
}

/// The `stall.start` result barn sends after authenticating the stall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallStartResult {
    /// The stall's state folder, as an absolute path.
    pub state: String,
    /// The declared jobs barn retains, keyed by name; none when the stall
    /// declares no jobs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, Job>,
}

/// The `host.start` parameters tack sends when it connects to `host.sock`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStartParams {
    /// The protocol version the host speaks.
    pub protocol: u32,
    /// The single-use host token barn gave tack at stall start.
    pub token: String,
    /// The tack daemon's pid, checked against the socket's peer pid.
    pub pid: u32,
}

impl Debug for HostStartParams {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostStartParams")
            .field("protocol", &self.protocol)
            .field("token", &"[redacted]")
            .field("pid", &self.pid)
            .finish()
    }
}

/// The `host.start` result barn sends after authenticating the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStartResult {
    /// The stall's name.
    pub stall: String,
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const PAGE: &str = include_str!("../../../docs/stall-protocol.md");

    /// Every JSON-RPC object on the protocol page, parsed, wrapped or not.
    fn lines() -> Vec<Value> {
        let mut objects = Vec::new();
        let mut buffer = String::new();
        let mut depth: i32 = 0;

        for line in PAGE.lines() {
            if buffer.is_empty() && !line.trim_start().starts_with('{') {
                continue;
            }

            buffer.push_str(line);
            buffer.push(' ');
            depth = line.chars().fold(depth, |n, c| match c {
                '{' => n.saturating_add(1),
                '}' => n.saturating_sub(1),
                _ => n,
            });

            if depth == 0 {
                objects.push(serde_json::from_str(&buffer).expect("the page's JSON object"));
                buffer.clear();
            }
        }

        objects
    }

    #[test]
    fn the_host_method_keeps_its_name() {
        assert_eq!(method::HOST_START, "host.start");
    }

    #[test]
    fn host_start_writes_the_pages_request() {
        let sent = lines()
            .into_iter()
            .find(|message| {
                message.get("method").and_then(Value::as_str) == Some(method::HOST_START)
            })
            .expect("the page sends host.start");
        let wire = sent.get("params").expect("host.start carries params");
        let params = HostStartParams {
            protocol: PROTOCOL_VERSION,
            token: "…".to_owned(),
            pid: 4321,
        };

        assert_eq!(sent.get("id"), Some(&json!(21)));
        assert_eq!(serde_json::to_value(&params).ok().as_ref(), Some(wire));
    }

    #[test]
    fn host_start_writes_the_pages_result() {
        let reply = lines()
            .into_iter()
            .find(|message| {
                message.get("id").and_then(Value::as_i64) == Some(21)
                    && message.get("result").is_some()
            })
            .expect("the page answers host.start");
        let wire = reply.get("result").expect("the reply carries a result");
        let result = HostStartResult {
            stall: "sourcefed".to_owned(),
        };

        assert_eq!(serde_json::to_value(&result).ok().as_ref(), Some(wire));
    }

    #[test]
    fn a_host_denial_writes_the_pages_reason() {
        let error = lines()
            .into_iter()
            .filter_map(|message| message.get("error").cloned())
            .find(|error| {
                error
                    .get("data")
                    .and_then(|data| data.get("reason"))
                    .and_then(Value::as_str)
                    == Some(deny::WRONG_PEER_PID)
            })
            .expect("the page names a host denial");

        assert_eq!(
            error,
            json!({
                "code": DENIED_CODE,
                "message": "denied",
                "data": {"reason": deny::WRONG_PEER_PID},
            })
        );
    }
}
