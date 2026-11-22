//! Limits on tool calls: a request budget per grant, a tighter one for writes, an hourly ceiling
//! per person, and caps on how many calls run at once. All in memory: one process serves an
//! instance, and a restart only gives everyone a fresh budget.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A token bucket per key.
struct Buckets {
    burst: f64,
    per_sec: f64,
    map: Mutex<HashMap<i64, Bucket>>,
}

impl Buckets {
    fn new(per_minute: u32, burst: u32) -> Self {
        Self { burst: burst as f64, per_sec: per_minute as f64 / 60.0, map: Mutex::new(HashMap::new()) }
    }
    fn hit(&self, key: i64, now: Instant) -> Result<(), u64> {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > 50_000 {
            let (burst, per_sec) = (self.burst, self.per_sec);
            m.retain(|_, b| b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * per_sec < burst);
        }
        let b = m.entry(key).or_insert(Bucket { tokens: self.burst, last: now });
        b.tokens = (b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * self.per_sec).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / self.per_sec).ceil().max(1.0) as u64)
        }
    }
}

/// A fixed hour window per key.
struct Hourly {
    limit: u32,
    map: Mutex<HashMap<i64, (Instant, u32)>>,
}

impl Hourly {
    fn hit(&self, key: i64, now: Instant) -> Result<(), u64> {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > 50_000 {
            m.retain(|_, (start, _)| now.saturating_duration_since(*start) < Duration::from_secs(3600));
        }
        let e = m.entry(key).or_insert((now, 0));
        if now.saturating_duration_since(e.0) >= Duration::from_secs(3600) {
            *e = (now, 0);
        }
        if e.1 >= self.limit {
            let wait = 3600u64.saturating_sub(now.saturating_duration_since(e.0).as_secs()).max(1);
            return Err(wait);
        }
        e.1 += 1;
        Ok(())
    }
}

pub struct Limits {
    reads: Buckets,
    writes: Buckets,
    hourly: Hourly,
    per_grant: Mutex<HashMap<i64, Arc<Semaphore>>>,
    process: Arc<Semaphore>,
}

/// Held while a call runs. Dropping it frees the slots.
pub struct Permit {
    _grant: OwnedSemaphorePermit,
    _process: OwnedSemaphorePermit,
}

/// Why a call was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    RateLimited(u64),
    Busy,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

impl Limits {
    pub fn new() -> Self {
        Self {
            // 120 a minute per grant with a burst of 60, writes 30 a minute, 3,000 an hour per person.
            reads: Buckets::new(120, 60),
            writes: Buckets::new(30, 30),
            hourly: Hourly { limit: 3000, map: Mutex::new(HashMap::new()) },
            per_grant: Mutex::new(HashMap::new()),
            // 4 at once per grant, 32 in the whole process.
            process: Arc::new(Semaphore::new(32)),
        }
    }

    /// Budgets far above any real use, for tests that make many calls in a second. The caps on
    /// calls running at once stay as they are.
    #[doc(hidden)]
    pub fn generous() -> Self {
        Self {
            reads: Buckets::new(60_000, 60_000),
            writes: Buckets::new(30_000, 30_000),
            hourly: Hourly { limit: 10_000_000, map: Mutex::new(HashMap::new()) },
            per_grant: Mutex::new(HashMap::new()),
            process: Arc::new(Semaphore::new(32)),
        }
    }

    pub fn admit(&self, grant: i64, user: i64, write: bool) -> Result<Permit, Refusal> {
        self.admit_at(grant, user, write, Instant::now())
    }

    pub(crate) fn admit_at(&self, grant: i64, user: i64, write: bool, now: Instant) -> Result<Permit, Refusal> {
        self.reads.hit(grant, now).map_err(Refusal::RateLimited)?;
        if write {
            self.writes.hit(grant, now).map_err(Refusal::RateLimited)?;
        }
        self.hourly.hit(user, now).map_err(Refusal::RateLimited)?;
        let sem = {
            let mut m = self.per_grant.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() > 10_000 {
                m.retain(|_, s| s.available_permits() < 4);
            }
            m.entry(grant).or_insert_with(|| Arc::new(Semaphore::new(4))).clone()
        };
        let g = sem.try_acquire_owned().map_err(|_| Refusal::Busy)?;
        let p = self.process.clone().try_acquire_owned().map_err(|_| Refusal::Busy)?;
        Ok(Permit { _grant: g, _process: p })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_burst_then_refill() {
        let l = Limits::new();
        let t0 = Instant::now();
        let mut ok = 0;
        for _ in 0..100 {
            if let Ok(p) = l.admit_at(1, 1, false, t0) {
                ok += 1;
                drop(p);
            } else {
                break;
            }
        }
        assert_eq!(ok, 60, "burst of 60");
        assert!(matches!(l.admit_at(1, 1, false, t0), Err(Refusal::RateLimited(_))));
        assert!(l.admit_at(1, 1, false, t0 + Duration::from_secs(2)).is_ok(), "two tokens a second");
        assert!(l.admit_at(2, 2, false, t0).is_ok(), "another grant has its own budget");
    }

    #[test]
    fn writes_have_a_tighter_budget() {
        let l = Limits::new();
        let t0 = Instant::now();
        let mut n = 0;
        while let Ok(p) = l.admit_at(1, 1, true, t0) {
            drop(p);
            n += 1;
            assert!(n <= 100);
        }
        assert_eq!(n, 30);
        assert!(l.admit_at(1, 1, false, t0).is_ok(), "reads are unaffected");
    }

    #[test]
    fn at_most_four_run_at_once_per_grant() {
        let l = Limits::new();
        let t0 = Instant::now();
        let held: Vec<_> = (0..4).map(|_| l.admit_at(7, 7, false, t0).unwrap()).collect();
        assert!(matches!(l.admit_at(7, 7, false, t0), Err(Refusal::Busy)));
        assert!(l.admit_at(8, 8, false, t0).is_ok());
        drop(held);
        assert!(l.admit_at(7, 7, false, t0).is_ok());
    }

    #[test]
    fn the_hourly_ceiling_is_per_person() {
        let l = Limits::new();
        let t0 = Instant::now();
        for i in 0..3000u64 {
            // Spread over time so the per-grant bucket never limits.
            let at = t0 + Duration::from_millis(i * 600);
            drop(l.admit_at(1, 5, false, at).unwrap_or_else(|_| panic!("call {i} refused")));
        }
        let at = t0 + Duration::from_millis(3000 * 600);
        assert!(
            matches!(l.admit_at(2, 5, false, at), Err(Refusal::RateLimited(_))),
            "a second grant of the same person shares the ceiling"
        );
        assert!(l.admit_at(3, 6, false, at).is_ok());
    }
}
