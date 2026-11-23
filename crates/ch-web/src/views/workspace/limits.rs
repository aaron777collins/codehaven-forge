//! Per-account limits for the workspace routes, in memory (spec section 5, Limits).
//!
//! Each kind of work has a rate per minute, a number that may run at once per account and,
//! for the expensive ones, a number that may run at once across the whole server. A permit
//! frees its slots when dropped, so a cancelled request never leaks one.

use ch_core::UserId;
use ch_core::state::AppState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Search,
    Snapshot,
    Commit,
    Workflows,
    Stream,
}

#[derive(Debug, Clone, Copy)]
struct Rule {
    per_minute: u32,
    at_once: u32,
    global: u32,
}

fn rule(k: Kind) -> Rule {
    match k {
        Kind::Search => Rule { per_minute: 30, at_once: 1, global: 4 },
        Kind::Snapshot => Rule { per_minute: 10, at_once: 1, global: 4 },
        Kind::Commit => Rule { per_minute: 30, at_once: 1, global: u32::MAX },
        Kind::Workflows => Rule { per_minute: 30, at_once: 2, global: u32::MAX },
        Kind::Stream => Rule { per_minute: u32::MAX, at_once: 4, global: u32::MAX },
    }
}

/// Bytes one account may commit from the browser in an hour.
pub const COMMIT_BYTES_PER_HOUR: u64 = 100 * 1024 * 1024;

#[derive(Default)]
struct State {
    started: HashMap<(i64, Kind), Vec<Instant>>,
    running: HashMap<(i64, Kind), u32>,
    global: HashMap<Kind, u32>,
    bytes: HashMap<i64, Vec<(Instant, u64)>>,
}

#[derive(Default, Clone)]
pub struct Limits(Arc<Mutex<State>>);

/// Why a request was turned away. The caller answers 429 with `rate_limited`.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Too many in the last minute: try again after this many seconds.
    Rate(u64),
    /// Too many running now, for this account or for the server.
    Busy,
}

pub struct Permit {
    limits: Limits,
    key: (i64, Kind),
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut s = self.limits.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = s.running.get_mut(&self.key) {
            *n = n.saturating_sub(1);
        }
        if let Some(n) = s.global.get_mut(&self.key.1) {
            *n = n.saturating_sub(1);
        }
    }
}

impl Limits {
    /// The limits of this server, made on first use.
    pub fn of(state: &AppState) -> Limits {
        static INIT: Mutex<()> = Mutex::new(());
        if let Some(l) = state.ext.get::<Limits>() {
            return (*l).clone();
        }
        let _g = INIT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = state.ext.get::<Limits>() {
            return (*l).clone();
        }
        let l = Limits::default();
        state.ext.insert(Arc::new(l.clone()));
        l
    }

    pub fn enter(&self, user: UserId, kind: Kind) -> Result<Permit, Refused> {
        self.enter_at(user, kind, Instant::now())
    }

    fn enter_at(&self, user: UserId, kind: Kind, now: Instant) -> Result<Permit, Refused> {
        let r = rule(kind);
        let key = (user.0, kind);
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let minute = Duration::from_secs(60);
        let times = s.started.entry(key).or_default();
        times.retain(|t| now.duration_since(*t) < minute);
        if r.per_minute != u32::MAX && times.len() as u32 >= r.per_minute {
            let oldest = times.first().copied().unwrap_or(now);
            return Err(Refused::Rate(minute.saturating_sub(now.duration_since(oldest)).as_secs().max(1)));
        }
        if *s.running.get(&key).unwrap_or(&0) >= r.at_once || *s.global.get(&kind).unwrap_or(&0) >= r.global {
            return Err(Refused::Busy);
        }
        if r.per_minute != u32::MAX {
            s.started.entry(key).or_default().push(now);
        }
        *s.running.entry(key).or_default() += 1;
        *s.global.entry(kind).or_default() += 1;
        Ok(Permit { limits: self.clone(), key })
    }

    /// Counts `n` bytes against the hourly commit budget, or refuses without counting.
    pub fn charge_commit_bytes(&self, user: UserId, n: u64) -> Result<(), Refused> {
        let now = Instant::now();
        let hour = Duration::from_secs(3600);
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let v = s.bytes.entry(user.0).or_default();
        v.retain(|(t, _)| now.duration_since(*t) < hour);
        let used: u64 = v.iter().map(|(_, b)| *b).sum();
        if used.saturating_add(n) > COMMIT_BYTES_PER_HOUR {
            return Err(Refused::Rate(600));
        }
        v.push((now, n));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_search_at_once_thirty_a_minute_four_on_the_server() {
        let l = Limits::default();
        let t = Instant::now();
        let p = l.enter_at(UserId(1), Kind::Search, t).unwrap();
        assert_eq!(l.enter_at(UserId(1), Kind::Search, t).err(), Some(Refused::Busy), "one at once");
        drop(p);
        for _ in 1..30 {
            drop(l.enter_at(UserId(1), Kind::Search, t).unwrap());
        }
        assert!(matches!(l.enter_at(UserId(1), Kind::Search, t), Err(Refused::Rate(_))), "thirty a minute");
        assert!(l.enter_at(UserId(1), Kind::Search, t + Duration::from_secs(61)).is_ok(), "the window moves");
        let held: Vec<Permit> = (2..6).map(|u| l.enter_at(UserId(u), Kind::Search, t).unwrap()).collect();
        assert_eq!(l.enter_at(UserId(9), Kind::Search, t).err(), Some(Refused::Busy), "four on the server");
        drop(held);
        assert!(l.enter_at(UserId(9), Kind::Search, t).is_ok());
    }

    #[test]
    fn four_streams_and_a_byte_budget() {
        let l = Limits::default();
        let held: Vec<Permit> = (0..4).map(|_| l.enter(UserId(1), Kind::Stream).unwrap()).collect();
        assert_eq!(l.enter(UserId(1), Kind::Stream).err(), Some(Refused::Busy));
        drop(held);
        assert!(l.enter(UserId(1), Kind::Stream).is_ok(), "freed on drop");
        assert!(l.charge_commit_bytes(UserId(1), COMMIT_BYTES_PER_HOUR).is_ok());
        assert!(l.charge_commit_bytes(UserId(1), 1).is_err());
        assert!(l.charge_commit_bytes(UserId(2), 1).is_ok(), "per account");
    }
}
