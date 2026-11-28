//! Restart, health and run root decisions. The process fixture runs in CI only.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use barn::Grant;
use barn::supervisor::{HealthCheck, HealthDecision, HealthTracker};
use barn::supervisor::{HostDeny, HostEndpoint, RunningStall};
use barn::supervisor::{RestartDecision, RestartOutcome, RestartPolicy};
use barn::supervisor::{StallSpec, Supervisor, SupervisorError, spawn_stall_reader};
use barn_stall::Job;
use barn_stall::protocol::{deny, method};
use serde_json::{Value, json};
use std::ffi::OsStr;
use tokio::io::BufReader as AsyncBufReader;
use tokio::net::UnixStream as AsyncUnixStream;
use tokio::time::timeout;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A scratch folder holding a state root and a granted program, removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> io::Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let dir = base.join(format!("barn-supervisor-{}-{n}", std::process::id()));

        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state"))?;
        fs::write(dir.join("stall"), b"")?;

        Ok(Self { dir })
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    async fn start_error(&self) -> Option<SupervisorError> {
        let program = self.dir.join("stall");
        let spec = StallSpec {
            name: "stall".to_owned(),
            program: program.clone(),
            args: Vec::new(),
            grant: Grant {
                executable: Some(program),
                ..Grant::default()
            },
        };
        let helper = self.dir.join("missing-barn-exec");

        Supervisor::new(self.state(), helper)
            .start(spec)
            .await
            .err()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.state(), fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn a_run_root_that_cannot_be_created_reports_the_creation() {
    let scratch = Scratch::new().expect("the scratch folder is created");
    let state = scratch.state();
    let read_only = fs::Permissions::from_mode(0o500);
    fs::set_permissions(&state, read_only).expect("the state root is locked");

    assert!(matches!(
        scratch.start_error().await,
        Some(SupervisorError::Io {
            operation: "creating barn run root",
            ..
        })
    ));
}

#[tokio::test]
async fn a_run_root_that_is_a_file_is_refused() {
    let scratch = Scratch::new().expect("the scratch folder is created");
    fs::write(scratch.state().join("run"), b"").expect("the run file is written");

    assert!(matches!(
        scratch.start_error().await,
        Some(SupervisorError::RunRoot)
    ));
}

#[tokio::test]
async fn a_run_root_others_can_open_is_refused() {
    let scratch = Scratch::new().expect("the scratch folder is created");
    let run = scratch.state().join("run");
    fs::create_dir(&run).expect("the run root is created");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o777))
        .expect("the run root is opened to others");

    assert!(matches!(
        scratch.start_error().await,
        Some(SupervisorError::RunRoot)
    ));
}

#[test]
fn the_fifth_crash_in_ten_minutes_stops_automatic_restarts() {
    let mut policy = RestartPolicy::default();

    for (now, delay) in [(0, 1), (1, 2), (3, 4), (7, 8)] {
        assert_eq!(
            policy.crashed(Duration::from_secs(now), Duration::ZERO),
            RestartDecision::After(Duration::from_secs(delay))
        );
    }

    assert_eq!(
        policy.crashed(Duration::from_secs(15), Duration::ZERO),
        RestartDecision::Failed
    );
    assert_eq!(
        policy.crashed(Duration::from_secs(615), Duration::from_secs(600)),
        RestartDecision::Failed
    );
}

#[test]
fn a_crash_expires_at_the_ten_minute_boundary() {
    let mut policy = RestartPolicy::default();

    for (now, delay) in [(0, 1), (100, 2), (200, 4), (300, 8), (600, 16)] {
        assert_eq!(
            policy.crashed(Duration::from_secs(now), Duration::ZERO),
            RestartDecision::After(Duration::from_secs(delay))
        );
    }

    assert_eq!(
        policy.crashed(Duration::from_secs(601), Duration::ZERO),
        RestartDecision::Failed
    );
}

#[test]
fn ten_healthy_minutes_reset_the_backoff() {
    let mut policy = RestartPolicy::default();

    for (now, uptime, delay) in [(0, 0, 1), (599, 599, 2), (1200, 600, 1), (1202, 0, 2)] {
        assert_eq!(
            policy.crashed(Duration::from_secs(now), Duration::from_secs(uptime)),
            RestartDecision::After(Duration::from_secs(delay))
        );
    }
}

#[test]
fn dispersed_crashes_double_the_delay_up_to_five_minutes() {
    let mut policy = RestartPolicy::default();
    let cases = [
        (0, 1),
        (180, 2),
        (360, 4),
        (540, 8),
        (720, 16),
        (900, 32),
        (1080, 64),
        (1260, 128),
        (1440, 256),
        (1800, 300),
        (2160, 300),
    ];

    for (now, delay) in cases {
        assert_eq!(
            policy.crashed(Duration::from_secs(now), Duration::from_secs(180)),
            RestartDecision::After(Duration::from_secs(delay))
        );
    }
}

#[test]
fn the_third_consecutive_miss_requests_a_restart() {
    use HealthCheck::{Answered, Missed};
    use HealthDecision::{Continue, Restart};
    use HealthTracker::{Healthy, OneMiss, TwoMisses};

    let mut tracker = HealthTracker::default();
    let checks = [
        (Answered, Healthy, Continue),
        (Missed, OneMiss, Continue),
        (Answered, Healthy, Continue),
        (Missed, OneMiss, Continue),
        (Missed, TwoMisses, Continue),
        (Answered, Healthy, Continue),
        (Missed, OneMiss, Continue),
        (Missed, TwoMisses, Continue),
        (Missed, HealthTracker::Restart, Restart),
        (Missed, HealthTracker::Restart, Restart),
        (Answered, HealthTracker::Restart, Restart),
    ];

    for (check, state, decision) in checks {
        assert_eq!(tracker.record(check), decision);
        assert_eq!(tracker, state);
    }
}

#[tokio::test]
async fn supervision_replaces_an_unhealthy_child_and_checks_the_replacement() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let scratch = Scratch::new().expect("scratch folder");
    let helper = scratch.dir.join("fixture-exec");
    // Exec keeps the PID the supervisor authenticates. Sandbox enforcement is
    // covered separately by the barn-exec integration tests.
    fs::write(&helper, "#!/bin/sh\nshift\nexec \"$@\"\n").expect("fixture helper");
    let executable = fs::Permissions::from_mode(0o700);
    fs::set_permissions(&helper, executable).expect("helper mode");
    let program = std::env::current_exe().expect("fixture executable");
    let spec = StallSpec {
        name: "stall".to_owned(),
        program: program.clone(),
        args: ["--exact", "health_stall_fixture"].map(Into::into).to_vec(),
        grant: Grant {
            executable: Some(program),
            ..Grant::default()
        },
    };
    let supervisor = Supervisor::new(scratch.state(), helper);
    let mut stall = supervisor.start(spec).await.expect("start first child");
    let connection = stall.accept().await.expect("authenticate first child");

    for _ in 0..2 {
        assert_eq!(
            stall.record_health(HealthCheck::Missed),
            HealthDecision::Continue
        );
    }

    let outcome = timeout(Duration::from_secs(18), stall.supervise(connection, |_| {}))
        .await
        .expect("health restart deadline")
        .expect("health restart");
    let RestartOutcome::Restarted { crash, mut stall } = outcome else {
        panic!("unhealthy child must restart");
    };
    assert_eq!(crash.signal(), Some(9));

    assert!(matches!(
        stall.accept().await,
        Err(SupervisorError::InvalidToken)
    ));
    let connection = stall.accept().await.expect("authenticate replacement");
    let mut frames = Vec::new();
    let outcome = timeout(
        Duration::from_secs(25),
        stall.supervise(connection, |frame| frames.push(frame)),
    )
    .await
    .expect("replacement supervision deadline")
    .expect("replacement supervision");
    let outcome = match outcome {
        RestartOutcome::Exited => "exited",
        RestartOutcome::Failed => "failed",
        RestartOutcome::Restarted { .. } => "restarted",
    };
    assert_eq!(outcome, "exited");
    assert_eq!(frames, [LOG_FRAME.as_bytes().to_vec()]);
}

#[tokio::test]
async fn a_start_reply_exposes_the_declared_jobs() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let scratch = Scratch::new().expect("scratch folder");
    let helper = scratch.dir.join("fixture-exec");
    fs::write(&helper, "#!/bin/sh\nshift\nexec \"$@\"\n").expect("fixture helper");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("helper mode");
    let program = std::env::current_exe().expect("fixture executable");
    let spec = StallSpec {
        name: "stall".to_owned(),
        program: program.clone(),
        args: ["--exact", "jobs_stall_fixture"].map(Into::into).to_vec(),
        grant: Grant {
            executable: Some(program),
            ..Grant::default()
        },
    };
    let supervisor = Supervisor::new(scratch.state(), helper);
    let mut stall = supervisor.start(spec).await.expect("start child");

    stall.accept().await.expect("authenticate child");

    assert_eq!(stall.jobs(), &declared_jobs());
    assert_eq!(
        stall.jobs().get("poll"),
        Some(&Job {
            every: "15s".to_owned(),
        })
    );
}

#[tokio::test]
async fn a_real_stall_execs_and_authenticates_without_a_cli_change() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let scratch = Scratch::new().expect("scratch folder");
    let barn = PathBuf::from(env!("CARGO_BIN_EXE_barn"));
    let beside_helper = barn.with_file_name("barn-exec");
    let beside_program = barn.with_file_name("barn-luau-stall");
    let debug = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let debug_helper = debug.join("barn-exec");
    let debug_program = debug.join("barn-luau-stall");

    // The CLI finds both binaries beside the running barn; use those same
    // paths here, without starting a CLI process. cargo-mutants builds in an
    // isolated target dir with no siblings, so build both binaries and look
    // in the target dir inherited from the test when neither usual place has
    // them.
    let (helper, program) = if beside_helper.is_file() && beside_program.is_file() {
        (beside_helper, beside_program)
    } else if debug_helper.is_file() && debug_program.is_file() {
        (debug_helper, debug_program)
    } else {
        let inherited_target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
        let target = inherited_target
            .as_ref()
            .map_or_else(|| debug.clone(), |dir| dir.join("debug"));

        let mut build = std::process::Command::new("cargo");
        build.args([
            "build",
            "--locked",
            "-p",
            "barn-exec",
            "-p",
            "barn-luau-stall",
        ]);
        if let Some(dir) = &inherited_target {
            build.env("CARGO_TARGET_DIR", dir);
        }
        let status = build.status().expect("build barn-exec and barn-luau-stall");
        assert!(
            status.success(),
            "build barn-exec and barn-luau-stall failed"
        );

        (target.join("barn-exec"), target.join("barn-luau-stall"))
    };
    assert!(helper.is_file(), "barn-exec must be built");
    assert!(program.is_file(), "barn-luau-stall must be built");

    let plugin = scratch.dir.join("plugin");
    fs::create_dir(&plugin).expect("plugin folder");
    fs::write(plugin.join("plugin.luau"), "return { name = 'fixture' }\n").expect("plugin module");
    let spec = StallSpec {
        name: "stall".to_owned(),
        program: program.clone(),
        args: vec![plugin.clone().into_os_string()],
        grant: Grant {
            executable: Some(program),
            read_paths: vec![plugin.clone()],
            write_paths: vec![plugin],
            network_hosts: Vec::new(),
        },
    };
    let supervisor = Supervisor::new(scratch.state(), helper);
    let mut stall = supervisor
        .start_for_host(spec, std::process::id())
        .await
        .expect("start the real stall under barn-exec");

    // `accept` admits only the child's pid and spends the token the stall
    // read from its environment, so this handshake proves the exec'd binary
    // sent `stall.start` from the expected pid with the right token.
    let accepted = timeout(Duration::from_secs(30), stall.accept())
        .await
        .expect("stall.start within the token deadline");
    assert!(
        accepted.is_ok(),
        "the exec'd stall must authenticate: {accepted:?}"
    );
}

const LOG_FRAME: &str = concat!(
    r#"{"jsonrpc":"2.0","method":"log","params":{"message":"replacement ready"}}"#,
    "\n"
);

#[test]
fn health_stall_fixture() {
    let Some(socket) = std::env::var_os("BARN_SOCKET").map(PathBuf::from) else {
        return;
    };

    let root = socket
        .ancestors()
        .nth(3)
        .expect("state root in socket path");
    let state = root.join("stall");
    let marker = state.join("started");
    let token = std::env::var("BARN_TOKEN").expect("start token");
    let first = !marker.exists();

    if !first {
        let old = fs::read_to_string(&marker).expect("first launch token");
        assert_ne!(old, token);
        let mut rejected =
            connect_stall(&socket, &old, &BTreeMap::new()).expect("offer the old token");
        let mut line = String::new();

        assert_eq!(rejected.read_line(&mut line).expect("old token refused"), 0);
    }

    let mut client =
        connect_stall(&socket, &token, &BTreeMap::new()).expect("offer the issued token");
    assert_eq!(
        read_message(&mut client),
        Some(json!({"jsonrpc":"2.0","id":"start","protocol":1,"result":{"state":state}}))
    );

    if first {
        fs::write(&marker, &token).expect("remember the first launch token");
        assert_health_request(&mut client, 1);
        writeln!(client.get_mut(), "{{").expect("send malformed reply");
        std::thread::sleep(Duration::from_secs(25));

        return;
    }

    for id in [1, 2] {
        assert_health_request(&mut client, id);

        if id == 1 {
            write!(client.get_mut(), "{LOG_FRAME}").expect("send log before reply");
        }

        let reply = json!({"jsonrpc":"2.0", "id":id, "result":null});
        writeln!(client.get_mut(), "{reply}").expect("send health reply");
    }
}

#[test]
fn jobs_stall_fixture() {
    let Some(socket) = std::env::var_os("BARN_SOCKET").map(PathBuf::from) else {
        return;
    };
    let token = std::env::var("BARN_TOKEN").expect("start token");
    let mut client =
        connect_stall(&socket, &token, &declared_jobs()).expect("connect with declared jobs");

    assert!(read_message(&mut client).is_some());
}

fn declared_jobs() -> BTreeMap<String, Job> {
    BTreeMap::from([(
        "poll".to_owned(),
        Job {
            every: "15s".to_owned(),
        },
    )])
}

fn connect_stall(
    socket: &Path,
    token: &str,
    jobs: &BTreeMap<String, Job>,
) -> io::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(socket)?;
    let deadline = Some(Duration::from_secs(35));
    stream.set_read_timeout(deadline)?;
    stream.set_write_timeout(deadline)?;
    let start = json!({
        "jsonrpc":"2.0", "id":"start", "method":"stall.start",
        "params":{"protocol":1,"token":token,"jobs":jobs}
    });
    writeln!(stream, "{start}")?;

    Ok(BufReader::new(stream))
}

fn read_message(client: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = String::new();
    client.read_line(&mut line).ok()?;

    serde_json::from_str(&line).ok()
}

fn assert_health_request(client: &mut BufReader<UnixStream>, id: u64) {
    assert_eq!(
        read_message(client),
        Some(json!({"jsonrpc":"2.0", "id":id, "method":"stall.health"}))
    );
}

const HOST_REQUEST: &str = concat!(
    r#"{"type":"hook","id":1,"version":1,"hook":"PreToolUse","#,
    r#""call":{"tool":"bash","name":"run","input":{"command":"git push"}}}"#,
);
const HOST_REPLY: &str = r#"{"type":"hook","id":1,"outcome":{"deny":{"reason":"push blocked"}}}"#;

/// Starts a stall with a host endpoint for `daemon_pid` and authenticates it.
async fn host_fixture(
    scratch: &Scratch,
    daemon_pid: u32,
) -> Result<(RunningStall, HostEndpoint, AsyncBufReader<AsyncUnixStream>), Box<dyn std::error::Error>>
{
    let helper = scratch.dir.join("fixture-exec");
    fs::write(&helper, "#!/bin/sh\nshift\nexec \"$@\"\n")?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;
    let program = std::env::current_exe()?;
    let spec = StallSpec {
        name: "stall".to_owned(),
        program: program.clone(),
        args: ["--exact", "broker_stall_fixture"].map(Into::into).to_vec(),
        grant: Grant {
            executable: Some(program),
            ..Grant::default()
        },
    };
    let supervisor = Supervisor::new(scratch.state(), helper);
    let mut stall = supervisor.start_for_host(spec, daemon_pid).await?;
    let endpoint = stall.host_endpoint();
    let connection = stall.accept().await?;

    Ok((stall, endpoint, connection))
}

fn host_start(id: u64, token: &str, pid: u32, protocol: u32) -> String {
    json!({
        "jsonrpc": "2.0", "id": id, "method": method::HOST_START,
        "params": {"protocol": protocol, "token": token, "pid": pid}
    })
    .to_string()
}

fn connect_host(endpoint: &HostEndpoint) -> io::Result<BufReader<UnixStream>> {
    let stream = UnixStream::connect(&endpoint.socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    Ok(BufReader::new(stream))
}

fn read_host_line(client: &mut BufReader<UnixStream>) -> serde_json::Result<Value> {
    let mut line = String::new();
    client.read_line(&mut line).map_err(serde_json::Error::io)?;

    serde_json::from_str(&line)
}

fn assert_denied(reply: &Value, reason: &str) {
    assert_eq!(
        reply,
        &json!({
            "jsonrpc": "2.0", "id": 21,
            "error": {"code": -32001, "message": "denied",
                      "data": {"reason": reason}}
        })
    );
}

#[tokio::test]
async fn the_host_endpoint_exposes_a_private_socket_and_a_fresh_token() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let (_stall, endpoint, _connection) = host_fixture(&scratch, std::process::id())
        .await
        .expect("host fixture");

    assert_eq!(
        endpoint.socket_path.file_name().and_then(OsStr::to_str),
        Some("host.sock")
    );
    assert_eq!(endpoint.token.len(), 64);
    let mode = fs::symlink_metadata(&endpoint.socket_path)
        .expect("host socket")
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn host_start_with_the_wrong_peer_pid_is_denied() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let wrong = std::process::id().wrapping_add(1);
    let (mut stall, endpoint, _connection) =
        host_fixture(&scratch, wrong).await.expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    writeln!(
        client.get_mut(),
        "{}",
        host_start(21, &endpoint.token, wrong, 1)
    )
    .expect("host.start");

    assert!(matches!(
        stall.accept_host().await,
        Err(SupervisorError::HostDenied(HostDeny::WrongPeerPid))
    ));
    // Peer identity is checked before host.start's line is read, so the
    // denial carries a null id rather than the id from that line.
    assert_eq!(
        read_host_line(&mut client).expect("deny"),
        json!({
            "jsonrpc": "2.0", "id": Value::Null,
            "error": {"code": -32001, "message": "denied",
                      "data": {"reason": deny::WRONG_PEER_PID}}
        })
    );
}

#[tokio::test]
async fn host_start_with_the_wrong_token_is_denied() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let (mut stall, endpoint, _connection) = host_fixture(&scratch, std::process::id())
        .await
        .expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    let wrong = "0".repeat(64);
    writeln!(
        client.get_mut(),
        "{}",
        host_start(21, &wrong, std::process::id(), 1)
    )
    .expect("host.start");

    assert!(matches!(
        stall.accept_host().await,
        Err(SupervisorError::HostDenied(HostDeny::TokenMismatch))
    ));
    assert_denied(
        &read_host_line(&mut client).expect("deny"),
        deny::TOKEN_MISMATCH,
    );
}

#[tokio::test]
async fn a_spent_host_token_is_denied_without_a_retry() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let pid = std::process::id();
    let (mut stall, endpoint, _connection) =
        host_fixture(&scratch, pid).await.expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    writeln!(
        client.get_mut(),
        "{}",
        host_start(21, &endpoint.token, pid, 1)
    )
    .expect("first host.start");
    let server = stall
        .accept_host()
        .await
        .expect("first host.start accepted");

    assert_eq!(
        read_host_line(&mut client).expect("reply")["result"],
        json!({"stall": "stall"})
    );
    drop(server);

    let mut again = connect_host(&endpoint).expect("reconnect host.sock");
    writeln!(
        again.get_mut(),
        "{}",
        host_start(22, &endpoint.token, pid, 1)
    )
    .expect("second host.start");

    assert!(matches!(
        stall.accept_host().await,
        Err(SupervisorError::HostDenied(HostDeny::TokenExpiredOrSpent))
    ));
    assert_eq!(
        read_host_line(&mut again).expect("deny")["error"]["data"]["reason"],
        deny::TOKEN_EXPIRED_OR_SPENT
    );
}

#[tokio::test]
async fn host_start_with_the_wrong_version_is_denied() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let pid = std::process::id();
    let (mut stall, endpoint, _connection) =
        host_fixture(&scratch, pid).await.expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    writeln!(
        client.get_mut(),
        "{}",
        host_start(21, &endpoint.token, pid, 2)
    )
    .expect("host.start");

    assert!(matches!(
        stall.accept_host().await,
        Err(SupervisorError::HostDenied(HostDeny::ProtocolMismatch))
    ));
    assert_denied(
        &read_host_line(&mut client).expect("deny"),
        deny::PROTOCOL_MISMATCH,
    );
}

#[tokio::test]
async fn a_correct_host_start_returns_the_stall_name() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let pid = std::process::id();
    let (mut stall, endpoint, _connection) =
        host_fixture(&scratch, pid).await.expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    writeln!(
        client.get_mut(),
        "{}",
        host_start(21, &endpoint.token, pid, 1)
    )
    .expect("host.start");

    let _server = stall.accept_host().await.expect("host.start accepted");
    assert_eq!(
        read_host_line(&mut client).expect("reply"),
        json!({"jsonrpc": "2.0", "id": 21, "result": {"stall": "stall"}})
    );
}

#[tokio::test]
async fn a_framed_request_is_forwarded_and_its_exact_reply_returned() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let pid = std::process::id();
    let (mut stall, endpoint, connection) =
        host_fixture(&scratch, pid).await.expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    writeln!(
        client.get_mut(),
        "{}",
        host_start(21, &endpoint.token, pid, 1)
    )
    .expect("host.start");
    let mut server = stall.accept_host().await.expect("host.start accepted");
    assert_eq!(
        read_host_line(&mut client).expect("reply")["result"],
        json!({"stall":"stall"})
    );

    writeln!(client.get_mut(), "{HOST_REQUEST}").expect("framed request");
    let (read_half, mut writer) = connection.into_inner().into_split();
    let mut rx = spawn_stall_reader(read_half);
    stall
        .serve_host(&mut server, &mut writer, &mut rx)
        .await
        .expect("broker the frame");

    let read = tokio::task::spawn_blocking(move || {
        let mut reply = String::new();
        client.read_line(&mut reply).expect("framed reply");
        reply
    });
    let reply = timeout(Duration::from_secs(2), read)
        .await
        .expect("framed reply within the deadline")
        .expect("framed reply task");
    assert_eq!(reply, format!("{HOST_REPLY}\n"));
}

#[tokio::test]
async fn a_host_that_closes_before_start_is_denied() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let (mut stall, endpoint, _connection) = host_fixture(&scratch, std::process::id())
        .await
        .expect("host fixture");
    let mut client = connect_host(&endpoint).expect("connect host.sock");
    client
        .get_mut()
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close the host socket");

    assert!(matches!(
        stall.accept_host().await,
        Err(SupervisorError::HostDenied(HostDeny::ClosedSocket))
    ));
    assert_eq!(
        read_host_line(&mut client).expect("deny")["error"]["data"]["reason"],
        deny::CLOSED_SOCKET
    );
}

#[test]
fn broker_stall_fixture() {
    let Some(socket) = std::env::var_os("BARN_SOCKET").map(PathBuf::from) else {
        return;
    };
    let token = std::env::var("BARN_TOKEN").expect("start token");
    let mut client =
        connect_stall(&socket, &token, &BTreeMap::new()).expect("connect with the token");
    assert!(read_message(&mut client).is_some());

    let mut request = String::new();
    client
        .read_line(&mut request)
        .expect("read the framed request");
    assert_eq!(request, format!("{HOST_REQUEST}\n"));
    writeln!(client.get_mut(), "{HOST_REPLY}").expect("send the framed reply");
}
