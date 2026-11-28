//! Starts a sandboxed stall, authenticates it and supervises its health.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Debug, Formatter};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use barn_stall::protocol::{
    DEFAULT_MAX_BODY_BYTES, DENIED_CODE, HostStartParams, HostStartResult, TOKEN_TTL_SECS, deny,
};
use barn_stall::{Job, StallStartParams, StallStartResult};
use barn_stall::{MAX_LINE_BYTES, PROTOCOL_VERSION, method};
use nono::{AccessMode, CapabilitySet, NonoError, SandboxState, UnixSocketMode};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
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
const HOST_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const HOST_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const DENY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN_BYTES: usize = 32;
const TOKEN_TTL: Duration = Duration::from_secs(TOKEN_TTL_SECS);
const PRIVATE_DIR: u32 = 0o700;
const PRIVATE_FILE: u32 = 0o600;
const SOCKET_FILE: &str = "proto.sock";
const HOST_SOCKET_FILE: &str = "host.sock";
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

/// The socket and single-use token tack uses to reach a running stall.
#[derive(Clone, PartialEq, Eq)]
pub struct HostEndpoint {
    /// The Unix socket tack connects to.
    pub socket_path: PathBuf,
    /// The single-use token tack offers in `host.start`.
    pub token: String,
}

impl Debug for HostEndpoint {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostEndpoint")
            .field("socket_path", &self.socket_path)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Why barn refuses a host or stops brokering one of its frames.
///
/// Every variant writes the reason fixed on the protocol page into a
/// `denied` reply before the connection closes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDeny {
    /// The peer is not the tack daemon barn was told to expect.
    WrongPeerPid,
    /// The peer does not own the host socket.
    WrongPeerUid,
    /// The host token is gone or past its deadline.
    TokenExpiredOrSpent,
    /// The offered host token does not match.
    TokenMismatch,
    /// The version or envelope of `host.start` does not match.
    ProtocolMismatch,
    /// A line is longer than the line cap.
    OversizeFrame,
    /// A line is not valid, or its body cannot be measured.
    BadFrame,
    /// A body is longer than the body cap.
    OversizeBody,
    /// The peer closed before finishing the exchange.
    ClosedSocket,
    /// The stall did not answer within the reply deadline.
    StalledReply,
    /// The stall's connection ended before it answered.
    PeerLoss,
}

impl HostDeny {
    /// The reason fixed on the protocol page for this denial.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::WrongPeerPid => deny::WRONG_PEER_PID,
            Self::WrongPeerUid => deny::WRONG_PEER_UID,
            Self::TokenExpiredOrSpent => deny::TOKEN_EXPIRED_OR_SPENT,
            Self::TokenMismatch => deny::TOKEN_MISMATCH,
            Self::ProtocolMismatch => deny::PROTOCOL_MISMATCH,
            Self::OversizeFrame | Self::OversizeBody => deny::OVERSIZE_FRAME,
            Self::BadFrame => deny::BAD_FRAME,
            Self::ClosedSocket | Self::StalledReply | Self::PeerLoss => deny::CLOSED_SOCKET,
        }
    }
}

/// One event from the stall's read half, assembled by its owning reader task.
#[derive(Debug, Eq, PartialEq)]
pub enum StallEvent {
    /// A complete newline-terminated line, including its newline.
    Line(Vec<u8>),
    /// The stall's write half closed.
    Exited,
    /// Reading the stall's stream failed.
    Loss,
    /// A line reached the protocol cap without ending within it.
    Oversize,
}

/// Env-gated diagnostic for the bounded stall-event channel. Setting
/// `BARN_TEST_TRACE` prints timestamped events to stderr; nothing else reads
/// the variable, so tracing cannot change supervision behavior.
fn trace_stall_event(message: impl FnOnce() -> String) {
    if std::env::var_os("BARN_TEST_TRACE").is_none() {
        return;
    }
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    eprintln!("[stall-event {:?}] {}", start.elapsed(), message());
}

/// The variant name of an event, without its payload.
fn stall_event_kind(event: &StallEvent) -> &'static str {
    match event {
        StallEvent::Line(_) => "Line",
        StallEvent::Exited => "Exited",
        StallEvent::Loss => "Loss",
        StallEvent::Oversize => "Oversize",
    }
}

/// The receiving end of the stall's event stream.
pub struct StallReceiver {
    rx: mpsc::Receiver<StallEvent>,
}

impl StallReceiver {
    /// Waits for the next event, or `None` once the reader task is gone.
    pub async fn recv(&mut self) -> Option<StallEvent> {
        trace_stall_event(|| "recv start: waiting for event".to_owned());
        let event = self.rx.recv().await;
        match &event {
            Some(event) => {
                trace_stall_event(|| format!("recv done: Some({})", stall_event_kind(event)))
            }
            None => trace_stall_event(|| "recv done: None".to_owned()),
        }
        event
    }
}

/// The stall's write half; one reader task owns its read half.
pub type StallWriter = OwnedWriteHalf;

/// Spawns the only task that reads a stall stream, returning its receiver.
///
/// The task assembles newline-terminated lines in its own buffer, so a reply
/// split across writes arrives as one `Line`. `Exited`, `Loss` and
/// `Oversize` are terminal: the task stops after sending one, or as soon as
/// the receiver is gone.
#[must_use]
pub fn spawn_stall_reader(stream: OwnedReadHalf) -> StallReceiver {
    let (tx, rx) = mpsc::channel(2);
    trace_stall_event(|| "channel created cap=2".to_owned());
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let limit = u64::try_from(MAX_LINE_BYTES.saturating_add(1)).unwrap_or(u64::MAX);

        loop {
            let mut line = Vec::new();
            let event = match (&mut reader).take(limit).read_until(b'\n', &mut line).await {
                Err(_) => StallEvent::Loss,
                Ok(_) if line.len() > MAX_LINE_BYTES => StallEvent::Oversize,
                Ok(_) if line.last() == Some(&b'\n') => StallEvent::Line(line),
                // An empty read or a line without a newline is the peer
                // closing before it finished the line.
                Ok(_) => StallEvent::Exited,
            };
            let terminal = !matches!(event, StallEvent::Line(_));

            trace_stall_event(|| format!("send start: kind={}", stall_event_kind(&event)));
            let sent = tx.send(event).await;
            trace_stall_event(|| format!("send done: {}", if sent.is_ok() { "Ok" } else { "Err" }));
            if sent.is_err() || terminal {
                break;
            }
        }
    });

    StallReceiver { rx }
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
    host: HostTransport,
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

/// The host listener, its single-use token and the tack daemon it expects.
struct HostTransport {
    listener: Option<UnixListener>,
    token: Option<Token>,
    daemon_pid: Option<u32>,
    uid: u32,
}

/// A start token and the moment it stops being accepted.
struct Token {
    secret: String,
    expires_at: Instant,
}

struct Prepared {
    listener: StdUnixListener,
    host_listener: StdUnixListener,
    run_dir: RunDir,
    state: String,
    uid: u32,
    token: String,
    host_token: String,
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
    /// A host request is refused with the reason named on the protocol page.
    #[error("host denied: {0:?}")]
    HostDenied(HostDeny),
    /// A named denial could not be written before its deadline.
    #[error("host denial timed out")]
    DenialTimeout,
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
            .launch(spec, None, Instant::now(), RestartPolicy::default())
            .await
    }

    /// Starts one child and records the tack daemon allowed on `host.sock`.
    ///
    /// Run this on a Tokio runtime with I/O and time enabled.
    ///
    /// # Errors
    /// Returns the stage that fails while preparing or starting the child.
    pub async fn start_for_host(&self, spec: StallSpec, daemon_pid: u32) -> Result<RunningStall> {
        self.clone()
            .launch(
                spec,
                Some(daemon_pid),
                Instant::now(),
                RestartPolicy::default(),
            )
            .await
    }

    async fn launch(
        self,
        spec: StallSpec,
        daemon_pid: Option<u32>,
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
        let host_listener = io_at(
            "registering host listener",
            UnixListener::from_std(prepared.host_listener),
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
            host: HostTransport {
                listener: Some(host_listener),
                token: Some(Token {
                    secret: prepared.host_token,
                    expires_at,
                }),
                daemon_pid,
                uid: prepared.uid,
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

    /// The socket and token tack uses to reach this stall.
    #[must_use]
    pub fn host_endpoint(&self) -> HostEndpoint {
        HostEndpoint {
            socket_path: self.transport.run_dir.path(HOST_SOCKET_FILE),
            token: self
                .host
                .token
                .as_ref()
                .map_or_else(String::new, |token| token.secret.clone()),
        }
    }

    /// Accepts tack on `host.sock` and answers `host.start`.
    ///
    /// Every refusal is written to the peer as a named `denied` reply before
    /// the connection closes; wrong peers are never retried in a loop. A spent
    /// or expired token still answers the next host with `TokenExpiredOrSpent`
    /// on the wire, and only a restart issues a new one.
    ///
    /// # Errors
    /// Returns the named denial, a timeout, or the failing I/O stage.
    pub async fn accept_host(&mut self) -> Result<BufReader<UnixStream>> {
        let Some(daemon_pid) = self.host.daemon_pid else {
            return Err(SupervisorError::HostDenied(HostDeny::WrongPeerPid));
        };

        self.host
            .accept_with_deadline(daemon_pid, &self.spec.name)
            .await
    }

    /// Accepts tack on `host.sock`, or names this stall when its child exits.
    ///
    /// The child wait is polled first, so an exit is not lost to a host that
    /// arrives at the same moment. The host keeps its own deadline and daemon
    /// guard: neither is cloned nor weakened here.
    ///
    /// # Errors
    /// Returns the named denial, timeout or I/O stage from the host, the
    /// failing wait, or the missing-daemon-pid denial.
    pub async fn accept_host_or_stall_exit(&mut self) -> Result<Option<BufReader<UnixStream>>> {
        let Some(daemon_pid) = self.host.daemon_pid else {
            return Err(SupervisorError::HostDenied(HostDeny::WrongPeerPid));
        };

        tokio::select! {
            biased;
            status = self.child.wait() => {
                io_at("waiting for stall exit", status)?;
                Ok(None)
            }
            host = self.host.accept_with_deadline(daemon_pid, &self.spec.name) => {
                host.map(Some)
            }
        }
    }

    /// Closes `host.sock` after tack's one host has been accepted.
    ///
    /// No second host can be legal, so the listener is dropped at once and a
    /// later `accept_host` fails without waiting for a peer. The socket path
    /// stays until the run directory is dropped.
    pub fn close_host_listener(&mut self) {
        self.host.close();
    }

    /// Waits for this stall's child to exit without consuming the stall.
    ///
    /// The wait needs only `&mut self` and touches no stream or listener, so
    /// a caller can race it against work on the host and stall streams and
    /// name a stall that died while the host was idle. It returns the status
    /// `wait_and_restart` would observe.
    ///
    /// # Errors
    /// Returns the failing wait operation.
    pub async fn stall_exit(&mut self) -> Result<ExitStatus> {
        io_at("waiting for stall exit", self.child.wait().await)
    }

    /// Brokers one framed request from `host` to the stall and its reply back.
    ///
    /// The frame bytes travel unchanged: no hook payload is read or changed.
    /// A stalled or failed write to the stall leaves that connection dead: no
    /// further frame can be brokered on it, and the host is denied. The stall's
    /// reply must carry the request's id: a mismatch or an unsolicited frame is
    /// denied as `bad frame`, and every denial after a request was read echoes
    /// that request's id.
    ///
    /// # Errors
    /// Returns a named denial for an oversize frame, a lost peer, a stalled
    /// reply or a mismatched reply id, else the failing I/O stage.
    pub async fn serve_host(
        &self,
        host: &mut BufReader<UnixStream>,
        stall: &mut StallWriter,
        rx: &mut StallReceiver,
    ) -> Result<()> {
        broker_host_frame(host, stall, rx, HOST_REPLY_TIMEOUT).await
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
                let partial = std::mem::take(&mut probe.frame);
                let id = probe.id;
                drain_on_exit(&mut connection, id, partial, &mut on_frame).await;

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
            .launch(self.spec, self.host.daemon_pid, self.epoch, self.restart)
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
/// order. A partial frame from the health read resumes before later frames,
/// so a line split by the exit is delivered whole. One deadline bounds the
/// whole drain, so a peer that trickles frames cannot hold up a restart by
/// spending the timeout again on each line.
async fn drain_on_exit(
    reader: &mut (impl AsyncBufRead + Unpin),
    current: u64,
    partial: Vec<u8>,
    mut on_frame: impl FnMut(Vec<u8>),
) -> DrainStop {
    let started = TokioInstant::now();
    let Some(deadline) = started.checked_add(EXIT_DRAIN_TIMEOUT) else {
        return DrainStop::Deadline;
    };
    let draining = collect_exit_frames(reader, current, partial, &mut on_frame);

    timeout_at(deadline, draining)
        .await
        .unwrap_or(DrainStop::Deadline)
}

/// Collects frames until the peer closes or a frame or byte cap is reached.
/// The first line resumes `partial`, the bytes a health read left without a
/// newline. The caller's deadline cancels a slow read as well as a slow run
/// of frames.
async fn collect_exit_frames(
    reader: &mut (impl AsyncBufRead + Unpin),
    current: u64,
    mut line: Vec<u8>,
    on_frame: &mut impl FnMut(Vec<u8>),
) -> DrainStop {
    let mut frames = 0usize;
    let mut bytes = line.len();

    while frames < MAX_EXIT_DRAIN_FRAMES && bytes < MAX_EXIT_DRAIN_BYTES {
        let remaining = MAX_EXIT_DRAIN_BYTES.saturating_sub(bytes);
        let limit = u64::try_from(remaining).unwrap_or(u64::MAX);
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
        let frame = std::mem::take(&mut line);

        if classify_health_frame(&frame, current) == HealthFrame::Ordinary {
            on_frame(frame);
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

impl HostTransport {
    /// Drops the listener so no further host can connect.
    fn close(&mut self) {
        self.listener = None;
    }

    /// Accepts one host connection and reads its `host.start` line within the
    /// handshake window, then completes the exchange before the token expires.
    ///
    /// The single-use token follows one rule: a peer-identity denial leaves it
    /// live for the real daemon; every other handshake failure spends it. A
    /// peer is authenticated from its socket credentials before its line is
    /// read, so a silent wrong-pid peer is refused at once and cannot burn the
    /// token. When no peer arrives before the window, the token is dropped
    /// with no peer to answer; a token already spent still takes one
    /// connection so its named deny reaches the wire.
    async fn accept_with_deadline(
        &mut self,
        daemon_pid: u32,
        stall_name: &str,
    ) -> Result<BufReader<UnixStream>> {
        let remaining = self.token.as_ref().map_or(Duration::ZERO, |token| {
            token.expires_at.saturating_duration_since(Instant::now())
        });
        let spent = remaining.is_zero();
        // A spent token still gets one peer so its named deny reaches the wire;
        // an unspent one never waits past its deadline either.
        let window = if spent {
            HOST_HANDSHAKE_TIMEOUT
        } else {
            HOST_HANDSHAKE_TIMEOUT.min(remaining)
        };
        let Some(listener) = self.listener.as_ref() else {
            self.token = None;

            return Err(SupervisorError::HostDenied(HostDeny::ClosedSocket));
        };
        let stream = if let Ok(accepted) = timeout(window, listener.accept()).await {
            io_at("accepting host socket", accepted)?.0
        } else {
            self.token = None;

            return Err(SupervisorError::HostDenied(HostDeny::TokenExpiredOrSpent));
        };

        self.accept_host_peer(stream, daemon_pid, stall_name, window, spent)
            .await
    }

    /// Completes the post-accept handshake: peer credentials, the early peer
    /// check, the bounded line read, and the authenticated exchange. The token
    /// rule and named denies match the caller's; `window` and `spent` are the
    /// values sampled before the accept so timing does not change the deny.
    async fn accept_host_peer(
        &mut self,
        stream: UnixStream,
        daemon_pid: u32,
        stall_name: &str,
        window: Duration,
        spent: bool,
    ) -> Result<BufReader<UnixStream>> {
        let mut reader = BufReader::new(stream);
        let cred = io_at(
            "reading host peer credentials",
            reader.get_ref().peer_cred(),
        )?;
        // A wrong peer is refused before the timed read: a silent wrong-pid
        // peer must leave the token live for the real daemon.
        if let Err(host_deny) = host_peer(cred.pid(), cred.uid(), daemon_pid, self.uid) {
            return self.refuse_host(&mut reader, &Value::Null, host_deny).await;
        }
        // The line arrives while the peer is still unauthenticated: bound its
        // read by the handshake window, not by the token's full lifetime.
        let line = match timeout(window, read_line_bounded(&mut reader)).await {
            Ok(Ok(line)) => line,
            Ok(Err(host_deny)) => {
                return self.refuse_host(&mut reader, &Value::Null, host_deny).await;
            }
            Err(_) => {
                self.token = None;
                let deny = if spent {
                    HostDeny::TokenExpiredOrSpent
                } else {
                    HostDeny::StalledReply
                };

                return refuse(&mut reader, &Value::Null, deny, DENY_WRITE_TIMEOUT).await;
            }
        };

        // `window` was sampled before the accept and the timed read. Recompute
        // the remaining time so the authenticated exchange cannot run past
        // `expires_at`; an already spent token keeps the handshake window.
        let exchange_limit = if spent {
            HOST_HANDSHAKE_TIMEOUT
        } else {
            self.token.as_ref().map_or(Duration::ZERO, |token| {
                token.expires_at.saturating_duration_since(Instant::now())
            })
        };
        let exchanged = timeout(
            exchange_limit,
            self.exchange(
                &mut reader,
                cred.pid(),
                cred.uid(),
                daemon_pid,
                stall_name,
                &line,
            ),
        )
        .await;

        match exchanged {
            Ok(Ok(())) => Ok(reader),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                self.token = None;
                let deny = if spent {
                    HostDeny::TokenExpiredOrSpent
                } else {
                    HostDeny::StalledReply
                };

                refuse(&mut reader, &Value::Null, deny, DENY_WRITE_TIMEOUT).await
            }
        }
    }

    /// Writes one named denial and applies the token rule: a peer-identity
    /// denial leaves the token live for the real daemon; every other handshake
    /// failure spends it.
    async fn refuse_host<T>(
        &mut self,
        reader: &mut BufReader<UnixStream>,
        id: &Value,
        host_deny: HostDeny,
    ) -> Result<T> {
        if !matches!(host_deny, HostDeny::WrongPeerPid | HostDeny::WrongPeerUid) {
            self.token = None;
        }
        refuse(reader, id, host_deny, DENY_WRITE_TIMEOUT).await
    }

    /// Completes the authenticated part of `host.start` on a connection whose
    /// peer credentials and request line have already been read.
    async fn exchange(
        &mut self,
        reader: &mut BufReader<UnixStream>,
        peer_pid: Option<i32>,
        host_uid: u32,
        daemon_pid: u32,
        stall_name: &str,
        line: &[u8],
    ) -> Result<()> {
        let id = request_id(line);

        if let Err(host_deny) = host_peer(peer_pid, host_uid, daemon_pid, self.uid) {
            return self.refuse_host(reader, &id, host_deny).await;
        }
        let (id, params) = match decode_host_start(line) {
            Ok(start) => start,
            Err(host_deny) => return self.refuse_host(reader, &id, host_deny).await,
        };
        if params.pid != daemon_pid {
            return self.refuse_host(reader, &id, HostDeny::WrongPeerPid).await;
        }
        if params.protocol != PROTOCOL_VERSION {
            return self
                .refuse_host(reader, &id, HostDeny::ProtocolMismatch)
                .await;
        }
        if let Some(host_deny) = host_token_deny(&mut self.token, &params.token, Instant::now()) {
            return self.refuse_host(reader, &id, host_deny).await;
        }
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": HostStartResult { stall: stall_name.to_owned() }
        });
        let frame = bounded_frame(&reply)?;
        io_at("answering host start", reader.write_all(&frame).await)?;

        Ok(())
    }
}

/// Maps a host peer's credentials to the named reason for a refusal.
fn host_peer(
    pid: Option<i32>,
    uid: u32,
    child: u32,
    owner: u32,
) -> std::result::Result<(), HostDeny> {
    match verify_peer(pid, uid, child, owner) {
        Ok(()) => Ok(()),
        Err(SupervisorError::MissingPeerPid | SupervisorError::PeerPidMismatch) => {
            Err(HostDeny::WrongPeerPid)
        }
        Err(SupervisorError::PeerUidMismatch) => Err(HostDeny::WrongPeerUid),
        Err(_) => Err(HostDeny::ClosedSocket),
    }
}

/// Spends a host token, naming the reason for a mismatch, expiry or reuse.
fn host_token_deny(saved: &mut Option<Token>, offer: &str, now: Instant) -> Option<HostDeny> {
    match spend_token(saved, offer, now) {
        Ok(()) => None,
        Err(SupervisorError::InvalidToken) => Some(HostDeny::TokenMismatch),
        Err(SupervisorError::TokenExpired | SupervisorError::TokenSpent) => {
            Some(HostDeny::TokenExpiredOrSpent)
        }
        Err(_) => Some(HostDeny::ClosedSocket),
    }
}

/// Reads one bounded line, naming the reason it cannot be read.
async fn read_line_bounded(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> std::result::Result<Vec<u8>, HostDeny> {
    let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX);
    let mut line = Vec::new();

    if reader
        .take(limit)
        .read_until(b'\n', &mut line)
        .await
        .is_err()
    {
        return Err(HostDeny::PeerLoss);
    }

    if line.last() == Some(&b'\n') {
        Ok(line)
    } else if line.len() == MAX_LINE_BYTES {
        Err(HostDeny::OversizeFrame)
    } else {
        Err(HostDeny::ClosedSocket)
    }
}

/// The id a denial echoes, or null when the line has no usable id.
fn request_id(frame: &[u8]) -> Value {
    serde_json::from_slice::<Value>(frame)
        .ok()
        .and_then(|line| line.get("id").cloned())
        .filter(|id| id.is_string() || id.is_i64() || id.is_u64())
        .unwrap_or(Value::Null)
}

/// Whether a stall line is the reply to this host request.
///
/// A reply carries the request's id and is either JSON-RPC 2.0 with exactly one
/// of `result` or `error`, or a framed reply with a `type` and exactly one of
/// `outcome`, `content` or `error`. A request, a notification, a line with more
/// than one payload or an unparseable line is an unsolicited frame, denied as
/// `bad frame` with the request's id.
fn answers_request(line: &[u8], id: &Value) -> bool {
    let Ok(reply) = serde_json::from_slice::<Value>(line) else {
        return false;
    };

    if reply.get("id") != Some(id) {
        return false;
    }

    let jsonrpc = reply.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && reply.get("result").is_some() != reply.get("error").is_some();
    let framed = reply.get("type").is_some()
        && ["outcome", "content", "error"]
            .into_iter()
            .filter(|field| reply.get(*field).is_some())
            .count()
            == 1;

    jsonrpc || framed
}

/// The `host.start` request, with the id used in its reply or denial.
fn decode_host_start(frame: &[u8]) -> std::result::Result<(Value, HostStartParams), HostDeny> {
    let mut request: Value = serde_json::from_slice(frame).map_err(|_| HostDeny::BadFrame)?;
    let request = request.as_object_mut().ok_or(HostDeny::BadFrame)?;

    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || request.get("method").and_then(Value::as_str) != Some(method::HOST_START)
    {
        return Err(HostDeny::ProtocolMismatch);
    }
    let id = request
        .remove("id")
        .filter(|value| value.is_string() || value.is_i64() || value.is_u64())
        .ok_or(HostDeny::BadFrame)?;
    let params = request.remove("params").ok_or(HostDeny::BadFrame)?;
    let params: HostStartParams = serde_json::from_value(params).map_err(|_| HostDeny::BadFrame)?;

    Ok((id, params))
}

/// Writes one named `denied` reply within `limit` and closes the connection.
///
/// A denial that cannot be written in time fails closed with `DenialTimeout`;
/// no denial is claimed to be on the wire in that case.
async fn refuse<T>(
    host: &mut (impl AsyncWrite + Unpin),
    id: &Value,
    host_deny: HostDeny,
    limit: Duration,
) -> Result<T> {
    let mut message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": DENIED_CODE,
            "message": "denied",
            "data": {"reason": host_deny.reason()}
        }
    });
    let frame = match bounded_frame(&message) {
        Ok(frame) => frame,
        Err(SupervisorError::FrameTooLarge) => {
            if let Some(fields) = message.as_object_mut() {
                fields.insert("id".to_owned(), Value::Null);
            }
            bounded_frame(&message)?
        }
        Err(error) => return Err(error),
    };
    let written = timeout(limit, host.write_all(&frame)).await;

    match written {
        Ok(result) => io_at("answering host denial", result)?,
        Err(_) => return Err(SupervisorError::DenialTimeout),
    }

    Err(SupervisorError::HostDenied(host_deny))
}

/// Denies the host when a frame is malformed or its body is over the cap,
/// echoing the in-flight request's id.
async fn deny_body_cap(
    host: &mut (impl AsyncWrite + Unpin),
    frame: &[u8],
    id: &Value,
) -> Result<()> {
    match body_within_cap(frame) {
        BodyCap::Fits => Ok(()),
        BodyCap::BadFrame => refuse(host, id, HostDeny::BadFrame, DENY_WRITE_TIMEOUT).await,
        BodyCap::TooLarge => refuse(host, id, HostDeny::OversizeBody, DENY_WRITE_TIMEOUT).await,
    }
}

/// Forwards one host frame to the stall and copies its reply back, with every
/// forwarding step bounded by `limit` and every denial by `DENY_WRITE_TIMEOUT`.
async fn broker_host_frame(
    host: &mut BufReader<UnixStream>,
    stall: &mut StallWriter,
    rx: &mut StallReceiver,
    limit: Duration,
) -> Result<()> {
    let read = timeout(limit, read_line_bounded(host)).await;
    let frame = match read {
        Ok(Ok(frame)) => frame,
        Ok(Err(host_deny)) => {
            return refuse(host, &Value::Null, host_deny, DENY_WRITE_TIMEOUT).await;
        }
        Err(_) => {
            return refuse(
                host,
                &Value::Null,
                HostDeny::StalledReply,
                DENY_WRITE_TIMEOUT,
            )
            .await;
        }
    };
    let id = request_id(&frame);
    deny_body_cap(host, &frame, &id).await?;

    // A partial write leaves a half frame on the stall stream. That
    // connection is dead from here: no further frame can be brokerable on it,
    // so barn denies this host instead of trying to resynchronize.
    let sent = timeout(limit, stall.write_all(&frame)).await;

    if !matches!(sent, Ok(Ok(()))) {
        return refuse(host, &id, HostDeny::PeerLoss, DENY_WRITE_TIMEOUT).await;
    }

    trace_stall_event(|| format!("host reply wait start: id={}", id));
    let reply = timeout(limit, rx.recv()).await;
    match &reply {
        Ok(Some(StallEvent::Line(_))) => {
            trace_stall_event(|| format!("host reply wait done: id={} outcome=Line", id));
        }
        Ok(Some(event)) => trace_stall_event(|| {
            format!(
                "host reply wait done: id={} outcome={}",
                id,
                stall_event_kind(event)
            )
        }),
        Ok(None) => {
            trace_stall_event(|| format!("host reply wait done: id={} outcome=None", id));
        }
        Err(_) => {
            trace_stall_event(|| format!("host reply wait done: id={} outcome=timeout", id));
        }
    }
    let line = match reply {
        Ok(Some(StallEvent::Line(line))) => line,
        // An over-cap reply keeps its named reason; the other terminal
        // events, and a vanished reader, are a lost peer.
        Ok(Some(StallEvent::Oversize)) => {
            return refuse(host, &id, HostDeny::OversizeFrame, DENY_WRITE_TIMEOUT).await;
        }
        Ok(_) => {
            return refuse(host, &id, HostDeny::PeerLoss, DENY_WRITE_TIMEOUT).await;
        }
        Err(_) => {
            return refuse(host, &id, HostDeny::StalledReply, DENY_WRITE_TIMEOUT).await;
        }
    };
    if !answers_request(&line, &id) {
        return refuse(host, &id, HostDeny::BadFrame, DENY_WRITE_TIMEOUT).await;
    }
    deny_body_cap(host, &line, &id).await?;

    answer_host_with_id(host, &line, limit, &id).await
}

/// An `AsyncWrite` that counts bytes accepted, so a timed-out reply can
/// tell whether any of it reached the wire.
struct CountingWriter<'a, W> {
    inner: &'a mut W,
    written: usize,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CountingWriter<'_, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut *this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(count)) => {
                this.written = this.written.saturating_add(count);

                Poll::Ready(Ok(count))
            }
            pending => pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

/// Copies one reply line to the host with a null denial id, for callers with
/// no request in flight.
#[cfg(test)]
async fn answer_host(
    host: &mut (impl AsyncWrite + Unpin),
    line: &[u8],
    limit: Duration,
) -> Result<()> {
    answer_host_with_id(host, line, limit, &Value::Null).await
}

/// Copies one reply line to the host under a deadline, then reports the
/// outcome. A write that emitted any bytes closes without a deny: appending a
/// denial to a half frame would corrupt it. Only a write that emitted nothing
/// gets the named `StalledReply` deny, echoing the request's id.
async fn answer_host_with_id(
    host: &mut (impl AsyncWrite + Unpin),
    line: &[u8],
    limit: Duration,
    id: &Value,
) -> Result<()> {
    let (sent, written) = {
        let mut counting = CountingWriter {
            inner: host,
            written: 0,
        };
        let sent = timeout(limit, counting.write_all(line)).await;

        (sent, counting.written)
    };

    match sent {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(SupervisorError::Io {
            operation: "answering host frame",
            source: error,
        }),
        Err(_) if written == 0 => {
            refuse(host, id, HostDeny::StalledReply, DENY_WRITE_TIMEOUT).await
        }
        Err(_) => Err(SupervisorError::HostDenied(HostDeny::StalledReply)),
    }
}

/// Why a frame's inline body does not fit, when it does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyCap {
    /// The decoded body fits the cap.
    Fits,
    /// The line is malformed, or its body cannot be measured.
    BadFrame,
    /// The decoded body is longer than the cap.
    TooLarge,
}

/// Whether a frame's inline body fits the cap after decoding.
///
/// A body barn cannot measure is denied: malformed JSON, a present `body` that
/// is not a string, and malformed base64 fail closed, while a frame with no
/// body stays valid.
fn body_within_cap(frame: &[u8]) -> BodyCap {
    let Ok(value) = serde_json::from_slice::<Value>(frame) else {
        return BodyCap::BadFrame;
    };
    let Some(body) = value.get("body") else {
        return BodyCap::Fits;
    };
    let Some(body) = body.as_str() else {
        return BodyCap::BadFrame;
    };
    let decoded = if value.get("body_encoding").and_then(Value::as_str) == Some("base64") {
        if !valid_base64(body) {
            return BodyCap::BadFrame;
        }

        base64_decoded_len(body)
    } else {
        body.len()
    };

    if decoded > DEFAULT_MAX_BODY_BYTES {
        BodyCap::TooLarge
    } else {
        BodyCap::Fits
    }
}

/// The decoded length of a base64 string, counted without a decoder.
fn base64_decoded_len(body: &str) -> usize {
    let padding = body.bytes().rev().take_while(|byte| *byte == b'=').count();
    let symbols = body.len().saturating_sub(padding);
    let tail = match symbols % 4 {
        2 => 1,
        3 => 2,
        _ => 0,
    };

    symbols
        .saturating_div(4)
        .saturating_mul(3)
        .saturating_add(tail)
}

/// Whether `body` is base64 that `base64_decoded_len` can measure.
///
/// Only the standard alphabet appears and `=` pads at most two places at the
/// end, completing a group of four. Unpadded input stays valid.
fn valid_base64(body: &str) -> bool {
    let bytes = body.as_bytes();
    let symbols = bytes
        .iter()
        .take_while(|byte| is_base64_symbol(**byte))
        .count();
    let padding = bytes.len().saturating_sub(symbols);

    if padding > 2
        || !bytes
            .get(symbols..)
            .is_none_or(|tail| tail.iter().all(|byte| *byte == b'='))
    {
        return false;
    }

    let remainder = symbols % 4;

    remainder != 1 && (padding == 0 || remainder.checked_add(padding) == Some(4))
}

/// Whether a byte is a standard base64 symbol, excluding `=` padding.
const fn is_base64_symbol(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'
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

    let host_socket = run_dir.path(HOST_SOCKET_FILE);
    let host_listener = io_at("binding host socket", StdUnixListener::bind(&host_socket))?;
    io_at(
        "setting host socket nonblocking",
        host_listener.set_nonblocking(true),
    )?;
    chmod(&host_socket, PRIVATE_FILE)?;

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
    let host_token = new_token()?;

    Ok(Prepared {
        listener,
        host_listener,
        run_dir,
        state,
        uid,
        token,
        host_token,
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
    for name in [SOCKET_FILE, POLICY_FILE, HOST_SOCKET_FILE] {
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
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;

    const START: &[u8] = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"stall.start","#,
        r#""params":{"protocol":1,"token":"secret"}}"#,
    )
    .as_bytes();

    #[test]
    fn stall_event_kind_names_every_variant_without_its_payload() {
        assert_eq!(
            stall_event_kind(&StallEvent::Line(vec![b'x', b'\n'])),
            "Line"
        );
        assert_eq!(stall_event_kind(&StallEvent::Exited), "Exited");
        assert_eq!(stall_event_kind(&StallEvent::Loss), "Loss");
        assert_eq!(stall_event_kind(&StallEvent::Oversize), "Oversize");
    }

    #[test]
    fn host_endpoint_debug_names_the_struct_and_redacts_the_token() {
        let endpoint = HostEndpoint {
            socket_path: PathBuf::from("/tmp/barn-host.sock"),
            token: "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_owned(),
        };

        assert_eq!(
            format!("{endpoint:?}"),
            r#"HostEndpoint { socket_path: "/tmp/barn-host.sock", token: "[redacted]" }"#
        );
    }

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

    #[test]
    fn every_host_deny_carries_the_reason_fixed_on_the_protocol_page() {
        for (host_deny, reason) in [
            (HostDeny::WrongPeerPid, deny::WRONG_PEER_PID),
            (HostDeny::WrongPeerUid, deny::WRONG_PEER_UID),
            (HostDeny::TokenExpiredOrSpent, deny::TOKEN_EXPIRED_OR_SPENT),
            (HostDeny::TokenMismatch, deny::TOKEN_MISMATCH),
            (HostDeny::ProtocolMismatch, deny::PROTOCOL_MISMATCH),
            (HostDeny::OversizeFrame, deny::OVERSIZE_FRAME),
            (HostDeny::BadFrame, deny::BAD_FRAME),
            (HostDeny::OversizeBody, deny::OVERSIZE_FRAME),
            (HostDeny::ClosedSocket, deny::CLOSED_SOCKET),
            (HostDeny::StalledReply, deny::CLOSED_SOCKET),
            (HostDeny::PeerLoss, deny::CLOSED_SOCKET),
        ] {
            assert_eq!(host_deny.reason(), reason);
        }
    }

    // WrongPeerUid cannot be produced from an end-to-end test: peer
    // credentials come from the OS socket, and every host the harness can
    // connect from this process has the socket owner's uid. Only a peer
    // running as a different uid (a second user or a setuid helper) would
    // differ, which the CI fixture cannot create. This unit test drives
    // `host_peer` with an explicit uid and covers the reason and its reply.
    #[test]
    fn a_host_peer_needs_both_the_expected_pid_and_owner_uid() {
        assert_eq!(host_peer(Some(17), 42, 17, 42), Ok(()));
        assert_eq!(host_peer(None, 42, 17, 42), Err(HostDeny::WrongPeerPid));
        assert_eq!(host_peer(Some(18), 42, 17, 42), Err(HostDeny::WrongPeerPid));
        assert_eq!(host_peer(Some(17), 43, 17, 42), Err(HostDeny::WrongPeerUid));
    }

    #[test]
    fn a_host_token_expires_at_its_deadline_and_spends_only_once() {
        let start = Instant::now();
        let deadline = start.checked_add(TOKEN_TTL).expect("deadline fits");
        let before = deadline
            .checked_sub(Duration::from_nanos(1))
            .expect("instant before fits");
        let after = deadline
            .checked_add(Duration::from_secs(1))
            .expect("instant after fits");
        let issued = || Token {
            secret: "secret".to_owned(),
            expires_at: deadline,
        };

        let mut saved = Some(issued());
        assert_eq!(host_token_deny(&mut saved, "secret", before), None);
        assert_eq!(
            host_token_deny(&mut saved, "secret", before),
            Some(HostDeny::TokenExpiredOrSpent)
        );

        for now in [deadline, after] {
            let mut saved = Some(issued());

            assert_eq!(
                host_token_deny(&mut saved, "secret", now),
                Some(HostDeny::TokenExpiredOrSpent)
            );
            assert!(saved.is_none());
        }

        let mut saved = Some(issued());
        assert_eq!(
            host_token_deny(&mut saved, "other", before),
            Some(HostDeny::TokenMismatch)
        );
        assert!(saved.is_some());
    }

    #[test]
    fn a_host_body_fits_the_cap_only_after_decoding() {
        const PREFIX: &[u8] = br#"{"body":""#;
        let mut exact = PREFIX.to_vec();
        exact.resize(PREFIX.len() + DEFAULT_MAX_BODY_BYTES, b'x');
        exact.extend_from_slice(b"\"}");
        assert_eq!(body_within_cap(&exact), BodyCap::Fits);

        exact.truncate(PREFIX.len() + DEFAULT_MAX_BODY_BYTES);
        exact.push(b'x');
        exact.extend_from_slice(b"\"}");
        assert_eq!(body_within_cap(&exact), BodyCap::TooLarge);
        assert_eq!(body_within_cap(b"not JSON"), BodyCap::BadFrame);
        assert_eq!(base64_decoded_len("aGk="), 2);
        assert_eq!(base64_decoded_len("aGk"), 2);
    }

    #[test]
    fn a_base64_tail_of_two_symbols_decodes_to_one_byte() {
        assert_eq!(base64_decoded_len("ag=="), 1);
    }

    #[test]
    fn a_base64_tail_of_three_symbols_decodes_to_two_bytes() {
        assert_eq!(base64_decoded_len("aGk="), 2);
    }

    #[test]
    fn a_standard_base64_symbol_is_plus_slash_or_alphanumeric() {
        assert!(is_base64_symbol(b'+'));
        assert!(is_base64_symbol(b'/'));
        assert!(is_base64_symbol(b'a'));
        assert!(is_base64_symbol(b'7'));
        assert!(!is_base64_symbol(b'='));
        assert!(!is_base64_symbol(b'*'));
    }

    #[test]
    fn a_base64_body_with_a_bad_alphabet_character_is_a_bad_frame() {
        for body in ["aGk*", "a$Gk", "a-Gk"] {
            let frame = format!(r#"{{"body_encoding":"base64","body":"{body}"}}"#);

            assert_eq!(
                body_within_cap(frame.as_bytes()),
                BodyCap::BadFrame,
                "{body}"
            );
        }
    }

    #[test]
    fn a_base64_body_with_bad_padding_is_a_bad_frame() {
        for body in ["a=", "ab=", "aG===", "aGk==", "abcd==", "a=Gk"] {
            let frame = format!(r#"{{"body_encoding":"base64","body":"{body}"}}"#);

            assert_eq!(
                body_within_cap(frame.as_bytes()),
                BodyCap::BadFrame,
                "{body}"
            );
        }
    }

    #[test]
    fn a_valid_base64_body_is_measured_at_the_cap_and_one_byte_over() {
        let encode = |decoded_len: usize| {
            let groups = decoded_len / 3;
            let tail = match decoded_len % 3 {
                1 => "QQ==",
                2 => "QUI=",
                _ => "",
            };
            format!("{}{}", "QUJD".repeat(groups), tail)
        };
        let frame = |body: &str| format!(r#"{{"body_encoding":"base64","body":"{body}"}}"#);

        let exact = encode(DEFAULT_MAX_BODY_BYTES);
        let over = encode(DEFAULT_MAX_BODY_BYTES + 1);
        assert_eq!(base64_decoded_len(&exact), DEFAULT_MAX_BODY_BYTES);
        assert_eq!(base64_decoded_len(&over), DEFAULT_MAX_BODY_BYTES + 1);
        assert_eq!(body_within_cap(frame(&exact).as_bytes()), BodyCap::Fits);
        assert_eq!(body_within_cap(frame(&over).as_bytes()), BodyCap::TooLarge);
    }

    #[test]
    fn a_body_that_cannot_be_measured_is_denied() {
        for frame in [
            br#"{"body":42}"#.as_slice(),
            br#"{"body":null}"#.as_slice(),
            br#"{"body":{}}"#.as_slice(),
            br#"{"body":["x"]}"#.as_slice(),
            b"not JSON".as_slice(),
        ] {
            assert_eq!(body_within_cap(frame), BodyCap::BadFrame);
        }
        assert_eq!(
            body_within_cap(br#"{"jsonrpc":"2.0","method":"x"}"#),
            BodyCap::Fits
        );
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

    /// A complete `host.start` line for the daemon barn expects.
    fn host_start(pid: u32, token: &str) -> Vec<u8> {
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": method::HOST_START,
            "params": {"protocol": PROTOCOL_VERSION, "token": token, "pid": pid}
        }))
        .expect("host.start JSON");
        line.push(b'\n');

        line
    }

    /// A `host.start` line with the given id and fixed, valid parameters.
    fn host_start_with_id(id: &Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method::HOST_START,
            "params": {"protocol": PROTOCOL_VERSION, "token": "secret", "pid": 21}
        })
        .to_string()
    }

    #[test]
    fn request_id_keeps_string_i64_and_u64_ids_and_nulls_a_bool() {
        for (id, expected) in [
            (json!("host-7"), json!("host-7")),
            (json!(-42_i64), json!(-42)),
            (json!(u64::MAX), json!(u64::MAX)),
            (json!(true), Value::Null),
        ] {
            let frame = host_start_with_id(&id);

            assert_eq!(request_id(frame.as_bytes()), expected, "{frame}");
        }
    }

    #[test]
    fn a_reply_needs_the_request_id_version_2_and_exactly_one_payload() {
        let id = json!(1);

        for (line, expected) in [
            (r#"{"jsonrpc":"2.0","id":1,"method":"stall.x"}"#, false),
            (r#"{"jsonrpc":"2.0","id":1,"result":null}"#, true),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"x"}}"#,
                true,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":1,"message":"x"}}"#,
                false,
            ),
            (r#"{"jsonrpc":"1.0","id":1,"result":null}"#, false),
            ("not JSON", false),
        ] {
            assert_eq!(answers_request(line.as_bytes(), &id), expected, "{line}");
        }
    }

    #[test]
    fn a_framed_reply_needs_the_request_id_a_type_and_exactly_one_payload() {
        let id = json!(1);

        for (line, expected) in [
            (r#"{"type":"hook","id":1,"outcome":{"allow":{}}}"#, true),
            (
                r#"{"type":"tool","id":1,"content":[{"kind":"text","text":"ok"}]}"#,
                true,
            ),
            (
                r#"{"type":"error","id":1,"error":"boom","message":"x"}"#,
                true,
            ),
            (
                r#"{"type":"hook","id":1,"version":1,"hook":"PreToolUse","call":{"tool":"bash","name":"run","input":{}}}"#,
                false,
            ),
            (
                r#"{"type":"hook","version":1,"hook":"PreToolUse","call":{"tool":"bash","name":"run","input":{}}}"#,
                false,
            ),
            (r#"{"type":"hook","id":2,"outcome":{"allow":{}}}"#, false),
        ] {
            assert_eq!(answers_request(line.as_bytes(), &id), expected, "{line}");
        }
    }

    #[test]
    fn host_start_denies_a_wrong_version_or_method_and_accepts_both_right() {
        for (version, named_method, denied) in [
            (json!("1.0"), method::HOST_START.to_owned(), true),
            (json!("2.0"), "other.method".to_owned(), true),
            (json!("2.0"), method::HOST_START.to_owned(), false),
        ] {
            let frame = json!({
                "jsonrpc": version,
                "id": 21,
                "method": named_method,
                "params": {"protocol": PROTOCOL_VERSION, "token": "secret", "pid": 21}
            })
            .to_string();
            let decoded = decode_host_start(frame.as_bytes());

            if denied {
                assert_eq!(decoded.err(), Some(HostDeny::ProtocolMismatch), "{frame}");
            } else {
                let (id, params) = decoded.expect("a valid host.start");
                assert_eq!(id, json!(21), "{frame}");
                assert_eq!(
                    params,
                    HostStartParams {
                        protocol: PROTOCOL_VERSION,
                        token: "secret".to_owned(),
                        pid: 21,
                    },
                    "{frame}"
                );
            }
        }
    }

    #[test]
    fn host_start_decodes_every_id_kind_and_refuses_a_bool_id() {
        for (id, expected) in [
            (json!("host-7"), Some(json!("host-7"))),
            (json!(-42_i64), Some(json!(-42))),
            (json!(u64::MAX), Some(json!(u64::MAX))),
            (json!(true), None),
        ] {
            let frame = host_start_with_id(&id);
            let decoded = decode_host_start(frame.as_bytes());

            match expected {
                Some(wanted) => {
                    let (got, params) = decoded.expect("a decodable host.start");
                    assert_eq!(got, wanted, "{frame}");
                    assert_eq!(
                        params,
                        HostStartParams {
                            protocol: PROTOCOL_VERSION,
                            token: "secret".to_owned(),
                            pid: 21,
                        },
                        "{frame}"
                    );
                }
                None => assert_eq!(decoded.err(), Some(HostDeny::BadFrame), "{frame}"),
            }
        }

        for (frame, deny) in [
            (
                json!({
                    "jsonrpc": "1.0",
                    "id": 21,
                    "method": method::HOST_START,
                    "params": {"protocol": PROTOCOL_VERSION, "token": "secret", "pid": 21}
                })
                .to_string(),
                HostDeny::ProtocolMismatch,
            ),
            (
                json!({
                    "jsonrpc": "2.0",
                    "id": 21,
                    "method": "other.method",
                    "params": {"protocol": PROTOCOL_VERSION, "token": "secret", "pid": 21}
                })
                .to_string(),
                HostDeny::ProtocolMismatch,
            ),
            (
                json!({
                    "jsonrpc": "2.0",
                    "id": 21,
                    "method": method::HOST_START,
                    "params": {"protocol": "1", "token": "secret", "pid": 21}
                })
                .to_string(),
                HostDeny::BadFrame,
            ),
            ("not JSON".to_owned(), HostDeny::BadFrame),
        ] {
            assert_eq!(
                decode_host_start(frame.as_bytes()).err(),
                Some(deny),
                "{frame}"
            );
        }
    }

    #[test]
    fn a_malformed_host_start_line_is_denied_as_a_bad_frame() {
        assert_eq!(
            decode_host_start(b"not JSON\n").err(),
            Some(HostDeny::BadFrame)
        );
    }

    #[test]
    fn a_host_start_without_params_is_denied_as_a_bad_frame() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": method::HOST_START,
        })
        .to_string();

        assert_eq!(
            decode_host_start(frame.as_bytes()).err(),
            Some(HostDeny::BadFrame),
            "{frame}"
        );
    }

    #[test]
    fn a_host_start_with_malformed_params_is_denied_as_a_bad_frame() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": method::HOST_START,
            "params": {"protocol": "1", "token": "secret", "pid": 21},
        })
        .to_string();

        assert_eq!(
            decode_host_start(frame.as_bytes()).err(),
            Some(HostDeny::BadFrame),
            "{frame}"
        );
    }

    #[test]
    fn a_non_object_host_start_envelope_is_denied_as_a_bad_frame() {
        let frame = json!([
            "2.0",
            21,
            method::HOST_START,
            {"protocol": PROTOCOL_VERSION, "token": "secret", "pid": 21}
        ])
        .to_string();

        assert_eq!(
            decode_host_start(frame.as_bytes()).err(),
            Some(HostDeny::BadFrame),
            "{frame}"
        );
    }

    #[test]
    fn a_host_start_without_an_id_is_denied_as_a_bad_frame() {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method::HOST_START,
            "params": {"protocol": PROTOCOL_VERSION, "token": "secret", "pid": 21},
        })
        .to_string();

        assert_eq!(
            decode_host_start(frame.as_bytes()).err(),
            Some(HostDeny::BadFrame),
            "{frame}"
        );
    }

    /// A host transport bound to `root/host.sock`, expecting this process.
    fn test_host_transport(root: &Path, token: Option<Token>) -> HostTransport {
        let socket = root.join(HOST_SOCKET_FILE);
        let std_listener = StdUnixListener::bind(&socket).expect("host socket");
        std_listener
            .set_nonblocking(true)
            .expect("nonblocking host socket");
        let owner = fs::metadata(&socket).expect("socket owner").uid();
        let listener = UnixListener::from_std(std_listener).expect("host listener");

        HostTransport {
            listener: Some(listener),
            token,
            daemon_pid: Some(std::process::id()),
            uid: owner,
        }
    }

    /// The exact `denied` reply, with the id the request carried.
    fn denied(id: &Value, reason: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": DENIED_CODE,
                "message": "denied",
                "data": {"reason": reason}
            }
        })
    }

    /// The one deny line a host reads before barn closes its connection.
    async fn read_deny(peer: &mut UnixStream) -> Value {
        let mut reply = Vec::new();
        timeout(Duration::from_millis(500), peer.read_to_end(&mut reply))
            .await
            .expect("deny arrives")
            .expect("read deny");
        assert_eq!(reply.last(), Some(&b'\n'));
        let line = reply.strip_suffix(b"\n").expect("newline");
        assert!(!line.contains(&b'\n'), "one line only");

        serde_json::from_slice(line).expect("deny JSON")
    }

    #[tokio::test]
    async fn a_spent_host_token_still_writes_the_named_deny_on_the_wire() {
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the past deadline fits");

        for token in [Some(issued(past)), None] {
            let fixture = PrepareFixture::new();
            let socket = fixture.0.join(HOST_SOCKET_FILE);
            let mut transport = test_host_transport(&fixture.0, token);
            let daemon = std::process::id();
            let mut peer = UnixStream::connect(&socket).await.expect("host connects");
            peer.write_all(&host_start(daemon, "secret"))
                .await
                .expect("host.start");

            let refused = transport.accept_with_deadline(daemon, "fixture").await;

            assert!(matches!(
                refused,
                Err(SupervisorError::HostDenied(HostDeny::TokenExpiredOrSpent))
            ));
            assert!(transport.token.is_none());
            assert_eq!(
                read_deny(&mut peer).await,
                denied(&json!(21), deny::TOKEN_EXPIRED_OR_SPENT)
            );
        }
    }

    /// Drives the real host socket through `host.start` and one broker step
    /// against a stall peer that stays quiet, returning the broker outcome and
    /// the one deny line the host reads after the accepted reply.
    async fn host_session(frame: &[u8]) -> (Result<()>, Value) {
        let fixture = PrepareFixture::new();
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        let daemon = std::process::id();
        let mut peer = BufReader::new(UnixStream::connect(&socket).await.expect("host connects"));
        peer.write_all(&host_start(daemon, "secret"))
            .await
            .expect("host.start");

        let mut host = transport
            .accept_with_deadline(daemon, "fixture")
            .await
            .expect("host.start accepted");
        let mut accepted = Vec::new();
        timeout(
            Duration::from_millis(500),
            peer.read_until(b'\n', &mut accepted),
        )
        .await
        .expect("host.start reply")
        .expect("read reply");
        assert_eq!(
            serde_json::from_slice::<Value>(&accepted).expect("reply JSON")["result"],
            json!({"stall": "fixture"})
        );

        let (stall_barn, _stall_peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, mut stall) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let writing = async { peer.write_all(frame).await.expect("host frame") };
        let (result, ()) = tokio::join!(
            broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_secs(2)),
            writing
        );
        drop(host);

        (result, read_deny(peer.get_mut()).await)
    }

    #[tokio::test]
    async fn an_oversize_frame_is_denied_end_to_end_on_the_host_endpoint() {
        let frame = vec![b'x'; MAX_LINE_BYTES];

        let (result, deny) = host_session(&frame).await;

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::OversizeFrame))
        ));
        assert_eq!(deny, denied(&Value::Null, deny::OVERSIZE_FRAME));
    }

    #[tokio::test]
    async fn an_oversize_body_is_denied_end_to_end_on_the_host_endpoint() {
        let prefix = br#"{"body":""#;
        let mut frame = prefix.to_vec();
        frame.resize(prefix.len() + DEFAULT_MAX_BODY_BYTES + 1, b'x');
        frame.extend_from_slice(b"\"}\n");

        let (result, deny) = host_session(&frame).await;

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::OversizeBody))
        ));
        assert_eq!(deny, denied(&Value::Null, deny::OVERSIZE_FRAME));
    }

    #[tokio::test]
    async fn a_slow_host_is_denied_on_the_wire_when_the_exchange_deadline_passes() {
        let fixture = PrepareFixture::new();
        let soon = Instant::now()
            .checked_add(Duration::from_millis(200))
            .expect("the near deadline fits");
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let mut transport = test_host_transport(&fixture.0, Some(issued(soon)));
        let daemon = std::process::id();
        let mut peer = UnixStream::connect(&socket).await.expect("host connects");

        // The peer connects but sends no `host.start`; the deadline ends the
        // exchange and the named deny still reaches the wire.
        let accepted = timeout(
            Duration::from_secs(1),
            transport.accept_with_deadline(daemon, "fixture"),
        )
        .await;

        assert!(matches!(
            accepted,
            Ok(Err(SupervisorError::HostDenied(HostDeny::StalledReply)))
        ));
        assert!(transport.token.is_none());
        assert_eq!(
            read_deny(&mut peer).await,
            denied(&Value::Null, deny::CLOSED_SOCKET)
        );
    }

    /// A peer that connects and sends nothing must be denied by the handshake
    /// window, not the token's full lifetime. The outer deadline sits between
    /// the two, so a wait for the TTL fails this test instead of passing late.
    #[tokio::test]
    async fn a_silent_host_is_denied_within_the_handshake_window_and_spends_the_token() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        let daemon = std::process::id();
        let mut peer = UnixStream::connect(&socket).await.expect("host connects");

        // The peer sends no line; a wait for the TTL would exceed this bound.
        let outside = HOST_HANDSHAKE_TIMEOUT
            .checked_add(Duration::from_secs(1))
            .expect("the window plus slack fits");
        let accepted = timeout(outside, transport.accept_with_deadline(daemon, "fixture")).await;

        assert!(matches!(
            accepted,
            Ok(Err(SupervisorError::HostDenied(HostDeny::StalledReply)))
        ));
        assert!(transport.token.is_none());
        assert_eq!(
            read_deny(&mut peer).await,
            denied(&Value::Null, deny::CLOSED_SOCKET)
        );
    }

    #[tokio::test]
    async fn a_spent_host_that_sends_no_line_is_denied_without_hanging() {
        let fixture = PrepareFixture::new();
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let mut transport = test_host_transport(&fixture.0, None);
        let daemon = std::process::id();
        let mut peer = UnixStream::connect(&socket).await.expect("host connects");

        let accepted = timeout(
            Duration::from_secs(5),
            transport.accept_with_deadline(daemon, "fixture"),
        )
        .await;

        assert!(matches!(
            accepted,
            Ok(Err(SupervisorError::HostDenied(
                HostDeny::TokenExpiredOrSpent
            )))
        ));
        assert_eq!(
            read_deny(&mut peer).await,
            denied(&Value::Null, deny::TOKEN_EXPIRED_OR_SPENT)
        );
    }

    /// A silent peer from the wrong PID is refused at the credential check,
    /// before any line is read. The named denial leaves the token live, so the
    /// real daemon can still complete `host.start` on the same transport.
    #[tokio::test]
    async fn a_silent_wrong_pid_peer_is_refused_without_spending_the_token() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        let daemon = std::process::id();
        let mut wrong_peer = UnixStream::connect(&socket).await.expect("peer connects");

        // The peer never sends a line. Without the check before the read, the
        // handshake window would deny it and spend the token meant for barn.
        let wrong_daemon = daemon.wrapping_add(1);
        let refused = transport
            .accept_with_deadline(wrong_daemon, "fixture")
            .await;

        assert!(matches!(
            refused,
            Err(SupervisorError::HostDenied(HostDeny::WrongPeerPid))
        ));
        assert_eq!(
            transport.token.as_ref().map(|token| token.secret.as_str()),
            Some("secret")
        );
        assert_eq!(
            read_deny(&mut wrong_peer).await,
            denied(&Value::Null, deny::WRONG_PEER_PID)
        );

        // The real daemon gets the live token.
        let mut peer = BufReader::new(UnixStream::connect(&socket).await.expect("host connects"));
        peer.write_all(&host_start(daemon, "secret"))
            .await
            .expect("host.start");

        let accepted = transport.accept_with_deadline(daemon, "fixture").await;

        assert!(accepted.is_ok());
        assert!(transport.token.is_none());
        let mut reply = Vec::new();
        timeout(
            Duration::from_millis(500),
            peer.read_until(b'\n', &mut reply),
        )
        .await
        .expect("host.start reply")
        .expect("read reply");
        assert_eq!(
            serde_json::from_slice::<Value>(&reply).expect("reply JSON")["result"],
            json!({"stall": "fixture"})
        );
    }

    /// With no peer at all, the accept half of the handshake ends within the
    /// short window, not the token's full TTL, and spends the token.
    #[tokio::test]
    async fn no_peer_is_denied_within_the_handshake_window_and_spends_the_token() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        let daemon = std::process::id();

        // No connection is made: a wait for the TTL would exceed this bound.
        let outside = HOST_HANDSHAKE_TIMEOUT
            .checked_add(Duration::from_secs(1))
            .expect("the window plus slack fits");
        let accepted = timeout(outside, transport.accept_with_deadline(daemon, "fixture")).await;

        assert!(matches!(
            accepted,
            Ok(Err(SupervisorError::HostDenied(
                HostDeny::TokenExpiredOrSpent
            )))
        ));
        assert!(transport.token.is_none());
    }

    #[tokio::test]
    async fn a_closed_host_listener_refuses_a_later_host_without_waiting() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        transport.close();

        let refused = timeout(
            Duration::from_millis(100),
            transport.accept_with_deadline(std::process::id(), "fixture"),
        )
        .await;

        assert!(matches!(
            refused,
            Ok(Err(SupervisorError::HostDenied(HostDeny::ClosedSocket)))
        ));
        assert!(transport.token.is_none());
    }

    #[tokio::test]
    async fn closing_a_running_stall_stops_later_host_accepts() {
        // This spawns an idle child, so it runs in CI only.
        if std::env::var_os("CI").is_none() {
            return;
        }

        let mut stall = idle_stall_for_brokering();
        stall.close_host_listener();

        let refused = timeout(Duration::from_millis(100), stall.accept_host()).await;

        assert!(matches!(
            refused,
            Ok(Err(SupervisorError::HostDenied(HostDeny::ClosedSocket)))
        ));
    }

    #[tokio::test]
    async fn a_stall_exit_wins_over_an_idle_host() {
        // This spawns a child that exits at once, so it runs in CI only.
        if std::env::var_os("CI").is_none() {
            return;
        }

        let mut stall = idle_stall_for_brokering();

        let outcome = timeout(Duration::from_secs(1), stall.accept_host_or_stall_exit()).await;

        assert!(matches!(outcome, Ok(Ok(None))));
    }

    /// A protocol error is not a peer-identity denial: the named deny reaches
    /// the wire and the token is spent.
    #[tokio::test]
    async fn a_protocol_error_is_denied_and_spends_the_token() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        let daemon = std::process::id();
        let mut peer = UnixStream::connect(&socket).await.expect("host connects");

        let mut frame = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": method::HOST_START,
            "params": {
                "protocol": PROTOCOL_VERSION.wrapping_add(1),
                "token": "secret",
                "pid": daemon,
            }
        }))
        .expect("host.start JSON");
        frame.push(b'\n');
        peer.write_all(&frame).await.expect("host.start");

        let refused = transport.accept_with_deadline(daemon, "fixture").await;

        assert!(matches!(
            refused,
            Err(SupervisorError::HostDenied(HostDeny::ProtocolMismatch))
        ));
        assert!(transport.token.is_none());
        assert_eq!(
            read_deny(&mut peer).await,
            denied(&json!(21), deny::PROTOCOL_MISMATCH)
        );
    }

    /// A peer that trickles one byte every 300 ms never completes its line:
    /// the handshake window ends it with a `StalledReply` deny and a spent
    /// token, well before the token's full lifetime.
    #[tokio::test]
    async fn a_dripped_line_is_denied_within_the_handshake_window() {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let socket = fixture.0.join(HOST_SOCKET_FILE);
        let mut transport = test_host_transport(&fixture.0, Some(issued(deadline)));
        let daemon = std::process::id();
        let mut peer = UnixStream::connect(&socket).await.expect("host connects");

        let drip = async {
            for _ in 0..6 {
                sleep(Duration::from_millis(300)).await;
                if peer.write_all(b"x").await.is_err() {
                    break;
                }
            }
        };

        // The outer bound is the window plus slack; a wait for the TTL fails.
        let outside = HOST_HANDSHAKE_TIMEOUT
            .checked_add(Duration::from_secs(1))
            .expect("the window plus slack fits");
        let (accepted, ()) = tokio::join!(
            timeout(outside, transport.accept_with_deadline(daemon, "fixture")),
            drip,
        );

        assert!(matches!(
            accepted,
            Ok(Err(SupervisorError::HostDenied(HostDeny::StalledReply)))
        ));
        assert!(transport.token.is_none());
        assert_eq!(
            read_deny(&mut peer).await,
            denied(&Value::Null, deny::CLOSED_SOCKET)
        );
    }

    /// A sink that stalls the reply frame but accepts the denial frame.
    ///
    /// A timeout re-polls the stalled write when its timer fires, so stalling
    /// only the first poll lets the reply through and hides the denial. The
    /// first frame seen is the one that stalls; a different frame is accepted.
    #[derive(Default)]
    struct StalledWriter {
        written: Vec<u8>,
        stalled: Vec<u8>,
    }

    impl AsyncWrite for StalledWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.stalled.is_empty() {
                self.stalled.extend_from_slice(buf);
            }
            if buf == self.stalled {
                return Poll::Pending;
            }
            self.written.extend_from_slice(buf);

            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A sink that takes one byte of a reply, then stalls forever.
    #[derive(Default)]
    struct PartialWriter {
        written: Vec<u8>,
    }

    impl AsyncWrite for PartialWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.written.is_empty() && !buf.is_empty() {
                let first = buf.get(..1).unwrap_or(buf);
                self.written.extend_from_slice(first);

                return Poll::Ready(Ok(first.len()));
            }
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_partial_reply_is_closed_without_appending_a_deny() {
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        let mut writer = PartialWriter::default();

        let result = answer_host(&mut writer, reply, Duration::from_millis(50)).await;

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::StalledReply))
        ));
        assert_eq!(writer.written, reply.get(..1).expect("one byte"));
    }

    /// A sink that accepts a few bytes per poll, so a large reply needs more
    /// than one write to finish.
    #[derive(Default)]
    struct ChunkedWriter {
        written: Vec<u8>,
    }

    impl AsyncWrite for ChunkedWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let count = buf.len().min(1024);
            self.written.extend_from_slice(&buf[..count]);

            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_large_reply_is_written_in_full_before_the_deadline() {
        let reply = vec![b'x'; 64 * 1024];
        let mut writer = ChunkedWriter::default();

        let answered = answer_host(&mut writer, &reply, Duration::from_secs(2)).await;

        assert!(matches!(answered, Ok(())));
        assert_eq!(writer.written, reply);
    }

    /// A sink that never accepts a byte, so only a deadline can end a write.
    struct NeverWriter;

    impl AsyncWrite for NeverWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_denial_to_a_host_that_never_takes_bytes_fails_closed() {
        let mut writer = NeverWriter;
        let refused: Result<()> = refuse(
            &mut writer,
            &Value::Null,
            HostDeny::PeerLoss,
            Duration::from_millis(50),
        )
        .await;

        assert!(matches!(refused, Err(SupervisorError::DenialTimeout)));
    }

    #[tokio::test]
    async fn a_line_cap_sized_denial_id_is_retried_with_null() {
        let id = json!("x".repeat(MAX_LINE_BYTES));
        let mut writer = ChunkedWriter::default();

        let refused: Result<()> =
            refuse(&mut writer, &id, HostDeny::BadFrame, Duration::from_secs(2)).await;

        assert!(matches!(
            refused,
            Err(SupervisorError::HostDenied(HostDeny::BadFrame))
        ));
        assert!(writer.written.len() <= MAX_LINE_BYTES);
        let line = writer.written.strip_suffix(b"\n").expect("newline");
        assert_eq!(
            serde_json::from_slice::<Value>(line).expect("deny JSON"),
            denied(&Value::Null, deny::BAD_FRAME)
        );
    }

    /// An inner sink that records whether the adapter forwarded flush and
    /// shutdown, and can return a chosen error from either on demand.
    #[derive(Default)]
    struct RecordingWriter {
        flushed: bool,
        shut_down: bool,
        flush_error: Option<io::ErrorKind>,
        shutdown_error: Option<io::ErrorKind>,
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushed = true;

            Poll::Ready(
                self.flush_error
                    .take()
                    .map_or(Ok(()), |kind| Err(io::Error::from(kind))),
            )
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.shut_down = true;

            Poll::Ready(
                self.shutdown_error
                    .take()
                    .map_or(Ok(()), |kind| Err(io::Error::from(kind))),
            )
        }
    }

    #[tokio::test]
    async fn counting_writer_forwards_flush_to_the_inner_writer() {
        let mut inner = RecordingWriter::default();

        let flushed = {
            let mut counting = CountingWriter {
                inner: &mut inner,
                written: 0,
            };

            timeout(Duration::from_secs(1), counting.flush()).await
        };

        assert!(matches!(flushed, Ok(Ok(()))));
        assert!(inner.flushed);
        assert!(!inner.shut_down);
    }

    #[tokio::test]
    async fn counting_writer_forwards_shutdown_to_the_inner_writer() {
        let mut inner = RecordingWriter::default();

        let shut_down = {
            let mut counting = CountingWriter {
                inner: &mut inner,
                written: 0,
            };

            timeout(Duration::from_secs(1), counting.shutdown()).await
        };

        assert!(matches!(shut_down, Ok(Ok(()))));
        assert!(inner.shut_down);
        assert!(!inner.flushed);
    }

    #[tokio::test]
    async fn counting_writer_propagates_an_inner_flush_error() {
        let mut inner = RecordingWriter {
            flush_error: Some(io::ErrorKind::BrokenPipe),
            ..RecordingWriter::default()
        };

        let failed = {
            let mut counting = CountingWriter {
                inner: &mut inner,
                written: 0,
            };

            timeout(Duration::from_secs(1), counting.flush()).await
        };

        assert_eq!(
            failed
                .expect("flush within deadline")
                .expect_err("inner flush error")
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(inner.flushed);
    }

    #[tokio::test]
    async fn counting_writer_propagates_an_inner_shutdown_error() {
        let mut inner = RecordingWriter {
            shutdown_error: Some(io::ErrorKind::ConnectionReset),
            ..RecordingWriter::default()
        };

        let failed = {
            let mut counting = CountingWriter {
                inner: &mut inner,
                written: 0,
            };

            timeout(Duration::from_secs(1), counting.shutdown()).await
        };

        assert_eq!(
            failed
                .expect("shutdown within deadline")
                .expect_err("inner shutdown error")
                .kind(),
            io::ErrorKind::ConnectionReset
        );
        assert!(inner.shut_down);
    }

    #[tokio::test]
    async fn a_host_that_stops_taking_the_reply_gets_the_stalled_reply_deny() {
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        let mut writer = StalledWriter::default();

        let stalled = answer_host(&mut writer, reply, Duration::from_millis(20)).await;

        assert!(matches!(
            stalled,
            Err(SupervisorError::HostDenied(HostDeny::StalledReply))
        ));
        assert_eq!(writer.written.last(), Some(&b'\n'));
        let line = writer.written.strip_suffix(b"\n").expect("one line");
        assert_eq!(
            serde_json::from_slice::<Value>(line).expect("deny JSON"),
            denied(&Value::Null, deny::CLOSED_SOCKET)
        );
    }

    #[tokio::test]
    async fn a_host_that_sends_no_frame_gets_a_stalled_reply_deny() {
        let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
        let (stall_barn, _stall_peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, mut stall) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut host = BufReader::new(host_barn);

        let result = timeout(
            Duration::from_secs(1),
            broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_millis(50)),
        )
        .await;
        drop(host);

        assert!(matches!(
            result,
            Ok(Err(SupervisorError::HostDenied(HostDeny::StalledReply)))
        ));
        assert_eq!(
            read_deny(&mut host_peer).await,
            denied(&Value::Null, deny::CLOSED_SOCKET)
        );
    }

    #[tokio::test]
    async fn a_stall_reply_with_a_wrong_or_missing_id_is_denied_as_bad_frame() {
        let request = concat!(r#"{"jsonrpc":"2.0","id":1,"method":"request"}"#, "\n").as_bytes();
        let replies = [
            concat!(r#"{"jsonrpc":"2.0","id":2,"result":null}"#, "\n").as_bytes(),
            concat!(r#"{"jsonrpc":"2.0","method":"log"}"#, "\n").as_bytes(),
        ];

        for reply in replies {
            let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
            let (stall_barn, mut stall_peer) = UnixStream::pair().expect("stall pair");
            let (stall_read, mut stall) = stall_barn.into_split();
            let mut rx = spawn_stall_reader(stall_read);
            let mut host = BufReader::new(host_barn);
            host_peer.write_all(request).await.expect("host frame");
            stall_peer.write_all(reply).await.expect("stall reply");

            let result =
                broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_millis(100)).await;
            drop(host);

            assert!(matches!(
                result,
                Err(SupervisorError::HostDenied(HostDeny::BadFrame))
            ));
            assert_eq!(
                read_deny(&mut host_peer).await,
                denied(&json!(1), deny::BAD_FRAME)
            );
        }
    }

    #[tokio::test]
    async fn a_host_frame_with_an_unmeasurable_body_is_denied() {
        for frame in [b"{\"body\":42}\n".as_slice(), b"not JSON\n".as_slice()] {
            let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
            let (stall_barn, _stall_peer) = UnixStream::pair().expect("stall pair");
            let (stall_read, mut stall) = stall_barn.into_split();
            let mut rx = spawn_stall_reader(stall_read);
            let mut host = BufReader::new(host_barn);
            host_peer.write_all(frame).await.expect("host frame");

            let result =
                broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_millis(100)).await;
            drop(host);

            assert!(matches!(
                result,
                Err(SupervisorError::HostDenied(HostDeny::BadFrame))
            ));
            assert_eq!(
                read_deny(&mut host_peer).await,
                denied(&Value::Null, deny::BAD_FRAME)
            );
        }
    }

    #[tokio::test]
    async fn a_host_frame_with_an_oversize_body_is_denied() {
        let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
        let (stall_barn, _stall_peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, mut stall) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut host = BufReader::new(host_barn);
        let prefix = br#"{"body":""#;
        let mut frame = prefix.to_vec();
        frame.resize(prefix.len() + DEFAULT_MAX_BODY_BYTES + 1, b'x');
        frame.extend_from_slice(b"\"}\n");

        let writing = async {
            host_peer
                .write_all(&frame)
                .await
                .expect("oversize host frame");
        };
        let (result, ()) = tokio::join!(
            broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_secs(2)),
            writing
        );
        drop(host);

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::OversizeBody))
        ));
        assert_eq!(
            read_deny(&mut host_peer).await,
            denied(&Value::Null, deny::OVERSIZE_FRAME)
        );
    }

    #[tokio::test]
    async fn a_dead_stall_peer_denies_the_host_with_peer_loss() {
        let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
        let (stall_barn, stall_peer) = UnixStream::pair().expect("stall pair");
        drop(stall_peer);
        let (stall_read, mut stall) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut host = BufReader::new(host_barn);

        host_peer
            .write_all(&host_start(1, "secret"))
            .await
            .expect("host frame");
        let result =
            broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_millis(100)).await;
        drop(host);

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::PeerLoss))
        ));
        assert_eq!(
            read_deny(&mut host_peer).await,
            denied(&json!(21), deny::CLOSED_SOCKET)
        );
    }

    #[tokio::test]
    async fn an_over_cap_stall_reply_is_denied_with_the_oversize_reason() {
        let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
        let (stall_barn, mut stall_peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, mut stall) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut host = BufReader::new(host_barn);

        host_peer
            .write_all(&host_start(1, "secret"))
            .await
            .expect("host frame");
        // One byte over the line cap, so the reader emits Oversize rather
        // than a line barn could copy back.
        let mut reply = vec![b'x'; MAX_LINE_BYTES];
        reply.push(b'\n');
        stall_peer
            .write_all(&reply)
            .await
            .expect("oversize stall reply");

        let result =
            broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_millis(100)).await;
        drop(host);

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::OversizeFrame))
        ));
        assert_eq!(
            read_deny(&mut host_peer).await,
            denied(&json!(21), deny::OVERSIZE_FRAME)
        );
    }

    #[tokio::test]
    async fn a_partial_stall_write_denies_the_host_with_peer_loss() {
        let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
        let (stall_barn, stall_peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, mut stall) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut host = BufReader::new(host_barn);
        // A valid request with a body far larger than a socket buffer: the
        // timed write to the silent stall gets part way in.
        let mut frame = br#"{"jsonrpc":"2.0","id":1,"method":"request","body":""#.to_vec();
        frame.resize(frame.len() + (1 << 20), b'x');
        frame.extend_from_slice(b"\"}\n");
        let writing = async {
            host_peer.write_all(&frame).await.expect("large host frame");
        };

        let (result, ()) = tokio::join!(
            broker_host_frame(&mut host, &mut stall, &mut rx, Duration::from_millis(500)),
            writing
        );
        drop(host);

        assert!(matches!(
            result,
            Err(SupervisorError::HostDenied(HostDeny::PeerLoss))
        ));
        assert_eq!(
            read_deny(&mut host_peer).await,
            denied(&json!(1), deny::CLOSED_SOCKET)
        );
        drop(stall_peer);
    }

    #[tokio::test]
    async fn a_reply_split_across_two_writes_arrives_as_one_line() {
        let (barn_side, mut peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, _stall_write) = barn_side.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n";
        let (head, tail) = reply.split_at(12);

        peer.write_all(head).await.expect("first half");
        sleep(Duration::from_millis(20)).await;
        peer.write_all(tail).await.expect("second half");

        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("one line within the deadline");

        assert_eq!(event, Some(StallEvent::Line(reply.to_vec())));
    }

    #[tokio::test]
    async fn two_frames_in_one_write_arrive_as_two_lines() {
        let (barn_side, mut peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, _stall_write) = barn_side.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let first = b"first line\n";
        let second = b"second line\n";
        let mut both = first.to_vec();
        both.extend_from_slice(second);

        peer.write_all(&both).await.expect("two frames");

        let first_event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first line within the deadline");
        let second_event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("second line within the deadline");

        assert_eq!(first_event, Some(StallEvent::Line(first.to_vec())));
        assert_eq!(second_event, Some(StallEvent::Line(second.to_vec())));
    }

    #[tokio::test]
    async fn an_over_cap_line_produces_oversize() {
        let (barn_side, mut peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, _stall_write) = barn_side.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut line = vec![b'x'; MAX_LINE_BYTES + 1];
        line.push(b'\n');

        peer.write_all(&line).await.expect("over-cap line");

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("oversize within the deadline");

        assert_eq!(event, Some(StallEvent::Oversize));
    }

    #[tokio::test]
    async fn a_line_at_the_byte_cap_is_delivered_whole() {
        let (barn_side, mut peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, _stall_write) = barn_side.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        // The cap counts the newline, so only MAX_LINE_BYTES - 1 bytes are
        // payload.
        let mut line = vec![b'x'; MAX_LINE_BYTES - 1];
        line.push(b'\n');
        assert_eq!(line.len(), MAX_LINE_BYTES);

        peer.write_all(&line).await.expect("cap line");

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("line within the deadline");

        assert_eq!(event, Some(StallEvent::Line(line)));
    }

    #[tokio::test]
    async fn a_line_one_byte_over_the_cap_produces_oversize() {
        let (barn_side, mut peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, _stall_write) = barn_side.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        // One more byte than the cap: MAX_LINE_BYTES payload plus newline.
        let mut line = vec![b'x'; MAX_LINE_BYTES];
        line.push(b'\n');
        assert_eq!(line.len(), MAX_LINE_BYTES + 1);

        peer.write_all(&line).await.expect("over-cap line");

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("oversize within the deadline");

        assert_eq!(event, Some(StallEvent::Oversize));
    }

    #[tokio::test]
    async fn an_eof_produces_exited() {
        let (barn_side, peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, _stall_write) = barn_side.into_split();
        let mut rx = spawn_stall_reader(stall_read);

        drop(peer);

        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("exit within the deadline");

        assert_eq!(event, Some(StallEvent::Exited));
    }

    /// A `RunningStall` whose listeners and child are idle: `serve_host`
    /// borrows `self` without reading it, so only the caller's two streams
    /// take part in the broker step. Spawning the child makes callers a
    /// process test.
    fn idle_stall_for_brokering() -> RunningStall {
        let fixture = PrepareFixture::new();
        let deadline = Instant::now()
            .checked_add(TOKEN_TTL)
            .expect("the token deadline fits");
        let child = Command::new("/usr/bin/true").spawn().expect("idle child");

        RunningStall {
            child,
            pid: std::process::id(),
            transport: test_transport(&fixture.0, Some(deadline)),
            host: test_host_transport(&fixture.0, Some(issued(deadline))),
            supervisor: Supervisor::new(fixture.0.clone(), PathBuf::from("/usr/bin/true")),
            spec: PrepareFixture::spec(),
            started: Instant::now(),
            epoch: Instant::now(),
            restart: RestartPolicy::default(),
            health: HealthTracker::default(),
        }
    }

    #[tokio::test]
    async fn serve_host_forwards_one_framed_request_and_returns_the_exact_reply() {
        // This spawns an idle child, so it runs in CI only.
        if std::env::var_os("CI").is_none() {
            return;
        }

        let stall = idle_stall_for_brokering();
        let (host_barn, mut host_peer) = UnixStream::pair().expect("host pair");
        let (stall_barn, mut stall_peer) = UnixStream::pair().expect("stall pair");
        let (stall_read, mut stall_writer) = stall_barn.into_split();
        let mut rx = spawn_stall_reader(stall_read);
        let mut host = BufReader::new(host_barn);
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"request\"}\n";
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n";
        let mut forwarded = Vec::new();

        let exchange = async {
            host_peer.write_all(request).await.expect("host frame");
            BufReader::new(&mut stall_peer)
                .read_until(b'\n', &mut forwarded)
                .await
                .expect("forwarded frame");
            stall_peer.write_all(reply).await.expect("stall reply");
        };
        let (served, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                stall.serve_host(&mut host, &mut stall_writer, &mut rx),
                exchange
            )
        })
        .await
        .expect("broker and stall finish within the deadline");

        assert!(matches!(served, Ok(())));
        assert_eq!(forwarded, request.to_vec());

        let mut received = Vec::new();
        timeout(
            Duration::from_secs(1),
            BufReader::new(&mut host_peer).read_until(b'\n', &mut received),
        )
        .await
        .expect("host reply within the deadline")
        .expect("read host reply");
        assert_eq!(received, reply.to_vec());
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
        // Peers arrive without a pause, keeping the listener's backlog full.
        let wrong_child = std::process::id().wrapping_add(1);
        let flood = tokio::spawn(async move {
            loop {
                let Ok(mut peer) = UnixStream::connect(&socket).await else {
                    break;
                };
                counted.fetch_add(1, Ordering::Relaxed);
                let _ = peer.write_all(START).await;
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

    #[test]
    fn a_case_variant_in_either_path_alone_is_rejected() {
        let spec = |name: &str| StallSpec {
            name: name.to_owned(),
            ..PrepareFixture::spec()
        };

        // Only the state root holds a case-only variant of "foo".
        let state_only = PrepareFixture::new();
        DirBuilder::new()
            .mode(PRIVATE_DIR)
            .create(state_only.0.join("FOO"))
            .expect("the state-only case variant");
        assert!(matches!(
            prepare(&state_only.0, &spec("foo")),
            Err(SupervisorError::NameCollision)
        ));

        // Only the run root holds a case-only variant of "foo".
        let run_only = PrepareFixture::new();
        let run = run_only.0.join("run");
        DirBuilder::new()
            .mode(PRIVATE_DIR)
            .create(&run)
            .expect("the run root");
        DirBuilder::new()
            .mode(PRIVATE_DIR)
            .create(run.join("FOO"))
            .expect("the run-only case variant");
        assert!(matches!(
            prepare(&run_only.0, &spec("foo")),
            Err(SupervisorError::NameCollision)
        ));
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

    /// The fixture program's ELF machine, read from its header without
    /// running it. The data encoding decides how `e_machine` is read.
    #[cfg(target_os = "linux")]
    fn elf_machine(path: &Path) -> Option<u16> {
        let mut file = File::open(path).ok()?;
        let mut header = [0_u8; 20];
        file.read_exact(&mut header).ok()?;
        // e_ident[EI_DATA]: 1 is little endian, 2 is big endian.
        let little = header[5] == 1;
        let raw = [header[18], header[19]];

        Some(if little {
            u16::from_le_bytes(raw)
        } else {
            u16::from_be_bytes(raw)
        })
    }

    #[cfg(target_os = "linux")]
    fn expected_system_loader_entries(machine: u16) -> Vec<PolicyEntry> {
        let loader = [
            "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            "/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
            "/lib64/ld-linux-x86-64.so.2",
            "/lib/ld-musl-x86_64.so.1",
        ]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists());

        let mut expected = Vec::new();

        if let Some(loader) = loader {
            let loader = loader.canonicalize().expect("system loader");
            let parent = loader.parent().expect("loader parent").to_path_buf();
            expected.push((loader.clone(), loader.clone(), true));

            let mut dirs = [
                (62_u16, "/usr/lib/x86_64-linux-gnu"),
                (62, "/lib/x86_64-linux-gnu"),
                (183, "/usr/lib/aarch64-linux-gnu"),
                (183, "/lib/aarch64-linux-gnu"),
            ]
            .into_iter()
            .filter(|(arch, _)| *arch == machine)
            .map(|(_, dir)| dir)
            .map(Path::new)
            .filter(|dir| dir.is_dir())
            .filter_map(|dir| dir.canonicalize().ok())
            .fold(Vec::new(), |mut dirs, dir| {
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
                dirs
            });

            if !dirs.contains(&parent) {
                let roots: &[&str] =
                    if loader.starts_with("/lib64") || loader.starts_with("/usr/lib64") {
                        &["/lib64", "/usr/lib64"]
                    } else {
                        &["/lib", "/usr/lib"]
                    };

                for &root in roots {
                    let root = Path::new(root);
                    if !root.exists() {
                        continue;
                    }
                    let root = root.canonicalize().expect("family root");
                    if !dirs.contains(&root) {
                        dirs.push(root);
                    }
                }
            }

            for dir in dirs {
                expected.push((dir.clone(), dir, false));
            }
        }

        expected
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

        #[cfg(target_os = "linux")]
        let machine = elf_machine(&program).expect("ELF machine");

        let mut expected = vec![
            (target.clone(), target, false),
            (program.clone(), program, true),
        ];

        #[cfg(target_os = "linux")]
        expected.extend(expected_system_loader_entries(machine));

        expected.push((state.clone(), state, false));

        assert_eq!(grants, expected);
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
            drain_on_exit(&mut connection, probe.id, Vec::new(), |frame| {
                delivered.push(frame);
            }),
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
            drain_on_exit(&mut reader, 3, Vec::new(), |frame| delivered.push(frame)).await,
            DrainStop::Closed
        );
        assert_eq!(delivered, [log_frame(1), log_frame(2), log_frame(3)]);
    }

    #[tokio::test]
    async fn a_partial_frame_buffered_at_exit_survives_in_order() {
        let first = log_frame(1);
        let partial = br#"{"jsonrpc":"2.0","method":"log","n":2"#;
        let tail = b",\"done\":true}\n";
        let completed = [partial.as_slice(), tail].concat();
        let mut input = tail.to_vec();
        input.extend_from_slice(&log_frame(3));
        let mut reader = BufReader::new(input.as_slice());
        let mut probe = HealthProbe {
            frame: partial.to_vec(),
            pending: vec![first.clone()],
            ..HealthProbe::default()
        };
        let mut delivered = Vec::new();

        probe
            .pending
            .drain(..)
            .for_each(|frame| delivered.push(frame));
        let drained = drain_on_exit(
            &mut reader,
            probe.id,
            std::mem::take(&mut probe.frame),
            |frame| delivered.push(frame),
        )
        .await;

        assert_eq!(drained, DrainStop::Closed);
        assert_eq!(delivered, [first, completed, log_frame(3)]);
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
        let stop = drain_on_exit(&mut reader, 1, Vec::new(), |f| delivered.push(f)).await;
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
            drain_on_exit(&mut reader, 1, Vec::new(), |frame| delivered.push(frame)),
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
            drain_on_exit(&mut reader, 1, Vec::new(), |frame| delivered.push(frame)),
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
            drain_on_exit(&mut reader, 1, Vec::new(), |frame| delivered.push(frame)).await,
            DrainStop::FrameLimit
        );
        assert_eq!(delivered, logs[..MAX_EXIT_DRAIN_FRAMES].to_vec());

        let mut large = b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"body\":\"".to_vec();
        large.resize(MAX_EXIT_DRAIN_BYTES, b'x');
        large.extend_from_slice(b"\"}\n");
        let mut reader = BufReader::new(large.as_slice());
        let mut delivered = Vec::new();

        assert_eq!(
            drain_on_exit(&mut reader, 1, Vec::new(), |frame| delivered.push(frame)).await,
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
        let stop = drain_on_exit(&mut reader, 1, Vec::new(), |frame| delivered.push(frame)).await;
        let total: usize = delivered.iter().map(Vec::len).sum();

        assert_eq!(stop, DrainStop::ByteLimit);
        assert_eq!(delivered.len(), count);
        assert_eq!(total, MAX_EXIT_DRAIN_BYTES);
        assert_eq!(delivered, vec![frame; count]);
    }
}
