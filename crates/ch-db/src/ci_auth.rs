//! Resolving the two CI credentials: a runner's and a job's.
//!
//! `ch-auth` calls these from `Authn::from_bearer` so that the CI stream and the identity stream
//! agree on where a runner credential and a job token live (`runners.credential_hash` and
//! `workflow_jobs.token_hash`), and on what each may do. A job token only works while its job is
//! running, only on its own repository, and never grants more than its job's `permissions:` allow
//! (stored in `workflow_jobs.token_permissions` when the job was created).

use crate::tokens::hash_token;
use ch_core::actor::JobPerms;
use ch_core::{JobId, Permission, RepoId, Result, RunnerId};
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
    let row: Option<PermRow> = sqlx::query_as(&format!(
        "SELECT j.id, j.repo_id, {FORK_FLAG} AS fork, {PERM_COLS}
         FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id JOIN repositories rp ON rp.id = j.repo_id
         WHERE j.token_hash = $1 AND j.status = 'in_progress' AND r.status <> 'completed'"
    ))
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (JobId(r.id), RepoId(r.repo_id), r.perms())))
}

/// A runtime token: the JWT the runner gives an action as `ACTIONS_RUNTIME_TOKEN`, which the
/// Actions toolkit reads its run and job ids from (ch-ctk.6). The runner signs it with HS256
/// and the sha256 of the job's own token as the key, so the server checks it against the hash it
/// already keeps and nothing new is stored. Like the job token it works only for its own job,
/// only while that job runs, and never past its `exp`.
pub async fn job_from_runtime_token(pool: &PgPool, token: &str) -> Result<Option<(JobId, RepoId, JobPerms)>> {
    let Some(claims) = runtime_claims(token) else { return Ok(None) };
    #[derive(sqlx::FromRow)]
    struct Row {
        key: Vec<u8>,
        #[sqlx(flatten)]
        perms: PermRow,
    }
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT j.token_hash AS key, j.id, j.repo_id, {FORK_FLAG} AS fork, {PERM_COLS}
         FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id JOIN repositories rp ON rp.id = j.repo_id
         WHERE j.id = $1 AND j.run_id = $2 AND j.status = 'in_progress' AND r.status <> 'completed' AND j.token_hash IS NOT NULL"
    ))
    .bind(claims.job)
    .bind(claims.run)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if !runtime_signature_ok(token, &row.key) {
        return Ok(None);
    }
    // Valid for the toolkit's cache and artifact services only: it carries no scope of the job
    // token, so it is refused everywhere else (statuses, issues, releases, git, packages).
    let fork = row.perms.perms().fork;
    Ok(Some((JobId(row.perms.id), RepoId(row.perms.repo_id), JobPerms::runtime_only(fork))))
}

struct RuntimeClaims {
    job: i64,
    run: i64,
}

/// The job and run a well-formed, unexpired runtime token names. Its signature is not checked
/// here.
fn runtime_claims(token: &str) -> Option<RuntimeClaims> {
    use base64::Engine;
    if token.len() > 4096 {
        return None;
    }
    let mut parts = token.split('.');
    let (h, p, sig, rest) = (parts.next()?, parts.next()?, parts.next()?, parts.next());
    if rest.is_some() || sig.is_empty() {
        return None;
    }
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header: serde_json::Value = serde_json::from_slice(&b64.decode(h).ok()?).ok()?;
    if header.get("alg")?.as_str()? != "HS256" {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_slice(&b64.decode(p).ok()?).ok()?;
    let exp = payload.get("exp")?.as_i64()?;
    if exp <= chrono::Utc::now().timestamp() {
        return None;
    }
    let job: i64 = payload.get("sub")?.as_str()?.strip_prefix("job:")?.parse().ok()?;
    // `Actions.Results:<run>:<job>`, the scope the toolkit reads.
    let scope = payload.get("scp")?.as_str()?.split(' ').find_map(|s| s.strip_prefix("Actions.Results:"))?.to_string();
    let (run, job2) = scope.split_once(':')?;
    let (run, job2): (i64, i64) = (run.parse().ok()?, job2.parse().ok()?);
    if job2 != job {
        return None;
    }
    Some(RuntimeClaims { job, run })
}

fn runtime_signature_ok(token: &str, key: &[u8]) -> bool {
    use base64::Engine;
    use hmac::Mac;
    let Some((signing_input, sig)) = token.rsplit_once('.') else { return false };
    let Ok(sig) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig) else { return false };
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(key) else { return false };
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&sig).is_ok()
}

/// The same answer for a job already identified by id (a live stream checking that its
/// credential still holds). Same conditions as `job_from_token`.
pub async fn job_by_id(pool: &PgPool, job: JobId) -> Result<Option<(RepoId, JobPerms)>> {
    let row: Option<PermRow> = sqlx::query_as(&format!(
        "SELECT j.id, j.repo_id, {FORK_FLAG} AS fork, {PERM_COLS}
         FROM workflow_jobs j JOIN workflow_runs r ON r.id = j.run_id JOIN repositories rp ON rp.id = j.repo_id
         WHERE j.id = $1 AND j.status = 'in_progress' AND r.status <> 'completed'"
    ))
    .bind(job.0)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (RepoId(r.repo_id), r.perms())))
}

/// Where ch-ci's `join_payload` keeps the fork flag: `{event, meta: {fork}}` (ch-x12). Reading a
/// top-level `fork` instead made every fork job a writer.
const FORK_FLAG: &str = "COALESCE((r.event_payload->'meta'->>'fork')::boolean, false)";

/// The stored token permissions, the repository's ceiling and the reserved bot account. The
/// ceiling is read on every use, so lowering it takes effect for jobs already running.
const PERM_COLS: &str = "j.token_permissions AS stored, rp.actions_token_ceiling AS ceiling, \
     (SELECT b.id FROM users b WHERE b.kind = 'bot' AND b.username = 'codehaven-actions') AS bot";

#[derive(sqlx::FromRow)]
struct PermRow {
    id: i64,
    repo_id: i64,
    fork: bool,
    stored: Option<serde_json::Value>,
    ceiling: String,
    bot: Option<i64>,
}

impl PermRow {
    fn perms(&self) -> JobPerms {
        job_perms(self.stored.as_ref(), &self.ceiling, self.fork, self.bot)
    }
}

/// What a job token may do. A job minted before tokens had permissions (no stored value) gets
/// the restricted default. Each stored scope is read, write or none, anything else is none; then
/// a `read` ceiling lowers every write, and a fork job reads contents at most and nothing else.
pub fn job_perms(stored: Option<&serde_json::Value>, ceiling: &str, fork: bool, bot: Option<i64>) -> JobPerms {
    let level = |scope: &str| -> Permission {
        let p = match stored.and_then(|v| v.get(scope)).and_then(|v| v.as_str()) {
            Some("write") => Permission::Write,
            Some("read") => Permission::Read,
            _ => Permission::None,
        };
        if ceiling == "write" { p } else { p.min(Permission::Read) }
    };
    let mut perms = match stored {
        None => JobPerms::default_token(fork),
        Some(_) => JobPerms {
            contents: level("contents"),
            packages: level("packages"),
            checks: level("checks"),
            statuses: level("statuses"),
            issues: level("issues"),
            pull_requests: level("pull-requests"),
            actions: level("actions").min(Permission::Read),
            fork,
            bot: None,
        },
    };
    if fork {
        perms = JobPerms { contents: perms.contents.min(Permission::Read), ..JobPerms::default_token(true) };
        perms.packages = Permission::None;
    }
    perms.bot = bot.map(ch_core::UserId);
    perms
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_permissions_are_read_and_capped() {
        let v = json!({"contents": "write", "issues": "write", "actions": "write", "statuses": "admin"});
        let p = job_perms(Some(&v), "write", false, Some(7));
        assert_eq!(p.contents, Permission::Write);
        assert_eq!(p.issues, Permission::Write);
        assert_eq!(p.actions, Permission::Read, "actions is read at most");
        assert_eq!(p.statuses, Permission::None, "an unknown level is none");
        assert_eq!(p.packages, Permission::None, "a scope not stored is none");
        assert_eq!(p.bot, Some(ch_core::UserId(7)));
        let capped = job_perms(Some(&v), "read", false, None);
        assert_eq!(capped.contents, Permission::Read);
        assert_eq!(capped.issues, Permission::Read);
    }

    fn mint(key: &[u8], payload: serde_json::Value) -> String {
        use base64::Engine;
        use hmac::Mac;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let input = format!("{}.{}", b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#), b64.encode(payload.to_string()));
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
        mac.update(input.as_bytes());
        format!("{input}.{}", b64.encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn runtime_tokens_are_read_and_checked() {
        let exp = chrono::Utc::now().timestamp() + 600;
        let good = json!({"sub": "job:7", "scp": "Actions.Results:3:7 Actions.UploadArtifacts", "exp": exp});
        let t = mint(b"k", good.clone());
        let c = runtime_claims(&t).map(|c| (c.job, c.run));
        assert_eq!(c, Some((7, 3)));
        assert!(runtime_signature_ok(&t, b"k"));
        assert!(!runtime_signature_ok(&t, b"other"), "another job's key");
        // Expired, a scope for another job, or no scope at all: not a token.
        assert!(runtime_claims(&mint(b"k", json!({"sub": "job:7", "scp": "Actions.Results:3:7", "exp": 1}))).is_none());
        assert!(runtime_claims(&mint(b"k", json!({"sub": "job:7", "scp": "Actions.Results:3:8", "exp": exp}))).is_none());
        assert!(runtime_claims(&mint(b"k", json!({"sub": "job:7", "exp": exp}))).is_none());
        assert!(runtime_claims("a.b").is_none() && runtime_claims("chj_x").is_none());
        // `none` instead of a signature algorithm is refused before anything is looked up.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let none = format!("{}.{}.x", b64.encode(br#"{"alg":"none"}"#), b64.encode(good.to_string()));
        assert!(runtime_claims(&none).is_none());
    }

    #[test]
    fn an_old_job_and_a_fork_get_the_least() {
        let old = job_perms(None, "write", false, None);
        assert_eq!((old.contents, old.packages, old.checks), (Permission::Read, Permission::Read, Permission::None));
        let fork = job_perms(Some(&json!({"contents": "write", "packages": "write"})), "write", true, None);
        assert_eq!((fork.contents, fork.packages, fork.runtime_write()), (Permission::Read, Permission::None, false));
    }
}
