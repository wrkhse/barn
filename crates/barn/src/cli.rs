//! barn's command line: the surface the supervisor answers.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use barn_stall::{LAUNCH_PROTOCOL, MAX_LAUNCH_LINE_BYTES};
use clap::{Parser, Subcommand};
use thiserror::Error;

use crate::Grant;
use crate::supervisor::{HostEndpoint, StallSpec, Supervisor, SupervisorError};

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
    /// Start one stall and print its host endpoint on stdout.
    Stall {
        /// What barn starts.
        #[command(subcommand)]
        action: StallCommand,
    },
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

/// What `barn stall` may do.
#[derive(Debug, Subcommand)]
pub enum StallCommand {
    /// Start one stall from an absolute plugin folder.
    Start {
        /// The absolute path of the plugin folder.
        #[arg(long)]
        plugin: PathBuf,
        /// The tack daemon pid barn allows on `host.sock`.
        #[arg(long)]
        daemon_pid: u32,
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
            Self::Stall { action } => match action {
                StallCommand::Start { plugin, daemon_pid } => format!(
                    "stall start --plugin {} --daemon-pid {daemon_pid}",
                    plugin.display()
                ),
            },
            Self::Stalls { action } => match action {
                None => "stalls".to_owned(),
                Some(StallsCommand::Add { project }) => format!("stalls add {project}"),
                Some(StallsCommand::Restart { stall }) => format!("stalls restart {stall}"),
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
/// The launch command writes exactly one protocol line to stdout: the endpoint
/// once the stall starts, or a named failure if it never does. That failure is
/// also reported on stderr. Other commands are still stubs: each says so on
/// stderr and exits non-zero.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Stall {
            action: StallCommand::Start { plugin, daemon_pid },
        } => match start(&plugin, daemon_pid) {
            Ok(()) => ExitCode::SUCCESS,
            Err(failure) => {
                report_failure(&mut io::stdout().lock(), &mut io::stderr().lock(), &failure);
                ExitCode::FAILURE
            }
        },
        other => {
            let typed = other.typed();
            eprintln!("barn {typed}: not built yet. B1 builds the supervisor that answers it.");
            ExitCode::FAILURE
        }
    }
}

/// Why a launch fails before or after the child starts.
#[derive(Debug, Error)]
enum StartError {
    /// The plugin folder is not an absolute path.
    #[error("plugin path is not absolute")]
    RelativePlugin,
    /// The plugin folder has no final component to name the stall.
    #[error("plugin folder has no file name")]
    PluginName,
    /// The running barn binary has no parent directory.
    #[error("barn executable has no folder")]
    BinaryDir,
    /// `barn-exec` is not beside the running barn binary.
    #[error("barn-exec is not beside the barn executable")]
    HelperMissing,
    /// The stall runtime is not beside the running barn binary.
    #[error("barn-luau-stall is not beside the barn executable")]
    RuntimeMissing,
    /// Neither `BARN_STATE_DIR` nor `HOME` is available.
    #[error("state directory is unavailable")]
    StateDirMissing,
    /// A run directory for this stall is already live.
    #[error("stall run directory is in use")]
    RunDirInUse,
    /// The socket path is not UTF-8, so it cannot name a JSON string.
    #[error("socket path is not UTF-8")]
    SocketPath,
    /// The supervisor refused or lost the stall.
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    /// The endpoint line could not be encoded.
    #[error("encoding the endpoint line: {0}")]
    EndpointJson(#[from] serde_json::Error),
    /// An operation at the runtime or stdout boundary failed.
    #[error("{operation}: {source}")]
    Io {
        /// The operation that fails.
        operation: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The endpoint line does not fit the launch line limit.
    #[error("endpoint line exceeds the launch line limit")]
    EndpointTooLong,
}

impl StartError {
    /// The machine-readable reason written in the failure line.
    const fn reason(&self) -> &'static str {
        match self {
            Self::RelativePlugin => "plugin path not absolute",
            Self::PluginName => "plugin folder has no name",
            Self::BinaryDir => "barn executable has no folder",
            Self::HelperMissing => "barn-exec not found",
            Self::RuntimeMissing => "stall runtime not found",
            Self::StateDirMissing => "state directory unavailable",
            Self::RunDirInUse => "stall run directory in use",
            Self::SocketPath => "socket path not UTF-8",
            Self::Supervisor(_) => "stall did not start",
            Self::EndpointJson(_) => "encoding endpoint line",
            Self::Io { operation, .. } => operation,
            Self::EndpointTooLong => "endpoint line too long",
        }
    }
}

/// A launch failure and whether the protocol line already reached stdout.
#[derive(Debug)]
struct StartFailure {
    error: StartError,
    endpoint_written: bool,
}

impl StartFailure {
    /// No bytes were written, so the named error line is still ours to write.
    const fn before(error: StartError) -> Self {
        Self {
            error,
            endpoint_written: false,
        }
    }

    /// The endpoint line may already have reached stdout; never write a
    /// second line over it.
    const fn after(error: StartError) -> Self {
        Self {
            error,
            endpoint_written: true,
        }
    }
}

impl From<StartError> for StartFailure {
    /// Outside the endpoint write, an error happens before any line is written.
    fn from(error: StartError) -> Self {
        Self::before(error)
    }
}

/// The launch inputs barn derives from its own process and the environment.
struct Launch {
    stall: StallSpec,
    helper: PathBuf,
    state_dir: PathBuf,
    daemon_pid: u32,
}

fn start(plugin: &Path, daemon_pid: u32) -> Result<(), StartFailure> {
    let current = env::current_exe().map_err(|source| StartError::Io {
        operation: "finding the barn executable",
        source,
    })?;
    let dir = current.parent().ok_or(StartError::BinaryDir)?;
    let helper = helper_from(dir)?;
    let program = runtime_from(dir)?;
    let state_dir = state_dir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StartError::Io {
            operation: "building the tokio runtime",
            source,
        })?;

    runtime.block_on(start_async(plugin, program, daemon_pid, helper, state_dir))
}

/// The sandbox helper beside the running binary; no `PATH` search.
fn helper_from(dir: &Path) -> Result<PathBuf, StartError> {
    let path = dir.join("barn-exec");

    if path.is_file() {
        Ok(path)
    } else {
        Err(StartError::HelperMissing)
    }
}

/// The stall runtime beside the running binary; no `PATH` search.
fn runtime_from(dir: &Path) -> Result<PathBuf, StartError> {
    let path = dir.join("barn-luau-stall");

    if path.is_file() {
        Ok(path)
    } else {
        Err(StartError::RuntimeMissing)
    }
}

/// Tack's `<tack state>/barn`, or the standalone default under `HOME`.
fn state_dir() -> Result<PathBuf, StartError> {
    state_dir_from(env::var_os("BARN_STATE_DIR"), env::var_os("HOME"))
}

/// Resolves the state directory from already-read values, so the choice is
/// testable without changing the process environment.
fn state_dir_from(
    configured: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, StartError> {
    if let Some(root) = configured.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root));
    }

    let home = home.ok_or(StartError::StateDirMissing)?;

    Ok(Path::new(&home).join(".local").join("state").join("barn"))
}

async fn start_async(
    plugin: &Path,
    program: PathBuf,
    daemon_pid: u32,
    helper: PathBuf,
    state_dir: PathBuf,
) -> Result<(), StartFailure> {
    let launch = prepare_launch(plugin, &program, daemon_pid, helper, state_dir)?;
    let supervisor = Supervisor::new(launch.state_dir, launch.helper);
    let running = supervisor
        .start_for_host(launch.stall, launch.daemon_pid)
        .await
        .map_err(StartError::from)?;
    let endpoint = running.host_endpoint();

    write_endpoint(&mut io::stdout().lock(), &endpoint)?;

    // The accept/supervise loop is a later slice; tack owns the process
    // lifetime. Park so `running` stays alive and keeps `host.sock`, the
    // token and the child that own the endpoint printed above.
    std::future::pending::<()>().await;

    Ok(())
}

/// Builds the stall and refuses a live run directory for its name.
fn prepare_launch(
    plugin: &Path,
    program: &Path,
    daemon_pid: u32,
    helper: PathBuf,
    state_dir: PathBuf,
) -> Result<Launch, StartError> {
    let stall = stall_spec(plugin, program)?;

    if run_dir_is_live(&state_dir.join("run").join(&stall.name))? {
        return Err(StartError::RunDirInUse);
    }

    Ok(Launch {
        stall,
        helper,
        state_dir,
        daemon_pid,
    })
}

/// Whether another start holds the run directory's lock.
///
/// An unheld directory is stale: the supervisor clears it in `RunDir::create`.
/// Only a live lock blocks a second start.
fn run_dir_is_live(path: &Path) -> Result<bool, StartError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(StartError::Io {
                operation: "opening the stall run directory",
                source,
            });
        }
    };

    match file.try_lock().map_err(io::Error::from) {
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(source) => Err(StartError::Io {
            operation: "locking the stall run directory",
            source,
        }),
        Ok(()) => {
            // We own this lock; release it so the supervisor can relock the
            // directory for the start this check is allowing.
            drop(file);
            Ok(false)
        }
    }
}

/// The runtime is the program; the plugin folder is its first argument and
/// the only folder the grant covers.
fn stall_spec(plugin: &Path, program: &Path) -> Result<StallSpec, StartError> {
    if !plugin.is_absolute() {
        return Err(StartError::RelativePlugin);
    }

    let name = plugin
        .file_name()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
        .ok_or(StartError::PluginName)?;
    let folder = plugin.to_path_buf();

    Ok(StallSpec {
        name,
        program: program.to_path_buf(),
        args: vec![folder.clone().into_os_string()],
        grant: Grant {
            executable: Some(program.to_path_buf()),
            read_paths: vec![folder.clone()],
            write_paths: vec![folder],
            network_hosts: Vec::new(),
        },
    })
}

fn write_endpoint(writer: &mut impl Write, endpoint: &HostEndpoint) -> Result<(), StartFailure> {
    write_endpoint_to(writer, endpoint)
}

/// Builds the success line first, then writes it: a build error happens
/// before any bytes exist, while a write error may have reached stdout.
fn write_endpoint_to(writer: &mut impl Write, endpoint: &HostEndpoint) -> Result<(), StartFailure> {
    let socket_path = endpoint
        .socket_path
        .to_str()
        .ok_or(StartFailure::before(StartError::SocketPath))?;
    let line = endpoint_line(socket_path, &endpoint.token).map_err(StartFailure::before)?;

    write_line(writer, &line).map_err(StartFailure::after)
}

/// The exact success line: protocol, socket path and token, then one newline.
fn endpoint_line(socket_path: &str, token: &str) -> Result<Vec<u8>, StartError> {
    let path = serde_json::to_string(socket_path)?;
    let secret = serde_json::to_string(token)?;
    let line =
        format!("{{\"protocol\":{LAUNCH_PROTOCOL},\"socket_path\":{path},\"token\":{secret}}}\n")
            .into_bytes();

    if line.len() > MAX_LAUNCH_LINE_BYTES {
        return Err(StartError::EndpointTooLong);
    }

    Ok(line)
}

/// The exact failure line: protocol and reason, then one newline.
fn error_line(reason: &str) -> Vec<u8> {
    let escaped = serde_json::to_string(reason).unwrap_or_else(|_| "\"error\"".to_owned());

    format!("{{\"protocol\":{LAUNCH_PROTOCOL},\"error\":{escaped}}}\n").into_bytes()
}

fn write_line(mut stdout: impl Write, line: &[u8]) -> Result<(), StartError> {
    stdout.write_all(line).map_err(|source| StartError::Io {
        operation: "writing the launch line",
        source,
    })?;

    stdout.flush().map_err(|source| StartError::Io {
        operation: "flushing the launch line",
        source,
    })
}

/// Writes the named error line to the pipe and the full report to stderr.
///
/// A line already partly written must not be followed by a second one; the
/// report on stderr is always written.
fn report_failure(stdout: &mut impl Write, stderr: &mut impl Write, failure: &StartFailure) {
    if !failure.endpoint_written {
        let line = error_line(failure.error.reason());
        let _ = write_line(stdout, &line);
    }

    let _ = writeln!(stderr, "barn stall start: {}", failure.error);
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::fs::File;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use super::{
        Cli, CliCommand, Grant, HostEndpoint, MAX_LAUNCH_LINE_BYTES, StallCommand, StallsCommand,
        StartError, StartFailure, endpoint_line, error_line, helper_from, prepare_launch,
        report_failure, runtime_from, stall_spec, start, start_async, state_dir, state_dir_from,
        write_endpoint, write_endpoint_to, write_line,
    };

    /// A sink that writes a prefix, then fails, to model a broken stdout.
    #[derive(Debug)]
    struct PartialWriter {
        bytes: Vec<u8>,
        fail_after: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let room = self.fail_after.saturating_sub(self.bytes.len());

            if room == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "partial sink"));
            }

            let written = buf.len().min(room);
            self.bytes.extend_from_slice(&buf[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_command_reads_back_as_it_was_typed() {
        assert_eq!(CliCommand::Status.typed(), "status");
        assert_eq!(CliCommand::Stalls { action: None }.typed(), "stalls");
        assert_eq!(
            CliCommand::Stall {
                action: StallCommand::Start {
                    plugin: PathBuf::from("/opt/plugins/sourcefed"),
                    daemon_pid: 4321,
                },
            }
            .typed(),
            "stall start --plugin /opt/plugins/sourcefed --daemon-pid 4321"
        );
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

    #[test]
    fn the_launch_argv_builds_the_stall_spec() {
        let cli = Cli::try_parse_from([
            "barn",
            "stall",
            "start",
            "--plugin",
            "/opt/plugins/sourcefed",
            "--daemon-pid",
            "4321",
        ])
        .expect("the launch argv");
        assert_eq!(
            cli.command.typed(),
            "stall start --plugin /opt/plugins/sourcefed --daemon-pid 4321"
        );

        let (plugin, daemon_pid) = (match cli.command {
            CliCommand::Stall {
                action: StallCommand::Start { plugin, daemon_pid },
            } => Some((plugin, daemon_pid)),
            _ => None,
        })
        .expect("stall start command");

        assert_eq!(daemon_pid, 4321);
        let runtime = PathBuf::from("/usr/local/bin/barn-luau-stall");
        let spec = stall_spec(&plugin, &runtime).expect("a stall spec");
        let folder = PathBuf::from("/opt/plugins/sourcefed");

        assert_eq!(spec.name, "sourcefed");
        assert_eq!(spec.program, runtime);
        assert_eq!(spec.args, vec![OsString::from(&folder)]);
        assert_eq!(
            spec.grant,
            Grant {
                executable: Some(runtime),
                read_paths: vec![folder.clone()],
                write_paths: vec![folder],
                network_hosts: Vec::new(),
            }
        );
    }

    #[test]
    fn a_relative_plugin_folder_is_a_named_failure() {
        assert_eq!(
            stall_spec(
                Path::new("plugins/sourcefed"),
                Path::new("/usr/local/bin/barn-luau-stall"),
            )
            .err()
            .map(|error| error.reason()),
            Some("plugin path not absolute")
        );
    }

    #[test]
    fn a_missing_stall_runtime_is_a_named_failure() {
        let dir = std::env::temp_dir().join(format!("barn-runtime-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("an empty binary folder");

        assert_eq!(
            runtime_from(&dir).err().map(|error| error.reason()),
            Some("stall runtime not found")
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_live_run_directory_refuses_a_second_start() {
        let root = std::env::temp_dir().join(format!("barn-cli-{}-live", std::process::id()));
        let run_dir = root.join("run").join("sourcefed");
        fs::create_dir_all(&run_dir).expect("a live run directory");
        let lock = File::open(&run_dir).expect("opening the live run directory");
        lock.try_lock().expect("locking the live run directory");

        let second = prepare_launch(
            Path::new("/opt/plugins/sourcefed"),
            Path::new("/usr/local/bin/barn-luau-stall"),
            4321,
            PathBuf::from("/usr/local/bin/barn-exec"),
            root.clone(),
        );

        assert_eq!(
            second.err().map(|error| error.reason()),
            Some("stall run directory in use")
        );
        drop(lock);
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_stale_run_directory_does_not_block_a_start() {
        let root = std::env::temp_dir().join(format!("barn-cli-{}-stale", std::process::id()));
        let run_dir = root.join("run").join("sourcefed");
        fs::create_dir_all(&run_dir).expect("an unlocked stale run directory");

        let launch = prepare_launch(
            Path::new("/opt/plugins/sourcefed"),
            Path::new("/usr/local/bin/barn-luau-stall"),
            4321,
            PathBuf::from("/usr/local/bin/barn-exec"),
            root.clone(),
        )
        .expect("an unlocked stale run directory must not block a start");

        assert_eq!(launch.stall.name, "sourcefed");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_missing_run_directory_does_not_block_a_start() {
        let root = std::env::temp_dir().join(format!("barn-cli-{}-missing", std::process::id()));

        let launch = prepare_launch(
            Path::new("/opt/plugins/sourcefed"),
            Path::new("/usr/local/bin/barn-luau-stall"),
            4321,
            PathBuf::from("/usr/local/bin/barn-exec"),
            root,
        )
        .expect("a missing run directory must not block a start");

        assert_eq!(launch.stall.name, "sourcefed");
    }

    #[test]
    fn the_launch_line_limit_is_eight_kibibytes() {
        assert_eq!(MAX_LAUNCH_LINE_BYTES, 8192);
    }

    #[test]
    fn the_success_line_is_byte_exact() {
        let line = endpoint_line("/var/lib/barn/run/sourcefed/host.sock", "one-use")
            .expect("an endpoint line");
        let expected = concat!(
            r#"{"protocol":1,"socket_path":"/var/lib/barn/run/sourcefed/host.sock","token":"#,
            r#""one-use"}"#,
            "\n"
        );

        assert_eq!(line, expected.as_bytes());
    }

    #[test]
    fn the_error_line_is_byte_exact() {
        assert_eq!(
            error_line("stall run directory in use"),
            b"{\"protocol\":1,\"error\":\"stall run directory in use\"}\n".to_vec()
        );
    }

    #[test]
    fn an_oversized_endpoint_is_a_named_failure() {
        let token = "x".repeat(MAX_LAUNCH_LINE_BYTES);
        assert_eq!(
            endpoint_line("/run/host.sock", &token)
                .err()
                .map(|error| error.reason()),
            Some("endpoint line too long")
        );

        let path = format!("/{}", "p".repeat(MAX_LAUNCH_LINE_BYTES));
        assert_eq!(
            endpoint_line(&path, "token")
                .err()
                .map(|error| error.reason()),
            Some("endpoint line too long")
        );
    }

    #[test]
    fn start_refuses_when_the_sandbox_helper_is_absent() {
        let failure = start(Path::new("/opt/plugins/sourcefed"), 4321)
            .expect_err("a launch without barn-exec must fail");

        assert_eq!(failure.error.reason(), "barn-exec not found");
        assert!(!failure.endpoint_written);
    }

    #[test]
    fn start_async_refuses_a_relative_plugin_folder() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime");
        let failure = runtime
            .block_on(start_async(
                Path::new("plugins/sourcefed"),
                PathBuf::from("/usr/local/bin/barn-luau-stall"),
                4321,
                PathBuf::from("/usr/local/bin/barn-exec"),
                PathBuf::from("/var/lib/barn/state"),
            ))
            .expect_err("a relative plugin folder must fail");

        assert_eq!(failure.error.reason(), "plugin path not absolute");
        assert!(!failure.endpoint_written);
    }

    #[test]
    fn write_line_writes_the_exact_bytes() {
        let line = b"{\"protocol\":1}\n";
        let mut output = Vec::new();

        write_line(&mut output, line).expect("a line write");

        assert_eq!(output, line.to_vec());
    }

    #[test]
    fn write_endpoint_writes_the_exact_success_line() {
        let endpoint = HostEndpoint {
            socket_path: PathBuf::from("/run/host.sock"),
            token: "one-use".to_owned(),
        };
        let mut output = Vec::new();

        write_endpoint_to(&mut output, &endpoint).expect("an endpoint write");

        let expected = concat!(
            r#"{"protocol":1,"socket_path":"/run/host.sock","token":"one-use"}"#,
            "\n"
        );
        assert_eq!(output, expected.as_bytes());
    }

    #[test]
    fn write_endpoint_writes_the_exact_success_line_to_its_sink() {
        let endpoint = HostEndpoint {
            socket_path: PathBuf::from("/run/host.sock"),
            token: "one-use".to_owned(),
        };
        let mut output = Vec::new();

        write_endpoint(&mut output, &endpoint).expect("an endpoint write");

        let expected = concat!(
            r#"{"protocol":1,"socket_path":"/run/host.sock","token":"one-use"}"#,
            "\n"
        );
        assert_eq!(output, expected.as_bytes());
    }

    #[test]
    fn a_missing_sandbox_helper_is_a_named_failure() {
        let dir = std::env::temp_dir().join(format!("barn-helper-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("an empty binary folder");

        assert_eq!(
            helper_from(&dir).err().map(|error| error.reason()),
            Some("barn-exec not found")
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_unset_state_directory_is_a_named_failure() {
        assert_eq!(
            state_dir_from(None, None).err().map(|error| error.reason()),
            Some("state directory unavailable")
        );
    }

    #[test]
    fn an_empty_state_dir_setting_falls_back_to_home() {
        let home = OsString::from("/home/barn");

        assert_eq!(
            state_dir_from(Some(OsString::new()), Some(home.clone())).expect("a state dir"),
            PathBuf::from("/home/barn/.local/state/barn")
        );
        assert_eq!(
            state_dir_from(Some(OsString::from("/custom/state")), Some(home)).expect("a state dir"),
            PathBuf::from("/custom/state")
        );
    }

    #[test]
    fn state_dir_reads_the_current_environment() {
        let configured = env::var_os("BARN_STATE_DIR").filter(|value| !value.is_empty());
        let expected = configured.map_or_else(
            || {
                let home = env::var_os("HOME").expect("HOME is set");
                Path::new(&home).join(".local").join("state").join("barn")
            },
            PathBuf::from,
        );

        assert_eq!(state_dir().expect("a state dir"), expected);
    }

    #[test]
    fn a_pre_endpoint_failure_writes_the_named_error_line() {
        let failure = StartFailure::before(StartError::RelativePlugin);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        report_failure(&mut stdout, &mut stderr, &failure);

        assert_eq!(
            stdout,
            b"{\"protocol\":1,\"error\":\"plugin path not absolute\"}\n".to_vec()
        );
        assert_eq!(
            stderr,
            b"barn stall start: plugin path is not absolute\n".to_vec()
        );
    }

    #[test]
    fn a_partial_stdout_write_is_after_and_adds_no_second_line() {
        let endpoint = HostEndpoint {
            socket_path: PathBuf::from("/run/host.sock"),
            token: "one-use".to_owned(),
        };
        let mut sink = PartialWriter {
            bytes: Vec::new(),
            fail_after: 5,
        };

        let failure = write_endpoint(&mut sink, &endpoint).expect_err("a partial write must fail");

        assert!(failure.endpoint_written);
        assert_eq!(failure.error.reason(), "writing the launch line");
        assert_eq!(sink.bytes, b"{\"pro".to_vec());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        report_failure(&mut stdout, &mut stderr, &failure);

        assert_eq!(stdout, b"".to_vec());
        assert_eq!(
            stderr,
            b"barn stall start: writing the launch line: partial sink\n".to_vec()
        );
    }

    #[test]
    fn a_pre_write_error_is_before_and_writes_the_exact_error_line() {
        let endpoint = HostEndpoint {
            socket_path: PathBuf::from("/run/host.sock"),
            token: "x".repeat(MAX_LAUNCH_LINE_BYTES),
        };
        let mut sink = Vec::new();

        let failure =
            write_endpoint(&mut sink, &endpoint).expect_err("an oversized line must fail");

        assert!(!failure.endpoint_written);
        assert_eq!(failure.error.reason(), "endpoint line too long");
        assert_eq!(sink, b"".to_vec());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        report_failure(&mut stdout, &mut stderr, &failure);

        assert_eq!(
            stdout,
            b"{\"protocol\":1,\"error\":\"endpoint line too long\"}\n".to_vec()
        );
        assert_eq!(
            stderr,
            b"barn stall start: endpoint line exceeds the launch line limit\n".to_vec()
        );
    }

    #[test]
    fn an_endpoint_line_at_the_byte_cap_is_accepted_and_one_over_is_refused() {
        let socket = "/run/host.sock";
        // The JSON frame around the two string bodies is 43 bytes; this socket
        // path is 14, so the token makes up the rest of the cap.
        let frame = 43_usize.saturating_add(socket.len());
        let token = "x".repeat(MAX_LAUNCH_LINE_BYTES.saturating_sub(frame));
        let at_cap = endpoint_line(socket, &token).expect("a line at the cap");
        let expected = format!(r#"{{"protocol":1,"socket_path":"{socket}","token":"{token}"}}"#);

        assert_eq!(at_cap.len(), MAX_LAUNCH_LINE_BYTES);
        assert_eq!(at_cap, format!("{expected}\n").into_bytes());

        let over_token = "x".repeat(
            MAX_LAUNCH_LINE_BYTES
                .saturating_sub(frame)
                .saturating_add(1),
        );
        let over = format!(r#"{{"protocol":1,"socket_path":"{socket}","token":"{over_token}"}}"#);

        assert_eq!(
            over.len().saturating_add(1),
            MAX_LAUNCH_LINE_BYTES.saturating_add(1)
        );
        assert_eq!(
            endpoint_line(socket, &over_token)
                .err()
                .map(|error| error.reason()),
            Some("endpoint line too long")
        );
    }
}
