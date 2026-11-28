//! The test-only helper binaries must fail on bad input. These kill the
//! `main -> Default::default()` mutants in the two wrapper fixtures: each
//! wrapper's real `main` returns a failure, so a `main` that returns success
//! can only be the mutant.
//!
//! Both helpers stop before touching a socket or applying a sandbox, but they
//! do start a process, so -- like exit.rs -- these tests run in CI only and
//! return at once on the owner's Mac.

use std::process::Command;

#[test]
fn helper_exec_reports_an_invalid_invocation() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_barn-test-exec"))
        .arg("--definitely-not-a-flag")
        .output()
        .expect("the exec helper starts");

    assert!(
        !output.status.success(),
        "the exec helper must reject an invalid argument: {output:?}"
    );
}

#[test]
fn helper_stall_reports_a_missing_folder() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_barn-test-stall"))
        .output()
        .expect("the stall helper starts");

    assert!(
        !output.status.success(),
        "the stall helper must reject a missing folder: {output:?}"
    );
}
