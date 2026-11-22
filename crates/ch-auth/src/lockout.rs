//! Lockout after repeated failures (ARCHITECTURE.md section 7.6).
//!
//! 10 failed passwords or codes for one account within 15 minutes lock password sign-in for
//! that account for 15 minutes, doubling on each repeat up to 24 hours. A second, looser
//! counter keyed by address stops one address from spraying many accounts. The attempt is
//! reserved atomically before the password is checked, so a burst of simultaneous guesses
//! gets the same budget as one at a time. Tokens and SSH keys never read this table, so a
//! lockout cannot cut off git.
//!
//! The key for an account that does not exist is its lower case name, so a made-up username
//! locks the same way and the response never says which usernames exist.
//!
//! The account counter is kept per account AND client address (`acct:5:203.0.113.9`), never
//! per account alone. Someone with no account who knows a username can burn their own
//! address's budget for that account, but cannot lock the owner out from the owner's own
//! address (ch-rx2). Guessing stays slowed down per account and address, and by the looser
//! per-address counter across accounts. IPv6 addresses count by /64, because one customer
//! usually holds a whole /64 and could otherwise pick a fresh address for every guess.

use ch_core::error::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

pub const ACCOUNT_THRESHOLD: i32 = 10;
pub const ADDRESS_THRESHOLD: i32 = 40;
pub const WINDOW_MINUTES: i64 = 15;
const BASE_LOCK_MINUTES: i64 = 15;
const MAX_LOCK_HOURS: i64 = 24;

pub fn account_key(id: i64) -> String {
    format!("acct:{id}")
}
pub fn name_key(login: &str) -> String {
    let lower: String = login.to_lowercase().chars().take(100).collect();
    format!("name:{lower}")
}
pub fn ip_key(ip: std::net::IpAddr) -> String {
    format!("ip:{}", net_part(ip))
}
/// The failure counter for one account as seen from one client address.
pub fn account_ip_key(id: i64, ip: std::net::IpAddr) -> String {
    format!("{}:{}", account_key(id), net_part(ip))
}
/// The same for a username that does not exist, so a made-up name behaves like a real one.
pub fn name_ip_key(login: &str, ip: std::net::IpAddr) -> String {
    format!("{}:{}", name_key(login), net_part(ip))
}

/// The address a budget is counted against: the IPv4 address, or the /64 of an IPv6 one.
fn net_part(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

/// How long the lock lasts at a given level (0 is the first lock).
pub fn lock_duration(level: i32) -> Duration {
    let minutes = BASE_LOCK_MINUTES.saturating_mul(1i64 << level.clamp(0, 20));
    Duration::minutes(minutes.min(MAX_LOCK_HOURS * 60))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locked {
    pub until: DateTime<Utc>,
    pub retry_after_secs: u64,
}

/// The outcome of reserving one attempt with [`Lockout::begin_attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// The key is locked. The attempt is refused and not counted.
    Locked(Locked),
    /// The attempt is reserved and may reach the password hasher. `locked_now` is the lock
    /// this very attempt caused: the threshold-th attempt is still allowed to verify, but it
    /// locks the key on its way in so every concurrent later attempt is refused.
    Allowed { locked_now: Option<Locked> },
}

#[derive(Clone)]
pub struct Lockout {
    pool: PgPool,
}

#[derive(sqlx::FromRow)]
struct Row {
    fail_count: i32,
    window_start: DateTime<Utc>,
    locked_until: Option<DateTime<Utc>>,
    lock_level: i32,
    last_failure_at: DateTime<Utc>,
}

impl Lockout {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// `Some` while the key is locked.
    pub async fn check(&self, key: &str) -> Result<Option<Locked>> {
        let until: Option<Option<DateTime<Utc>>> =
            sqlx::query_scalar("SELECT locked_until FROM login_failures WHERE key = $1").bind(key).fetch_optional(&self.pool).await?;
        let now = Utc::now();
        Ok(until.flatten().filter(|u| *u > now).map(|u| Locked { until: u, retry_after_secs: (u - now).num_seconds().max(1) as u64 }))
    }

    /// Reserves one attempt atomically, BEFORE the password is checked. One transaction holds
    /// the row lock (the `record_failure` pattern), so however many callers run at once, at
    /// most `threshold` per lock cycle ever see [`Attempt::Allowed`]: the attempt that
    /// reaches the threshold sets the lock on its way in. The window, level and quiet-day
    /// rules are `record_failure`'s.
    pub async fn begin_attempt(&self, key: &str, threshold: i32) -> Result<Attempt> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO login_failures (key, fail_count, window_start, lock_level, last_failure_at) VALUES ($1, 0, now(), 0, now()) ON CONFLICT (key) DO NOTHING")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        let row: Row = sqlx::query_as(
            "SELECT fail_count, window_start, locked_until, lock_level, last_failure_at FROM login_failures WHERE key = $1 FOR UPDATE",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        let now = Utc::now();
        if let Some(until) = row.locked_until.filter(|u| *u > now) {
            tx.commit().await?;
            return Ok(Attempt::Locked(Locked { until, retry_after_secs: (until - now).num_seconds().max(1) as u64 }));
        }
        // Quiet for a day: forget earlier lockouts, so one bad week does not count forever.
        let level = if now - row.last_failure_at > Duration::hours(MAX_LOCK_HOURS) { 0 } else { row.lock_level };
        let (mut count, mut window_start) = (row.fail_count, row.window_start);
        if now - window_start > Duration::minutes(WINDOW_MINUTES) {
            count = 0;
            window_start = now;
        }
        count += 1;
        let locked_now = if count >= threshold {
            let until = now + lock_duration(level);
            sqlx::query("UPDATE login_failures SET fail_count = 0, window_start = $2, locked_until = $3, lock_level = $4, last_failure_at = $2 WHERE key = $1")
                .bind(key)
                .bind(now)
                .bind(until)
                .bind(level + 1)
                .execute(&mut *tx)
                .await?;
            Some(Locked { until, retry_after_secs: (until - now).num_seconds().max(1) as u64 })
        } else {
            sqlx::query(
                "UPDATE login_failures SET fail_count = $2, window_start = $3, lock_level = $4, last_failure_at = $5 WHERE key = $1",
            )
            .bind(key)
            .bind(count)
            .bind(window_start)
            .bind(level)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            None
        };
        tx.commit().await?;
        Ok(Attempt::Allowed { locked_now })
    }

    /// Counts one failure. Returns the new lock when this failure caused one. Failures while
    /// already locked are not counted, so the lock does not creep. The password entry points
    /// reserve with `begin_attempt` before the hash runs; this is for counters that count
    /// only after the fact, such as the per-session code attempts.
    pub async fn record_failure(&self, key: &str, threshold: i32) -> Result<Option<Locked>> {
        match self.begin_attempt(key, threshold).await? {
            Attempt::Allowed { locked_now } => Ok(locked_now),
            Attempt::Locked(_) => Ok(None),
        }
    }

    /// Gives one reservation back: a correct sign-in must not eat the address budget. Does
    /// nothing while the key is locked.
    pub async fn release(&self, key: &str) -> Result<()> {
        sqlx::query("UPDATE login_failures SET fail_count = GREATEST(fail_count - 1, 0) WHERE key = $1 AND (locked_until IS NULL OR locked_until <= now())")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A good sign-in clears the account's counters and its lock history.
    pub async fn record_success(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM login_failures WHERE key = $1").bind(key).execute(&self.pool).await?;
        Ok(())
    }

    /// Clears exact keys.
    pub async fn clear(&self, keys: &[String]) -> Result<()> {
        sqlx::query("DELETE FROM login_failures WHERE key = ANY($1)").bind(keys).execute(&self.pool).await?;
        Ok(())
    }

    /// `admin unlock`, a password reset: clears the account's counters for every client
    /// address, and its name counters too when given a name. Matches `key` itself and every
    /// `key:<address>` under it (a prefix test, not LIKE, so a name cannot act as a pattern).
    pub async fn clear_all_addresses(&self, base_keys: &[String]) -> Result<()> {
        sqlx::query(
            "DELETE FROM login_failures WHERE EXISTS (SELECT 1 FROM unnest($1::text[]) AS b WHERE key = b OR left(key, length(b) + 1) = b || ':')",
        )
        .bind(base_keys)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_durations_double_up_to_a_day() {
        assert_eq!(lock_duration(0), Duration::minutes(15));
        assert_eq!(lock_duration(1), Duration::minutes(30));
        assert_eq!(lock_duration(2), Duration::minutes(60));
        assert_eq!(lock_duration(5), Duration::minutes(480));
        assert_eq!(lock_duration(7), Duration::hours(24));
        assert_eq!(lock_duration(40), Duration::hours(24));
    }

    #[test]
    fn keys() {
        assert_eq!(account_key(5), "acct:5");
        assert_eq!(name_key("Alice"), "name:alice");
        assert_eq!(ip_key("10.1.2.3".parse().unwrap()), "ip:10.1.2.3");
        assert_eq!(name_key(&"x".repeat(500)).len(), 105);
        assert_eq!(account_ip_key(5, "10.1.2.3".parse().unwrap()), "acct:5:10.1.2.3");
        assert_eq!(account_ip_key(5, "::ffff:10.1.2.3".parse().unwrap()), "acct:5:10.1.2.3");
        assert_eq!(name_ip_key("Ghost", "10.1.2.3".parse().unwrap()), "name:ghost:10.1.2.3");
        assert_eq!(ip_key("2001:db8:1:2:aaaa::1".parse().unwrap()), ip_key("2001:db8:1:2::9".parse().unwrap()));
        assert_ne!(ip_key("2001:db8:1:3::1".parse().unwrap()), ip_key("2001:db8:1:2::1".parse().unwrap()));
    }

    #[sqlx::test(migrator = "ch_db::MIGRATOR")]
    async fn ten_failures_lock_and_the_eleventh_is_refused(pool: PgPool) {
        let l = Lockout::new(pool);
        for i in 1..ACCOUNT_THRESHOLD {
            assert!(l.record_failure("acct:1", ACCOUNT_THRESHOLD).await.unwrap().is_none(), "failure {i}");
            assert!(l.check("acct:1").await.unwrap().is_none());
        }
        let locked = l.record_failure("acct:1", ACCOUNT_THRESHOLD).await.unwrap().expect("tenth failure locks");
        assert!((14 * 60..=15 * 60).contains(&locked.retry_after_secs));
        assert!(l.check("acct:1").await.unwrap().is_some());
        assert!(l.check("acct:2").await.unwrap().is_none(), "other accounts are unaffected");
        // While locked, more failures do not extend the lock.
        assert!(l.record_failure("acct:1", ACCOUNT_THRESHOLD).await.unwrap().is_none());
    }

    #[sqlx::test(migrator = "ch_db::MIGRATOR")]
    async fn second_lock_is_longer_and_success_clears_everything(pool: PgPool) {
        let l = Lockout::new(pool.clone());
        for _ in 0..ACCOUNT_THRESHOLD {
            l.record_failure("acct:9", ACCOUNT_THRESHOLD).await.unwrap();
        }
        // Pretend the first lock has run out.
        sqlx::query("UPDATE login_failures SET locked_until = now() - interval '1 second' WHERE key = 'acct:9'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(l.check("acct:9").await.unwrap().is_none());
        let mut second = None;
        for _ in 0..ACCOUNT_THRESHOLD {
            second = l.record_failure("acct:9", ACCOUNT_THRESHOLD).await.unwrap();
        }
        let second = second.expect("locks again");
        assert!((29 * 60..=30 * 60).contains(&second.retry_after_secs), "doubled to 30 minutes, got {}", second.retry_after_secs);
        l.record_success("acct:9").await.unwrap();
        assert!(l.check("acct:9").await.unwrap().is_none());
    }

    #[sqlx::test(migrator = "ch_db::MIGRATOR")]
    async fn old_failures_age_out_of_the_window(pool: PgPool) {
        let l = Lockout::new(pool.clone());
        for _ in 0..ACCOUNT_THRESHOLD - 1 {
            l.record_failure("acct:3", ACCOUNT_THRESHOLD).await.unwrap();
        }
        sqlx::query("UPDATE login_failures SET window_start = now() - interval '16 minutes' WHERE key = 'acct:3'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(l.record_failure("acct:3", ACCOUNT_THRESHOLD).await.unwrap().is_none(), "window restarted, so this is failure 1 again");
    }

    #[sqlx::test(migrator = "ch_db::MIGRATOR")]
    async fn unlock_clears_keys(pool: PgPool) {
        let l = Lockout::new(pool);
        for _ in 0..ACCOUNT_THRESHOLD {
            l.record_failure("acct:4", ACCOUNT_THRESHOLD).await.unwrap();
        }
        assert!(l.check("acct:4").await.unwrap().is_some());
        l.clear(&["acct:4".into(), "name:x".into()]).await.unwrap();
        assert!(l.check("acct:4").await.unwrap().is_none());
    }

    #[sqlx::test(migrator = "ch_db::MIGRATOR")]
    async fn concurrent_begin_attempts_yield_exactly_the_budget(pool: PgPool) {
        let l = Lockout::new(pool);
        // Thirty reservations at once on one key: the row lock hands out exactly the
        // threshold, and the attempt that reaches the threshold locks the key for the rest.
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..30 {
            let l = l.clone();
            set.spawn(async move { l.begin_attempt("acct:77", 10).await.unwrap() });
        }
        let (mut allowed, mut locked) = (0, 0);
        while let Some(joined) = set.join_next().await {
            match joined.unwrap() {
                Attempt::Allowed { .. } => allowed += 1,
                Attempt::Locked(_) => locked += 1,
            }
        }
        assert_eq!(allowed, 10, "the budget is exactly the threshold");
        assert_eq!(locked, 20);
        assert!(l.check("acct:77").await.unwrap().is_some(), "the tenth reservation locked the key");
    }
}
