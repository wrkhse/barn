//! barn's command line: the surface the supervisor answers.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// barn: the only daemon on a wrkhse machine.
#[derive(Debug, Parser)]
#[command(
    name = "barn",
    version,
    about = "The only daemon on a wrkhse machine: projects run as stalls under it"
)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: CliCommand,
}

/// A command barn takes.
#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Show every stall, what it may touch, and how it is doing.
    Status,
    /// List the stalls, or control one.
    Stalls {
        /// What to do with a stall. Without it, the stalls are listed.
        #[command(subcommand)]
        action: Option<StallsCommand>,
    },
    /// Run one stall in the foreground, under the same guard.
    Run {
        /// The stall's name.
        stall: String,
    },
    /// Print a stall's log.
    Logs {
        /// The stall's name.
        stall: String,
        /// Keep printing as the stall writes.
        #[arg(long)]
        follow: bool,
    },
}

/// What `barn stalls` may do to one stall.
#[derive(Debug, Subcommand)]
pub enum StallsCommand {
    /// Show a project's grant, then write its stall file once you accept it.
    Add {
        /// The project's name.
        project: String,
    },
    /// Drain and restart one stall.
    Restart {
        /// The stall's name.
        stall: String,
    },
    /// Start one stall.
    Start {
        /// The stall's name.
        stall: String,
    },
    /// Stop one stall.
    Stop {
        /// The stall's name.
        stall: String,
    },
}

impl CliCommand {
    /// The command as a person typed it, for the notice this slice prints.
    #[must_use]
    pub fn typed(&self) -> String {
        match self {
            Self::Status => "status".to_owned(),
            Self::Stalls { action } => match action {
                None => "stalls".to_owned(),
                Some(StallsCommand::Add { project }) => format!("stalls add {project}"),
                Some(StallsCommand::Restart { stall }) => format!("stalls restart {stall}"),
                Some(StallsCommand::Start { stall }) => format!("stalls start {stall}"),
                Some(StallsCommand::Stop { stall }) => format!("stalls stop {stall}"),
            },
            Self::Run { stall } => format!("run {stall}"),
            Self::Logs { stall, follow } => {
                if *follow {
                    format!("logs {stall} --follow")
                } else {
                    format!("logs {stall}")
                }
            }
        }
    }
}

/// Runs the command a person typed.
///
/// Nothing is built behind it yet: B1 builds the supervisor that answers every
/// one of them. Each command says so on stderr and exits non-zero, so a script
/// calling barn never reads a stub as a success.
pub fn run() -> ExitCode {
    let cli    =    Cli::parse();
    let typed = cli.command.typed();
    eprintln!("barn {typed}: not built yet. B1 builds the supervisor that answers it.");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, StallsCommand};

    #[test]
    fn a_command_reads_back_as_it_was_typed() {
        assert_eq!(CliCommand::Status.typed(), "status");
        assert_eq!(CliCommand::Stalls { action: None }.typed(), "stalls");
        assert_eq!(
            CliCommand::Stalls {
                action: Some(StallsCommand::Restart {
                    stall: "sourcefed".to_owned(),
                }),
            }
            .typed(),
            "stalls restart sourcefed"
        );
        assert_eq!(
            CliCommand::Logs {
                stall: "sourcefed".to_owned(),
                follow: true,
            }
            .typed(),
            "logs sourcefed --follow"
        );
    }
}
