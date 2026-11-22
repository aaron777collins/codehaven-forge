//! A small Postgres job queue. Workers claim with `FOR UPDATE SKIP LOCKED`, so any number
//! of them can poll at once and each job runs once. A job whose lease expires goes back in
//! the queue (or fails for good after `max_attempts`).

use ch_core::{Error, Result};
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: String,
    pub payload: Value,
    pub run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub max_attempts: Option<i32>,
    /// At most one queued job per `(kind, dedupe_key)`. A second enqueue is a no-op.
    pub dedupe_key: Option<String>,
}

impl NewJob {
    pub fn new(kind: &str, payload: Value) -> Self {
        Self { kind: kind.into(), payload, run_at: None, max_attempts: None, dedupe_key: None }
    }
    pub fn dedupe(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }
    pub fn run_at(mut self, at: chrono::DateTime<chrono::Utc>) -> Self {
        self.run_at = Some(at);
        self
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
}

#[derive(Clone)]
pub struct JobQueue {
    pool: PgPool,
}

impl JobQueue {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueues in the caller's executor, so it can join their transaction. Returns the id,
    /// or `None` when a queued job with the same dedupe key already exists.
    pub async fn enqueue<'e, E: sqlx::PgExecutor<'e>>(executor: E, job: NewJob) -> Result<Option<i64>> {
        let id = sqlx::query_scalar!(
            "INSERT INTO background_jobs (kind, payload, run_at, max_attempts, dedupe_key)
             VALUES ($1, $2, COALESCE($3, now()), COALESCE($4, 5), $5)
             ON CONFLICT (kind, dedupe_key) WHERE state = 'queued' AND dedupe_key IS NOT NULL DO NOTHING
             RETURNING id",
            job.kind,
            job.payload,
            job.run_at,
            job.max_attempts,
            job.dedupe_key,
        )
        .fetch_optional(executor)
        .await?;
        Ok(id)
    }

    /// Takes the oldest due job of one of `kinds`, locked to `worker` for `lease`.
    pub async fn claim(&self, worker: &str, kinds: &[String], lease: Duration) -> Result<Option<Job>> {
        let row = sqlx::query!(
            r#"UPDATE background_jobs SET state = 'running', locked_by = $1,
                      locked_until = now() + make_interval(secs => $3), attempts = attempts + 1
               WHERE id = (
                   SELECT id FROM background_jobs
                   WHERE state = 'queued' AND run_at <= now() AND kind = ANY($2)
                   ORDER BY run_at, id
                   FOR UPDATE SKIP LOCKED
                   LIMIT 1)
               RETURNING id, kind, payload, attempts, max_attempts"#,
            worker,
            kinds,
            lease.as_secs_f64(),
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Job { id: r.id, kind: r.kind, payload: r.payload, attempts: r.attempts, max_attempts: r.max_attempts }))
    }

    pub async fn complete(&self, id: i64) -> Result<()> {
        let n = sqlx::query!("UPDATE background_jobs SET state = 'done', finished_at = now(), locked_by = NULL, locked_until = NULL WHERE id = $1 AND state = 'running'", id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Records a failure. The job is retried after `retry_in`, or fails for good once it
    /// has used its attempts.
    pub async fn fail(&self, id: i64, error: &str, retry_in: Duration) -> Result<()> {
        sqlx::query!(
            r#"UPDATE background_jobs SET
                 state = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'queued' END,
                 run_at = now() + make_interval(secs => $3),
                 last_error = left($2, 2000),
                 finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END,
                 locked_by = NULL, locked_until = NULL
               WHERE id = $1 AND state = 'running'"#,
            id,
            error,
            retry_in.as_secs_f64(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Puts jobs whose lease ran out back in the queue. Returns how many.
    pub async fn reap_expired(&self) -> Result<u64> {
        let n = sqlx::query!(
            r#"UPDATE background_jobs SET
                 state = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'queued' END,
                 last_error = 'lease expired',
                 finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END,
                 locked_by = NULL, locked_until = NULL
               WHERE state = 'running' AND locked_until < now()"#
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    /// Deletes finished jobs older than `age`.
    pub async fn purge_finished(&self, age: Duration) -> Result<u64> {
        let n = sqlx::query!(
            "DELETE FROM background_jobs WHERE state IN ('done', 'failed') AND finished_at < now() - make_interval(secs => $1)",
            age.as_secs_f64()
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn kinds() -> Vec<String> {
        vec!["t".to_string()]
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn claim_complete(pool: PgPool) {
        let q = JobQueue::new(pool.clone());
        let id = JobQueue::enqueue(&pool, NewJob::new("t", json!({"a": 1}))).await.unwrap().unwrap();
        let job = q.claim("w1", &kinds(), Duration::from_secs(30)).await.unwrap().unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.payload["a"], 1);
        assert_eq!(job.attempts, 1);
        assert!(q.claim("w2", &kinds(), Duration::from_secs(30)).await.unwrap().is_none());
        q.complete(id).await.unwrap();
        assert!(q.complete(id).await.is_err(), "completing twice is an error");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn only_claims_requested_kinds_and_due_jobs(pool: PgPool) {
        let q = JobQueue::new(pool.clone());
        JobQueue::enqueue(&pool, NewJob::new("other", json!({}))).await.unwrap();
        JobQueue::enqueue(&pool, NewJob::new("t", json!({})).run_at(chrono::Utc::now() + chrono::Duration::hours(1))).await.unwrap();
        assert!(q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn dedupe_key_collapses_queued_jobs(pool: PgPool) {
        let first = JobQueue::enqueue(&pool, NewJob::new("t", json!({})).dedupe("repo:1")).await.unwrap();
        let second = JobQueue::enqueue(&pool, NewJob::new("t", json!({})).dedupe("repo:1")).await.unwrap();
        let other = JobQueue::enqueue(&pool, NewJob::new("t", json!({})).dedupe("repo:2")).await.unwrap();
        assert!(first.is_some());
        assert!(second.is_none());
        assert!(other.is_some());
        // Once the first has been claimed, the key is free again.
        let q = JobQueue::new(pool.clone());
        let _ = q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().unwrap();
        assert!(JobQueue::enqueue(&pool, NewJob::new("t", json!({})).dedupe("repo:1")).await.unwrap().is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn failure_retries_then_gives_up(pool: PgPool) {
        let q = JobQueue::new(pool.clone());
        let mut j = NewJob::new("t", json!({}));
        j.max_attempts = Some(2);
        let id = JobQueue::enqueue(&pool, j).await.unwrap().unwrap();
        let a = q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().unwrap();
        q.fail(a.id, "boom", Duration::ZERO).await.unwrap();
        let b = q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().unwrap();
        assert_eq!(b.attempts, 2);
        q.fail(b.id, "boom again", Duration::ZERO).await.unwrap();
        assert!(q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().is_none());
        let state: String = sqlx::query_scalar("SELECT state FROM background_jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(state, "failed");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn retry_waits_for_backoff(pool: PgPool) {
        let q = JobQueue::new(pool.clone());
        JobQueue::enqueue(&pool, NewJob::new("t", json!({}))).await.unwrap();
        let a = q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().unwrap();
        q.fail(a.id, "boom", Duration::from_secs(3600)).await.unwrap();
        assert!(q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn expired_lease_is_reaped_and_requeued(pool: PgPool) {
        let q = JobQueue::new(pool.clone());
        JobQueue::enqueue(&pool, NewJob::new("t", json!({}))).await.unwrap();
        let a = q.claim("w", &kinds(), Duration::from_millis(50)).await.unwrap().unwrap();
        assert_eq!(q.reap_expired().await.unwrap(), 0, "lease still valid");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(q.reap_expired().await.unwrap(), 1);
        let b = q.claim("w2", &kinds(), Duration::from_secs(5)).await.unwrap().unwrap();
        assert_eq!(b.id, a.id);
        assert_eq!(b.attempts, 2);
    }

    // Many claimers poll at once. Every job must be taken exactly once.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn concurrent_claimers_never_share_a_job(pool: PgPool) {
        let q = JobQueue::new(pool.clone());
        for n in 0..60 {
            JobQueue::enqueue(&pool, NewJob::new("t", json!({ "n": n }))).await.unwrap();
        }
        let seen = Arc::new(Mutex::new(Vec::<i64>::new()));
        let mut handles = vec![];
        for w in 0..10 {
            let q = q.clone();
            let seen = seen.clone();
            handles.push(tokio::spawn(async move {
                while let Some(job) = q.claim(&format!("w{w}"), &kinds(), Duration::from_secs(30)).await.unwrap() {
                    seen.lock().await.push(job.id);
                    q.complete(job.id).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 60);
        assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 60, "a job was claimed twice");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn enqueue_joins_the_callers_transaction(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        JobQueue::enqueue(&mut *tx, NewJob::new("t", json!({}))).await.unwrap();
        tx.rollback().await.unwrap();
        let q = JobQueue::new(pool);
        assert!(q.claim("w", &kinds(), Duration::from_secs(5)).await.unwrap().is_none());
    }
}
