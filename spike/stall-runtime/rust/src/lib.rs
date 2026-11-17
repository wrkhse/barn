//! stall-b: a Rust stall runtime spike for sourcefed.
//!
//! The harness spawns the binary with the lane socket on fd 3. A tokio
//! multi-thread runtime owns the lane, the HTTP/SSE server, the fan-out and the
//! store writer; plugin realms live on their own OS threads (see plugin.rs).
//!
//! The crate is a library plus a thin binary so the pure logic can be tested
//! from `tests/` without starting a socket or a realm.

pub mod http;
pub mod lane;
pub mod model;
pub mod plugin;
pub mod rpc;
pub mod schedule;
pub mod sse;
pub mod store;
pub mod target;

/// stderr only: the lane is reserved for JSON-RPC and must stay clean.
pub fn log_line(level: &str, message: &str) {
    eprintln!("{} {level} {message}", model::iso8601_ms(model::now_ms()));
}
