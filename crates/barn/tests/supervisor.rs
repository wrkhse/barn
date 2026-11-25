//! Restart, health and run root decisions, without spawning or signalling a
//! process. The run root tests name a helper that does not exist, so even a
//! start that passes the run root check cannot start one.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use barn::Grant;
use barn::supervisor::{
    HealthCheck, HealthDecision, HealthTracker, RestartDecision, RestartPolicy,
};
use barn::supervisor::{StallSpec, Supervisor, SupervisorError};

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
