//! In-memory rate limits (ARCHITECTURE.md section 7.6). One Codehaven process serves an
//! instance, so a map in memory is enough, and a restart only gives everyone a fresh budget.
//!
//! `Limiter` is a token bucket: `burst` tokens, refilled at `per_minute / 60` a second.
//! `Quota` is a fixed window counter, for the 5,000 an hour API ceiling.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct Limiter {
    burst: f64,
    per_sec: f64,
    map: Mutex<HashMap<String, Bucket>>,
}

const MAX_KEYS: usize = 100_000;

impl Limiter {
    pub fn new(per_minute: u32, burst: u32) -> Self {
        Self { burst: burst.max(1) as f64, per_sec: per_minute as f64 / 60.0, map: Mutex::new(HashMap::new()) }
    }

    fn refill(b: &mut Bucket, now: Instant, burst: f64, per_sec: f64) {
        let dt = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + dt * per_sec).min(burst);
        b.last = now;
    }

    /// Takes one token. `Err(seconds)` says how long until one is free.
    pub fn hit(&self, key: &str) -> Result<(), u64> {
        self.hit_at(key, Instant::now())
    }

    pub(crate) fn hit_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > MAX_KEYS {
            // Under a flood of distinct keys, forget buckets that have refilled.
            let (burst, per_sec) = (self.burst, self.per_sec);
            map.retain(|_, b| {
                Self::refill(b, now, burst, per_sec);
                b.tokens < burst
            });
        }
        let b = map.entry(key.to_string()).or_insert(Bucket { tokens: self.burst, last: now });
        Self::refill(b, now, self.burst, self.per_sec);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / self.per_sec).ceil().max(1.0) as u64)
        }
    }

    /// True when the next `hit` would be refused. Takes nothing.
    pub fn is_limited(&self, key: &str) -> Option<u64> {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let b = map.get_mut(key)?;
        Self::refill(b, now, self.burst, self.per_sec);
        if b.tokens >= 1.0 { None } else { Some(((1.0 - b.tokens) / self.per_sec).ceil().max(1.0) as u64) }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaState {
    pub limit: u32,
    pub remaining: u32,
    /// Unix time the window ends.
    pub reset: i64,
}

pub struct Quota {
    limit: u32,
    window: Duration,
    map: Mutex<HashMap<String, (Instant, i64, u32)>>,
}

impl Quota {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self { limit, window, map: Mutex::new(HashMap::new()) }
    }

    /// Counts one call. `Err` carries the state with `remaining == 0`.
    pub fn hit(&self, key: &str) -> Result<QuotaState, QuotaState> {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > MAX_KEYS {
            let w = self.window;
            map.retain(|_, (start, _, _)| now.saturating_duration_since(*start) < w);
        }
        let entry = map.entry(key.to_string()).or_insert_with(|| (now, chrono::Utc::now().timestamp() + self.window.as_secs() as i64, 0));
        if now.saturating_duration_since(entry.0) >= self.window {
            *entry = (now, chrono::Utc::now().timestamp() + self.window.as_secs() as i64, 0);
        }
        if entry.2 >= self.limit {
            return Err(QuotaState { limit: self.limit, remaining: 0, reset: entry.1 });
        }
        entry.2 += 1;
        Ok(QuotaState { limit: self.limit, remaining: self.limit - entry.2, reset: entry.1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_refuse_then_refill() {
        let l = Limiter::new(10, 5);
        let t0 = Instant::now();
        for i in 0..5 {
            assert!(l.hit_at("ip", t0).is_ok(), "burst {i}");
        }
        let wait = l.hit_at("ip", t0).unwrap_err();
        assert!((1..=6).contains(&wait), "wait {wait}");
        assert!(l.hit_at("other", t0).is_ok(), "keys are separate");
        // 10 a minute is one every 6 seconds.
        assert!(l.hit_at("ip", t0 + Duration::from_secs(7)).is_ok());
        assert!(l.hit_at("ip", t0 + Duration::from_secs(7)).is_err());
    }

    #[test]
    fn is_limited_takes_nothing() {
        let l = Limiter::new(30, 2);
        assert!(l.is_limited("a").is_none());
        l.hit("a").unwrap();
        l.hit("a").unwrap();
        assert!(l.is_limited("a").is_some());
        assert!(l.is_limited("a").is_some());
    }

    #[test]
    fn quota_counts_down() {
        let q = Quota::new(3, Duration::from_secs(3600));
        assert_eq!(q.hit("t").unwrap().remaining, 2);
        assert_eq!(q.hit("t").unwrap().remaining, 1);
        assert_eq!(q.hit("t").unwrap().remaining, 0);
        let s = q.hit("t").unwrap_err();
        assert_eq!((s.limit, s.remaining), (3, 0));
        assert!(q.hit("u").is_ok());
    }
}
