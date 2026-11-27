//! The Luau stall binary: authenticate to barn and load a plugin; the framed serve loop is the next slice.
//!
//! The plugin folder is the first process argument and must be absolute; a
//! relative path would resolve against a different directory after barn
//! changes the child's working directory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{self, Formatter};
use std::io;
use std::path::PathBuf;

use barn_luau::{Capabilities, EngineError, Limits, Sandbox};
use barn_luau_stall::bridge::real_clock;
use barn_stall::client::{self, ClientError};

const MEMORY_BYTES: usize = 16 * 1024 * 1024;
const CALL_DEADLINE_MS: u64 = barn_stall::protocol::DRAIN_DEADLINE_MS;

/// Why the stall could not start or finish serving.
#[derive(Debug)]
pub enum MainError {
    /// No plugin folder was supplied.
    MissingFolder,
    /// The plugin folder was not an absolute path.
    RelativeFolder(PathBuf),
    /// The stall could not authenticate to barn.
    Client(ClientError),
    /// The socket could not be handed to the async runtime.
    Socket(io::Error),
    /// The engine limits were invalid.
    Limits(EngineError),
    /// The plugin could not be loaded into a sandbox.
    Sandbox(EngineError),
    /// The stall started but cannot serve until the framed loop lands.
    NotServingYet,
}

impl fmt::Display for MainError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFolder => formatter.write_str("no plugin folder was supplied"),
            Self::RelativeFolder(folder) => {
                write!(
                    formatter,
                    "plugin folder is not absolute: {}",
                    folder.display()
                )
            }
            Self::Client(error) => write!(formatter, "stall start failed: {error}"),
            Self::Socket(error) => write!(formatter, "preparing the socket failed: {error}"),
            Self::Limits(error) => write!(formatter, "invalid engine limits: {error}"),
            Self::Sandbox(error) => write!(formatter, "loading the plugin failed: {error}"),
            Self::NotServingYet => {
                formatter.write_str("the framed serve loop is not available in this slice")
            }
        }
    }
}

impl std::error::Error for MainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Socket(error) => Some(error),
            Self::Limits(error) | Self::Sandbox(error) => Some(error),
            Self::MissingFolder | Self::RelativeFolder(_) | Self::NotServingYet => None,
        }
    }
}

/// Resolve the plugin folder from process arguments, as `args_os` yields them
/// with the program name first.
fn folder_from_args(mut args: impl Iterator<Item = OsString>) -> Result<PathBuf, MainError> {
    // argv[0] is the program name; the folder is the first supplied argument.
    let folder = args
        .nth(1)
        .map(PathBuf::from)
        .ok_or(MainError::MissingFolder)?;
    if !folder.is_absolute() {
        return Err(MainError::RelativeFolder(folder));
    }
    Ok(folder)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), MainError> {
    let folder = folder_from_args(std::env::args_os())?;

    // `from_env` reads BARN_SOCKET and BARN_TOKEN from the process environment;
    // this map declares the jobs the stall runs, and there are none yet.
    let declared_jobs = BTreeMap::new();
    let (stream, _) = client::from_env(declared_jobs).map_err(MainError::Client)?;
    let socket = stream.into_inner();
    socket.set_nonblocking(true).map_err(MainError::Socket)?;
    let _stream = tokio::net::UnixStream::from_std(socket).map_err(MainError::Socket)?;

    let limits = Limits::new(MEMORY_BYTES, CALL_DEADLINE_MS).map_err(MainError::Limits)?;
    let _sandbox = Sandbox::new(&folder, limits, Capabilities::default(), real_clock())
        .map_err(MainError::Sandbox)?;

    not_serving_yet()
}

/// The stall started but cannot serve yet. Slice 2b replaces this with the
/// framed serve loop; until then a started stall must not report success by
/// exiting cleanly.
const fn not_serving_yet() -> Result<(), MainError> {
    Err(MainError::NotServingYet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_budget_is_sixteen_mib() {
        assert_eq!(MEMORY_BYTES, 16_777_216usize);
    }

    #[test]
    fn call_deadline_matches_the_drain_budget() {
        assert_eq!(CALL_DEADLINE_MS, 500u64);
    }

    #[test]
    fn display_names_the_startup_failure() {
        assert_eq!(
            MainError::MissingFolder.to_string(),
            "no plugin folder was supplied"
        );
        assert_eq!(
            MainError::RelativeFolder(PathBuf::from("relative/plugin")).to_string(),
            "plugin folder is not absolute: relative/plugin"
        );
    }

    #[test]
    fn display_wraps_the_underlying_error_messages() {
        assert_eq!(
            MainError::Client(ClientError::MissingSocket).to_string(),
            "stall start failed: BARN_SOCKET is not set"
        );
        assert_eq!(
            MainError::Socket(io::Error::other("boom")).to_string(),
            "preparing the socket failed: boom"
        );
        assert_eq!(
            MainError::Limits(EngineError::Script("bad limits".into())).to_string(),
            "invalid engine limits: bad limits"
        );
        assert_eq!(
            MainError::Sandbox(EngineError::Script("bad plugin".into())).to_string(),
            "loading the plugin failed: bad plugin"
        );
    }

    #[test]
    fn not_serving_yet_has_its_exact_reason() {
        assert_eq!(
            MainError::NotServingYet.to_string(),
            "the framed serve loop is not available in this slice"
        );
        assert!(std::error::Error::source(&MainError::NotServingYet).is_none());
    }

    #[test]
    fn source_reports_the_underlying_error() {
        let missing = std::error::Error::source(&MainError::MissingFolder);

        assert!(missing.is_none());

        let client = std::error::Error::source(&MainError::Client(ClientError::MissingSocket))
            .expect("a client failure carries its source");

        assert_eq!(client.to_string(), "BARN_SOCKET is not set");
    }

    #[test]
    fn source_reports_socket_limits_and_sandbox_errors() {
        let socket_error = MainError::Socket(io::Error::other("boom"));
        let socket =
            std::error::Error::source(&socket_error).expect("a socket failure carries its source");

        assert_eq!(socket.to_string(), "boom");

        let limits_error = MainError::Limits(EngineError::Script("bad limits".into()));
        let limits =
            std::error::Error::source(&limits_error).expect("a limits failure carries its source");

        assert_eq!(limits.to_string(), "bad limits");

        let sandbox_error = MainError::Sandbox(EngineError::Script("bad plugin".into()));
        let sandbox = std::error::Error::source(&sandbox_error)
            .expect("a sandbox failure carries its source");

        assert_eq!(sandbox.to_string(), "bad plugin");
    }

    #[test]
    fn folder_from_args_reports_a_missing_argument() {
        assert!(matches!(
            folder_from_args(std::iter::empty()),
            Err(MainError::MissingFolder)
        ));
    }

    #[test]
    fn folder_from_args_reports_a_relative_folder_with_its_path() {
        let relative = PathBuf::from("relative/plugin");

        match folder_from_args(
            [
                OsString::from("barn-luau-stall"),
                relative.clone().into_os_string(),
            ]
            .into_iter(),
        ) {
            Err(MainError::RelativeFolder(folder)) => assert_eq!(folder, relative),
            other => panic!("expected a relative-folder error, got {other:?}"),
        }
    }

    #[test]
    fn folder_from_args_returns_the_exact_absolute_folder() {
        let absolute = PathBuf::from("/plugins/demo");

        let folder = folder_from_args(
            [
                OsString::from("barn-luau-stall"),
                absolute.clone().into_os_string(),
            ]
            .into_iter(),
        )
        .expect("an absolute folder is accepted");

        assert_eq!(folder, absolute);
    }

    #[test]
    fn the_launcher_withholds_success_until_the_serve_loop() {
        let error = not_serving_yet().expect_err("a started stall must not report success");

        assert!(matches!(error, MainError::NotServingYet));
        assert_eq!(
            error.to_string(),
            "the framed serve loop is not available in this slice"
        );
    }

    /// Calls the real `main` only to kill the whole-main-returns-`Ok` mutant
    /// CI shard 1 missed; the exact `not_serving_yet` result is covered
    /// separately by `the_launcher_withholds_success_until_the_serve_loop`.
    /// Like the process tests in `crates/barn/tests/exit.rs`, it runs in CI
    /// only and returns at once without `CI` set.
    #[test]
    fn main_never_reports_success_in_ci() {
        if std::env::var_os("CI").is_none() {
            return;
        }

        assert!(!matches!(main(), Ok(())));
    }
}
