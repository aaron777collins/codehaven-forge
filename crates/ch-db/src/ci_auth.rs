//! Resolving the two CI credentials: a runner's and a job's.
//!
//! `ch-auth` calls these from `Authn::from_bearer` so that the CI stream and the identity stream
//! agree on where a runner credential and a job token live (`runners.credential_hash` and
//! `workflow_jobs.token_hash`), and on what each may do. A job token only works while its job is
//! running, and never grants more than reading contents and writing checks for its own repository.

use crate::tokens::hash_token;
use ch_core::actor::JobPerms;
use ch_core::{JobId, RepoId, Result, RunnerId};
use sqlx::PgPool;

pub const RUNNER_REGISTRATION_PREFIX: &str = "chrt_";
pub const RUNNER_CREDENTIAL_PREFIX: &str = "chr_";
pub const JOB_TOKEN_PREFIX: &str = "chj_";

/// The runner this credential belongs to, when it exists and has not been revoked. Notes the
/// time, so the runners page can show when each last called in.
pub async fn runner_from_credential(pool: &PgPool, token: &str) -> Result<Option<RunnerId>> {
    if !token.starts_with(RUNNER_CREDENTIAL_PREFIX) {
        return Ok(None);
    }
    let id: Option<i64> =
        sqlx::query_scalar("UPDATE runners SET last_seen_at = now() WHERE credential_hash = $1 AND revoked_at IS NULL RETURNING id")
            .bind(hash_token(token))
            .fetch_optional(pool)
            .await?;
    Ok(id.map(RunnerId))
}

/// The job this token was minted for, while that job is running. A pull request from a fork gets
/// a token that cannot write checks, artifacts or caches.
pub async fn job_from_token(pool: &PgPool, token: &str) -> Result<Option<(JobId, RepoId, JobPerms)>> {
    if !token.starts_with(JOB_TOKEN_PREFIX) {
        return Ok(None);
    }
    let row: Option<(i64, i64, bool)> = sqlx::query_as(
        "SELECT j.id, j.repo_id, COALESCE((r.event_payload->'meta'->>'fork')::boolean, false)
         FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id
         WHERE j.token_hash = $1 AND j.status = 'in_progress' AND r.status <> 'completed'",
    )
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(job, repo, fork)| (JobId(job), RepoId(repo), JobPerms { contents_read: true, write_checks: !fork })))
}
