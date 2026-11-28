//! barn's command line: the surface the supervisor answers.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use barn_stall::protocol::{DRAIN_DEADLINE_MS, method};
use barn_stall::{LAUNCH_PROTOCOL, MAX_LAUNCH_LINE_BYTES};
use clap::{Parser, Subcommand};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::timeout;

use crate::Grant;
use crate::supervisor::{
    HostDeny, HostEndpoint, RunningStall, StallEvent, StallReceiver, StallSpec, StallWriter,
    Supervisor, SupervisorError, spawn_stall_reader, stall_control_request,
};

/// How long a stall may take to connect back after the endpoint is written.
const STALL_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The id of the single drain request barn sends on the stall connection.
const DRAIN_ID: u64 = 3;

/// How long a clean host close waits to tell a live stall from a dead one.
const STALL_PROBE_TIMEOUT: Duration = Duration::from_millis(50);

/// The margin barn adds to the stall's drain budget for its own read loop.
const DRAIN_MARGIN_MS: u64 = 500;

/// How long barn waits for the stall's drain and the child's own exit: the
/// stall's budget plus a margin for barn's read loop.
const DRAIN_WAIT_TIMEOUT: Duration =
    Duration::from_millis(DRAIN_DEADLINE_MS.saturating_add(DRAIN_MARGIN_MS));

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
    /// The plugin folder does not exist.
    #[error("plugin folder missing")]
    PluginMissing,
    /// The plugin path names something other than a directory.
    #[error("plugin folder is not a directory")]
    PluginNotDirectory,
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
    /// The configured state directory is not an absolute path.
    #[error("state directory path is not absolute")]
    RelativeStateDir,
    /// A run directory for this stall is already live.
    #[error("stall run directory is in use")]
    RunDirInUse,
    /// The socket path is not UTF-8, so it cannot name a JSON string.
    #[error("socket path is not UTF-8")]
    SocketPath,
    /// The supervisor refused or lost the stall.
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    /// The stall did not connect back before the handshake deadline.
    #[error("stall handshake timed out")]
    StallHandshakeTimeout,
    /// The stall's connection ended while the host was parked.
    #[error("stall exited while the host connection was parked")]
    StallExited,
    /// The stall's connection ended, or its child exited, while no host frame
    /// was in flight.
    #[error("stall exited while the host was idle")]
    StallDeathIdle,
    /// The stall kept writing to its socket with no host frame in flight.
    #[error("stall streamed unsolicited data while idle")]
    StallFlood,
    /// The stall's reader reported a failed connection while barn was idle.
    #[error("stall connection lost while idle")]
    StallLoss,
    /// The stall wrote a line past the protocol limit while barn was idle.
    #[error("stall wrote an oversized line while idle")]
    StallOversize,
    /// The stall did not acknowledge the drain, or its child exit wait failed.
    #[error("stall drain was unacknowledged or failed")]
    DrainFailed,
    /// The host's connection failed before it sent a frame.
    #[error("host connection lost")]
    HostLoss,
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
            Self::PluginMissing => "plugin folder missing",
            Self::PluginNotDirectory => "plugin folder is not a directory",
            Self::PluginName => "plugin folder has no name",
            Self::BinaryDir => "barn executable has no folder",
            Self::HelperMissing => "barn-exec not found",
            Self::RuntimeMissing => "stall runtime not found",
            Self::StateDirMissing => "state directory unavailable",
            Self::RelativeStateDir => "state directory path not absolute",
            Self::RunDirInUse => "stall run directory in use",
            Self::SocketPath => "socket path not UTF-8",
            Self::Supervisor(_) => "stall did not start",
            Self::StallHandshakeTimeout => "stall handshake timed out",
            Self::StallExited => "stall exited while parked",
            Self::StallDeathIdle => "stall exited while idle",
            Self::StallFlood => "stall streamed unsolicited data",
            Self::StallLoss => "stall connection lost",
            Self::StallOversize => "stall line oversized",
            Self::DrainFailed => "stall drain unacknowledged or failed",
            Self::HostLoss => "host connection lost",
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

/// A supervisor failure after the endpoint reached stdout: the named line is
/// already there, so the failure is only reported on stderr.
fn after_supervisor(error: SupervisorError) -> StartFailure {
    StartFailure::after(StartError::from(error))
}

/// Names the failure a stall event reports while the host is parked waiting
/// for its one connection. A line with no frame in flight is unsolicited
/// data; an ended reader is stall death.
const fn parked_event_failure(event: Option<&StallEvent>) -> StartError {
    match event {
        Some(StallEvent::Line(_)) => StartError::StallFlood,
        Some(StallEvent::Exited) | None => StartError::StallExited,
        Some(StallEvent::Loss) => StartError::StallLoss,
        Some(StallEvent::Oversize) => StartError::StallOversize,
    }
}

/// Names the failure a stall event reports while the host is idle between
/// frames. The events name stall death while idle instead of parked.
const fn idle_event_failure(event: Option<&StallEvent>) -> StartError {
    match event {
        Some(StallEvent::Line(_)) => StartError::StallFlood,
        Some(StallEvent::Exited) | None => StartError::StallDeathIdle,
        Some(StallEvent::Loss) => StartError::StallLoss,
        Some(StallEvent::Oversize) => StartError::StallOversize,
    }
}

/// Whether the reader has already reported stall death at a host boundary.
///
/// The bounded wait keeps a live but idle stall from holding the exit open.
/// Any line taken here is unsolicited: no frame was sent to the stall.
async fn stall_died(rx: &mut StallReceiver) -> bool {
    matches!(
        timeout(STALL_PROBE_TIMEOUT, rx.recv()).await,
        Ok(None | Some(StallEvent::Exited | StallEvent::Loss))
    )
}

/// Whether a line is the success reply to the drain this side sent. A
/// `stall draining` deny for an in-flight call is not that reply.
fn is_drain_reply(line: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return false;
    };

    value.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && value.get("id").and_then(Value::as_u64) == Some(DRAIN_ID)
        && value.get("result").is_some()
        && value.get("error").is_none()
}

/// Sends one `stall.drain` and reads until its success reply arrives.
///
/// Lines before the reply, such as a `stall draining` deny for a call that
/// did not finish, are skipped. The reader caps each line, and a terminal
/// event fails the drain; the whole exchange is bounded by the caller's
/// deadline.
async fn drain_reply(stall: &mut StallWriter, rx: &mut StallReceiver) -> bool {
    let request = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{DRAIN_ID},\"method\":\"{}\"}}\n",
        method::STALL_DRAIN
    )
    .into_bytes();

    if stall.write_all(&request).await.is_err() {
        return false;
    }

    loop {
        match rx.recv().await {
            Some(StallEvent::Line(line)) if is_drain_reply(&line) => return true,
            Some(StallEvent::Line(_)) => {}
            Some(StallEvent::Exited | StallEvent::Loss | StallEvent::Oversize) | None => {
                return false;
            }
        }
    }
}

/// Asks the stall to finish its in-flight work, then waits for the child to
/// leave on its own before the drop can send SIGKILL. One deadline bounds the
/// whole exit, so a stall that never answers cannot hold it open.
///
/// Returns `DrainFailed` on the timeout, an unacknowledged drain, a failed
/// child exit wait, or a non-zero child exit; `Ok(())` only when the reply and
/// the exit both succeeded.
async fn drain_stall(
    stall: &mut StallWriter,
    rx: &mut StallReceiver,
    running: &mut RunningStall,
) -> Result<(), StartError> {
    let draining = async {
        let replied = drain_reply(stall, rx).await;
        let exited = running
            .stall_exit()
            .await
            .is_ok_and(|status| status.success());

        replied && exited
    };

    match timeout(DRAIN_WAIT_TIMEOUT, draining).await {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(StartError::DrainFailed),
    }
}

/// Drains the stall on SIGTERM, closes the host listener and ends cleanly.
///
/// The child leaves on its own during the drain, so `running` can drop its
/// run directory and socket. The close is idempotent: the listener may still
/// be open when the signal arrives before tack's host is accepted. A failed
/// drain stays the named `DrainFailed` failure.
async fn drain_on_sigterm(
    stall: &mut StallWriter,
    rx: &mut StallReceiver,
    running: &mut RunningStall,
) -> Result<(), StartFailure> {
    drain_stall(stall, rx, running)
        .await
        .map_err(StartFailure::after)?;

    running.close_host_listener();
    Ok(())
}

/// Brokers host frames until the host leaves, SIGTERM arrives or the stall
/// dies. A clean host close or SIGTERM drains the stall and ends cleanly;
/// every other end is a named failure reported after the endpoint line.
async fn broker_host(
    terminate: &mut Signal,
    host: &mut BufReader<UnixStream>,
    stall: &mut StallWriter,
    rx: &mut StallReceiver,
    running: &mut RunningStall,
) -> Result<(), StartFailure> {
    // tack owns one host connection. Park until a frame's first byte arrives:
    // an idle host is not a failure, and only a stall death ends the wait.
    // Once a byte arrives the frame has started, so the reply timeout covers
    // the rest of the frame and the stall's answer.
    loop {
        tokio::select! {
            biased;
            _ = terminate.recv() => {
                return drain_on_sigterm(stall, rx, running).await;
            }
            readable = host.fill_buf() => match readable {
                Ok(bytes) if !bytes.is_empty() => {}
                Ok(_) => {
                    if stall_died(rx).await {
                        return Err(StartFailure::after(StartError::StallExited));
                    }
                    drain_stall(stall, rx, running)
                        .await
                        .map_err(StartFailure::after)?;

                    return Ok(());
                }
                Err(_) => return Err(StartFailure::after(StartError::HostLoss)),
            },
            event = rx.recv() => {
                if let Some(StallEvent::Line(line)) = &event
                    && let Some(id) = stall_control_request(line)
                {
                    running
                        .serve_stall_control(host, stall, line, id)
                        .await
                        .map_err(after_supervisor)?;

                    continue;
                }

                return Err(StartFailure::after(idle_event_failure(event.as_ref())));
            }
            _ = running.stall_exit() => {
                return Err(StartFailure::after(StartError::StallDeathIdle));
            }
        }

        let served = tokio::select! {
            biased;
            _ = terminate.recv() => {
                return drain_on_sigterm(stall, rx, running).await;
            }
            served = running.serve_host(host, stall, rx) => served,
        };

        match served {
            Ok(()) => {}
            Err(SupervisorError::HostDenied(HostDeny::PeerLoss)) => {
                if stall_died(rx).await {
                    return Err(StartFailure::after(StartError::StallExited));
                }

                return Err(StartFailure::after(StartError::HostLoss));
            }
            Err(error) => return Err(after_supervisor(error)),
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
        let root = PathBuf::from(root);

        if !root.is_absolute() {
            return Err(StartError::RelativeStateDir);
        }

        return Ok(root);
    }

    let home = home
        .filter(|value| !value.is_empty())
        .ok_or(StartError::StateDirMissing)?;
    let root = PathBuf::from(home);

    if !root.is_absolute() {
        return Err(StartError::RelativeStateDir);
    }

    Ok(root.join(".local").join("state").join("barn"))
}

async fn start_async(
    plugin: &Path,
    program: PathBuf,
    daemon_pid: u32,
    helper: PathBuf,
    state_dir: PathBuf,
) -> Result<(), StartFailure> {
    let launch = prepare_launch(plugin, &program, daemon_pid, helper, state_dir)?;

    // A terminate signal ends the launch from here on. The stream is cancel
    // safe, so a signal that loses an accept race stays pending for the next
    // one; install it before every wait that can end the launch.
    let mut terminate = signal(SignalKind::terminate()).map_err(|source| {
        StartFailure::before(StartError::Io {
            operation: "installing the terminate signal handler",
            source,
        })
    })?;

    let supervisor = Supervisor::new(launch.state_dir, launch.helper);
    let mut spawn = std::pin::pin!(supervisor.start_for_host(launch.stall, launch.daemon_pid));
    let mut running = tokio::select! {
        biased;
        _ = terminate.recv() => {
            // The child is already spawned, so let the spawn finish and own
            // its run directory before dropping the stall: that drop removes
            // the run directory, socket and child.
            let running = spawn.await.map_err(StartError::from)?;

            drop(running);
            return Ok(());
        }
        started = &mut spawn => started.map_err(StartError::from)?,
    };
    let endpoint = running.host_endpoint();

    write_endpoint(&mut io::stdout().lock(), &endpoint)?;

    // Keep the endpoint's owner resident: tack drives the host socket. The
    // stall connects back at launch, so accept it before the host that will
    // send frames for it. Both waits are bounded, and each races SIGTERM, so
    // neither a dead stall nor a terminate signal can strand the launch.
    let stall_stream = tokio::select! {
        biased;
        _ = terminate.recv() => {
            // The stall has not connected yet, so there is no connection to
            // drain. Close the listener and let `running` drop its run dir.
            running.close_host_listener();
            return Ok(());
        }
        accepted = timeout(STALL_HANDSHAKE_TIMEOUT, running.accept()) => accepted
            .map_err(|_| StartFailure::after(StartError::StallHandshakeTimeout))?
            .map_err(after_supervisor)?,
    };
    let (read_half, write_half) = stall_stream.into_inner().into_split();
    let mut stall: StallWriter = write_half;
    let mut rx = spawn_stall_reader(read_half);
    let mut host = tokio::select! {
        biased;
        _ = terminate.recv() => {
            return drain_on_sigterm(&mut stall, &mut rx, &mut running).await;
        }
        event = rx.recv() => return Err(StartFailure::after(parked_event_failure(event.as_ref()))),
        accepted = running.accept_host_or_stall_exit() => match accepted.map_err(after_supervisor)? {
            Some(host) => host,
            None => return Err(StartFailure::after(StartError::StallExited)),
        },
    };
    // tack owns exactly one host connection. Closing the listener here makes
    // any second peer fail at connect time rather than reach the stall.
    running.close_host_listener();

    broker_host(&mut terminate, &mut host, &mut stall, &mut rx, &mut running).await
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

/// Resolves opening the run directory: a missing directory is stale, while
/// any other failure is named at the open boundary.
fn opened_run_dir(result: io::Result<File>) -> Result<Option<File>, StartError> {
    match result {
        Ok(file) => Ok(Some(file)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StartError::Io {
            operation: "opening the stall run directory",
            source,
        }),
    }
}

/// Resolves locking the run directory: a contended lock is live, an acquired
/// one is stale, and any other failure is named at the lock boundary.
fn locked_run_dir(result: io::Result<()>) -> Result<bool, StartError> {
    match result {
        Ok(()) => Ok(false),
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(source) => Err(StartError::Io {
            operation: "locking the stall run directory",
            source,
        }),
    }
}

/// Whether another start holds the run directory's lock.
///
/// An unheld directory is stale: the supervisor clears it in `RunDir::create`.
/// Only a live lock blocks a second start.
fn run_dir_is_live(path: &Path) -> Result<bool, StartError> {
    let Some(file) = opened_run_dir(File::open(path))? else {
        return Ok(false);
    };

    if locked_run_dir(file.try_lock().map_err(io::Error::from))? {
        return Ok(true);
    }

    // We own this lock; release it so the supervisor can relock the
    // directory for the start this check is allowing.
    drop(file);
    Ok(false)
}

/// The runtime is the program; the plugin folder is its first argument and
/// the only folder the grant covers.
fn stall_spec(plugin: &Path, program: &Path) -> Result<StallSpec, StartError> {
    if !plugin.is_absolute() {
        return Err(StartError::RelativePlugin);
    }

    if !plugin.exists() {
        return Err(StartError::PluginMissing);
    }

    if !plugin.is_dir() {
        return Err(StartError::PluginNotDirectory);
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

/// The exact failure line: protocol and reason, then one newline. A reason
/// too long for the protocol is shortened one character at a time on a UTF-8
/// boundary, so a failure always emits a line within the launch line limit.
fn error_line(reason: &str) -> Vec<u8> {
    let mut reason = reason;

    loop {
        let escaped = serde_json::to_string(reason).unwrap_or_else(|_| "\"error\"".to_owned());
        let line = format!("{{\"protocol\":{LAUNCH_PROTOCOL},\"error\":{escaped}}}\n").into_bytes();

        if line.len() <= MAX_LAUNCH_LINE_BYTES || reason.is_empty() {
            return line;
        }

        let Some((cut, _)) = reason.char_indices().next_back() else {
            return line;
        };

        match reason.get(..cut) {
            Some(prefix) => reason = prefix,
            None => return line,
        }
    }
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use barn_stall::MAX_LINE_BYTES;
    use clap::Parser;
    use tokio::net::UnixStream;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::{
        Cli, CliCommand, DRAIN_DEADLINE_MS, DRAIN_MARGIN_MS, DRAIN_WAIT_TIMEOUT, Grant, HostDeny,
        HostEndpoint, MAX_LAUNCH_LINE_BYTES, StallCommand, StallEvent, StallReceiver, StallSpec,
        StallWriter, StallsCommand, StartError, StartFailure, Supervisor, SupervisorError,
        drain_reply, drain_stall, endpoint_line, error_line, helper_from, idle_event_failure,
        is_drain_reply, locked_run_dir, opened_run_dir, parked_event_failure, prepare_launch,
        report_failure, runtime_from, spawn_stall_reader, stall_died, stall_spec, start,
        start_async, state_dir, state_dir_from, write_endpoint, write_endpoint_to, write_line,
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
        let root = std::env::temp_dir().join(format!("barn-cli-argv-{}", std::process::id()));
        let plugin_dir = root.join("sourcefed");
        fs::create_dir_all(&plugin_dir).expect("a plugin folder");

        let cli = Cli::try_parse_from([
            "barn",
            "stall",
            "start",
            "--plugin",
            plugin_dir.to_str().expect("a UTF-8 plugin path"),
            "--daemon-pid",
            "4321",
        ])
        .expect("the launch argv");
        assert_eq!(
            cli.command.typed(),
            format!(
                "stall start --plugin {} --daemon-pid 4321",
                plugin_dir.display()
            )
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
        let folder = plugin_dir;

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
        fs::remove_dir_all(&root).expect("cleanup");
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
    fn a_non_directory_plugin_folder_is_a_named_failure() {
        let file = std::env::temp_dir().join(format!("barn-plugin-file-{}", std::process::id()));
        fs::write(&file, "not a folder").expect("a file, not a folder");

        assert_eq!(
            stall_spec(&file, Path::new("/usr/local/bin/barn-luau-stall"))
                .err()
                .map(|error| error.reason()),
            Some("plugin folder is not a directory")
        );
        fs::remove_file(&file).expect("cleanup");
    }

    #[test]
    fn a_missing_plugin_folder_is_a_named_failure() {
        let missing =
            std::env::temp_dir().join(format!("barn-plugin-missing-{}", std::process::id()));

        assert_eq!(
            stall_spec(&missing, Path::new("/usr/local/bin/barn-luau-stall"))
                .err()
                .map(|error| error.reason()),
            Some("plugin folder missing")
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
        let plugin = root.join("plugin").join("sourcefed");
        fs::create_dir_all(&plugin).expect("a plugin folder");
        let lock = File::open(&run_dir).expect("opening the live run directory");
        lock.try_lock().expect("locking the live run directory");

        let second = prepare_launch(
            &plugin,
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
        let plugin = root.join("plugin").join("sourcefed");
        fs::create_dir_all(&plugin).expect("a plugin folder");

        let launch = prepare_launch(
            &plugin,
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
        let plugin = root.join("plugin").join("sourcefed");
        fs::create_dir_all(&plugin).expect("a plugin folder");

        let launch = prepare_launch(
            &plugin,
            Path::new("/usr/local/bin/barn-luau-stall"),
            4321,
            PathBuf::from("/usr/local/bin/barn-exec"),
            root.clone(),
        )
        .expect("a missing run directory must not block a start");

        assert_eq!(launch.stall.name, "sourcefed");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn an_unexpected_open_error_is_a_named_io_failure() {
        let failure =
            opened_run_dir(Err(io::Error::other("boom"))).expect_err("a named open failure");

        assert_eq!(failure.reason(), "opening the stall run directory");
        assert!(matches!(
            failure,
            StartError::Io {
                operation: "opening the stall run directory",
                ..
            }
        ));
    }

    #[test]
    fn an_unexpected_lock_error_is_a_named_io_failure() {
        let failure =
            locked_run_dir(Err(io::Error::other("boom"))).expect_err("a named lock failure");

        assert_eq!(failure.reason(), "locking the stall run directory");
        assert!(matches!(
            failure,
            StartError::Io {
                operation: "locking the stall run directory",
                ..
            }
        ));
    }

    #[test]
    fn a_missing_run_directory_is_not_live_via_the_open_helper() {
        let opened = opened_run_dir(Err(io::Error::from(io::ErrorKind::NotFound)))
            .expect("a missing run directory is not a failure");

        assert!(opened.is_none());
    }

    #[test]
    fn a_contended_lock_is_live_via_the_lock_helper() {
        let live = locked_run_dir(Err(io::Error::from(io::ErrorKind::WouldBlock)))
            .expect("a contended lock is not a failure");

        assert!(live);
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
    fn an_overlong_error_reason_is_bounded_and_parses() {
        let reason = "é".repeat(MAX_LAUNCH_LINE_BYTES);
        let line = error_line(&reason);

        assert!(line.len() <= MAX_LAUNCH_LINE_BYTES);
        let body = line.strip_suffix(b"\n").expect("one trailing newline");
        assert!(!body.contains(&b'\n'));
        assert_eq!(line.last(), Some(&b'\n'));

        let value: serde_json::Value = serde_json::from_slice(&line).expect("a JSON failure line");

        assert!(value.is_object());
        assert_eq!(
            value.get("protocol").and_then(serde_json::Value::as_u64),
            Some(1)
        );

        let error = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .expect("an error string");

        assert_ne!(error, "");
        assert!(reason.starts_with(error));
    }

    #[test]
    fn named_launch_failures_keep_their_machine_readable_reasons() {
        let malformed =
            serde_json::from_str::<serde_json::Value>("{").expect_err("a malformed JSON document");

        let cases = [
            (StartError::PluginName, "plugin folder has no name"),
            (StartError::SocketPath, "socket path not UTF-8"),
            (StartError::BinaryDir, "barn executable has no folder"),
            (
                StartError::Supervisor(SupervisorError::InvalidName),
                "stall did not start",
            ),
            (
                StartError::EndpointJson(malformed),
                "encoding endpoint line",
            ),
        ];

        for (error, reason) in cases {
            assert_eq!(error.reason(), reason);
        }
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
    fn a_relative_state_dir_setting_is_a_named_failure() {
        assert_eq!(
            state_dir_from(
                Some(OsString::from("relative/state")),
                Some(OsString::from("/home/barn"))
            )
            .err()
            .map(|error| error.reason()),
            Some("state directory path not absolute")
        );
    }

    #[test]
    fn a_relative_home_is_a_named_state_dir_failure() {
        let failure = state_dir_from(None, Some(OsString::from("relative/home")))
            .expect_err("a relative HOME must leave no absolute state directory");

        assert!(matches!(failure, StartError::RelativeStateDir));
        assert_eq!(failure.reason(), "state directory path not absolute");
    }

    #[test]
    fn an_empty_home_is_a_named_state_dir_failure() {
        let failure = state_dir_from(None, Some(OsString::new()))
            .expect_err("an empty HOME must leave no state directory");

        assert!(matches!(failure, StartError::StateDirMissing));
        assert_eq!(failure.reason(), "state directory unavailable");
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

    #[test]
    fn a_stall_handshake_timeout_is_a_named_after_failure() {
        let failure = StartFailure::after(StartError::StallHandshakeTimeout);

        assert_eq!(failure.error.reason(), "stall handshake timed out");
        assert!(failure.endpoint_written);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        report_failure(&mut stdout, &mut stderr, &failure);

        assert_eq!(stdout, b"".to_vec());
        assert_eq!(
            stderr,
            b"barn stall start: stall handshake timed out\n".to_vec()
        );
    }

    #[test]
    fn a_lost_stall_is_a_named_after_failure() {
        let failure = StartFailure::after(StartError::StallExited);

        assert_eq!(failure.error.reason(), "stall exited while parked");
        assert!(failure.endpoint_written);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        report_failure(&mut stdout, &mut stderr, &failure);

        assert_eq!(stdout, b"".to_vec());
        assert_eq!(
            stderr,
            b"barn stall start: stall exited while the host connection was parked\n".to_vec()
        );
    }

    /// Splits a stall socket into the writer barn keeps and the reader task.
    fn stall_halves(stream: UnixStream) -> (StallWriter, StallReceiver) {
        let (read_half, write_half) = stream.into_split();
        (write_half, spawn_stall_reader(read_half))
    }

    #[tokio::test]
    async fn a_closed_stall_is_detected_without_a_live_peer() {
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (_stall, mut rx) = stall_halves(barn_side);
        drop(peer);

        let event = rx.recv().await;

        assert_eq!(event, Some(StallEvent::Exited));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_dropped_peer_is_detected_by_the_stall_probe() {
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (_stall, mut rx) = stall_halves(barn_side);
        drop(peer);

        let died = timeout(Duration::from_secs(2), stall_died(&mut rx))
            .await
            .expect("the stall probe answered before the deadline");

        assert!(died, "a dropped peer must read as stall death");
    }

    #[tokio::test]
    async fn a_live_stall_keeps_its_buffered_reply() {
        let (barn_side, mut peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        tokio::io::AsyncWriteExt::write_all(&mut peer, reply)
            .await
            .expect("a reply on the stall socket");
        let (_stall, mut rx) = stall_halves(barn_side);

        let event = rx.recv().await;

        assert!(matches!(event, Some(StallEvent::Line(line)) if line == reply.to_vec()));
    }

    /// Small, newline-terminated lines whose total length is exactly `total`.
    fn newline_terminated_bytes(total: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(total);
        while bytes.len().saturating_add(2) <= total {
            bytes.extend_from_slice(b"x\n");
        }
        if bytes.len() < total {
            bytes.push(b'\n');
        }
        bytes
    }

    #[tokio::test]
    async fn a_flooding_stall_is_named_instead_of_drained_forever() {
        let (barn_side, mut peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (_stall, mut rx) = stall_halves(barn_side);

        let writing = tokio::spawn(async move {
            let mut flood = vec![b'x'; MAX_LINE_BYTES];
            flood.push(b'\n');

            tokio::io::AsyncWriteExt::write_all(&mut peer, &flood)
                .await
                .expect("the peer wrote a flood");
        });

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the reader named the flood before the deadline");

        writing.abort();

        assert!(matches!(event, Some(StallEvent::Oversize)));
        assert!(matches!(
            idle_event_failure(Some(&StallEvent::Oversize)),
            StartError::StallOversize
        ));
    }

    #[tokio::test]
    async fn a_stream_of_complete_lines_ends_cleanly() {
        let (barn_side, mut peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (_stall, mut rx) = stall_halves(barn_side);

        let at_limit = newline_terminated_bytes(64 * 1024);
        let total = at_limit.len();

        let exchange = async {
            let writing = async {
                tokio::io::AsyncWriteExt::write_all(&mut peer, &at_limit)
                    .await
                    .expect("the peer wrote the limit");
                drop(peer);
            };

            let draining = async {
                let mut drained = 0usize;
                loop {
                    match rx.recv().await {
                        Some(StallEvent::Line(line)) => {
                            drained = drained.saturating_add(line.len());
                        }
                        Some(StallEvent::Exited) | None => break,
                        Some(StallEvent::Oversize | StallEvent::Loss) => {
                            panic!("a line at the drain limit must not be named a failure")
                        }
                    }
                }
                drained
            };

            let ((), drained) = tokio::join!(writing, draining);
            drained
        };

        let drained = timeout(Duration::from_secs(10), exchange)
            .await
            .expect("the reader drained the limit before the deadline");

        assert_eq!(drained, total);
    }

    #[tokio::test]
    async fn a_closed_peer_mid_line_is_a_named_failure() {
        let (barn_side, mut peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (_stall, mut rx) = stall_halves(barn_side);

        // The reader must never hand a truncated fragment to serve_host as a
        // reply; it names the loss instead.
        tokio::io::AsyncWriteExt::write_all(&mut peer, b"{\"jsonrpc\":")
            .await
            .expect("a partial stall line");
        drop(peer);

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the reader ended within the deadline");

        assert_eq!(event, Some(StallEvent::Exited));
        assert!(matches!(
            idle_event_failure(event.as_ref()),
            StartError::StallDeathIdle
        ));
    }

    #[tokio::test]
    async fn a_partial_line_stays_with_the_reader_when_cancelled() {
        let (barn_side, mut peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let prefix = b"{\"jsonrpc\":\"2.0\",\"id\":1,";
        tokio::io::AsyncWriteExt::write_all(&mut peer, prefix)
            .await
            .expect("a partial stall line");
        let (_stall, mut rx) = stall_halves(barn_side);

        // The rest of the line arrives only after the timeout cancels recv.
        let (release_tx, release_rx) = oneshot::channel();
        let writing = tokio::spawn(async move {
            release_rx.await.expect("the release signal");
            tokio::io::AsyncWriteExt::write_all(&mut peer, b"\"result\":null}\n")
                .await
                .expect("the rest of the stall line");
        });

        let cancelled = timeout(Duration::from_millis(20), rx.recv()).await;
        assert!(cancelled.is_err(), "recv was cancelled mid-line");

        release_tx
            .send(())
            .expect("the peer is still waiting to write");
        writing.await.expect("the peer finished writing");

        // The reader kept the fragment, so the next recv returns the whole
        // line, not a fragment read as a reply.
        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the completed line arrives before the deadline");

        assert!(matches!(
            event,
            Some(StallEvent::Line(line))
                if line == b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n".to_vec()
        ));
    }

    #[tokio::test]
    async fn a_frame_split_across_writes_is_delivered_completely() {
        let (barn_side, mut peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (_stall, mut rx) = stall_halves(barn_side);

        let frame = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        let (head, tail) = frame.split_at(12);

        // The reader sees the fragment while the peer waits to write the
        // rest, so it must accumulate before emitting a line.
        let writing = tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(&mut peer, head)
                .await
                .expect("the first half of the frame");
            tokio::time::sleep(Duration::from_millis(30)).await;
            tokio::io::AsyncWriteExt::write_all(&mut peer, tail)
                .await
                .expect("the second half of the frame");
        });

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the whole frame arrives before the deadline");

        writing.await.expect("the peer finished writing");

        assert!(matches!(event, Some(StallEvent::Line(line)) if line == frame.to_vec()));
    }

    #[test]
    fn a_stall_event_names_a_failure_in_each_host_state() {
        assert!(matches!(
            parked_event_failure(Some(&StallEvent::Line(Vec::new()))),
            StartError::StallFlood
        ));
        assert!(matches!(
            idle_event_failure(Some(&StallEvent::Line(Vec::new()))),
            StartError::StallFlood
        ));
        assert!(matches!(
            parked_event_failure(Some(&StallEvent::Exited)),
            StartError::StallExited
        ));
        assert!(matches!(
            idle_event_failure(Some(&StallEvent::Exited)),
            StartError::StallDeathIdle
        ));
        assert!(matches!(
            parked_event_failure(None),
            StartError::StallExited
        ));
        assert!(matches!(
            idle_event_failure(None),
            StartError::StallDeathIdle
        ));
        assert!(matches!(
            parked_event_failure(Some(&StallEvent::Loss)),
            StartError::StallLoss
        ));
        assert!(matches!(
            idle_event_failure(Some(&StallEvent::Loss)),
            StartError::StallLoss
        ));
        assert!(matches!(
            idle_event_failure(Some(&StallEvent::Oversize)),
            StartError::StallOversize
        ));
        assert!(matches!(
            parked_event_failure(Some(&StallEvent::Oversize)),
            StartError::StallOversize
        ));
    }

    #[tokio::test]
    async fn a_partial_host_frame_from_a_closed_peer_is_a_named_denial() {
        // This starts a child with `barn-exec`, so it runs in CI only.
        if std::env::var_os("CI").is_none() {
            return;
        }

        let root = std::env::temp_dir().join(format!("barn-cli-host-{}", std::process::id()));
        let helper = root.join("fixture-exec");
        fs::create_dir_all(&root).expect("a scratch state root");
        fs::write(&helper, "#!/bin/sh\nshift\nexec \"$@\"\n").expect("a helper script");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("helper mode");

        let supervisor = Supervisor::new(root.join("state"), helper);
        let program = PathBuf::from("/usr/bin/true");
        let spec = StallSpec {
            name: "host".to_owned(),
            program: program.clone(),
            args: Vec::new(),
            grant: Grant {
                executable: Some(program),
                ..Grant::default()
            },
        };
        let running = supervisor
            .start_for_host(spec, std::process::id())
            .await
            .expect("a running stall");
        let (host_barn, mut host_peer) = tokio::net::UnixStream::pair().expect("a host pair");
        let (stall_barn, _stall_peer) = tokio::net::UnixStream::pair().expect("a stall pair");
        let mut host = tokio::io::BufReader::new(host_barn);
        let (mut stall, mut rx) = stall_halves(stall_barn);

        // The line never ends, and the peer's write half closes: the broker
        // reads a partial line and a clean EOF, naming `closed socket`.
        let truncated = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"request\"";
        tokio::io::AsyncWriteExt::write_all(&mut host_peer, truncated)
            .await
            .expect("a truncated host frame");
        tokio::io::AsyncWriteExt::shutdown(&mut host_peer)
            .await
            .expect("the host peer's write half-close");

        let denied = running.serve_host(&mut host, &mut stall, &mut rx).await;

        assert!(matches!(
            denied,
            Err(SupervisorError::HostDenied(HostDeny::ClosedSocket))
        ));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn a_drain_sends_the_exact_frame_and_skips_denials_to_the_reply() {
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (mut stall, mut rx) = stall_halves(barn_side);
        let mut peer = tokio::io::BufReader::new(peer);

        let serving = async {
            let mut request = Vec::new();
            tokio::io::AsyncBufReadExt::read_until(&mut peer, b'\n', &mut request)
                .await
                .expect("the drain frame");
            assert_eq!(
                request,
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}\n"
            );

            tokio::io::AsyncWriteExt::write_all(
                &mut peer,
                b"{\"jsonrpc\":\"2.0\",\"id\":9,\"error\":{\"code\":-32001,\"message\":\"denied\",\"data\":{\"reason\":\"stall draining\"}}}\n",
            )
            .await
            .expect("a stall draining denial");
            tokio::io::AsyncWriteExt::write_all(
                &mut peer,
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}\n",
            )
            .await
            .expect("the drain reply");
        };

        // Bound the whole exchange: a drain that never writes the request
        // leaves the peer waiting in read_until forever.
        let (replied, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                timeout(Duration::from_secs(2), drain_reply(&mut stall, &mut rx)),
                serving
            )
        })
        .await
        .expect("the drain exchange finished within the deadline");

        assert!(replied.expect("drain reply within deadline"));
    }

    #[tokio::test]
    async fn an_unrelated_line_does_not_end_the_drain_wait() {
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (mut stall, mut rx) = stall_halves(barn_side);
        let mut peer = tokio::io::BufReader::new(peer);

        // The peer answers the frame with one unrelated JSON-RPC line, then
        // stays open: no drain reply and no close. The wait must keep going.
        let serving = async {
            let mut request = Vec::new();
            tokio::io::AsyncBufReadExt::read_until(&mut peer, b'\n', &mut request)
                .await
                .expect("the drain frame");
            assert_eq!(
                request,
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}\n"
            );
            tokio::io::AsyncWriteExt::write_all(
                &mut peer,
                b"{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":null}\n",
            )
            .await
            .expect("an unrelated JSON-RPC line");
        };

        let (drained, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                timeout(Duration::from_millis(300), drain_reply(&mut stall, &mut rx)),
                serving
            )
        })
        .await
        .expect("the drain exchange finished within the deadline");

        assert!(
            drained.is_err(),
            "the drain wait outlives an unrelated line"
        );
    }

    #[tokio::test]
    async fn a_drain_ends_when_the_stall_closes_without_a_reply() {
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (mut stall, mut rx) = stall_halves(barn_side);
        drop(peer);

        assert!(
            !timeout(Duration::from_secs(2), drain_reply(&mut stall, &mut rx))
                .await
                .expect("drain reply within deadline")
        );
    }

    #[tokio::test]
    async fn a_stall_that_never_acknowledges_the_drain_is_a_failure() {
        // This starts a child with `barn-exec`, so it runs in CI only.
        if std::env::var_os("CI").is_none() {
            return;
        }

        let root = std::env::temp_dir().join(format!("barn-cli-drain-{}", std::process::id()));
        let helper = root.join("fixture-exec");
        fs::create_dir_all(&root).expect("a scratch state root");
        fs::write(&helper, "#!/bin/sh\nshift\nexec \"$@\"\n").expect("a helper script");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("helper mode");

        let supervisor = Supervisor::new(root.join("state"), helper);
        let program = PathBuf::from("/usr/bin/true");
        let spec = StallSpec {
            name: "drain".to_owned(),
            program: program.clone(),
            args: Vec::new(),
            grant: Grant {
                executable: Some(program),
                ..Grant::default()
            },
        };
        let mut running = supervisor
            .start_for_host(spec, std::process::id())
            .await
            .expect("a running stall");
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        drop(peer);
        let (mut stall, mut rx) = stall_halves(barn_side);

        let outcome = timeout(
            Duration::from_secs(2),
            drain_stall(&mut stall, &mut rx, &mut running),
        )
        .await
        .expect("the drain finished within the deadline");

        assert!(matches!(outcome, Err(StartError::DrainFailed)));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[tokio::test]
    async fn a_stall_that_exits_non_zero_after_the_drain_is_a_failure() {
        // This starts a child with `barn-exec`, so it runs in CI only.
        if std::env::var_os("CI").is_none() {
            return;
        }

        let root = std::env::temp_dir().join(format!("barn-d-{}", std::process::id()));
        let helper = root.join("fixture-exec");
        fs::create_dir_all(&root).expect("a scratch state root");
        fs::write(&helper, "#!/bin/sh\nshift\nexit 1\n").expect("a helper script");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("helper mode");

        let supervisor = Supervisor::new(root.join("state"), helper);
        let program = PathBuf::from("/usr/bin/true");
        let spec = StallSpec {
            name: "drain".to_owned(),
            program: program.clone(),
            args: Vec::new(),
            grant: Grant {
                executable: Some(program),
                ..Grant::default()
            },
        };
        let mut running = supervisor
            .start_for_host(spec, std::process::id())
            .await
            .expect("a running stall");
        let (barn_side, peer) = tokio::net::UnixStream::pair().expect("a socket pair");
        let (mut stall, mut rx) = stall_halves(barn_side);
        let mut peer = tokio::io::BufReader::new(peer);

        let serving = async {
            let mut request = Vec::new();
            tokio::io::AsyncBufReadExt::read_until(&mut peer, b'\n', &mut request)
                .await
                .expect("the drain frame");
            assert_eq!(
                request,
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}\n"
            );

            tokio::io::AsyncWriteExt::write_all(
                &mut peer,
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}\n",
            )
            .await
            .expect("the drain reply");
        };

        let (outcome, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(drain_stall(&mut stall, &mut rx, &mut running), serving)
        })
        .await
        .expect("the drain exchange finished within the deadline");

        assert!(matches!(outcome, Err(StartError::DrainFailed)));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn only_the_drain_success_reply_ends_the_wait() {
        assert!(is_drain_reply(
            b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}\n"
        ));
        assert!(!is_drain_reply(
            b"{\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32001,\"message\":\"denied\"}}\n"
        ));
        assert!(!is_drain_reply(
            b"{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":null}\n"
        ));
        assert!(!is_drain_reply(
            b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"log\"}\n"
        ));
        assert!(!is_drain_reply(b"not JSON\n"));
    }

    #[test]
    fn an_idle_stall_death_and_a_host_loss_are_named_after_failures() {
        let idle = StartFailure::after(StartError::StallDeathIdle);
        let host = StartFailure::after(StartError::HostLoss);

        assert_eq!(idle.error.reason(), "stall exited while idle");
        assert!(idle.endpoint_written);
        assert_eq!(host.error.reason(), "host connection lost");
        assert!(host.endpoint_written);
    }

    #[test]
    fn the_drain_wait_has_a_margin_over_the_stall_budget() {
        assert_eq!(
            DRAIN_WAIT_TIMEOUT,
            Duration::from_millis(DRAIN_DEADLINE_MS.saturating_add(DRAIN_MARGIN_MS))
        );
        assert!(DRAIN_WAIT_TIMEOUT > Duration::from_millis(DRAIN_DEADLINE_MS));
    }

    #[test]
    fn the_drain_line_cap_is_the_protocol_line_limit() {
        assert_eq!(MAX_LINE_BYTES, 8_388_608);
    }
}
