//! Applies the stall's sandbox before replacing itself with the stall.

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use nono::{CapabilitySet, NonoError, Sandbox, SandboxState};
use thiserror::Error;

#[derive(Debug, PartialEq, Eq)]
struct Invocation {
    policy: PathBuf,
    program: PathBuf,
    args: Vec<OsString>,
}

#[derive(Debug, Error)]
enum ExecError {
    #[error("missing sandbox policy path")]
    MissingPolicy,
    #[error("missing target program")]
    MissingProgram,
    #[error("reading sandbox policy: {0}")]
    Read(#[source] io::Error),
    #[error("decoding sandbox policy: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("loading sandbox capabilities: {0}")]
    Capabilities(#[source] NonoError),
    #[error("applying stall sandbox: {0}")]
    Apply(#[source] NonoError),
    #[error("executing stall: {0}")]
    Exec(#[source] io::Error),
}

type ExecResult<T> = Result<T, ExecError>;

fn parse_args(mut args: impl Iterator<Item = OsString>) -> ExecResult<Invocation> {
    let policy = args.next().ok_or(ExecError::MissingPolicy)?;
    let program = args.next().ok_or(ExecError::MissingProgram)?;

    Ok(Invocation {
        policy: policy.into(),
        program: program.into(),
        args: args.collect(),
    })
}

fn decode_policy(json: &[u8]) -> ExecResult<CapabilitySet> {
    let state: SandboxState = serde_json::from_slice(json)?;

    state.to_caps().map_err(ExecError::Capabilities)
}

fn load_policy(path: &Path) -> ExecResult<CapabilitySet> {
    let json = fs::read(path).map_err(ExecError::Read)?;

    decode_policy(&json)
}

fn run(invocation: Invocation) -> ExecResult<Infallible> {
    let caps = load_policy(&invocation.policy)?;
    let mut target = Command::new(invocation.program);
    target.args(invocation.args);

    Sandbox::apply_auto(&caps).map_err(ExecError::Apply)?;

    Err(ExecError::Exec(target.exec()))
}

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)).and_then(run) {
        Ok(never) => match never {},
        Err(error) => {
            eprintln!("barn-exec: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    #[test]
    fn parses_paths_and_preserves_raw_target_arguments() {
        let arg = OsString::from_vec(vec![0xff]);
        let args = ["policy.json".into(), "/bin/stall".into(), arg.clone()];

        assert_eq!(
            parse_args(args.into_iter()).ok(),
            Some(Invocation {
                policy: "policy.json".into(),
                program: "/bin/stall".into(),
                args: vec![arg],
            })
        );
    }

    #[test]
    fn names_each_missing_argument() {
        let missing_policy = parse_args(std::iter::empty()).err();
        let missing_program = parse_args(["policy.json".into()].into_iter()).err();

        assert_eq!(
            missing_policy.as_ref().map(ToString::to_string).as_deref(),
            Some("missing sandbox policy path")
        );
        assert_eq!(
            missing_program.as_ref().map(ToString::to_string).as_deref(),
            Some("missing target program")
        );
    }

    #[test]
    fn decodes_policy_with_network_and_dns_blocked() {
        let policy = br#"{"fs":[],"net_blocked":true,"dns_blocked":true}"#;
        let flags = decode_policy(policy)
            .ok()
            .map(|caps| (caps.is_network_blocked(), caps.dns_enabled()));

        assert_eq!(flags, Some((true, false)));
    }

    #[test]
    fn rejects_invalid_json_and_invalid_capabilities() {
        let bad_path = concat!(
            r#"{"fs":[{"original":"","resolved":"","access":"bad","#,
            r#""is_file":true}],"net_blocked":true}"#,
        );

        assert!(matches!(decode_policy(b"["), Err(ExecError::Decode(_))));
        assert!(matches!(
            decode_policy(bad_path.as_bytes()),
            Err(ExecError::Capabilities(_))
        ));
        assert!(matches!(
            load_policy(Path::new("")),
            Err(ExecError::Read(_))
        ));
    }
}
