//! barn's command line: the surface the supervisor answers.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use barn_stall::MAX_LINE_BYTES;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
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
    /// Start one stall from a JSON spec and print its host endpoint.
    Start {
        /// The path to the JSON spec.
        spec: PathBuf,
        /// The path to the `barn-exec` helper.
        #[arg(long)]
        helper: PathBuf,
        /// The trusted state root.
        #[arg(long)]
        state_dir: PathBuf,
        /// The tack daemon pid barn allows on `host.sock`.
        #[arg(long)]
        host_pid: u32,
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
                Some(StallsCommand::Start {
                    spec,
                    helper,
                    state_dir,
                    host_pid,
                }) => format!(
                    "stalls start {} --helper {} --state-dir {} --host-pid {host_pid}",
                    spec.display(),
                    helper.display(),
                    state_dir.display()
                ),
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
/// Every command but `stalls start` is still a stub: B1 builds the supervisor
/// that answers it. Each stub says so on stderr and exits non-zero, so a
/// script calling barn never reads a stub as a success.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Stalls {
            action:
                Some(StallsCommand::Start {
                    spec,
                    helper,
                    state_dir,
                    host_pid,
                }),
        } => match start(&spec, helper, state_dir, host_pid) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("barn stalls start: {error}");
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
    /// The spec file could not be read or validated.
    #[error(transparent)]
    Spec(#[from] SpecError),
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
    /// The endpoint line does not fit the protocol's line limit.
    #[error("endpoint line exceeds the protocol line limit")]
    EndpointTooLong,
}

/// Why a spec file does not describe one startable stall.
#[derive(Debug, Error)]
enum SpecError {
    /// The spec file could not be read.
    #[error("reading spec file: {0}")]
    Read(#[from] io::Error),
    /// The spec is not valid JSON.
    #[error("parsing spec JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A required field is absent.
    #[error("missing spec field `{0}`")]
    Missing(String),
    /// A field has the wrong JSON type.
    #[error("spec field `{0}` has the wrong type; expected {1}")]
    WrongType(String, &'static str),
    /// A path field is not absolute.
    #[error("spec field `{0}` must be an absolute path")]
    Relative(String),
}

fn start(
    spec: &Path,
    helper: PathBuf,
    state_dir: PathBuf,
    host_pid: u32,
) -> Result<(), StartError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StartError::Io {
            operation: "building the tokio runtime",
            source,
        })?;

    runtime.block_on(start_async(spec, helper, state_dir, host_pid))
}

async fn start_async(
    spec: &Path,
    helper: PathBuf,
    state_dir: PathBuf,
    host_pid: u32,
) -> Result<(), StartError> {
    let stall = parse_spec(spec)?;
    let supervisor = Supervisor::new(state_dir, helper);
    let mut running = supervisor.start_for_host(stall, host_pid).await?;
    let endpoint = running.host_endpoint();

    write_endpoint(&endpoint)?;

    let connection = running.accept().await?;
    running.supervise(connection, |_| {}).await?;

    Ok(())
}

fn write_endpoint(endpoint: &HostEndpoint) -> Result<(), StartError> {
    let mut line = serde_json::to_vec(&json!({
        "socket_path": endpoint.socket_path,
        "token": endpoint.token,
    }))?;

    if line.len().saturating_add(1) > MAX_LINE_BYTES {
        return Err(StartError::EndpointTooLong);
    }
    line.push(b'\n');

    let mut stdout = io::stdout().lock();
    stdout.write_all(&line).map_err(|source| StartError::Io {
        operation: "writing the endpoint line",
        source,
    })?;
    stdout.flush().map_err(|source| StartError::Io {
        operation: "flushing the endpoint line",
        source,
    })
}

fn parse_spec(path: &Path) -> Result<StallSpec, SpecError> {
    let bytes = fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes)?;

    if !value.is_object() {
        return Err(SpecError::WrongType("spec".to_owned(), "object"));
    }

    let name = required_string(&value, "name", "name")?;
    let program = required_absolute(&value, "program", "program")?;
    let args = required_strings(&value, "args", "args")?;
    let grant = required_object(&value, "grant", "grant")?;
    let executable = required_absolute(grant, "executable", "grant.executable")?;
    let read_paths = required_paths(grant, "read_paths", "grant.read_paths")?;
    let write_paths = required_paths(grant, "write_paths", "grant.write_paths")?;
    let network_hosts = required_strings(grant, "network_hosts", "grant.network_hosts")?;

    Ok(StallSpec {
        name,
        program,
        args: args.into_iter().map(OsString::from).collect(),
        grant: Grant {
            executable: Some(executable),
            read_paths,
            write_paths,
            network_hosts,
        },
    })
}

fn member<'a>(object: &'a Value, key: &str, label: &'static str) -> Result<&'a Value, SpecError> {
    object
        .get(key)
        .ok_or_else(|| SpecError::Missing(label.to_owned()))
}

fn required_object<'a>(
    object: &'a Value,
    key: &str,
    label: &'static str,
) -> Result<&'a Value, SpecError> {
    let child = member(object, key, label)?;

    if child.is_object() {
        Ok(child)
    } else {
        Err(SpecError::WrongType(label.to_owned(), "object"))
    }
}

fn required_string(object: &Value, key: &str, label: &'static str) -> Result<String, SpecError> {
    let value = member(object, key, label)?;

    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SpecError::WrongType(label.to_owned(), "string"))
}

fn required_strings(
    object: &Value,
    key: &str,
    label: &'static str,
) -> Result<Vec<String>, SpecError> {
    let values = member(object, key, label)?
        .as_array()
        .ok_or_else(|| SpecError::WrongType(label.to_owned(), "array"))?;

    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| SpecError::WrongType(format!("{label}[{index}]"), "string"))
        })
        .collect()
}

fn required_absolute(object: &Value, key: &str, label: &'static str) -> Result<PathBuf, SpecError> {
    let path = PathBuf::from(required_string(object, key, label)?);

    if path.is_absolute() {
        Ok(path)
    } else {
        Err(SpecError::Relative(label.to_owned()))
    }
}

fn required_paths(
    object: &Value,
    key: &str,
    label: &'static str,
) -> Result<Vec<PathBuf>, SpecError> {
    required_strings(object, key, label)?
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            let path = PathBuf::from(raw);

            if path.is_absolute() {
                Ok(path)
            } else {
                Err(SpecError::Relative(format!("{label}[{index}]")))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
            CliCommand::Stalls {
                action: Some(StallsCommand::Start {
                    spec: PathBuf::from("/tmp/stall.json"),
                    helper: PathBuf::from("/usr/local/bin/barn-exec"),
                    state_dir: PathBuf::from("/var/lib/barn"),
                    host_pid: 42,
                }),
            }
            .typed(),
            "stalls start /tmp/stall.json --helper /usr/local/bin/barn-exec --state-dir /var/lib/barn --host-pid 42"
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
