//! The Rust stall SDK.
//!
//! A stall is a project's logic — its request handlers, its background jobs and
//! the events it emits — in a child process barn starts and guards. A stall
//! never opens a listener, never holds a secret and never reaches the network
//! directly: barn owns all of that. The stall connects to the private Unix
//! socket at `BARN_SOCKET` and sends `stall.start` with `BARN_TOKEN` as its first
//! message.
//!
//! This crate holds the protocol's fixed parts: its version, its limits, its
//! method names, and the message bodies the plan already settles. Whatever is
//! not fixed is listed in the [protocol page][protocol], as "for B1 to confirm".
//!
//! [protocol]: https://github.com/wrkhse/barn/blob/main/docs/stall-protocol.md

pub mod protocol;

pub use protocol::{
    BodyEncoding, DEFAULT_MAX_BODY_BYTES, DENIED_CODE, Headers, Job, MAX_LINE_BYTES, NetAuth,
    NetRequestParams, NetResponse, PROTOCOL_VERSION, StallStartParams, StallStartResult, method,
};
