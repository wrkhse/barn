//! The commands as the binary a person runs, so this test starts a process. That
//! is why it runs in CI only: AGENTS.md says nothing that starts, signals or
//! kills a process runs on the owner's Mac. Without `CI` set it returns at once.

use std::process::Command;

/// What every stub command says before it exits.
const NOT_BUILT: &str = "not built yet";

fn in_ci() -> bool {
    std::env::var_os("CI").is_some()
}

#[test]
fn every_stub_command_says_it_is_not_built_and_exits_one() {
    if !in_ci() {
        return;
    }

    let cases: [&[&str]; 4] = [
        &["status"],
        &["stalls"],
        &["run", "sourcefed"],
        &["logs", "sourcefed"],
    ];

    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_barn"))
            .args(args)
            .output()
            .expect("the barn binary starts");
        let code = output
            .status
            .code()
            .expect("the barn binary exits on its own");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(code, 1, "{args:?} exited {code}");
        assert!(stderr.contains(NOT_BUILT), "{args:?} said {stderr}");
    }
}
