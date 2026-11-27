//! The Rust stall SDK.
//!
//! A stall is a project's logic — its request handlers, its background jobs and
//! the events it emits — in a child process barn starts and guards. A stall
//! never opens a listener and never reaches the network directly: barn owns
//! both. It holds only a single-use startup token, not a long-lived secret,
//! and connects to the private Unix socket at `BARN_SOCKET`. Its first message
//! is `stall.start`, carrying that token in `BARN_TOKEN`.
//!
//! This crate holds the protocol's fixed parts: its version, its limits, its
//! method names, and the message bodies the plan already settles. Whatever is
//! not fixed is listed in the [protocol page][protocol], as "for B1 to confirm".
//!
//! [protocol]: https://github.com/wrkhse/barn/blob/main/docs/stall-protocol.md

pub mod client;
pub mod frames;
pub mod launch;
pub mod protocol;

pub use client::{ClientError, connect, from_env};
pub use frames::{Content, FRAMED_VERSION, Meta, Outcome, Reply, Request, ToolCall};
pub use launch::{LAUNCH_PROTOCOL, Launch, LaunchError, MAX_LAUNCH_LINE_BYTES, parse};
pub use protocol::{
    BodyEncoding, DEFAULT_MAX_BODY_BYTES, DENIED_CODE, Headers, HostStartParams, HostStartResult,
    Job, MAX_LINE_BYTES, NetAuth, NetRequestParams, NetResponse, PROTOCOL_VERSION,
    StallStartParams, StallStartResult, deny, method,
};
