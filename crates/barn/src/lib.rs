//! barn: the only daemon on a wrkhse machine.
//!
//! barn runs each project's stall in its own sandboxed child process, holds
//! every listener and speaks its protocol, and brokers the network, secrets,
//! webhooks, MCP and upgrades. This crate holds the grant policy and the
//! supervisor for sandboxed stalls; the `cli` feature adds the command line.

#[cfg(feature = "cli")]
pub mod cli;
pub mod policy;
pub mod supervisor;

#[cfg(feature = "cli")]
pub use cli::{Cli, CliCommand, StallsCommand};
pub use policy::{Grant, PolicyError};
