//! The command line's surface, as the plan writes it.

use barn::cli::{Cli, CliCommand, StallsCommand};
use clap::Parser;

#[test]
fn barn_status_parses() {
    let cli = Cli::try_parse_from(["barn", "status"]).expect("barn status parses");
    assert!(matches!(cli.command, CliCommand::Status));
}

#[test]
fn barn_stalls_lists_without_an_action() {
    let cli = Cli::try_parse_from(["barn", "stalls"]).expect("barn stalls parses");
    assert!(matches!(cli.command, CliCommand::Stalls { action: None }));
}

#[test]
fn barn_stalls_restart_names_a_stall() {
    let cli = Cli::try_parse_from(["barn", "stalls", "restart", "sourcefed"])
        .expect("barn stalls restart parses");
    assert!(matches!(
        cli.command,
        CliCommand::Stalls {
            action: Some(StallsCommand::Restart { stall })
        } if stall == "sourcefed"
    ));
}

#[test]
fn barn_stalls_add_names_a_project() {
    let cli = Cli::try_parse_from(["barn", "stalls", "add", "sourcefed"])
        .expect("barn stalls add parses");
    assert!(matches!(
        cli.command,
        CliCommand::Stalls {
            action: Some(StallsCommand::Add { project })
        } if project == "sourcefed"
    ));
}

#[test]
fn barn_run_names_a_stall() {
    let cli = Cli::try_parse_from(["barn", "run", "sourcefed"]).expect("barn run parses");
    assert!(matches!(
        cli.command,
        CliCommand::Run { stall } if stall == "sourcefed"
    ));
}

#[test]
fn barn_logs_follows() {
    let cli =
        Cli::try_parse_from(["barn", "logs", "sourcefed", "--follow"]).expect("barn logs parses");
    assert!(matches!(
        cli.command,
        CliCommand::Logs {
            stall,
            follow: true
        } if stall == "sourcefed"
    ));
}

#[test]
fn an_unknown_command_is_refused() {
    assert!(Cli::try_parse_from(["barn", "lanes"]).is_err());
}
