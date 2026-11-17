//! The Rust stall SDK.
//!
//! A stall is a project's logic — its request handlers, its background jobs and
//! the events it emits — in a child process barn starts and guards. A stall
//! never opens a listener, never holds a secret and never reaches the network
//! directly: barn owns all of that, and the two speak the stall protocol over a
//! socket pair it passes as file descriptor 3.
//!
//! This crate holds the protocol's fixed parts: its version, its limits, its
//! method names, and the message bodies the plan already settles. Whatever is
//! not fixed is listed in the [protocol page][protocol], as "for B1 to confirm".
//!
//! [protocol]: https://github.com/wrkhse/barn/blob/main/docs/stall-protocol.md

// The proof commit's licence break, kept as a real dependency of this crate so
// the licences check sees it.
use snap as _;

pub mod protocol;

pub use protocol::{
    BodyEncoding, DEFAULT_MAX_BODY_BYTES, DENIED_CODE, Headers, Job, MAX_LINE_BYTES, NetAuth,
    NetRequestParams, NetResponse, PROTOCOL_VERSION, StallStartParams, StallStartResult, method,
};
