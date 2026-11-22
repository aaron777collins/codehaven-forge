//! A clock the scheduler reads, so tests can move time.

use chrono::{DateTime, Duration, Utc};
use std::sync::Mutex;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to.
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    pub fn new(at: DateTime<Utc>) -> Self {
        Self(Mutex::new(at))
    }
    pub fn set(&self, at: DateTime<Utc>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }
    pub fn advance(&self, by: Duration) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *g += by;
    }
}

impl ManualClock {
    /// What time the clock says, for assertions.
    pub fn now_for_test(&self) -> DateTime<Utc> {
        self.now()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
