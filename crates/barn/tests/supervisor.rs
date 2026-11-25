//! Restart and health decisions, without spawning or signalling a process.

use std::time::Duration;

use barn::supervisor::{
    HealthCheck, HealthDecision, HealthTracker, RestartDecision, RestartPolicy,
};

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
