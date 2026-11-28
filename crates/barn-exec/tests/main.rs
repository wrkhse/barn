//! Real sandbox and process tests run in CI only.

use std::env;
use std::error::Error;
use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use barn::Grant;
use barn::supervisor::{RestartOutcome, StallSpec, Supervisor, SupervisorError};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout};

const DEADLINE: Duration = Duration::from_secs(15);
type TestResult = Result<(), Box<dyn Error>>;

#[tokio::test]
async fn barn_accepts_a_sandboxed_child_only_once() -> TestResult {
    if env::var_os("CI").is_none() {
        return Ok(());
    }

    let root = Scratch::new()?;
    let program = env::current_exe()?;
    let spec = fixture(program, &["--exact", "sandbox_client_fixture"]);
    let mut stall = root.supervisor().start(spec).await?;
    let socket = stall.socket_path();
    let dir = socket
        .parent()
        .ok_or_else(|| io::Error::other("missing socket parent"))?;
    let modes = (mode(dir)?, mode(&socket)?, mode(&dir.join("policy.json"))?);

    if modes != (0o700, 0o600, 0o600) {
        let message = format!("unexpected run, socket, policy modes: {modes:?}");

        return Err(io::Error::other(message).into());
    }

    let mut stream = stall.accept().await?;
    let mut next = String::new();
    timeout(DEADLINE, stream.read_line(&mut next)).await??;
    if next != "{\"next\":true}\n" {
        return Err(io::Error::other("wrong follow-on frame").into());
    }

    // The marker can only arrive after the spent-token peer's frame is queued.
    let mut marker = String::new();
    timeout(DEADLINE, stream.read_line(&mut marker)).await??;
    if marker != "{\"spent_attempt\":true}\n" {
        return Err(io::Error::other("wrong spent-attempt marker").into());
    }

    drop(stream);

    match stall.accept().await {
        Err(SupervisorError::TokenSpent) => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(io::Error::other("barn accepted a spent token").into()),
    }

    if !matches!(
        timeout(DEADLINE, stall.wait_and_restart()).await??,
        RestartOutcome::Exited
    ) {
        return Err(io::Error::other("stall did not exit cleanly").into());
    }

    Ok(())
}

#[tokio::test]
async fn barn_ignores_the_wrong_pid_and_authenticates_the_child() -> TestResult {
    if env::var_os("CI").is_none() {
        return Ok(());
    }

    let root = Scratch::new()?;
    let program = env::current_exe()?;
    let spec = fixture(program, &["--exact", "waiting_stall_fixture"]);
    let mut stall = root.supervisor().start(spec).await?;
    let mut wrong = UnixStream::connect(stall.socket_path()).await?;
    wrong.shutdown().await?;

    let release_child = async {
        let mut byte = [0];
        if wrong.read(&mut byte).await? != 0 {
            return Err(io::Error::other("barn replied to the wrong PID"));
        }

        fs::write(root.0.join("fixture").join("connect"), b"")
    };
    let (authenticated, refused) = timeout(DEADLINE, async {
        tokio::join!(stall.accept(), release_child)
    })
    .await?;
    refused?;
    drop(authenticated?);

    if !matches!(
        timeout(DEADLINE, stall.wait_and_restart()).await??,
        RestartOutcome::Exited
    ) {
        return Err(io::Error::other("authenticated child did not exit cleanly").into());
    }

    Ok(())
}

#[tokio::test]
async fn barn_restarts_a_crashed_child_with_a_fresh_pid() -> TestResult {
    if env::var_os("CI").is_none() {
        return Ok(());
    }

    let root = Scratch::new()?;
    let spec = fixture(PathBuf::from("/bin/sh"), &["-c", "exit 23"]);
    let stall = root.supervisor().start(spec).await?;
    let old_pid = stall.pid();
    let started = Instant::now();
    let result = timeout(DEADLINE, stall.wait_and_restart()).await??;
    let RestartOutcome::Restarted { crash, stall } = result else {
        return Err(io::Error::other("child did not restart").into());
    };

    if crash.code() != Some(23) {
        let message = format!("unexpected exit status: {crash}");

        return Err(io::Error::other(message).into());
    }

    if stall.pid() == old_pid || started.elapsed() < Duration::from_secs(1) {
        return Err(io::Error::other("restart lacks backoff or a new PID").into());
    }

    Ok(())
}

#[tokio::test]
async fn a_sandboxed_macos_sh_runs_its_command() -> TestResult {
    if !cfg!(target_os = "macos") || env::var_os("CI").is_none() {
        return Ok(());
    }

    let root = Scratch::new()?;
    let spec = fixture(PathBuf::from("/bin/sh"), &["-c", "exit 0"]);
    let stall = root.supervisor().start(spec).await?;
    let outcome = timeout(DEADLINE, stall.wait_and_restart()).await??;

    if !matches!(outcome, RestartOutcome::Exited) {
        let message = "sandboxed /bin/sh did not exit cleanly";

        return Err(io::Error::other(message).into());
    }

    Ok(())
}

#[tokio::test]
async fn a_stall_not_granted_sh_is_refused_before_it_starts() -> TestResult {
    if !cfg!(target_os = "macos") || env::var_os("CI").is_none() {
        return Ok(());
    }

    let root = Scratch::new()?;
    let mut spec = fixture(PathBuf::from("/bin/sh"), &["-c", "exit 0"]);
    spec.grant.executable = Some(env::current_exe()?);

    match root.supervisor().start(spec).await {
        Err(SupervisorError::ExecutableNotGranted) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(io::Error::other("barn started an ungranted /bin/sh").into()),
    }
}

#[test]
fn sandbox_client_fixture() -> TestResult {
    let Some(socket) = env::var_os("BARN_SOCKET").map(PathBuf::from) else {
        return Ok(());
    };
    let token = env::var("BARN_TOKEN")?;
    let policy = socket.with_file_name("policy.json");

    // The unsandboxed helper reads the file; the sandboxed target cannot.
    let denied = fs::File::open(policy).err().map(|error| error.kind());
    if denied != Some(io::ErrorKind::PermissionDenied) {
        return Err(io::Error::other("stall can read the sandbox policy").into());
    }

    let mut first = connect(&socket, &token, "{\"next\":true}\n")?;
    let mut reply = String::new();
    first.read_line(&mut reply)?;
    let response: Value = serde_json::from_str(&reply)?;
    let root = socket
        .ancestors()
        .nth(3)
        .ok_or_else(|| io::Error::other("missing state root"))?;

    let expected = json!({
        "jsonrpc":"2.0", "id":"start", "protocol":1, "result":{"state":root.join("fixture")}
    });

    if response != expected {
        return Err(io::Error::other("unexpected stall state response").into());
    }

    let mut second = connect(&socket, &token, "")?;
    // Written after the spent-token frame: one stream preserves byte order, so
    // seeing this marker means that peer is already in the listener backlog.
    first.get_mut().write_all(b"{\"spent_attempt\":true}\n")?;
    let mut extra = String::new();

    if second.read_line(&mut extra)? != 0 {
        return Err(io::Error::other("barn accepted a spent token").into());
    }

    Ok(())
}

#[test]
fn waiting_stall_fixture() -> TestResult {
    let Some(socket) = env::var_os("BARN_SOCKET").map(PathBuf::from) else {
        return Ok(());
    };
    let root = socket
        .ancestors()
        .nth(3)
        .ok_or_else(|| io::Error::other("missing state root"))?;
    let state = root.join("fixture");
    let start = state.join("connect");
    let began = std::time::Instant::now();

    while !start.try_exists()? {
        if began.elapsed() >= DEADLINE {
            return Err(io::Error::other("wrong peer was not closed in time").into());
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    let token = env::var("BARN_TOKEN")?;
    let mut client = connect(&socket, &token, "")?;
    let mut reply = String::new();
    client.read_line(&mut reply)?;
    let response: Value = serde_json::from_str(&reply)?;
    let expected = json!({"jsonrpc":"2.0","id":"start","protocol":1,"result":{"state":state}});

    if response != expected {
        return Err(io::Error::other("unexpected stall state response").into());
    }

    Ok(())
}

type Client = BufReader<StdUnixStream>;

fn connect(socket: &Path, token: &str, tail: &str) -> io::Result<Client> {
    let mut stream = StdUnixStream::connect(socket)?;
    stream.set_read_timeout(Some(DEADLINE))?;
    stream.set_write_timeout(Some(DEADLINE))?;
    let login = json!({
        "jsonrpc":"2.0", "id":"start", "method":"stall.start",
        "params":{"protocol":1,"token":token}
    });
    let frame = format!("{login}\n{tail}");
    stream.write_all(frame.as_bytes())?;

    Ok(BufReader::new(stream))
}

fn mode(path: &Path) -> io::Result<u32> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

fn fixture(program: PathBuf, args: &[&str]) -> StallSpec {
    let read_paths = ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .collect();

    StallSpec {
        name: "fixture".to_owned(),
        args: args.iter().map(Into::into).collect(),
        grant: Grant {
            executable: Some(program.clone()),
            read_paths,
            ..Grant::default()
        },
        program,
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("barn-{}-{seq}", std::process::id());
        let path = Path::new("/tmp").join(name);
        DirBuilder::new().mode(0o700).create(&path)?;

        Ok(Self(path))
    }

    fn supervisor(&self) -> Supervisor {
        Supervisor::new(
            self.0.clone(),
            PathBuf::from(env!("CARGO_BIN_EXE_barn-exec")),
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// The real main reports a bad invocation on stderr and exits failing; the
// `main -> Default::default()` mutant exits success. Like the other process
// tests here, this runs in CI only -- AGENTS.md keeps process starts off the
// owner's Mac.
#[test]
fn barn_exec_reports_an_invalid_invocation() {
    if env::var_os("CI").is_none() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_barn-exec"))
        .arg("--definitely-not-a-flag")
        .output()
        .expect("barn-exec starts");

    assert!(
        !output.status.success(),
        "barn-exec must reject an invalid argument: {output:?}"
    );
}
