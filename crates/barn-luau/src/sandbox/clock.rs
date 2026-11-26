/// A host-supplied source of monotonic microseconds for engine deadlines.
pub trait Clock: std::fmt::Debug + Send + Sync {
    /// Return the current tick count in microseconds.
    fn micros(&self) -> u128;
}

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
#[derive(Debug)]
pub(super) struct FakeClock {
    micros: AtomicU64,
}

#[cfg(test)]
impl FakeClock {
    pub(super) fn new() -> Self {
        Self {
            micros: AtomicU64::new(0),
        }
    }

    pub(super) fn advance(&self, by: Duration) {
        let delta = u64::try_from(by.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(1000);
        let _ = self
            .micros
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(delta))
            });
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn micros(&self) -> u128 {
        u128::from(self.micros.load(Ordering::Relaxed))
    }
}
