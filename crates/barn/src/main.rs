//! barn's entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    barn::cli::run()
}
