//! barn: the only daemon on a wrkhse machine.
//!
//! barn runs each project's stall in its own sandboxed child process, holds
//! every listener and speaks its protocol, and brokers the network, secrets,
//! webhooks, MCP and upgrades. The supervisor that does all of that is B1; this
//! crate holds the command line's shape until then.

pub mod cli;

pub use cli::{Cli, CliCommand, StallsCommand};
