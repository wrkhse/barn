//! Starts a sandboxed stall, authenticates it and supervises its health.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use barn_stall::protocol::TOKEN_TTL_SECS;
use barn_stall::{Job, StallStartParams, StallStartResult};
use barn_stall::{MAX_LINE_BYTES, PROTOCOL_VERSION, method};
use nono::{AccessMode, CapabilitySet, NonoError, SandboxState, UnixSocketMode};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::time::{Instant as TokioInstant, MissedTickBehavior, interval_at};
use tokio::time::{sleep, timeout, timeout_at};

use crate::{Grant, PolicyError};

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_mins(5);
const CRASH_WINDOW: Duration = Duration::from_mins(10);
const CRASH_LIMIT: usize = 5;
const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PENDING_HEALTH_FRAMES: usize = 4;
const MAX_LATE_HEALTH_REPLIES: usize = 3;
const MAX_EXIT_DRAIN_FRAMES: usize = 64;
const MAX_EXIT_DRAIN_BYTES: usize = 64 * 1024;
const EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_BYTES: usize = 32;
const TOKEN_TTL: Duration = Duration::from_secs(TOKEN_TTL_SECS);
const PRIVATE_DIR: u32 = 0o700;
const PRIVATE_FILE: u32 = 0o600;
const SOCKET_FILE: &str = "proto.sock";
const POLICY_FILE: &str = "policy.json";
/// The shell shim; a grant that lets a stall execute it gets `SHELL_GRANTS`.
const SHELL_SHIM: &str = "/bin/sh";
/// macOS's `/bin/sh` reads this link to choose the shell it runs.
const SHELL_LINK: &str = "/private/var/select/sh";
/// The link and the shells it can name, each granted read as a literal.
#[cfg(target_os = "macos")]
const SHELL_GRANTS: &[&str] = &[SHELL_LINK, "/bin/bash", "/bin/zsh", "/bin/dash"];
#[cfg(not(target_os = "macos"))]
const SHELL_GRANTS: &[&str] = &[];
const OVERSIZE_REPLY: &[u8] = concat!(
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"#,
    r#""message":"stall start message exceeds the protocol line limit"}}"#,
    "\n"
)
.as_bytes();

/// A stall and the in-memory grant that bounds it.
pub struct StallSpec {
    /// A single path component; `run` is reserved in any ASCII case.
    pub name: String,
    /// The absolute path to the program to run, as allowed by the grant.
    pub program: PathBuf,
    /// The program's arguments.
    pub args: Vec<OsString>,
    /// The sandbox grant.
    pub grant: Grant,
}

/// Starts stalls with the named `barn-exec` helper under a trusted state root.
#[derive(Clone)]
pub struct Supervisor {
    state_dir: PathBuf,
    helper: PathBuf,
}

/// A child, its listener and its single-use authentication state.
pub struct RunningStall {
    child: Child,
    pid: u32,
    transport: Transport,
    supervisor: Supervisor,
    spec: StallSpec,
    started: Instant,
    epoch: Instant,
    restart: RestartPolicy,
    health: HealthTracker,
}

struct Transport {
    listener: UnixListener,
    run_dir: RunDir,
    state: String,
    uid: u32,
    token: Option<Token>,
    jobs: BTreeMap<String, Job>,
}

/// A start token and the moment it stops being accepted.
struct Token {
    secret: String,
    expires_at: Instant,
}

struct Prepared {
    listener: StdUnixListener,
    run_dir: RunDir,
    state: String,
    uid: u32,
    token: String,
    program: PathBuf,
}

struct RunDir {
    path: PathBuf,
    _lock: File,
}

type StallStream = BufReader<UnixStream>;

#[derive(Default)]
struct HealthProbe {
    id: u64,
    frame: Vec<u8>,
    pending: Vec<Vec<u8>>,
}

/// What happens after a running child exits.
pub enum RestartOutcome {
    /// The child exits successfully; barn does not restart it.
    Exited,
    /// The child exceeds the crash limit and stays stopped.
    Failed,
    /// barn starts a replacement child after the backoff.
    Restarted {
        /// The exit status of the crashed or unhealthy child.
        crash: ExitStatus,
        /// The replacement, with a fresh listener and token.
        stall: Box<RunningStall>,
    },
}

/// Whether the crash permits another start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartDecision {
    /// Start a replacement after this delay.
    After(Duration),
    /// Leave the stall stopped until explicitly started.
    Failed,
}

/// Tracks crash frequency and exponential backoff across child launches.
pub struct RestartPolicy {
    crashes: VecDeque<Duration>,
    next_delay: Duration,
    failed: bool,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            crashes: VecDeque::with_capacity(CRASH_LIMIT),
            next_delay: FIRST_BACKOFF,
            failed: false,
        }
    }
}

impl RestartPolicy {
    /// Records a crash at a monotonic time relative to the first start.
    ///
    /// Pass the elapsed uptime of the child that crashed. A run of at least
    /// ten minutes resets both the backoff and the crash window.
    pub fn crashed(&mut self, now: Duration, uptime: Duration) -> RestartDecision {
        if self.failed {
            return RestartDecision::Failed;
        }

        if uptime >= CRASH_WINDOW {
            self.next_delay = FIRST_BACKOFF;
        }

        self.crashes
            .retain(|crash| now.saturating_sub(*crash) < CRASH_WINDOW);
        self.crashes.push_back(now);

        if self.crashes.len() >= CRASH_LIMIT {
            self.failed = true;
            return RestartDecision::Failed;
        }

        let delay = self.next_delay;
        self.next_delay = delay.saturating_mul(2).min(MAX_BACKOFF);

        RestartDecision::After(delay)
    }
}

/// The outcome of one check of a running child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthCheck {
    /// The child answers the check.
    Answered,
    /// The child misses the check.
    Missed,
}

/// Whether a child continues running after a health check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthDecision {
    /// Leave the child running.
    Continue,
    /// Restart the child.
    Restart,
}

/// Counts consecutive missed checks for one child.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HealthTracker {
    /// No consecutive misses.
    #[default]
    Healthy,
    /// One check goes unanswered.
    OneMiss,
    /// Two checks go unanswered.
    TwoMisses,
    /// Three checks go unanswered; the caller restarts the child.
    Restart,
}

impl HealthTracker {
    /// Records a check. A replacement child starts with a fresh tracker.
    pub fn record(&mut self, check: HealthCheck) -> HealthDecision {
        use HealthCheck::{Answered, Missed};
        use HealthTracker::{Healthy, OneMiss, Restart, TwoMisses};

        *self = match (*self, check) {
            (Restart, _) | (TwoMisses, Missed) => Restart,
            (Healthy | OneMiss | TwoMisses, Answered) => Healthy,
            (Healthy, Missed) => OneMiss,
            (OneMiss, Missed) => TwoMisses,
        };

        if *self == Restart {
            HealthDecision::Restart
        } else {
            HealthDecision::Continue
        }
    }
}

/// Why a stall cannot start, connect or restart.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// The name is reserved or not a single path component.
    #[error("invalid stall name")]
    InvalidName,
    /// A name differs only in case from one already on disk.
    #[error("stall name collides with one differing only in case")]
    NameCollision,
    /// The target is not the executable named in the grant.
    #[error("stall program does not match its executable grant")]
    ExecutableNotGranted,
    /// The state path cannot be put in a JSON string.
    #[error("stall state path is not UTF-8")]
    StatePath,
    /// The run root is not a private directory.
    #[error("run root is not a private directory")]
    RunRoot,
    /// The stall state is not a private directory owned by the state root's owner.
    #[error("stall state is not a private directory")]
    StateDirectory,
    /// The grant cannot be converted into a sandbox policy.
    #[error("building stall grant: {0}")]
    Grant(#[from] PolicyError),
    /// nono cannot grant access to the connect-back socket.
    #[error("granting access to the stall socket: {0}")]
    SocketPolicy(#[source] NonoError),
    /// nono cannot grant access to the stall's state directory.
    #[error("granting access to the stall state directory: {0}")]
    StatePolicy(#[source] NonoError),
    /// nono cannot grant read access to macOS's `/bin/sh` link or its shells.
    #[error("granting access to the macOS shell: {0}")]
    ShellPolicy(#[source] NonoError),
    /// Preparing the transport fails.
    #[error("preparing stall transport: {0}")]
    Prepare(#[from] tokio::task::JoinError),
    /// A filesystem, process, or socket operation fails.
    #[error("{operation}: {source}")]
    Io {
        /// The operation that fails.
        operation: &'static str,
        /// The underlying system error.
        source: io::Error,
    },
    /// A process starts without an observable PID.
    #[error("spawned stall has no PID")]
    MissingChildPid,
    /// The peer's PID is unavailable.
    #[error("stall socket peer has no PID")]
    MissingPeerPid,
    /// The peer's PID differs from the spawned child's PID.
    #[error("stall socket peer PID does not match the spawned child")]
    PeerPidMismatch,
    /// The peer's UID differs from the socket owner's UID.
    #[error("stall socket peer UID does not match the socket owner")]
    PeerUidMismatch,
    /// The connection or its first message does not arrive before the deadline.
    #[error("stall connection timed out")]
    ConnectTimeout,
    /// A child does not exit before the deadline after barn asks it to stop.
    #[error("unhealthy stall did not exit after termination")]
    TerminationTimeout,
    /// The peer closes before completing a line.
    #[error("stall start message ended before its newline")]
    IncompleteFrame,
    /// The peer's first message exceeds the protocol line limit.
    #[error("stall start message exceeds the protocol line limit")]
    FrameTooLarge,
    /// The first line is not a JSON-RPC start request.
    #[error("expected a JSON-RPC stall.start request")]
    InvalidStart,
    /// JSON cannot be read or written.
    #[error("encoding or decoding stall JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The peer speaks an unsupported version.
    #[error("unsupported stall protocol version: {0}")]
    Version(u32),
    /// The token does not match.
    #[error("invalid stall token")]
    InvalidToken,
    /// The token has already been spent.
    #[error("stall token already spent")]
    TokenSpent,
    /// The token's deadline has passed; it is dropped unspent.
    #[error("stall token expired")]
    TokenExpired,
}

type Result<T> = std::result::Result<T, SupervisorError>;

impl Supervisor {
    /// Uses a trusted state root and the path to the `barn-exec` helper.
    #[must_use]
    pub const fn new(state_dir: PathBuf, helper: PathBuf) -> Self {
        Self { state_dir, helper }
    }

    /// Starts one child and listens for its handshake.
    ///
    /// Run this on a Tokio runtime with I/O and time enabled.
    ///
    /// # Errors
    /// Returns the stage that fails while preparing or starting the child.
    pub async fn start(&self, spec: StallSpec) -> Result<RunningStall> {
        self.clone()
            .launch(spec, Instant::now(), RestartPolicy::default())
            .await
    }

    async fn launch(
        self,
        spec: StallSpec,
        epoch: Instant,
        restart: RestartPolicy,
    ) -> Result<RunningStall> {
        let state_dir = self.state_dir.clone();
        let (spec, prepared) = tokio::task::spawn_blocking(move || {
            let prepared = prepare(&state_dir, &spec)?;

            Ok::<_, SupervisorError>((spec, prepared))
        })
        .await??;

        let listener = io_at(
            "registering stall listener",
            UnixListener::from_std(prepared.listener),
        )?;
        let env = ["PATH", "HOME", "LANG"]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key, value)));
        let child = Command::new(&self.helper)
            .arg(prepared.run_dir.path(POLICY_FILE))
            .arg(prepared.program)
            .args(&spec.args)
            .env_clear()
            .envs(env)
            .env("BARN_SOCKET", prepared.run_dir.path(SOCKET_FILE))
            .env("BARN_TOKEN", &prepared.token)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let child = io_at("spawning barn-exec", child)?;
        let started = Instant::now();
        let pid = child.id().ok_or(SupervisorError::MissingChildPid)?;
        // A deadline past the clock's range fails closed: the token expires now.
        let expires_at = started.checked_add(TOKEN_TTL).unwrap_or(started);

        Ok(RunningStall {
            child,
            pid,
            transport: Transport {
                listener,
                run_dir: prepared.run_dir,
                state: prepared.state,
                uid: prepared.uid,
                token: Some(Token {
                    secret: prepared.token,
                    expires_at,
                }),
                jobs: BTreeMap::new(),
            },
            supervisor: self,
            spec,
            started,
            epoch,
            restart,
            health: HealthTracker::default(),
        })
    }
}

impl RunningStall {
    /// The jobs the stall declared, keyed by name.
    #[must_use]
    pub const fn jobs(&self) -> &BTreeMap<String, Job> {
        &self.transport.jobs
    }

    /// The PID shared by the helper and the program it executes.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The path a stall uses to connect back to barn.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.transport.run_dir.path(SOCKET_FILE)
    }

    /// Accepts a connection from the child and responds to its start request.
    ///
    /// The returned reader keeps bytes the peer sent after the handshake.
    /// Connections from other PIDs are dropped; only the token deadline bounds
    /// a flood of them, so one peer cannot end the real child's handshake.
    ///
    /// # Errors
    /// Returns `TokenSpent` when the token is gone, `TokenExpired` when the
    /// deadline passes, or a named error for an invalid start.
    pub async fn accept(&mut self) -> Result<BufReader<UnixStream>> {
        self.transport.accept(self.pid).await
    }

    /// Records one health check and returns the decision for this child.
    pub fn record_health(&mut self, check: HealthCheck) -> HealthDecision {
        self.health.record(check)
    }

    /// Checks the accepted connection every ten seconds until the child exits or
    /// misses three checks, then applies the restart policy for this child.
    /// Passes buffered non-health lines to `on_frame` in arrival order after
    /// each check, including a miss, or when the child exits during a check.
    /// The caller accepts and supervises a replacement from the returned outcome.
    /// Dropping the future closes the connection and stops the owned child.
    ///
    /// # Errors
    /// Returns the stage that fails while waiting, stopping or restarting.
    pub async fn supervise(
        mut self,
        stream: StallStream,
        mut on_frame: impl FnMut(Vec<u8>),
    ) -> Result<RestartOutcome> {
        let now = TokioInstant::now();
        let start = now.checked_add(HEALTH_INTERVAL).unwrap_or(now);
        let mut ticks = interval_at(start, HEALTH_INTERVAL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut probe = HealthProbe::default();
        let mut connection = stream;

        loop {
            let check = tokio::select! {
                biased;
                status = self.child.wait() => {
                    io_at("waiting for stall", status)?;

                    None
                }
                check = async {
                    ticks.tick().await;
                    probe.tick(&mut connection, &mut on_frame).await
                } => Some(check),
            };

            probe.pending.drain(..).for_each(&mut on_frame);
            let Some(check) = check else {
                drain_on_exit(&mut connection, probe.id, &mut on_frame).await;

                break;
            };

            if self.record_health(check) == HealthDecision::Restart {
                break;
            }
        }

        // Close the connection before the child is stopped or replaced.
        drop(connection);

        self.wait_and_restart().await
    }

    /// Waits for the child to exit and applies the restart policy on a crash
    /// or after three consecutive missed health checks.
    ///
    /// Dropping the future stops supervising this child.
    ///
    /// # Errors
    /// Returns the stage that fails while waiting or starting a replacement.
    pub async fn wait_and_restart(mut self) -> Result<RestartOutcome> {
        let unhealthy = self.health == HealthTracker::Restart;
        let status = if unhealthy {
            io_at("terminating unhealthy stall", self.child.start_kill())?;
            let status = timeout(STOP_TIMEOUT, self.child.wait())
                .await
                .map_err(|_| SupervisorError::TerminationTimeout)?;

            io_at("waiting for unhealthy stall", status)?
        } else {
            io_at("waiting for stall", self.child.wait().await)?
        };

        if status.success() && !unhealthy {
            return Ok(RestartOutcome::Exited);
        }

        let now = Instant::now();
        let decision = self.restart.crashed(
            now.saturating_duration_since(self.epoch),
            now.saturating_duration_since(self.started),
        );
        let RestartDecision::After(delay) = decision else {
            return Ok(RestartOutcome::Failed);
        };

        drop(self.transport);
        sleep(delay).await;

        let stall = self
            .supervisor
            .launch(self.spec, self.epoch, self.restart)
            .await?;

        Ok(RestartOutcome::Restarted {
            crash: status,
            stall: Box::new(stall),
        })
    }
}

impl HealthProbe {
    async fn tick(
        &mut self,
        connection: &mut StallStream,
        on_frame: impl FnMut(Vec<u8>),
    ) -> HealthCheck {
        let Some(id) = self.id.checked_add(1) else {
            return HealthCheck::Missed;
        };
        let Some(deadline) = TokioInstant::now().checked_add(HEALTH_TIMEOUT) else {
            return HealthCheck::Missed;
        };
        self.id = id;
        let method = method::STALL_HEALTH;
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method});
        let Ok(line) = bounded_frame(&request) else {
            return HealthCheck::Missed;
        };

        // A cancelled write may leave a partial frame; this check is missed.
        let sent = timeout_at(deadline, connection.write_all(&line)).await;

        if !matches!(sent, Ok(Ok(()))) {
            return HealthCheck::Missed;
        }

        // The caller owns the connection, so a cancelled tick leaves it open.
        let reading = self.read_reply(connection, on_frame);
        let reply = timeout_at(deadline, reading).await;

        reply.unwrap_or(HealthCheck::Missed)
    }

    async fn read_reply(
        &mut self,
        reader: &mut (impl AsyncBufRead + Unpin),
        mut on_frame: impl FnMut(Vec<u8>),
    ) -> HealthCheck {
        // Allow two late answers from consecutive missed probes before this one.
        for _ in 0..MAX_LATE_HEALTH_REPLIES {
            loop {
                let remaining = MAX_LINE_BYTES.saturating_sub(self.frame.len());
                let limit = u64::try_from(remaining).unwrap_or(u64::MAX);
                if reader
                    .take(limit)
                    .read_until(b'\n', &mut self.frame)
                    .await
                    .is_err()
                {
                    return HealthCheck::Missed;
                }

                if self.frame.last() != Some(&b'\n') {
                    return HealthCheck::Missed;
                }

                let line = std::mem::take(&mut self.frame);
                match classify_health_frame(&line, self.id) {
                    HealthFrame::Late => break,
                    HealthFrame::Matching => return HealthCheck::Answered,
                    HealthFrame::Invalid(_) => return HealthCheck::Missed,
                    HealthFrame::Ordinary => self.keep_frame(line, &mut on_frame),
                }
            }
        }

        HealthCheck::Missed
    }

    /// Holds at most the cap of caller frames; older ones go to the caller.
    fn keep_frame(&mut self, line: Vec<u8>, mut on_frame: impl FnMut(Vec<u8>)) {
        if self.pending.len() >= MAX_PENDING_HEALTH_FRAMES {
            for frame in self.pending.drain(..) {
                on_frame(frame);
            }
        }

        self.pending.push(line);
    }
}

/// Why a success reply cannot answer the current health check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InvalidHealthReply {
    /// Zero cannot name a check.
    Zero,
    /// An id ahead of the probe cannot answer an earlier check.
    Future,
    /// An id that is not a number cannot name a check.
    NonNumeric,
}

/// How a line on the health connection relates to the current probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HealthFrame {
    /// A line for the caller, such as a log.
    Ordinary,
    /// The success reply to an earlier check.
    Late,
    /// The success reply to this check.
    Matching,
    /// A success reply that cannot name a check, with its reason.
    Invalid(InvalidHealthReply),
}

/// Classifies one line by its JSON-RPC success reply id.
fn classify_health_frame(line: &[u8], current: u64) -> HealthFrame {
    let reply: Value = match serde_json::from_slice(line) {
        Ok(reply) => reply,
        Err(_) => return HealthFrame::Ordinary,
    };

    if reply.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || reply.get("result").is_none()
        || reply.get("error").is_some()
        || reply.get("method").is_some()
    {
        return HealthFrame::Ordinary;
    }

    match reply.get("id").and_then(Value::as_u64) {
        None => HealthFrame::Invalid(InvalidHealthReply::NonNumeric),
        Some(0) => HealthFrame::Invalid(InvalidHealthReply::Zero),
        Some(id) if id < current => HealthFrame::Late,
        Some(id) if id == current => HealthFrame::Matching,
        Some(_) => HealthFrame::Invalid(InvalidHealthReply::Future),
    }
}

/// Why draining the connection at exit stops.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainStop {
    /// The peer closed and the buffer holds no more frames.
    Closed,
    /// The frame cap is reached.
    FrameLimit,
    /// The byte cap is reached.
    ByteLimit,
    /// The total deadline passed before the drain finished.
    Deadline,
}

/// Delivers the caller frames left on the connection at exit, in arrival
/// order. One deadline bounds the whole drain, so a peer that trickles frames
/// cannot hold up a restart by spending the timeout again on each line.
async fn drain_on_exit(
    reader: &mut (impl AsyncBufRead + Unpin),
    current: u64,
    mut on_frame: impl FnMut(Vec<u8>),
) -> DrainStop {
    let started = TokioInstant::now();
    let Some(deadline) = started.checked_add(EXIT_DRAIN_TIMEOUT) else {
        return DrainStop::Deadline;
    };
    let draining = collect_exit_frames(reader, current, &mut on_frame);

    timeout_at(deadline, draining)
        .await
        .unwrap_or(DrainStop::Deadline)
}

/// Collects frames until the peer closes or a frame or byte cap is reached.
/// The caller's deadline cancels a slow read as well as a slow run of frames.
async fn collect_exit_frames(
    reader: &mut (impl AsyncBufRead + Unpin),
    current: u64,
    on_frame: &mut impl FnMut(Vec<u8>),
) -> DrainStop {
    let mut frames = 0usize;
    let mut bytes = 0usize;

    while frames < MAX_EXIT_DRAIN_FRAMES && bytes < MAX_EXIT_DRAIN_BYTES {
        let remaining = MAX_EXIT_DRAIN_BYTES.saturating_sub(bytes);
        let limit = u64::try_from(remaining).unwrap_or(u64::MAX);
        let mut line = Vec::new();
        let read = reader
            .take(limit)
            .read_until(b'\n', &mut line)
            .await
            .unwrap_or(0);

        if read == 0 {
            return DrainStop::Closed;
        }

        if line.last() != Some(&b'\n') {
            return DrainStop::ByteLimit;
        }

        bytes = bytes.saturating_add(read);
        frames = frames.saturating_add(1);

        if classify_health_frame(&line, current) == HealthFrame::Ordinary {
            on_frame(line);
        }
    }

    if frames >= MAX_EXIT_DRAIN_FRAMES {
        DrainStop::FrameLimit
    } else {
        DrainStop::ByteLimit
    }
}

/// The child barn spawned and the owner of its socket.
struct PeerChecks {
    child: u32,
    owner: u32,
}

impl PeerChecks {
    const fn new(child: u32, owner: u32) -> Self {
        Self { child, owner }
    }

    /// Accepts only the child; wrong peers are dropped without a count or a
    /// cap. The token deadline bounds the wait, so a flood cannot fail the
    /// real child's handshake.
    fn check(&self, pid: Option<i32>, uid: u32) -> Result<bool> {
        match verify_peer(pid, uid, self.child, self.owner) {
            Ok(()) => Ok(true),
            Err(SupervisorError::PeerPidMismatch) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl Transport {
    /// Holds the listener open until the token expires. Wrong peers are
    /// dropped without a cap; a flood ends as `TokenExpired` with the token
    /// consumed. A spent token is refused as `TokenSpent` after its pending
    /// peer is closed, so the child sees that connection end.
    async fn accept(&mut self, child_pid: u32) -> Result<BufReader<UnixStream>> {
        let spent = self.token.is_none();
        let remaining = self.token.as_ref().map_or(Duration::ZERO, |token| {
            token.expires_at.saturating_duration_since(Instant::now())
        });
        let accepted = timeout(remaining, self.handshake(child_pid)).await;

        match accepted {
            Ok(result) => result,
            Err(_) if spent => Err(SupervisorError::TokenSpent),
            Err(_) => {
                self.token = None;

                Err(SupervisorError::TokenExpired)
            }
        }
    }

    async fn handshake(&mut self, child_pid: u32) -> Result<BufReader<UnixStream>> {
        let checks = PeerChecks::new(child_pid, self.uid);
        let stream = loop {
            let accepted = self.listener.accept().await;
            let (stream, _) = io_at("accepting stall socket", accepted)?;
            let cred = io_at("reading stall peer credentials", stream.peer_cred())?;

            if checks.check(cred.pid(), cred.uid())? {
                break stream;
            }
        };

        let mut reader = BufReader::new(stream);
        let (id, params) = match read_start(&mut reader).await {
            Err(SupervisorError::FrameTooLarge) => {
                refuse_oversize(&mut reader).await?;

                return Err(SupervisorError::FrameTooLarge);
            }
            result => result?,
        };
        spend_token(&mut self.token, &params.token, Instant::now())?;
        self.jobs = send_reply(&mut reader, &id, &self.state, &params.jobs).await?;

        Ok(reader)
    }
}

/// Writes the `stall.start` reply, or the oversize error when it cannot fit.
async fn send_reply<W>(
    out: &mut W,
    id: &Value,
    dir: &str, // the state folder
    jobs: &BTreeMap<String, Job>,
) -> Result<BTreeMap<String, Job>>
where
    W: AsyncWrite + Unpin,
{
    let frame = match reply_frame(id, dir, jobs) {
        Ok(frame) => frame,
        Err(SupervisorError::FrameTooLarge) => {
            refuse_oversize(out).await?;

            return Err(SupervisorError::FrameTooLarge);
        }
        Err(error) => return Err(error),
    };
    io_at("answering stall start", out.write_all(&frame).await)?;

    Ok(declared_jobs(&frame))
}

/// Reads the jobs barn declared in the start reply it sent.
fn declared_jobs(frame: &[u8]) -> BTreeMap<String, Job> {
    serde_json::from_slice::<Value>(frame)
        .ok()
        .and_then(|reply| reply.get("result").cloned())
        .and_then(|result| serde_json::from_value::<StallStartResult>(result).ok())
        .map_or_default(|result| result.jobs)
}

async fn refuse_oversize(writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    io_at(
        "answering oversized stall start",
        writer.write_all(OVERSIZE_REPLY).await,
    )
}

/// The `stall.start` reply as one newline-terminated line within the limit.
fn reply_frame(id: &Value, state: &str, jobs: &BTreeMap<String, Job>) -> Result<Vec<u8>> {
    let reply = StallStartResult {
        state: state.to_owned(),
        jobs: jobs.clone(),
    };
    let response = json!({"jsonrpc":"2.0","id":id,"result":reply});

    bounded_frame(&response)
}

fn bounded_frame(message: &Value) -> Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(message)?;
    frame.push(b'\n');

    if frame.len() > MAX_LINE_BYTES {
        return Err(SupervisorError::FrameTooLarge);
    }

    Ok(frame)
}

type StartRequest = (Value, StallStartParams);

async fn read_start(buf: &mut (impl AsyncBufRead + Unpin)) -> Result<StartRequest> {
    let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX);
    let mut frame = Vec::new();
    io_at(
        "reading stall start",
        buf.take(limit).read_until(b'\n', &mut frame).await,
    )?;

    if frame.last() == Some(&b'\n') {
        decode_start(&frame)
    } else if frame.len() == MAX_LINE_BYTES {
        Err(SupervisorError::FrameTooLarge)
    } else {
        Err(SupervisorError::IncompleteFrame)
    }
}

fn decode_start(frame: &[u8]) -> Result<StartRequest> {
    let mut request: Value = serde_json::from_slice(frame)?;

    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || request.get("method").and_then(Value::as_str) != Some(method::STALL_START)
    {
        return Err(SupervisorError::InvalidStart);
    }

    let request = request
        .as_object_mut()
        .ok_or(SupervisorError::InvalidStart)?;
    let id = request
        .remove("id")
        .filter(|value| value.is_string() || value.is_i64() || value.is_u64())
        .ok_or(SupervisorError::InvalidStart)?;
    let params = request
        .remove("params")
        .ok_or(SupervisorError::InvalidStart)?;
    let params: StallStartParams = serde_json::from_value(params)?;

    if params.protocol != PROTOCOL_VERSION {
        return Err(SupervisorError::Version(params.protocol));
    }

    Ok((id, params))
}

fn verify_peer(pid: Option<i32>, uid: u32, child: u32, owner: u32) -> Result<()> {
    let pid = pid.ok_or(SupervisorError::MissingPeerPid)?;

    if u32::try_from(pid).ok() != Some(child) {
        return Err(SupervisorError::PeerPidMismatch);
    }

    if uid != owner {
        return Err(SupervisorError::PeerUidMismatch);
    }

    Ok(())
}

fn spend_token(saved: &mut Option<Token>, offer: &str, now: Instant) -> Result<()> {
    let issued = saved.as_ref().ok_or(SupervisorError::TokenSpent)?;

    if now >= issued.expires_at {
        *saved = None;

        return Err(SupervisorError::TokenExpired);
    }

    let expected = &issued.secret;

    // The length is public; equal-length comparisons touch every byte.
    if expected.len() != offer.len() {
        return Err(SupervisorError::InvalidToken);
    }

    let mut difference = 0;

    for (left, right) in expected.bytes().zip(offer.bytes()) {
        difference |= black_box(left ^ right);
    }

    if difference != 0 {
        return Err(SupervisorError::InvalidToken);
    }

    *saved = None;

    Ok(())
}

fn prepare(state_dir: &Path, spec: &StallSpec) -> Result<Prepared> {
    let name = OsStr::new(&spec.name);

    if spec.name.eq_ignore_ascii_case("run") || Path::new(name).file_name() != Some(name) {
        return Err(SupervisorError::InvalidName);
    }

    let allowed = spec.grant.executable.as_ref() == Some(&spec.program);

    if !spec.program.is_absolute() || !allowed {
        return Err(SupervisorError::ExecutableNotGranted);
    }

    let program = io_at("resolving stall program", spec.program.canonicalize())?;
    let (run_dir, state) = run_paths(state_dir, &spec.name)?;
    let grant = pin_executable(&spec.grant, program.clone());
    let caps = grant
        .to_capability_set()?
        .allow_path(&state, AccessMode::ReadWrite)
        .map_err(SupervisorError::StatePolicy)?;
    let shell = shell_grants(&grant, Path::new(SHELL_SHIM), SHELL_GRANTS);
    let caps = allow_shell(caps, shell)?;

    let socket = run_dir.path(SOCKET_FILE);
    let listener = io_at("binding stall socket", StdUnixListener::bind(&socket))?;
    io_at(
        "setting stall socket nonblocking",
        listener.set_nonblocking(true),
    )?;
    chmod(&socket, PRIVATE_FILE)?;
    let uid = io_at("reading stall socket owner", fs::metadata(&socket))?.uid();

    let caps = caps
        .allow_unix_socket(socket, UnixSocketMode::Connect)
        .map_err(SupervisorError::SocketPolicy)?;
    let mut state_policy = SandboxState::from_caps(&caps);

    // Reconstruct filesystem grants from the resolved names in the helper,
    // except the shell link: Seatbelt checks the link by its own name.
    for path in &mut state_policy.fs {
        if path.original != Path::new(SHELL_LINK) {
            path.original.clone_from(&path.resolved);
        }
    }

    write_policy(&run_dir.path(POLICY_FILE), &state_policy)?;
    let token = new_token()?;

    Ok(Prepared {
        listener,
        run_dir,
        state,
        uid,
        token,
        program,
    })
}

/// `paths` when the grant lets the stall execute `shim`, else none.
///
/// The executable may name the shim, or a read path may cover it: Seatbelt
/// lets a stall execute what it can read, so it can start the shim itself.
fn shell_grants<'a>(grant: &Grant, shim: &Path, paths: &'a [&str]) -> &'a [&'a str] {
    let resolved = shim.canonicalize().ok();
    let executable = grant
        .executable
        .as_ref()
        .and_then(|path| path.canonicalize().ok());
    let names_shim = resolved.is_some() && executable == resolved;

    if names_shim || grant.allows_read(shim) {
        paths
    } else {
        &[]
    }
}

/// Read access to each of `paths` that exists, as a literal, so a stall can
/// run macOS's `/bin/sh`. A missing path is skipped.
fn allow_shell(mut caps: CapabilitySet, paths: &[&str]) -> Result<CapabilitySet> {
    let paths = paths.iter().map(Path::new);

    for path in paths.filter(|path| path.exists()) {
        caps = caps
            .allow_file(path, AccessMode::Read)
            .map_err(SupervisorError::ShellPolicy)?;
    }

    Ok(caps)
}

/// The grant with its executable set to the program barn-exec runs, so a
/// symlink that changes after resolution cannot move the policy elsewhere.
fn pin_executable(grant: &Grant, program: PathBuf) -> Grant {
    Grant {
        executable: Some(program),
        ..grant.clone()
    }
}

/// Whether `dir` holds an entry whose name matches only without case.
fn case_variant(dir: &Path, name: &str) -> Result<bool> {
    for entry in io_at("reading barn directory", fs::read_dir(dir))? {
        let entry = io_at("reading barn directory entry", entry)?;
        let existing = entry.file_name();
        let Some(existing) = existing.to_str() else {
            continue;
        };

        if existing != name && existing.eq_ignore_ascii_case(name) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn run_paths(state_dir: &Path, name: &str) -> Result<(RunDir, String)> {
    io_at(
        "creating barn state root",
        DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR)
            .create(state_dir),
    )?;
    let state_dir = io_at("resolving barn state root", state_dir.canonicalize())?;
    let owner = io_at("reading barn state root owner", fs::metadata(&state_dir))?.uid();
    let run_root = state_dir.join("run");
    io_at("creating barn run root", ensure_private_dir(&run_root))?;
    let root = io_at("checking barn run root", fs::symlink_metadata(&run_root))?;

    if !is_private_dir(&root, owner) {
        return Err(SupervisorError::RunRoot);
    }

    // A case-only variant shares these paths on a case-insensitive volume.
    if case_variant(&state_dir, name)? || case_variant(&run_root, name)? {
        return Err(SupervisorError::NameCollision);
    }

    let state = state_dir
        .join(name)
        .into_os_string()
        .into_string()
        .map_err(|_| SupervisorError::StatePath)?;
    io_at(
        "creating stall state directory",
        ensure_private_dir(Path::new(&state)),
    )?;
    let meta = io_at(
        "checking stall state directory",
        fs::symlink_metadata(&state),
    )?;

    if !is_private_dir(&meta, owner) {
        return Err(SupervisorError::StateDirectory);
    }

    let run_dir = RunDir::create(run_root.join(name), owner)?;

    Ok((run_dir, state))
}

fn is_private_dir(meta: &fs::Metadata, owner: u32) -> bool {
    meta.is_dir() && meta.uid() == owner && meta.mode() & 0o7777 == PRIVATE_DIR
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(PRIVATE_DIR).create(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        result => result,
    }
}

impl RunDir {
    fn create(path: PathBuf, uid: u32) -> Result<Self> {
        let mut builder = DirBuilder::new();
        builder.mode(PRIVATE_DIR);

        match builder.create(&path) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let lock = lock_dir(&path, uid);
                let _stale = io_at("locking stale stall run directory", lock)?;
                io_at("removing stale stall run directory", remove_run_dir(&path))?;
                io_at(
                    "creating private stall run directory",
                    builder.create(&path),
                )?;
            }
            result => io_at("creating private stall run directory", result)?,
        }

        let lock = io_at("locking stall run directory", lock_dir(&path, uid))?;
        let dir = Self { path, _lock: lock };
        chmod(&dir.path, PRIVATE_DIR)?;

        Ok(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

fn lock_dir(path: &Path, uid: u32) -> io::Result<File> {
    let meta = fs::symlink_metadata(path)?;

    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }

    let dir = File::open(path)?;
    dir.try_lock()?;
    let locked = dir.metadata()?;
    let current = fs::symlink_metadata(path)?;

    // A competing start can replace the directory before the lock is held.
    check_locked_dir(&meta, &locked, &current)?;

    Ok(dir)
}

fn check_locked_dir(
    meta: &fs::Metadata,
    locked: &fs::Metadata,
    current: &fs::Metadata,
) -> io::Result<()> {
    if (meta.dev(), meta.ino()) != (locked.dev(), locked.ino())
        || (locked.dev(), locked.ino()) != (current.dev(), current.ino())
    {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }

    Ok(())
}

fn remove_run_dir(path: &Path) -> io::Result<()> {
    // Only remove the files barn owns; never traverse the stall's state.
    for name in [SOCKET_FILE, POLICY_FILE] {
        match fs::remove_file(path.join(name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
    }

    fs::remove_dir(path)
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = remove_run_dir(&self.path);
    }
}

fn write_policy(path: &Path, state: &SandboxState) -> Result<()> {
    let json = serde_json::to_vec(state)?;
    let mut file = io_at(
        "opening stall policy",
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE)
            .open(path),
    )?;
    io_at(
        "setting stall policy permissions",
        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE)),
    )?;

    io_at("writing stall policy", file.write_all(&json))
}

fn chmod(path: &Path, mode: u32) -> Result<()> {
    io_at(
        "setting private stall permissions",
        fs::set_permissions(path, fs::Permissions::from_mode(mode)),
    )
}

fn new_token() -> Result<String> {
    use std::fmt::Write as _;

    let mut bytes = [0; TOKEN_BYTES];
    let mut random = io_at("opening token source", File::open("/dev/urandom"))?;
    io_at("reading stall token", random.read_exact(&mut bytes))?;
    let mut token = String::new();

    for byte in bytes {
        // Formatting into a String cannot fail.
        let _ = write!(token, "{byte:02x}");
    }

    Ok(token)
}

fn io_at<T>(operation: &'static str, result: io::Result<T>) -> Result<T> {
    result.map_err(|source| SupervisorError::Io { operation, source })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileTypeExt as _;

    use super::*;

    const START: &[u8] = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"stall.start","#,
        r#""params":{"protocol":1,"token":"secret"}}"#,
    )
    .as_bytes();

    #[test]
    fn peer_credentials_require_both_a_matching_pid_and_uid() {
        assert!(verify_peer(Some(17), 42, 17, 42).is_ok());
        assert!(matches!(
            verify_peer(None, 42, 17, 42),
            Err(SupervisorError::MissingPeerPid)
        ));
        assert!(matches!(
            verify_peer(Some(18), 42, 17, 42),
            Err(SupervisorError::PeerPidMismatch)
        ));
        assert!(matches!(
            verify_peer(Some(-1), 42, u32::MAX, 42),
            Err(SupervisorError::PeerPidMismatch)
        ));
        assert!(matches!(
            verify_peer(Some(17), 43, 17, 42),
            Err(SupervisorError::PeerUidMismatch)
        ));
    }

    /// The accept loop calls `check` until the token deadline, so a flood of
    /// wrong peers cannot keep the child's own start from being accepted.
    #[test]
    fn wrong_pid_peers_never_end_the_handshake_and_a_later_child_is_accepted() {
        let wrong = Some(18);
        let checks = PeerChecks::new(17, 42);
        let flood = 1024;

        for _ in 0..flood {
            assert_eq!(checks.check(wrong, 42).ok(), Some(false));
        }
        assert_eq!(checks.check(Some(17), 42).ok(), Some(true));
    }

    fn issued(expires_at: Instant) -> Token {
        Token {
            secret: "secret".to_owned(),
            expires_at,
        }
    }

    /// A transport bound to `root/proto.sock`, with a token at `expiry`.
    fn test_transport(root: &Path, expiry: Option<Instant>) -> Transport {
        let socket = root.join(SOCKET_FILE);
        let std_listener = StdUnixListener::bind(&socket).expect("stall socket");
        std_listener
            .set_nonblocking(true)
            .expect("nonblocking stall socket");
        let owner = fs::metadata(&socket).expect("socket owner").uid();
        let listener = UnixListener::from_std(std_listener).expect("stall listener");
        let lock = File::open(root).expect("run directory lock");
        Transport {
            listener,
            run_dir: RunDir {
                path: root.to_path_buf(),
                _lock: lock,
            },
            state: String::new(),
            uid: owner,
            token: expiry.map(issued),
            jobs: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn a_second_accept_with_a_spent_token_returns_token_spent() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let socket = fixture.0.join(SOCKET_FILE);
        let mut transport = test_transport(&fixture.0, Some(deadline));
        let child = std::process::id();

        let mut start = START.to_vec();
        start.push(b'\n');
        let mut peer = UnixStream::connect(&socket)
            .await
            .expect("the child connects");
        peer.write_all(&start).await.expect("the start request");
        let stream = transport.accept(child).await.expect("the first handshake");
        drop(stream);
        drop(peer);

        // No peer sends a second frame: a spent token is refused at once.
        let short = Duration::from_millis(100);
        let second = timeout(short, transport.accept(child)).await;
        assert!(matches!(second, Ok(Err(SupervisorError::TokenSpent))));
    }

    #[tokio::test]
    async fn a_spent_token_refusal_closes_the_pending_peer() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let socket = fixture.0.join(SOCKET_FILE);
        let mut transport = test_transport(&fixture.0, Some(deadline));
        let child = std::process::id();
        let mut start = START.to_vec();
        start.push(b'\n');

        let mut peer = UnixStream::connect(&socket)
            .await
            .expect("the child connects");
        peer.write_all(&start).await.expect("the start request");
        drop(transport.accept(child).await.expect("the first handshake"));
        drop(peer);

        let mut spent_peer = UnixStream::connect(&socket)
            .await
            .expect("the child reconnects with the spent token");
        spent_peer
            .write_all(&start)
            .await
            .expect("the second start request");
        let refused = transport.accept(child).await;

        let mut extra = Vec::new();
        let short = Duration::from_millis(100);
        let closed = timeout(short, spent_peer.read_to_end(&mut extra)).await;
        assert!(matches!(refused, Err(SupervisorError::TokenSpent)));
        assert!(matches!(closed, Ok(Ok(0))));
    }

    #[tokio::test]
    async fn a_token_at_its_deadline_expires_when_no_peer_connects() {
        let fixture = PrepareFixture::new();
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the past deadline fits");
        let mut transport = test_transport(&fixture.0, Some(past));

        assert!(matches!(
            transport.accept(std::process::id()).await,
            Err(SupervisorError::TokenExpired)
        ));
        assert!(transport.token.is_none());
    }

    #[tokio::test]
    async fn wrong_pid_peers_are_bounded_only_by_the_token_expiry() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(500))
            .expect("the token deadline fits");
        let socket = fixture.0.join(SOCKET_FILE);
        let mut transport = test_transport(&fixture.0, Some(deadline));
        let arrivals = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&arrivals);

        // This process is never the expected child, so every peer is wrong.
        let wrong_child = std::process::id().wrapping_add(1);
        let flood = tokio::spawn(async move {
            loop {
                let Ok(mut peer) = UnixStream::connect(&socket).await else {
                    break;
                };
                counted.fetch_add(1, Ordering::Relaxed);
                let _ = peer.write_all(START).await;
                sleep(Duration::from_millis(5)).await;
            }
        });

        let accepted = transport.accept(wrong_child).await;
        flood.abort();

        assert!(arrivals.load(Ordering::Relaxed) > 1);
        assert!(matches!(accepted, Err(SupervisorError::TokenExpired)));
        assert!(transport.token.is_none());
    }

    #[test]
    fn only_the_exact_token_is_spent() {
        let now = Instant::now();
        let deadline = now.checked_add(TOKEN_TTL).expect("the deadline fits");
        let mut saved = Some(issued(deadline));

        for offered in ["secre", "secrets", "xecret", "secrex", "secreT"] {
            assert!(matches!(
                spend_token(&mut saved, offered, now),
                Err(SupervisorError::InvalidToken)
            ));
        }

        assert!(matches!(spend_token(&mut saved, "secret", now), Ok(())));
        assert!(saved.is_none());
        assert!(matches!(
            spend_token(&mut saved, "secret", now),
            Err(SupervisorError::TokenSpent)
        ));
    }

    #[test]
    fn a_token_expires_at_its_deadline() {
        let start = Instant::now();
        let deadline = start.checked_add(TOKEN_TTL).expect("the deadline fits");
        let before = deadline
            .checked_sub(Duration::from_nanos(1))
            .expect("the instant before the deadline fits");
        let after = deadline
            .checked_add(Duration::from_secs(1))
            .expect("the instant after the deadline fits");

        let mut saved = Some(issued(deadline));
        assert!(matches!(spend_token(&mut saved, "secret", before), Ok(())));
        assert!(saved.is_none());

        for now in [deadline, after] {
            let mut saved = Some(issued(deadline));

            assert!(matches!(
                spend_token(&mut saved, "secret", now),
                Err(SupervisorError::TokenExpired)
            ));
            assert!(saved.is_none());
        }
    }

    #[test]
    fn tokens_are_fresh_fixed_length_lowercase_hex() {
        let first = new_token().ok();
        let second = new_token().ok();
        let shape = |token: &str| {
            let hex = token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));

            (token.len(), hex)
        };

        assert_eq!(first.as_deref().map(shape), Some((64, true)));
        assert_eq!(second.as_deref().map(shape), Some((64, true)));
        assert_ne!(first, second);
    }

    #[test]
    fn refusing_invalid_specs_does_not_need_a_state_root() {
        let mut spec = StallSpec {
            name: String::new(),
            program: "/does-not-exist".into(),
            args: Vec::new(),
            grant: Grant::default(),
        };

        for name in ["", ".", "..", "run", "RUN", "rUn", "../stall", "x/x", "x/"] {
            spec.name = name.to_owned();
            assert!(matches!(
                prepare(Path::new(""), &spec),
                Err(SupervisorError::InvalidName)
            ));
        }

        spec.name = "stall".to_owned();
        assert!(matches!(
            prepare(Path::new(""), &spec),
            Err(SupervisorError::ExecutableNotGranted)
        ));

        spec.program = "relative".into();
        spec.grant.executable = Some("relative".into());
        assert!(matches!(
            prepare(Path::new(""), &spec),
            Err(SupervisorError::ExecutableNotGranted)
        ));
    }

    #[test]
    fn names_differing_only_in_case_are_rejected() {
        let fixture = PrepareFixture::new();
        let spec = |name: &str| StallSpec {
            name: name.to_owned(),
            ..PrepareFixture::spec()
        };
        let first = prepare(&fixture.0, &spec("Foo")).expect("the first name");

        for variant in ["foo", "FOO", "fOo"] {
            assert!(matches!(
                prepare(&fixture.0, &spec(variant)),
                Err(SupervisorError::NameCollision)
            ));
        }

        drop(first);
        assert!(prepare(&fixture.0, &spec("Foo")).is_ok());
    }

    #[tokio::test]
    async fn only_complete_lines_within_the_byte_limit_are_decoded() {
        let mut exact = START.to_vec();
        exact.resize(MAX_LINE_BYTES.saturating_sub(1), b' ');
        exact.push(b'\n');

        assert_eq!(
            read_start(&mut BufReader::new(exact.as_slice())).await.ok(),
            Some((
                json!(1),
                StallStartParams {
                    protocol: 1,
                    token: "secret".to_owned(),
                    jobs: BTreeMap::new()
                }
            ))
        );

        let mut over = START.to_vec();
        over.resize(MAX_LINE_BYTES, b' ');
        over.push(b'\n');

        assert!(matches!(
            read_start(&mut BufReader::new(over.as_slice())).await,
            Err(SupervisorError::FrameTooLarge)
        ));
        assert!(matches!(
            read_start(&mut BufReader::new(START)).await,
            Err(SupervisorError::IncompleteFrame)
        ));
    }

    #[test]
    fn only_versioned_start_requests_with_an_id_are_accepted() {
        for (version, id, method) in [
            ("1.0", json!(1), "stall.start"),
            ("2.0", json!(1), "stall.health"),
            ("2.0", json!(null), "stall.start"),
            ("2.0", json!(true), "stall.start"),
            ("2.0", json!(1.5), "stall.start"),
        ] {
            let request = json!({
                "jsonrpc": version, "id": id, "method": method,
                "params": {"protocol": 1, "token": "secret"}
            });
            let frame = request.to_string();

            assert!(matches!(
                decode_start(frame.as_bytes()),
                Err(SupervisorError::InvalidStart)
            ));
        }

        let unsupported = json!({
            "jsonrpc": "2.0", "id": 1, "method": "stall.start",
            "params": {"protocol": 2, "token": "secret"}
        });
        let frame = unsupported.to_string();

        assert!(matches!(
            decode_start(frame.as_bytes()),
            Err(SupervisorError::Version(2))
        ));

        for frame in [
            br#"{"jsonrpc":"2.0","method":"stall.start","params":{}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"stall.start"}"#,
        ] {
            assert!(matches!(
                decode_start(frame),
                Err(SupervisorError::InvalidStart)
            ));
        }

        assert!(matches!(decode_start(b"["), Err(SupervisorError::Json(_))));
    }

    #[test]
    fn an_id_above_the_signed_range_is_accepted() {
        let request = json!({
            "jsonrpc": "2.0", "id": u64::MAX, "method": "stall.start",
            "params": {"protocol": 1, "token": "secret"}
        });
        let frame = request.to_string();

        assert_eq!(
            decode_start(frame.as_bytes()).ok(),
            Some((
                json!(u64::MAX),
                StallStartParams {
                    protocol: 1,
                    token: "secret".to_owned(),
                    jobs: BTreeMap::new()
                }
            ))
        );
    }

    /// The reply to id 1 with an empty state, newline included.
    const EMPTY_REPLY: &[u8] = concat!(
        r#"{"jsonrpc":"2.0","id":1,"#,
        r#""result":{"state":""}}"#,
        "\n"
    )
    .as_bytes();

    /// A state that makes the reply to id 1 exactly `MAX_LINE_BYTES` long.
    fn filling_state() -> String {
        "a".repeat(MAX_LINE_BYTES.saturating_sub(EMPTY_REPLY.len()))
    }

    #[test]
    fn a_reply_fills_the_line_limit_and_no_more() {
        assert_eq!(
            reply_frame(&json!(1), "", &BTreeMap::new()).ok(),
            Some(EMPTY_REPLY.to_vec())
        );

        let exact = filling_state();
        let below = exact.get(1..).unwrap_or_default().to_owned();
        let above = format!("{exact}a");
        let length = |state: &str| {
            let frame = reply_frame(&json!(1), state, &BTreeMap::new());

            frame.map(|frame| frame.len())
        };

        assert_eq!(length(&below).ok(), Some(MAX_LINE_BYTES.saturating_sub(1)));
        assert_eq!(length(&exact).ok(), Some(MAX_LINE_BYTES));
        assert!(matches!(
            length(&above),
            Err(SupervisorError::FrameTooLarge)
        ));
    }

    // A `Vec` sink never blocks, so a reply too large for a pipe still
    // returns and the exact bytes decide the test.
    #[tokio::test]
    async fn a_reply_that_fits_is_written_as_one_line() {
        let mut written = Vec::new();

        assert!(matches!(
            send_reply(&mut written, &json!(1), "", &BTreeMap::new()).await,
            Ok(jobs) if jobs.is_empty()
        ));
        assert_eq!(written, EMPTY_REPLY);

        let exact = filling_state();
        let line = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"state":"{exact}"}}}}"#);
        let mut written = Vec::new();

        assert!(matches!(
            send_reply(&mut written, &json!(1), &exact, &BTreeMap::new()).await,
            Ok(jobs) if jobs.is_empty()
        ));
        assert_eq!(written, format!("{line}\n").into_bytes());
    }

    #[tokio::test]
    async fn an_oversized_reply_is_answered_with_the_line_limit_error() {
        let expected = concat!(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"#,
            r#""message":"stall start message exceeds the protocol line limit"}}"#,
            "\n"
        )
        .as_bytes();
        let above = format!("{}a", filling_state());
        let mut written = Vec::new();

        assert!(matches!(
            send_reply(&mut written, &json!(1), &above, &BTreeMap::new()).await,
            Err(SupervisorError::FrameTooLarge)
        ));
        assert_eq!(written, expected);
    }

    #[test]
    fn the_oversize_reply_uses_the_message_fixed_on_the_page() {
        const PAGE: &str = include_str!("../../../docs/stall-protocol.md");
        let line = PAGE
            .lines()
            .find(|line| line.starts_with(r#"{"jsonrpc":"2.0","id":null,"error""#))
            .expect("the page fixes the oversize error line");
        let page: Value = serde_json::from_str(line).expect("the page's error JSON");
        let reply: Value = serde_json::from_slice(OVERSIZE_REPLY).expect("barn's error JSON");

        assert_eq!(reply, page);
    }

    #[test]
    fn a_start_reply_declares_the_jobs_the_stall_sent() {
        let jobs = BTreeMap::from([(
            "poll".to_owned(),
            Job {
                every: "15s".to_owned(),
            },
        )]);
        let frame = reply_frame(&json!(1), "", &jobs).expect("reply with jobs");
        let reply: Value = serde_json::from_slice(&frame).expect("reply JSON");

        assert_eq!(
            reply,
            json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {"state": "", "jobs": {"poll": {"every": "15s"}}}
            })
        );
        assert_eq!(declared_jobs(&frame), jobs);
        assert_eq!(declared_jobs(b"not JSON"), BTreeMap::new());
    }

    #[test]
    fn the_policy_grants_the_resolved_program() {
        let grant = Grant {
            read_paths: vec!["/read".into()],
            write_paths: vec!["/write".into()],
            network_hosts: vec!["example.com".to_owned()],
            executable: Some("/link".into()),
        };

        assert_eq!(
            pin_executable(&grant, "/resolved".into()),
            Grant {
                read_paths: vec!["/read".into()],
                write_paths: vec!["/write".into()],
                network_hosts: vec!["example.com".to_owned()],
                executable: Some("/resolved".into()),
            }
        );
    }

    struct PrepareFixture(PathBuf);

    impl PrepareFixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};

            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let root = Path::new("/tmp").join(format!("barn-prep-{pid}-{n}"));
            DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .expect("scratch root");

            Self(root.canonicalize().expect("canonical scratch root"))
        }

        fn spec() -> StallSpec {
            let program = std::env::current_exe().expect("test executable");

            StallSpec {
                name: "fixture".to_owned(),
                program: program.clone(),
                args: Vec::new(),
                grant: Grant {
                    executable: Some(program),
                    ..Grant::default()
                },
            }
        }
    }

    impl Drop for PrepareFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prepare_replaces_a_stale_run_directory_and_keeps_state() {
        let fixture = PrepareFixture::new();
        let spec = PrepareFixture::spec();
        let first = prepare(&fixture.0, &spec).expect("first prepare");
        let path = first.run_dir.path.clone();
        let record = Path::new(&first.state).join("record");
        fs::write(&record, b"keep").expect("stall record");

        assert_eq!(
            prepare_failure(&fixture.0, &spec),
            Some((
                "locking stale stall run directory",
                io::ErrorKind::WouldBlock
            ))
        );
        let socket = fs::symlink_metadata(path.join(SOCKET_FILE)).ok();
        assert_eq!(socket.map(|meta| meta.file_type().is_socket()), Some(true));
        drop(first);

        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("stale directory");
        let address = path.join(SOCKET_FILE);
        let stale = StdUnixListener::bind(&address).expect("stale socket");
        drop(stale);
        fs::write(path.join(POLICY_FILE), b"stale").expect("stale policy");

        let second = prepare(&fixture.0, &spec).expect("reclaim stale run dir");
        let bound = second.listener.local_addr().expect("new socket address");
        assert_eq!(bound.as_pathname(), Some(address.as_path()));
        assert_eq!(fs::read(record).ok().as_deref(), Some(b"keep".as_slice()));
    }

    type PrepareFailure = (&'static str, io::ErrorKind);

    fn prepare_failure(root: &Path, spec: &StallSpec) -> Option<PrepareFailure> {
        let result = prepare(root, spec);
        let Err(SupervisorError::Io { operation, source }) = result else {
            return None;
        };

        Some((operation, source.kind()))
    }

    #[test]
    fn prepare_does_not_remove_unknown_entries_or_follow_a_run_directory_symlink() {
        let fixture = PrepareFixture::new();
        let spec = PrepareFixture::spec();
        let first = prepare(&fixture.0, &spec).expect("first prepare");
        let run = first.run_dir.path.clone();
        let state = PathBuf::from(&first.state);
        drop(first);

        fs::create_dir(&run).expect("stale directory");
        chmod(&run, PRIVATE_DIR).expect("private stale directory");
        let unknown = run.join("unknown");
        fs::write(&unknown, b"keep").expect("unknown entry");
        assert_eq!(
            prepare_failure(&fixture.0, &spec),
            Some((
                "removing stale stall run directory",
                io::ErrorKind::DirectoryNotEmpty
            ))
        );
        assert_eq!(fs::read(&unknown).ok().as_deref(), Some(b"keep".as_slice()));
        fs::remove_file(unknown).expect("remove fixture entry");
        fs::remove_dir(&run).expect("remove stale directory");

        let record = state.join(POLICY_FILE);
        fs::write(&record, b"state").expect("state entry named like a run entry");
        std::os::unix::fs::symlink(&state, &run).expect("run directory symlink");
        assert_eq!(
            prepare_failure(&fixture.0, &spec),
            Some((
                "locking stale stall run directory",
                io::ErrorKind::PermissionDenied
            ))
        );
        assert_eq!(fs::read(record).ok().as_deref(), Some(b"state".as_slice()));
    }

    #[test]
    fn prepare_grants_read_write_on_the_created_state_directory() {
        let fixture = PrepareFixture::new();
        let state = fixture.0.join("fixture");
        let mut spec = PrepareFixture::spec();
        spec.grant.read_paths.push(state.clone());

        let prepared = prepare(&fixture.0, &spec).expect("prepare fresh state");
        let mode = fs::symlink_metadata(&state)
            .ok()
            .filter(fs::Metadata::is_dir)
            .map(|metadata| metadata.mode() & 0o777);
        assert_eq!(mode, Some(0o700));
        assert_eq!(Path::new(&prepared.state), state);

        let path = prepared.run_dir.path(POLICY_FILE);
        let bytes = fs::read(path).expect("written policy");
        let policy: SandboxState = serde_json::from_slice(&bytes).expect("policy");
        let grants: Vec<_> = policy
            .fs
            .iter()
            .filter(|cap| cap.resolved == state)
            .map(|cap| (&cap.original, cap.access.as_str(), cap.is_file))
            .collect();
        assert_eq!(
            grants,
            [(&state, "read", false), (&state, "read+write", false)]
        );
    }

    #[test]
    fn the_policy_names_resolved_paths() {
        let fixture = PrepareFixture::new();
        let target = fixture.0.join("target");
        let link = fixture.0.join("link");
        fs::create_dir(&target).expect("link target");
        std::os::unix::fs::symlink(&target, &link).expect("read path link");
        let mut spec = PrepareFixture::spec();
        spec.grant.read_paths.push(link);
        let program = spec.program.canonicalize().expect("canonical program");
        let state = fixture.0.join("fixture");

        let prepared = prepare(&fixture.0, &spec).expect("prepare");
        let bytes = fs::read(prepared.run_dir.path(POLICY_FILE)).expect("policy");
        let policy: SandboxState = serde_json::from_slice(&bytes).expect("policy");
        let grants: Vec<_> = policy
            .fs
            .into_iter()
            .map(|cap| (cap.original, cap.resolved, cap.is_file))
            .collect();

        assert_eq!(
            grants,
            [
                (target.clone(), target, false),
                (program.clone(), program, true),
                (state.clone(), state, false),
            ]
        );
    }

    type PolicyEntry = (PathBuf, PathBuf, bool);

    /// The shim literals in the policy `prepare` writes for `spec`.
    fn shim_entries(spec: &StallSpec) -> Vec<PolicyEntry> {
        let fixture = PrepareFixture::new();
        let prepared = prepare(&fixture.0, spec).expect("prepare");
        let bytes = fs::read(prepared.run_dir.path(POLICY_FILE)).expect("policy");
        let policy: SandboxState = serde_json::from_slice(&bytes).expect("policy");
        let shim = [SHELL_LINK, "/bin/bash", "/bin/zsh", "/bin/dash"];

        policy
            .fs
            .into_iter()
            .filter(|cap| shim.map(Path::new).contains(&cap.original.as_path()))
            .map(|cap| (cap.original, cap.resolved, cap.is_file))
            .collect()
    }

    /// The literals a stall that may run `/bin/sh` gets: on macOS the link,
    /// kept by its own name, and each shell that exists; elsewhere none.
    fn expected_shim_entries() -> Vec<PolicyEntry> {
        if !cfg!(target_os = "macos") {
            return Vec::new();
        }

        let link = Path::new(SHELL_LINK).canonicalize().expect("shell link");
        let shells = ["/bin/bash", "/bin/zsh", "/bin/dash"]
            .map(PathBuf::from)
            .into_iter()
            .filter(|path| path.exists())
            .map(|path| (path.clone(), path, true));

        std::iter::once((SHELL_LINK.into(), link, true))
            .chain(shells)
            .collect()
    }

    fn shell_spec() -> StallSpec {
        let mut spec = PrepareFixture::spec();
        spec.program = SHELL_SHIM.into();
        spec.grant.executable = Some(SHELL_SHIM.into());

        spec
    }

    #[test]
    fn a_stall_granted_the_shell_gets_the_shim_literals() {
        assert_eq!(shim_entries(&shell_spec()), expected_shim_entries());
    }

    #[test]
    fn a_read_path_over_the_shell_gets_the_shim_literals() {
        let mut spec = PrepareFixture::spec();
        spec.grant.read_paths.push("/bin".into());

        assert_eq!(shim_entries(&spec), expected_shim_entries());
    }

    #[test]
    fn a_stall_not_granted_the_shell_gets_no_shim_literals() {
        assert_eq!(shim_entries(&PrepareFixture::spec()), []);
    }

    /// Stand-ins for `SHELL_GRANTS`, which is empty off macOS.
    const FAKE_GRANTS: &[&str] = &["/fake/link", "/fake/shell"];

    /// An empty file standing in for the shim, so it canonicalizes.
    fn fake_shim(fixture: &PrepareFixture) -> PathBuf {
        let shim = fixture.0.join("sh");
        fs::write(&shim, b"").expect("fake shim");

        shim
    }

    #[test]
    fn a_grant_naming_the_shim_gets_the_shell_grants() {
        let fixture = PrepareFixture::new();
        let shim = fake_shim(&fixture);
        let grant = Grant {
            executable: Some(shim.clone()),
            ..Grant::default()
        };

        assert_eq!(shell_grants(&grant, &shim, FAKE_GRANTS), FAKE_GRANTS);
    }

    #[test]
    fn a_read_path_over_the_shim_gets_the_shell_grants() {
        let fixture = PrepareFixture::new();
        let shim = fake_shim(&fixture);
        let grant = Grant {
            read_paths: vec![fixture.0.clone()],
            ..Grant::default()
        };

        assert_eq!(shell_grants(&grant, &shim, FAKE_GRANTS), FAKE_GRANTS);
    }

    #[test]
    fn an_unrelated_grant_gets_no_shell_grants() {
        let fixture = PrepareFixture::new();
        let shim = fake_shim(&fixture);
        let other = fixture.0.join("other");
        fs::write(&other, b"").expect("other program");
        let grant = Grant {
            executable: Some(other),
            ..Grant::default()
        };
        let none: &[&str] = &[];

        assert_eq!(shell_grants(&grant, &shim, FAKE_GRANTS), none);
    }

    #[test]
    fn a_stall_not_granted_the_shell_cannot_run_it() {
        let fixture = PrepareFixture::new();
        let mut spec = shell_spec();
        spec.grant.executable = PrepareFixture::spec().grant.executable;
        let refused = prepare(&fixture.0, &spec).err();

        assert!(matches!(
            refused,
            Some(SupervisorError::ExecutableNotGranted)
        ));
        assert_eq!(
            refused.map(|error| error.to_string()).as_deref(),
            Some("stall program does not match its executable grant")
        );
    }

    #[test]
    fn the_shell_grant_skips_a_missing_path() {
        let fixture = PrepareFixture::new();
        let present = fixture.0.join("present");
        fs::write(&present, b"").expect("present file");
        let missing = fixture.0.join("missing");
        let paths = [&present, &missing].map(|path| path.to_str().expect("UTF-8"));

        let caps = allow_shell(CapabilitySet::new(), &paths).expect("shell grant");
        let grants: Vec<_> = caps
            .fs_capabilities()
            .iter()
            .map(|cap| (&cap.resolved, cap.access, cap.is_file))
            .collect();

        assert_eq!(grants, [(&present, AccessMode::Read, true)]);
    }

    #[test]
    fn prepare_refuses_existing_state_without_mode_0700() {
        let fixture = PrepareFixture::new();
        let state = fixture.0.join("fixture");
        let spec = PrepareFixture::spec();
        fs::create_dir(&state).expect("existing stall state");

        for mode in [0o500, 0o710, 0o701] {
            chmod(&state, mode).expect("set existing state mode");

            assert!(matches!(
                prepare(&fixture.0, &spec),
                Err(SupervisorError::StateDirectory)
            ));
        }

        chmod(&state, PRIVATE_DIR).expect("make the state private");
        let prepared = prepare(&fixture.0, &spec).expect("accept private state");
        assert_eq!(Path::new(&prepared.state), state);
    }

    #[test]
    fn a_private_directory_must_belong_to_the_expected_owner() {
        let fixture = PrepareFixture::new();
        let meta = fs::symlink_metadata(&fixture.0).expect("private directory");
        let file = fixture.0.join("file");
        fs::write(&file, b"").expect("regular file");
        chmod(&file, PRIVATE_DIR).expect("match the directory mode");
        let file = fs::symlink_metadata(file).expect("regular file metadata");

        assert!(is_private_dir(&meta, meta.uid()));
        assert!(!is_private_dir(&meta, meta.uid() ^ 1));
        assert!(!is_private_dir(&file, meta.uid()));
    }

    #[test]
    fn run_directory_creation_surfaces_errors_other_than_already_exists() {
        let fixture = PrepareFixture::new();
        let path = fixture.0.join("missing").join("run");
        let uid = fs::metadata(&fixture.0).expect("fixture metadata").uid();
        let failure = match RunDir::create(path, uid) {
            Err(SupervisorError::Io { operation, source }) => Some((operation, source.kind())),
            _ => None,
        };

        assert_eq!(
            failure,
            Some((
                "creating private stall run directory",
                io::ErrorKind::NotFound
            ))
        );
    }

    #[test]
    fn locking_requires_directory_type_owner_and_private_mode_independently() {
        let fixture = PrepareFixture::new();
        let uid = fs::metadata(&fixture.0).expect("fixture metadata").uid();
        let file = fixture.0.join("file");
        fs::write(&file, b"").expect("regular file");
        chmod(&file, PRIVATE_DIR).expect("match the directory mode");
        let denied = Some(io::ErrorKind::PermissionDenied);
        let error = |path: &Path, uid| lock_dir(path, uid).err().map(|err| err.kind());

        assert_eq!(error(&file, uid), denied);
        assert_eq!(error(&fixture.0, uid ^ 1), denied);

        chmod(&fixture.0, 0o740).expect("add only a group read bit");
        assert_eq!(error(&fixture.0, uid), denied);

        chmod(&fixture.0, PRIVATE_DIR).expect("restore private mode");
        assert_eq!(error(&fixture.0, uid), None);
    }

    #[test]
    fn locking_rejects_replacement_before_or_after_opening_the_directory() {
        let fixture = PrepareFixture::new();
        let replacement = fixture.0.join("replacement");
        fs::create_dir(&replacement).expect("different directory");
        let a = fs::metadata(&fixture.0).expect("original directory");
        let b = fs::metadata(replacement).expect("replacement metadata");

        assert_eq!(
            check_locked_dir(&a, &a, &a).err().map(|err| err.kind()),
            None
        );
        for (original, locked, current) in [(&a, &b, &b), (&a, &a, &b)] {
            assert_eq!(
                check_locked_dir(original, locked, current)
                    .err()
                    .map(|err| err.kind()),
                Some(io::ErrorKind::WouldBlock)
            );
        }
    }

    #[test]
    fn removing_run_files_propagates_unlink_errors_other_than_not_found() {
        let fixture = PrepareFixture::new();
        let entry = fixture.0.join(SOCKET_FILE);
        fs::create_dir(&entry).expect("an entry unlink cannot remove");
        // Unix platforms differ on the exact error for unlinking a directory.
        let unlink_error = fs::remove_file(entry).expect_err("cannot unlink").kind();

        assert_eq!(
            remove_run_dir(&fixture.0).err().map(|err| err.kind()),
            Some(unlink_error)
        );
    }

    fn health_pair() -> (HealthProbe, StallStream, BufReader<UnixStream>) {
        let (barn, stall) = UnixStream::pair().expect("connected sockets");
        let probe = HealthProbe::default();

        (probe, BufReader::new(barn), BufReader::new(stall))
    }

    async fn health_request(stall: &mut BufReader<UnixStream>, id: u64) {
        let mut line = String::new();
        timeout(HEALTH_TIMEOUT, stall.read_line(&mut line))
            .await
            .expect("request arrives")
            .expect("read the request");

        assert_eq!(line.as_bytes().last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&line).ok(),
            Some(json!({"id":id, "jsonrpc":"2.0", "method":"stall.health"}))
        );
    }

    #[tokio::test]
    async fn a_health_tick_requires_a_complete_matching_success_reply() {
        let health = "stall.health";
        let success = json!({"jsonrpc":"2.0", "id":1, "result":null});
        let cases = [
            (success, HealthCheck::Answered),
            (
                json!({"jsonrpc":"1.0", "id":1, "result":null}),
                HealthCheck::Missed,
            ),
            (
                json!({"jsonrpc":"2.0", "id":2, "result":null}),
                HealthCheck::Missed,
            ),
            (
                json!({"jsonrpc":"2.0", "id":0, "result":null}),
                HealthCheck::Missed,
            ),
            (
                json!({"jsonrpc":"2.0", "id":"1", "result":null}),
                HealthCheck::Missed,
            ),
            (json!({"jsonrpc":"2.0", "id":1}), HealthCheck::Missed),
            (
                json!({"jsonrpc":"2.0", "id":1, "result":null, "error":null}),
                HealthCheck::Missed,
            ),
            (
                json!({"jsonrpc":"2.0", "id":1, "result":null, "method":health}),
                HealthCheck::Missed,
            ),
            (json!([]), HealthCheck::Missed),
        ];

        for (reply, expected) in cases {
            let (mut probe, mut connection, mut stall) = health_pair();
            let (decision, ()) = tokio::join!(probe.tick(&mut connection, |_| {}), async {
                health_request(&mut stall, 1).await;
                stall
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .expect("reply");
                stall.shutdown().await.expect("end of replies");
            });

            assert_eq!(decision, expected, "{reply}");
        }
    }

    #[tokio::test]
    async fn a_matching_health_reply_is_not_late_and_preserves_unexpected_lines() {
        let mut probe = HealthProbe {
            id: 2,
            ..HealthProbe::default()
        };
        let log = b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"n\":1}\n";
        let other = b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"n\":2}\n";
        let late = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        let current = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}\n";
        let input = [log.as_slice(), late, other, current].concat();
        let mut reader = input.as_slice();

        assert_eq!(
            probe.read_reply(&mut reader, |_| {}).await,
            HealthCheck::Answered
        );
        assert_eq!(probe.pending, [log.to_vec(), other.to_vec()]);
        assert_eq!(reader, b"");
    }

    #[tokio::test]
    async fn a_reply_whose_id_equals_the_probe_id_is_the_matching_answer() {
        let line = br#"{"jsonrpc":"2.0","id":7,"result":null}"#;
        let mut with_newline = line.to_vec();
        with_newline.push(b'\n');

        assert_eq!(
            classify_health_frame(&with_newline, 7),
            HealthFrame::Matching
        );

        let mut probe = HealthProbe {
            id: 7,
            ..HealthProbe::default()
        };
        let mut reader = with_newline.as_slice();

        assert_eq!(
            probe.read_reply(&mut reader, |_| {}).await,
            HealthCheck::Answered
        );
    }

    #[tokio::test]
    async fn four_log_frames_before_the_matching_reply_are_answered_and_kept() {
        let count = MAX_PENDING_HEALTH_FRAMES.saturating_add(1);
        let logs: Vec<Vec<u8>> = (0..count)
            .map(|n| {
                let tail = format!(r#""method":"log","n":{n}"#);
                let mut line = format!(r#"{{"jsonrpc":"2.0",{tail}}}"#);
                line.push('\n');

                line.into_bytes()
            })
            .collect();
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        let mut frames = logs.concat();
        frames.extend_from_slice(reply);
        let (mut probe, mut connection, mut stall) = health_pair();
        let mut delivered = Vec::new();
        let mut sink = |frame: Vec<u8>| delivered.push(frame);

        let (check, ()) = tokio::join!(probe.tick(&mut connection, &mut sink), async {
            health_request(&mut stall, 1).await;
            stall
                .write_all(&frames)
                .await
                .expect("health reply after the log frames");
        });

        // supervise drains what a tick leaves pending after the check.
        probe.pending.drain(..).for_each(&mut sink);
        assert_eq!(check, HealthCheck::Answered);
        assert_eq!(delivered, logs);
    }

    #[tokio::test]
    async fn a_check_without_a_reply_misses_and_three_misses_restart() {
        let (mut probe, mut connection, mut stall) = health_pair();
        let mut tracker = HealthTracker::default();

        let (check, ()) = tokio::join!(probe.tick(&mut connection, |_| {}), async {
            health_request(&mut stall, 1).await;
            stall.shutdown().await.expect("end of frames");
        });

        assert_eq!(check, HealthCheck::Missed);

        for expected in [
            HealthDecision::Continue,
            HealthDecision::Continue,
            HealthDecision::Restart,
        ] {
            assert_eq!(tracker.record(check), expected);
        }
    }

    #[tokio::test]
    async fn an_invalid_health_reply_is_a_named_miss_not_a_caller_frame() {
        for (id, reason) in [
            (json!(0), InvalidHealthReply::Zero),
            (json!(2), InvalidHealthReply::Future),
            (json!("1"), InvalidHealthReply::NonNumeric),
        ] {
            let mut probe = HealthProbe {
                id: 1,
                ..HealthProbe::default()
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": null});
            let line = format!("{reply}\n");

            assert_eq!(
                classify_health_frame(line.as_bytes(), 1),
                HealthFrame::Invalid(reason)
            );

            let mut reader = line.as_bytes();
            let mut delivered = Vec::new();
            assert_eq!(
                probe
                    .read_reply(&mut reader, |frame| delivered.push(frame))
                    .await,
                HealthCheck::Missed
            );
            assert_eq!(probe.pending, Vec::<Vec<u8>>::new());
            assert_eq!(delivered, Vec::<Vec<u8>>::new());
        }
    }

    #[tokio::test]
    async fn a_health_tick_bounds_how_many_late_replies_it_reads() {
        use HealthCheck::{Answered, Missed};

        for (late, expected) in [(2, Answered), (3, Missed)] {
            let (mut probe, mut connection, mut stall) = health_pair();
            probe.id = 2;
            let old = concat!(r#"{"jsonrpc":"2.0","id":2,"result":null}"#, "\n");
            let current = "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}\n";
            let replies = format!("{}{current}", old.repeat(late));

            let (decision, ()) = tokio::join!(probe.tick(&mut connection, |_| {}), async {
                health_request(&mut stall, 3).await;
                stall
                    .write_all(replies.as_bytes())
                    .await
                    .expect("health replies");
            });
            assert_eq!(decision, expected);
        }
    }

    #[tokio::test]
    async fn a_health_timeout_keeps_the_partial_reply_for_the_next_tick() {
        let (mut probe, mut connection, mut stall) = health_pair();
        let (missed, ()) = tokio::join!(probe.tick(&mut connection, |_| {}), async {
            health_request(&mut stall, 1).await;
            stall
                .write_all(b"{\"jsonrpc\":\"2.0\",")
                .await
                .expect("partial reply");
        });
        assert_eq!(missed, HealthCheck::Missed);

        let (answered, ()) = tokio::join!(probe.tick(&mut connection, |_| {}), async {
            health_request(&mut stall, 2).await;
            let replies = concat!(
                "\"id\":1,\"result\":null}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}\n",
            );
            stall
                .write_all(replies.as_bytes())
                .await
                .expect("late and current replies");
        });
        assert_eq!(answered, HealthCheck::Answered);
    }

    #[tokio::test]
    async fn a_health_reply_cannot_use_more_than_the_line_limit() {
        for (size, expected) in [
            (MAX_LINE_BYTES, HealthCheck::Answered),
            (MAX_LINE_BYTES.saturating_add(1), HealthCheck::Missed),
        ] {
            let (mut probe, mut connection, mut stall) = health_pair();
            let mut reply = br#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_vec();
            reply.resize(size.saturating_sub(1), b' ');
            reply.push(b'\n');

            let (decision, ()) = tokio::join!(probe.tick(&mut connection, |_| {}), async {
                health_request(&mut stall, 1).await;
                let _ = timeout(HEALTH_TIMEOUT, stall.write_all(&reply)).await;
            });
            assert_eq!(decision, expected);
        }
    }

    #[tokio::test]
    async fn an_exit_during_a_check_keeps_the_connection_for_the_drain() {
        let (mut probe, mut connection, mut stall) = health_pair();
        let mut delivered = Vec::new();

        tokio::join!(
            async {
                let checking = probe.tick(&mut connection, |frame| delivered.push(frame));
                let cancelled = timeout(Duration::from_millis(500), checking).await;

                assert!(cancelled.is_err(), "the child wins the select mid-check");
            },
            async {
                health_request(&mut stall, 1).await;
                stall
                    .write_all(&[log_frame(1), log_frame(2)].concat())
                    .await
                    .expect("buffered logs before the exit");
            }
        );

        probe
            .pending
            .drain(..)
            .for_each(|frame| delivered.push(frame));
        stall
            .write_all(&log_frame(3))
            .await
            .expect("a frame still on the socket");
        drop(stall);
        let drained = timeout(
            EXIT_DRAIN_TIMEOUT.saturating_mul(4),
            drain_on_exit(&mut connection, probe.id, |frame| delivered.push(frame)),
        )
        .await;

        assert_eq!(drained, Ok(DrainStop::Closed));
        assert_eq!(delivered, [log_frame(1), log_frame(2), log_frame(3)]);
    }

    /// A caller log frame with a sequence number.
    fn log_frame(n: usize) -> Vec<u8> {
        format!("{{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"n\":{n}}}\n").into_bytes()
    }

    #[tokio::test]
    async fn draining_at_exit_delivers_buffered_logs_in_arrival_order() {
        let matching = b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}\n";
        let input = [log_frame(1), matching.to_vec(), log_frame(2), log_frame(3)].concat();
        let mut reader = BufReader::new(input.as_slice());
        let mut delivered = Vec::new();

        assert_eq!(
            drain_on_exit(&mut reader, 3, |frame| delivered.push(frame)).await,
            DrainStop::Closed
        );
        assert_eq!(delivered, [log_frame(1), log_frame(2), log_frame(3)]);
    }

    #[tokio::test]
    async fn a_trickle_of_exit_frames_stops_at_one_total_deadline() {
        let (barn, mut stall) = UnixStream::pair().expect("connected sockets");
        let mut reader = BufReader::new(barn);
        let writer = tokio::spawn(async move {
            for n in 0..MAX_EXIT_DRAIN_FRAMES {
                stall
                    .write_all(&log_frame(n))
                    .await
                    .expect("a trickled frame");
                sleep(EXIT_DRAIN_TIMEOUT / 4).await;
            }
        });

        let started = TokioInstant::now();
        let mut delivered = Vec::new();
        let stop = drain_on_exit(&mut reader, 1, |f| delivered.push(f)).await;
        let elapsed = started.elapsed();
        writer.abort();

        assert_eq!(stop, DrainStop::Deadline);
        assert!(elapsed < EXIT_DRAIN_TIMEOUT.saturating_mul(2));
        assert!(delivered.len() < MAX_EXIT_DRAIN_FRAMES);
    }

    #[tokio::test]
    async fn an_incomplete_exit_line_is_abandoned_at_the_deadline() {
        let (barn, mut stall) = UnixStream::pair().expect("connected sockets");
        let mut reader = BufReader::new(barn);
        stall
            .write_all(&log_frame(1))
            .await
            .expect("a complete log frame");
        stall
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"log\"")
            .await
            .expect("an incomplete line");

        let mut delivered = Vec::new();
        let drained = timeout(
            EXIT_DRAIN_TIMEOUT.saturating_mul(4),
            drain_on_exit(&mut reader, 1, |frame| delivered.push(frame)),
        )
        .await;

        assert_eq!(drained, Ok(DrainStop::Deadline));
        assert_eq!(delivered, [log_frame(1)]);

        stall
            .write_all(&log_frame(2))
            .await
            .expect("a frame after the deadline");
        drop(stall);
        let mut delivered = Vec::new();
        let drained = timeout(
            EXIT_DRAIN_TIMEOUT.saturating_mul(4),
            drain_on_exit(&mut reader, 1, |frame| delivered.push(frame)),
        )
        .await;

        assert_eq!(drained, Ok(DrainStop::Closed));
        assert_eq!(delivered, [log_frame(2)]);
    }

    #[tokio::test]
    async fn draining_at_exit_stops_at_the_named_caps() {
        let logs: Vec<Vec<u8>> = (0..MAX_EXIT_DRAIN_FRAMES.saturating_add(2))
            .map(log_frame)
            .collect();
        let input = logs.concat();
        let mut reader = BufReader::new(input.as_slice());
        let mut delivered = Vec::new();

        assert_eq!(
            drain_on_exit(&mut reader, 1, |frame| delivered.push(frame)).await,
            DrainStop::FrameLimit
        );
        assert_eq!(delivered, logs[..MAX_EXIT_DRAIN_FRAMES].to_vec());

        let mut large = b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"body\":\"".to_vec();
        large.resize(MAX_EXIT_DRAIN_BYTES, b'x');
        large.extend_from_slice(b"\"}\n");
        let mut reader = BufReader::new(large.as_slice());
        let mut delivered = Vec::new();

        assert_eq!(
            drain_on_exit(&mut reader, 1, |frame| delivered.push(frame)).await,
            DrainStop::ByteLimit
        );
        assert_eq!(delivered, Vec::<Vec<u8>>::new());
    }

    #[tokio::test]
    async fn draining_at_exit_stops_at_the_exact_byte_boundary() {
        let count = 8usize;
        let prefix = br#"{"jsonrpc":"2.0","method":"log","body":""#;
        let suffix = b"\"}\n";
        let frame_len = MAX_EXIT_DRAIN_BYTES / count;
        let mut frame = prefix.to_vec();
        frame.resize(frame_len.saturating_sub(suffix.len()), b'x');
        frame.extend_from_slice(suffix);
        assert_eq!(frame.len() * count, MAX_EXIT_DRAIN_BYTES);

        let mut input = frame.repeat(count);
        input.extend_from_slice(&log_frame(99));
        let mut reader = BufReader::new(input.as_slice());
        let mut delivered = Vec::new();
        let stop = drain_on_exit(&mut reader, 1, |frame| delivered.push(frame)).await;
        let total: usize = delivered.iter().map(Vec::len).sum();

        assert_eq!(stop, DrainStop::ByteLimit);
        assert_eq!(delivered.len(), count);
        assert_eq!(total, MAX_EXIT_DRAIN_BYTES);
        assert_eq!(delivered, vec![frame; count]);
    }
}
