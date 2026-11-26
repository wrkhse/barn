//! The framed request and reply contract between tack and a stall.
//!
//! tack holds no VM. It writes one JSON frame per line to a stall and
//! reads one framed reply. A hook frame carries a tool call for the stall to
//! decide; a tool frame asks the stall to run one. The stall answers a hook
//! with an outcome, a tool with content, and either with an error.
//!
//! These frames ride the connection barn brokers. They are a contract of their
//! own, versioned by [`FRAMED_VERSION`], and are distinct from the stall
//! protocol's JSON-RPC messages in [`crate::protocol`].
//!
//! The shared examples live in `fixtures/frames.jsonl`: each request line is
//! followed by its reply line, and the tests round-trip every line through
//! these types.

use serde::{Deserialize, Serialize};

/// The version of this framed contract carried in a request's `version`.
pub const FRAMED_VERSION: u32 = 1;

/// A frame tack sends to a stall.
///
/// The `type` tag picks the variant. A hook asks the stall to decide a tool
/// call; a tool asks it to run one. Both carry a version and an id, and a reply
/// repeats the id so tack can pair the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Request {
    /// A tool call for the stall to allow, deny, rewrite, annotate or drop.
    Hook {
        /// The id the matching reply repeats.
        id: u64,
        /// The framed contract version, [`FRAMED_VERSION`].
        version: u32,
        /// The hook's name, such as `PreToolUse`.
        hook: String,
        /// The call the hook decides on.
        call: ToolCall,
        /// Trace context from the caller, when it supplied one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        _meta: Option<Meta>,
    },
    /// A tool the stall runs and answers with content.
    Tool {
        /// The id the matching reply repeats.
        id: u64,
        /// The framed contract version, [`FRAMED_VERSION`].
        version: u32,
        /// The call to run.
        call: ToolCall,
    },
}

/// The frame a stall sends back.
///
/// The `type` tag picks the variant, and every variant repeats the request's
/// id. A hook carries the decision, a tool its result, and an error the reason
/// the stall could not answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Reply {
    /// The decision for a hook request.
    Hook {
        /// The id of the request this answers.
        id: u64,
        /// The decision the stall reached.
        outcome: Outcome,
    },
    /// The result of a tool request.
    Tool {
        /// The id of the request this answers.
        id: u64,
        /// What the tool produced.
        content: Content,
    },
    /// A failure to answer a request.
    Error {
        /// The id of the request this answers.
        id: u64,
        /// A short machine-readable name for the failure.
        error: String,
        /// The human-readable reason.
        message: String,
    },
}

/// The tool call a hook decides on or a tool request runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The tool's name, such as `bash`.
    pub tool: String,
    /// The operation on the tool, such as `run`.
    pub name: String,
    /// The arguments as JSON.
    pub input: serde_json::Value,
    /// The caller's session id, passed through unchanged and absent when the
    /// caller did not supply one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Trace context the caller supplied with a hook request.
///
/// It is data only, never a permission: the stall learns where the call came
/// from, and no field here grants access to a tool or a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// A W3C trace parent, when the caller sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// The W3C trace state, when the caller sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// A stall's decision on a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Let the call through unchanged.
    Allow,
    /// Refuse the call, with the reason the stall gives.
    Deny {
        /// Why the call is refused.
        reason: String,
    },
    /// Replace the input of a tool call before it runs.
    Rewrite {
        /// The replacement input, of any shape the tool takes.
        input: serde_json::Value,
    },
    /// Add fields that travel with the delivered event.
    Annotate {
        /// Fields keyed by name, such as `{"chauffeurTriaged": true}`.
        fields: serde_json::Map<String, serde_json::Value>,
    },
    /// Remove the call from the run.
    Drop,
}

/// What a tool produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    /// How to read the text, such as `text` or `markdown`.
    pub kind: String,
    /// The text itself.
    pub text: String,
}
