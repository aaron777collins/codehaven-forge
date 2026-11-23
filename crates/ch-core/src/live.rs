//! What signed a request in, and checking it again while a long response is still open.
//!
//! A log stream can stay open for many minutes. Checking the caller only when it opened would
//! let it outlive a sign-out, a suspension or lost repo access. The gate puts a `Credential` in
//! the request extensions. It holds no secret. `LiveAuth` re-resolves it without moving the
//! session's last-seen time or expiry, and `guarded` runs that check before every batch and on a
//! one second timer, so an idle stream and a chatty one both close within two seconds.

use crate::Permission;
use crate::error::Error;
use crate::ids::*;
use crate::state::AppState;
use axum::response::sse::Event;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, MissedTickBehavior};

/// The credential behind a request, with no secret in it.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// SHA-256 of the session cookie, as the sessions table stores it.
    Session {
        hash: Vec<u8>,
    },
    Token {
        token_id: TokenId,
    },
    Job {
        job_id: JobId,
    },
    Runner {
        runner_id: RunnerId,
    },
    /// Basic with a password: the user and a fingerprint of their password hash, so a password
    /// change ends it.
    Basic {
        user_id: UserId,
        fingerprint: Vec<u8>,
    },
    App {
        grant_id: GrantId,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hashes stay out of logs.
        match self {
            Credential::Session { .. } => f.write_str("Credential::Session"),
            Credential::Token { token_id } => write!(f, "Credential::Token({})", token_id.0),
            Credential::Job { job_id } => write!(f, "Credential::Job({})", job_id.0),
            Credential::Runner { runner_id } => write!(f, "Credential::Runner({})", runner_id.0),
            Credential::Basic { user_id, .. } => write!(f, "Credential::Basic({})", user_id.0),
            Credential::App { grant_id } => write!(f, "Credential::App({})", grant_id.0),
        }
    }
}

/// The answer of one live check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Still allowed.
    Open,
    /// Access ended: close the stream with nothing more.
    Closed,
    /// The check itself failed (a database error): close with a `retry` event. Reconnecting
    /// authenticates again.
    Retry,
}

/// Something a guarded stream asks before it sends more.
pub trait LiveCheck: Send + Sync {
    fn check(&self) -> BoxFuture<'_, Verdict>;
}

/// An extra condition, such as an off switch. `Ok(false)` closes the stream.
pub type ExtraCheck = Arc<dyn Fn(AppState) -> BoxFuture<'static, crate::Result<bool>> + Send + Sync>;

/// The standard live check: the credential still resolves, the person still reads the repo,
/// the job is still in that run in that repo, and any extra condition holds.
pub struct LiveAuth {
    state: AppState,
    credential: Credential,
    repo: RepoId,
    job: Option<(RunId, JobId)>,
    extra: Option<ExtraCheck>,
}

impl LiveAuth {
    pub fn new(state: AppState, credential: Credential, repo: RepoId) -> Self {
        Self { state, credential, repo, job: None, extra: None }
    }

    pub fn job(mut self, run: RunId, job: JobId) -> Self {
        self.job = Some((run, job));
        self
    }

    pub fn extra(mut self, check: ExtraCheck) -> Self {
        self.extra = Some(check);
        self
    }

    async fn run(&self) -> Verdict {
        let actor = match self.state.authn.recheck(&self.credential).await {
            Ok(Some(a)) => a,
            Ok(None) => return Verdict::Closed,
            Err(e) => return error_verdict(e),
        };
        match self.state.authz.require(&actor, self.repo, Permission::Read).await {
            Ok(()) => {}
            Err(e) => return error_verdict(e),
        }
        if let Some((run, job)) = self.job {
            let found: std::result::Result<Option<i64>, sqlx::Error> =
                sqlx::query_scalar("SELECT id FROM workflow_jobs WHERE id = $1 AND run_id = $2 AND repo_id = $3")
                    .bind(job.0)
                    .bind(run.0)
                    .bind(self.repo.0)
                    .fetch_optional(&self.state.db)
                    .await;
            match found {
                Ok(Some(_)) => {}
                Ok(None) => return Verdict::Closed,
                Err(_) => return Verdict::Retry,
            }
        }
        if let Some(extra) = &self.extra {
            match extra(self.state.clone()).await {
                Ok(true) => {}
                Ok(false) => return Verdict::Closed,
                Err(e) => return error_verdict(e),
            }
        }
        Verdict::Open
    }
}

fn error_verdict(e: Error) -> Verdict {
    match e {
        Error::NotFound | Error::Forbidden(_) | Error::Unauthorized(_) => Verdict::Closed,
        _ => Verdict::Retry,
    }
}

impl LiveCheck for LiveAuth {
    fn check(&self) -> BoxFuture<'_, Verdict> {
        Box::pin(self.run())
    }
}

/// The longest any guarded stream stays open. The browser reconnects after it.
pub const STREAM_CAP: Duration = Duration::from_secs(30 * 60);
/// How often an idle stream checks, and how stale a check may be before a batch goes out.
pub const RECHECK_EVERY: Duration = Duration::from_secs(1);

/// The event a stream ends with when its check could not run. The page reconnects.
pub fn retry_event() -> Event {
    Event::default().event("retry").data("{}")
}

struct Guard<S> {
    check: Arc<dyn LiveCheck>,
    inner: Pin<Box<S>>,
    tick: tokio::time::Interval,
    checked_at: Instant,
    deadline: Instant,
    done: bool,
}

/// Wraps a server-sent event stream so it ends when access does. See the module notes.
pub fn guarded<S>(check: Arc<dyn LiveCheck>, inner: S) -> impl Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static
where
    S: Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static,
{
    guarded_with(check, inner, STREAM_CAP)
}

/// `guarded` with a chosen cap, for tests.
pub fn guarded_with<S>(
    check: Arc<dyn LiveCheck>,
    inner: S,
    cap: Duration,
) -> impl Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static
where
    S: Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static,
{
    let now = Instant::now();
    let mut tick = tokio::time::interval_at(now + RECHECK_EVERY, RECHECK_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // `checked_at` starts in the past so the first batch is checked before it goes out.
    let g = Guard { check, inner: Box::pin(inner), tick, checked_at: now - RECHECK_EVERY * 2, deadline: now + cap, done: false };
    futures::stream::unfold(g, |mut g| async move {
        if g.done {
            return None;
        }
        loop {
            let deadline = g.deadline;
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return None,
                _ = g.tick.tick() => {
                    match g.check.check().await {
                        Verdict::Open => g.checked_at = Instant::now(),
                        Verdict::Closed => return None,
                        Verdict::Retry => {
                            g.done = true;
                            return Some((Ok(retry_event()), g));
                        }
                    }
                }
                // Polling an `unfold` stream again after this branch lost is safe: the stream
                // keeps its own pending future, so no event is lost or repeated.
                item = g.inner.next() => {
                    let item = item?;
                    if g.checked_at.elapsed() >= RECHECK_EVERY {
                        match g.check.check().await {
                            Verdict::Open => g.checked_at = Instant::now(),
                            Verdict::Closed => return None,
                            Verdict::Retry => {
                                g.done = true;
                                return Some((Ok(retry_event()), g));
                            }
                        }
                    }
                    return Some((item, g));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    /// 0 open, 1 closed, 2 retry.
    struct Switch(AtomicU8, AtomicUsize);
    impl LiveCheck for Switch {
        fn check(&self) -> BoxFuture<'_, Verdict> {
            self.1.fetch_add(1, Ordering::SeqCst);
            let v = match self.0.load(Ordering::SeqCst) {
                0 => Verdict::Open,
                1 => Verdict::Closed,
                _ => Verdict::Retry,
            };
            Box::pin(async move { v })
        }
    }

    fn switch() -> Arc<Switch> {
        Arc::new(Switch(AtomicU8::new(0), AtomicUsize::new(0)))
    }

    fn ev(n: usize) -> std::result::Result<Event, Infallible> {
        Ok(Event::default().data(n.to_string()))
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_stream_closes_within_two_seconds_of_losing_access() {
        let sw = switch();
        let mut s = Box::pin(guarded(sw.clone(), futures::stream::pending()));
        let start = Instant::now();
        let flip = sw.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3500)).await;
            flip.0.store(1, Ordering::SeqCst);
        });
        assert!(s.next().await.is_none());
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(3500) && took <= Duration::from_millis(5500), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_chatty_stream_is_checked_before_batches_and_closes_too() {
        let sw = switch();
        let inner = futures::stream::unfold(0usize, |n| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some((ev(n), n + 1))
        });
        let mut s = Box::pin(guarded(sw.clone(), inner));
        // The first batch is checked before it goes out.
        assert!(s.next().await.is_some());
        assert_eq!(sw.1.load(Ordering::SeqCst), 1);
        let start = Instant::now();
        sw.0.store(1, Ordering::SeqCst);
        let mut sent = 0;
        while s.next().await.is_some() {
            sent += 1;
        }
        assert!(start.elapsed() <= Duration::from_secs(2), "{:?}", start.elapsed());
        assert!(sent < 40, "{sent}");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_goes_out_when_the_first_check_fails() {
        let sw = switch();
        sw.0.store(1, Ordering::SeqCst);
        let mut s = Box::pin(guarded(sw, futures::stream::iter(vec![ev(1), ev(2)])));
        assert!(s.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_check_ends_with_retry() {
        let sw = switch();
        sw.0.store(2, Ordering::SeqCst);
        let mut s = Box::pin(guarded(sw, futures::stream::pending()));
        assert!(s.next().await.is_some(), "the retry event");
        assert!(s.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_cap_ends_every_stream() {
        let sw = switch();
        let mut s = Box::pin(guarded_with(sw, futures::stream::pending(), Duration::from_secs(10)));
        let start = Instant::now();
        assert!(s.next().await.is_none());
        assert!(start.elapsed() >= Duration::from_secs(10) && start.elapsed() < Duration::from_secs(11));
    }
}
