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
    let row: Option<(i64, i64, bool)> = sqlx::query_as(&format!(
        "SELECT j.id, j.repo_id, {FORK_FLAG}
         FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id
         WHERE j.token_hash = $1 AND j.status = 'in_progress' AND r.status <> 'completed'"
    ))
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(job, repo, fork)| (JobId(job), RepoId(repo), job_perms(fork))))
}

/// The same answer for a job already identified by id (a live stream checking that its
/// credential still holds). Same conditions as `job_from_token`.
pub async fn job_by_id(pool: &PgPool, job: JobId) -> Result<Option<(RepoId, JobPerms)>> {
    let row: Option<(i64, bool)> = sqlx::query_as(&format!(
        "SELECT j.repo_id, {FORK_FLAG}
         FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id
         WHERE j.id = $1 AND j.status = 'in_progress' AND r.status <> 'completed'"
    ))
    .bind(job.0)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(repo, fork)| (RepoId(repo), job_perms(fork))))
}

/// Where ch-ci's `join_payload` keeps the fork flag: `{event, meta: {fork}}` (ch-x12). Reading a
/// top-level `fork` instead made every fork job a writer.
const FORK_FLAG: &str = "COALESCE((r.event_payload->'meta'->>'fork')::boolean, false)";

fn job_perms(fork: bool) -> JobPerms {
    JobPerms { contents_read: true, write_checks: !fork }
}
