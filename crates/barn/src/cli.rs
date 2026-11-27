//! barn's command line: the surface the supervisor answers.

use std::env;
use std::ffi::OsStr;
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
/// on success, or a named failure. Other commands are still stubs: each says
/// so on stderr and exits non-zero.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Stall {
            action: StallCommand::Start { plugin, daemon_pid },
        } => match start(&plugin, daemon_pid) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                let line = error_line(error.reason());
                let _ = write_line(io::stdout().lock(), &line);
                eprintln!("barn stall start: {error}");
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

/// The launch inputs barn derives from its own process and the environment.
struct Launch {
    stall: StallSpec,
    helper: PathBuf,
    state_dir: PathBuf,
    daemon_pid: u32,
}

fn start(plugin: &Path, daemon_pid: u32) -> Result<(), StartError> {
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
    if let Some(root) = env::var_os("BARN_STATE_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root));
    }

    let home = env::var_os("HOME").ok_or(StartError::StateDirMissing)?;

    Ok(Path::new(&home).join(".local").join("state").join("barn"))
}

async fn start_async(
    plugin: &Path,
    program: PathBuf,
    daemon_pid: u32,
    helper: PathBuf,
    state_dir: PathBuf,
) -> Result<(), StartError> {
    let launch = prepare_launch(plugin, &program, daemon_pid, helper, state_dir)?;
    let supervisor = Supervisor::new(launch.state_dir, launch.helper);
    let mut running = supervisor
        .start_for_host(launch.stall, launch.daemon_pid)
        .await?;
    let endpoint = running.host_endpoint();

    write_endpoint(&endpoint)?;

    let connection = running.accept().await?;
    running.supervise(connection, |_| {}).await?;

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

    if state_dir.join("run").join(&stall.name).exists() {
        return Err(StartError::RunDirInUse);
    }

    Ok(Launch {
        stall,
        helper,
        state_dir,
        daemon_pid,
    })
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

fn write_endpoint(endpoint: &HostEndpoint) -> Result<(), StartError> {
    let socket_path = endpoint
        .socket_path
        .to_str()
        .ok_or(StartError::SocketPath)?;
    let line = endpoint_line(socket_path, &endpoint.token)?;

    write_line(io::stdout().lock(), &line)
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use super::{
        Cli, CliCommand, Grant, StallCommand, StallsCommand, endpoint_line, error_line,
        prepare_launch, runtime_from, stall_spec,
    };

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

        let Some((plugin, daemon_pid)) = (match cli.command {
            CliCommand::Stall {
                action: StallCommand::Start { plugin, daemon_pid },
            } => Some((plugin, daemon_pid)),
            _ => None,
        }) else {
            return;
        };

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
        let root = std::env::temp_dir().join(format!("barn-cli-{}", std::process::id()));
        let run_dir = root.join("run").join("sourcefed");
        fs::create_dir_all(&run_dir).expect("a live run directory");

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
        fs::remove_dir_all(&root).expect("cleanup");
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
        let token = "x".repeat(barn_stall::MAX_LAUNCH_LINE_BYTES);
        assert_eq!(
            endpoint_line("/run/host.sock", &token)
                .err()
                .map(|error| error.reason()),
            Some("endpoint line too long")
        );

        let path = format!("/{}", "p".repeat(barn_stall::MAX_LAUNCH_LINE_BYTES));
        assert_eq!(
            endpoint_line(&path, "token")
                .err()
                .map(|error| error.reason()),
            Some("endpoint line too long")
        );
    }
}
