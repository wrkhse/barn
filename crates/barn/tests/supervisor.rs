//! Restart, health and run root decisions. The process fixture runs in CI only.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use barn::Grant;
use barn::supervisor::{HealthCheck, HealthDecision, HealthTracker};
use barn::supervisor::{RestartDecision, RestartOutcome, RestartPolicy};
use barn::supervisor::{StallSpec, Supervisor, SupervisorError};
use barn_stall::Job;
use serde_json::{Value, json};
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

    assert_eq!(stall.jobs(), declared_jobs().as_slice());
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
        let mut rejected = connect_stall(&socket, &old, &[]).expect("offer the old token");
        let mut line = String::new();

        assert_eq!(rejected.read_line(&mut line).expect("old token refused"), 0);
    }

    let mut client = connect_stall(&socket, &token, &[]).expect("offer the issued token");
    assert_eq!(
        read_message(&mut client),
        Some(json!({"jsonrpc":"2.0","id":"start","result":{"state":state}}))
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

fn declared_jobs() -> Vec<Job> {
    vec![Job {
        every: "15s".to_owned(),
    }]
}

fn connect_stall(socket: &Path, token: &str, jobs: &[Job]) -> io::Result<BufReader<UnixStream>> {
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
