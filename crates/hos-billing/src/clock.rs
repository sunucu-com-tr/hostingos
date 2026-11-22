//! Injectable time source. Every business decision that depends on "today"
//! (due dates, reminders, suspension, proration) reads [`Clock`], so tests can
//! move time forward with [`ManualClock`].

use std::sync::Mutex;

use chrono::{DateTime, Duration, NaiveDate, Utc};

/// A source of the current time.
pub trait Clock: Send + Sync + 'static {
    /// Current instant.
    fn now(&self) -> DateTime<Utc>;

    /// Current UTC date (due dates are calendar dates in UTC).
    fn today(&self) -> NaiveDate {
        self.now().date_naive()
    }
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to (tests, simulations).
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    /// A clock frozen at `now`.
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// A clock frozen at noon (UTC) of `date`.
    pub fn at_date(date: NaiveDate) -> Self {
        Self::new(date.and_hms_opt(12, 0, 0).unwrap_or_default().and_utc())
    }

    /// Jumps to `now`.
    pub fn set(&self, now: DateTime<Utc>) {
        *self.lock() = now;
    }

    /// Jumps to noon (UTC) of `date`.
    pub fn set_date(&self, date: NaiveDate) {
        self.set(date.and_hms_opt(12, 0, 0).unwrap_or_default().and_utc());
    }

    /// Moves forward (or backward with a negative duration).
    pub fn advance(&self, by: Duration) {
        let mut guard = self.lock();
        *guard += by;
    }

    /// Moves forward by whole days.
    pub fn advance_days(&self, days: i64) {
        self.advance(Duration::days(days));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DateTime<Utc>> {
        self.now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 31).unwrap();
        let c = ManualClock::at_date(d);
        assert_eq!(c.today(), d);
        c.advance_days(1);
        assert_eq!(c.today(), NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
        c.set_date(d);
        assert_eq!(c.today(), d);
        assert!(SystemClock.now() > c.now());
    }
}
