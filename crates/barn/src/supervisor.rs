//! Starts a sandboxed stall and verifies its connect-back handshake.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use barn_stall::{MAX_LINE_BYTES, PROTOCOL_VERSION, method};
use barn_stall::{StallStartParams, StallStartResult};
use nono::{NonoError, SandboxState, UnixSocketMode};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::BufReader;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

use crate::{Grant, PolicyError};

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_mins(5);
const CRASH_WINDOW: Duration = Duration::from_mins(10);
const CRASH_LIMIT: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_BYTES: usize = 32;
const PRIVATE_DIR: u32 = 0o700;
const PRIVATE_FILE: u32 = 0o600;
const SOCKET_FILE: &str = "proto.sock";
const POLICY_FILE: &str = "policy.json";
const OVERSIZE_REPLY: &[u8] = concat!(
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"#,
    r#""message":"stall start message exceeds the protocol line limit"}}"#,
    "\n"
)
.as_bytes();

/// A stall and the in-memory grant that bounds it.
pub struct StallSpec {
    /// The stall's name; it must be a single path component other than `run`.
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
}

struct Transport {
    listener: UnixListener,
    run_dir: RunDir,
    state: String,
    uid: u32,
    token: Option<String>,
}

struct Prepared {
    listener: StdUnixListener,
    run_dir: RunDir,
    state: String,
    uid: u32,
    token: String,
    program: PathBuf,
}

struct RunDir(PathBuf);

/// What happens after a running child exits.
pub enum RestartOutcome {
    /// The child exits successfully; barn does not restart it.
    Exited,
    /// The child exceeds the crash limit and stays stopped.
    Failed,
    /// barn starts a replacement child after the backoff.
    Restarted {
        /// The status of the child that crashed.
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
    /// The target is not the executable named in the grant.
    #[error("stall program does not match its executable grant")]
    ExecutableNotGranted,
    /// The state path cannot be put in a JSON string.
    #[error("stall state path is not UTF-8")]
    StatePath,
    /// The run root is not a private directory.
    #[error("run root is not a private directory")]
    RunRoot,
    /// The grant cannot be converted into a sandbox policy.
    #[error("building stall grant: {0}")]
    Grant(#[from] PolicyError),
    /// nono cannot grant access to the connect-back socket.
    #[error("granting access to the stall socket: {0}")]
    SocketPolicy(#[source] NonoError),
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
        let pid = child.id().ok_or(SupervisorError::MissingChildPid)?;

        Ok(RunningStall {
            child,
            pid,
            transport: Transport {
                listener,
                run_dir: prepared.run_dir,
                state: prepared.state,
                uid: prepared.uid,
                token: Some(prepared.token),
            },
            supervisor: self,
            spec,
            started: Instant::now(),
            epoch,
            restart,
        })
    }
}

impl RunningStall {
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

    /// Accepts one connection and responds to its start request.
    ///
    /// The returned reader keeps bytes the peer sent after the handshake.
    /// A rejected connection is closed and the listener stays available.
    ///
    /// # Errors
    /// Returns a named error for a timed-out, unauthenticated or invalid start.
    pub async fn accept(&mut self) -> Result<BufReader<UnixStream>> {
        timeout(CONNECT_TIMEOUT, self.transport.accept(self.pid))
            .await
            .map_err(|_| SupervisorError::ConnectTimeout)?
    }

    /// Waits for the child to exit and applies the restart policy on a crash.
    ///
    /// Dropping the future stops supervising this child.
    ///
    /// # Errors
    /// Returns the stage that fails while waiting or starting a replacement.
    pub async fn wait_and_restart(mut self) -> Result<RestartOutcome> {
        let status = io_at("waiting for stall", self.child.wait().await)?;

        if status.success() {
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

impl Transport {
    async fn accept(&mut self, child_pid: u32) -> Result<BufReader<UnixStream>> {
        let accepted = self.listener.accept().await;
        let (stream, _) = io_at("accepting stall socket", accepted)?;
        let cred = io_at("reading stall peer credentials", stream.peer_cred())?;
        verify_peer(cred.pid(), cred.uid(), child_pid, self.uid)?;

        let mut reader = BufReader::new(stream);
        let (id, params) = match read_start(&mut reader).await {
            Err(SupervisorError::FrameTooLarge) => {
                io_at(
                    "answering oversized stall start",
                    reader.write_all(OVERSIZE_REPLY).await,
                )?;

                return Err(SupervisorError::FrameTooLarge);
            }
            result => result?,
        };
        spend_token(&mut self.token, &params.token)?;

        let reply = StallStartResult {
            state: self.state.clone(),
        };
        let response = json!({"jsonrpc":"2.0","id":id,"result":reply});
        let mut frame = serde_json::to_vec(&response)?;
        frame.push(b'\n');

        if frame.len() > MAX_LINE_BYTES {
            return Err(SupervisorError::FrameTooLarge);
        }

        io_at("answering stall start", reader.write_all(&frame).await)?;

        Ok(reader)
    }
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

fn spend_token(saved: &mut Option<String>, offered: &str) -> Result<()> {
    let expected = saved.as_ref().ok_or(SupervisorError::TokenSpent)?;

    // The length is public; equal-length comparisons touch every byte.
    if expected.len() != offered.len() {
        return Err(SupervisorError::InvalidToken);
    }

    let mut difference = 0;

    for (left, right) in expected.bytes().zip(offered.bytes()) {
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

    if name == "run" || Path::new(name).file_name() != Some(name) {
        return Err(SupervisorError::InvalidName);
    }

    let allowed = spec.grant.executable.as_ref() == Some(&spec.program);

    if !spec.program.is_absolute() || !allowed {
        return Err(SupervisorError::ExecutableNotGranted);
    }

    let program = io_at("resolving stall program", spec.program.canonicalize())?;
    let grant = Grant {
        executable: Some(program.clone()),
        ..spec.grant.clone()
    };
    let caps = grant.to_capability_set()?;
    let (run_dir, state) = run_paths(state_dir, &spec.name)?;
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

    // Reconstruct filesystem grants from the resolved names in the helper.
    for path in &mut state_policy.fs {
        path.original.clone_from(&path.resolved);
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

fn run_paths(state_dir: &Path, name: &str) -> Result<(RunDir, String)> {
    io_at(
        "creating barn state root",
        DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR)
            .create(state_dir),
    )?;
    let state_dir = io_at("resolving barn state root", state_dir.canonicalize())?;
    let state = state_dir
        .join(name)
        .into_os_string()
        .into_string()
        .map_err(|_| SupervisorError::StatePath)?;
    let run_root = state_dir.join("run");

    match DirBuilder::new().mode(PRIVATE_DIR).create(&run_root) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        result => io_at("creating barn run root", result)?,
    }

    let root = io_at("checking barn run root", fs::symlink_metadata(&run_root))?;

    if !root.is_dir() || root.mode() & 0o077 != 0 {
        return Err(SupervisorError::RunRoot);
    }

    let path = run_root.join(name);
    io_at(
        "creating private stall run directory",
        DirBuilder::new().mode(PRIVATE_DIR).create(&path),
    )?;
    let run_dir = RunDir(path);
    chmod(&run_dir.0, PRIVATE_DIR)?;

    Ok((run_dir, state))
}

impl RunDir {
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        // Only remove the files barn owns; never traverse the stall's state.
        let _ = fs::remove_file(self.path(SOCKET_FILE));
        let _ = fs::remove_file(self.path(POLICY_FILE));
        let _ = fs::remove_dir(&self.0);
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

    #[test]
    fn only_the_exact_token_is_spent() {
        let mut saved = Some("secret".to_owned());

        for offered in ["secre", "secrets", "xecret", "secrex", "secreT"] {
            assert!(matches!(
                spend_token(&mut saved, offered),
                Err(SupervisorError::InvalidToken)
            ));
        }

        assert!(spend_token(&mut saved, "secret").is_ok());
        assert_eq!(saved, None);
        assert!(matches!(
            spend_token(&mut saved, "secret"),
            Err(SupervisorError::TokenSpent)
        ));
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

        for name in ["", ".", "..", "run", "../stall", "x/x", "x/"] {
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
                    token: "secret".to_owned()
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
}
