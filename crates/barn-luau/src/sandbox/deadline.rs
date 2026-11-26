use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::clock::Clock;

#[derive(Debug)]
pub(super) struct Deadline {
    clock: Arc<dyn Clock>,
    started: Mutex<u128>,
    budget_us: AtomicU64,
}

impl Deadline {
    pub(super) fn with_clock(clock: Arc<dyn Clock>) -> Self {
        let started = clock.micros();

        Self {
            clock,
            started: Mutex::new(started),
            budget_us: AtomicU64::new(u64::MAX),
        }
    }

    pub(super) fn begin(&self, ms: u64) {
        let Ok(mut started) = self.started.lock() else {
            // A poisoned clock cannot time a call. Arm an already-expired
            // budget so the interrupt denies with the named deadline limit.
            self.budget_us.store(0, Ordering::Relaxed);
            return;
        };

        *started = self.clock.micros();
        self.budget_us
            .store(ms.saturating_mul(1000).min(u64::MAX - 1), Ordering::Relaxed);
    }

    pub(super) fn disarm(&self) {
        self.budget_us.store(u64::MAX, Ordering::Relaxed);
    }

    pub(super) fn expired(&self) -> bool {
        let budget = self.budget_us.load(Ordering::Relaxed);
        if budget == u64::MAX {
            return false;
        }
        let Ok(started) = self.started.lock() else {
            // Fail closed: a poisoned clock becomes a named deadline deny.
            return true;
        };
        let started = *started;
        let now = self.clock.micros();

        if now < started {
            // A backwards clock cannot prove the budget is intact.
            return true;
        }

        now.saturating_sub(started) >= u128::from(budget)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::sandbox::clock::FakeClock;

    #[test]
    fn begin_arms_a_budget_and_disarm_clears_it() {
        let deadline = Deadline::with_clock(Arc::new(FakeClock::new()));
        assert!(!deadline.expired(), "a fresh clock has no budget");

        deadline.begin(0);
        assert!(deadline.expired(), "a zero budget is immediately expired");

        deadline.disarm();
        assert!(!deadline.expired(), "disarming clears the budget");
    }

    #[test]
    fn a_saturated_budget_stays_below_the_disarmed_sentinel() {
        let deadline = Deadline::with_clock(Arc::new(FakeClock::new()));
        deadline.begin(u64::MAX);

        assert_eq!(deadline.budget_us.load(Ordering::Relaxed), u64::MAX - 1);
    }

    #[test]
    fn with_clock_arms_from_the_injected_clock() {
        let clock = Arc::new(FakeClock::new());
        let deadline = Deadline::with_clock(Arc::<FakeClock>::clone(&clock));

        deadline.begin(10);
        clock.advance(Duration::from_millis(9));
        assert!(!deadline.expired(), "nine milliseconds is within ten");

        clock.advance(Duration::from_millis(2));
        assert!(deadline.expired(), "eleven milliseconds exceeds ten");
    }

    #[test]
    fn a_clock_that_moved_backwards_fails_closed() {
        let clock = Arc::new(FakeClock::new());
        let deadline = Deadline::with_clock(Arc::<FakeClock>::clone(&clock));
        deadline.begin(5);

        // Push the captured start ahead of the clock to model a backwards step.
        let ahead = clock.micros().saturating_add(6_000);
        *deadline.started.lock().expect("lock start") = ahead;

        assert!(deadline.expired(), "a backwards clock fails closed");
    }
}
