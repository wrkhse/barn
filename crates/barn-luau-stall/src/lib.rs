//! The library surface of the Luau stall binary.
//!
//! The binary in `main.rs` authenticates to barn through the environment
//! handshake, [`bridge`] maps barn's framed requests onto a Luau sandbox, and
//! [`serve`] runs the framed loop over the stall's authenticated socket.

pub mod bridge;
pub mod serve;
