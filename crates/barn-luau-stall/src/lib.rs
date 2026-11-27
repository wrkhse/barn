//! The library surface of the Luau stall binary.
//!
//! The binary in `main.rs` authenticates to barn through the environment
//! handshake, and [`bridge`] maps barn's framed requests onto a Luau sandbox.

pub mod bridge;
