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

    pub(super) fn micros(&self) -> u128 {
        self.clock.micros()
    }

    pub(super) fn begin(&self, ms: u64) {
        self.begin_us(u128::from(ms).saturating_mul(1000));
    }

    pub(super) fn begin_us(&self, us: u128) {
        let Ok(mut started) = self.started.lock() else {
            // A poisoned clock cannot time a call. Arm an already-expired
            // budget so the interrupt denies with the named deadline limit.
            self.budget_us.store(0, Ordering::Relaxed);
            return;
        };

        *started = self.clock.micros();
        self.budget_us.store(
            u64::try_from(us.min(u128::from(u64::MAX - 1))).unwrap_or(u64::MAX - 1), // fallback unreachable: input is clamped to u64::MAX - 1
            Ordering::Relaxed,
        );
    }

    pub(super) fn disarm(&self) {
        self.budget_us.store(u64::MAX, Ordering::Relaxed);
    }

    /// Snapshot the armed deadline so a caller can restore it on any exit,
    /// including one that never reaches an explicit [`Self::disarm`].
    #[cfg_attr(not(test), expect(dead_code, reason = "test-only locked entry"))]
    pub(super) fn guard(&self) -> DeadlineGuard<'_> {
        // A poisoned start clock cannot be trusted; snapshot it as zero.
        let started = self.started.lock().map_or(0, |started| *started);
        let budget = self.budget_us.load(Ordering::Relaxed);

        DeadlineGuard {
            deadline: self,
            started,
            budget,
        }
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "test-only locked entry"))]
    fn restore(&self, started: u128, budget: u64) {
        self.budget_us.store(budget, Ordering::Relaxed);
        if let Ok(mut guard) = self.started.lock() {
            *guard = started;
        }
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

/// Restores a snapshotted deadline when a call exits, so a call that returns
/// early cannot leave the next one running without its budget.
#[cfg_attr(not(test), expect(dead_code, reason = "test-only locked entry"))]
pub(super) struct DeadlineGuard<'a> {
    deadline: &'a Deadline,
    started: u128,
    budget: u64,
}

impl Drop for DeadlineGuard<'_> {
    fn drop(&mut self) {
        self.deadline.restore(self.started, self.budget);
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

    #[test]
    fn begin_us_arms_a_microsecond_budget() {
        let clock = Arc::new(FakeClock::new());
        let deadline = Deadline::with_clock(Arc::<FakeClock>::clone(&clock));

        deadline.begin_us(10);
        clock.advance(Duration::from_micros(11));
        assert!(deadline.expired(), "ten microseconds expires the budget");

        deadline.begin(10);
        assert!(
            !deadline.expired(),
            "the same value read as milliseconds has not expired"
        );
        clock.advance(Duration::from_millis(10));
        assert!(deadline.expired(), "ten milliseconds expires the budget");
    }

    #[test]
    fn guard_restores_the_armed_budget_after_disarm() {
        let clock = Arc::new(FakeClock::new());
        let deadline = Deadline::with_clock(Arc::<FakeClock>::clone(&clock));
        deadline.begin_us(100);
        clock.advance(Duration::from_micros(10));

        {
            let guard = deadline.guard();
            clock.advance(Duration::from_micros(5));
            deadline.begin_us(1);
            deadline.disarm();
            assert!(!deadline.expired(), "a disarmed deadline never expires");
            drop(guard);
        }

        assert_eq!(deadline.budget_us.load(Ordering::Relaxed), 100);
        assert_eq!(*deadline.started.lock().expect("lock start"), 0);
        assert!(!deadline.expired(), "the restored budget is still intact");
        clock.advance(Duration::from_micros(85));
        assert!(deadline.expired(), "the restored budget expires on its own");
    }

    #[test]
    fn guard_restores_after_an_early_return() {
        fn exit_early(deadline: &Deadline, early: bool) {
            let _guard = deadline.guard();
            deadline.begin_us(1);
            if early {
                return;
            }
            deadline.disarm();
        }

        let clock = Arc::new(FakeClock::new());
        let deadline = Deadline::with_clock(Arc::<FakeClock>::clone(&clock));
        deadline.begin_us(500);

        exit_early(&deadline, true);

        assert_eq!(deadline.budget_us.load(Ordering::Relaxed), 500);
        clock.advance(Duration::from_micros(500));
        assert!(
            deadline.expired(),
            "the restored budget governs the next call"
        );
    }

    #[test]
    fn a_panicking_handler_restores_the_previous_deadline() {
        let clock = Arc::new(FakeClock::new());
        let deadline = Deadline::with_clock(Arc::<FakeClock>::clone(&clock));
        deadline.begin(50);
        let guard = deadline.guard();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            deadline.begin_us(1);
            panic!("handler panicked");
        }));
        assert!(panicked.is_err(), "the handler panicked");
        drop(guard);

        clock.advance(Duration::from_millis(5));
        assert!(
            !deadline.expired(),
            "the pre-panic budget was restored, not the one-microsecond budget"
        );
    }
}
