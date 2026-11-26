//! Deployment environments (ch-bj5).
//!
//! A job that names `environment:` waits for what that environment of the repository asks:
//!
//! * a **branch filter**: only runs of matching branches may deploy (a pattern `tags/v*` is how a
//!   tag is allowed, and a plain pattern never matches a tag). A pull request from
//!   a fork never matches a filter.
//! * **required reviewers**: someone on the list, with Write on the repository, approves or
//!   rejects the job. One of them is enough.
//! * a **wait timer**: minutes counted from the moment the job reaches the environment.
//!
//! The environment's **secrets** go to the job only when all of that is settled, and never to a
//! pull request from a fork. They are sealed like repository secrets and read back by nobody.
//!
//! The job stays `waiting` while it is held. [`check_gate`] is asked each time the run is
//! advanced, so a decision or a timer running out only has to advance the run again.

use crate::ctx::Ctx;
use crate::runs::{JobRow, RunRow, advance_run};
use crate::spec::RunMeta;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::ClientInfo;
use ch_core::{Actor, AuthVia, Error, FieldError, Permission, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_REVIEWERS: usize = 6;
pub const MAX_BRANCH_PATTERNS: usize = 20;
pub const MAX_ENVIRONMENTS: i64 = 100;
pub const MAX_ENV_SECRETS: i64 = 100;
pub const MAX_WAIT_MINUTES: i32 = 43_200;
const MAX_COMMENT: usize = 1000;

/// What a held job is told.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Nothing holds it.
    Open,
    /// Waiting for a reviewer or for the timer.
    Hold,
    /// It may not run, and the reason goes into its log.
    Fail(String),
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EnvRow {
    pub id: i64,
    pub name: String,
    pub wait_minutes: i32,
    pub branches: Vec<String>,
}

/// Looks an environment up by name, ignoring case. The column is CITEXT, but a bound text
/// parameter makes Postgres compare as text, so every name comparison casts the parameter
/// (ch-e7b): `environment: STG` must meet the gate of `stg`.
pub(crate) async fn find_in(conn: &mut PgConnection, repo: i64, name: &str) -> Result<Option<EnvRow>> {
    Ok(sqlx::query_as("SELECT id, name::text, wait_minutes, branches FROM environments WHERE repo_id = $1 AND name = $2::citext")
        .bind(repo)
        .bind(name)
        .fetch_optional(conn)
        .await?)
}

/// The branch or tag a run deploys, as the branch filter sees it, and whether it is a tag. A pull
/// request run has no branch of its own, so it uses the pull request's head branch. Only
/// `refs/tags/` is a tag: anything else is a branch, so a tag can never pass for one.
fn deploy_name(run: &RunRow) -> (String, bool) {
    let r = run.git_ref.as_deref().unwrap_or_default();
    if let Some(t) = r.strip_prefix("refs/tags/") {
        (t.to_string(), true)
    } else if let Some(b) = r.strip_prefix("refs/heads/") {
        (b.to_string(), false)
    } else {
        (run.head_branch.clone().unwrap_or_default(), false)
    }
}

/// The patterns that apply to one kind of ref. A pattern that starts with `tags/` (after an
/// optional `!`) is a tag pattern and applies to tags only, with the prefix removed. Every
/// other pattern is a branch pattern and applies to branches only. A branch named `tags/x`
/// therefore cannot be matched by a filter: nothing deploys by a name that looks like a tag.
fn patterns_for(patterns: &[String], tag: bool) -> Vec<String> {
    patterns
        .iter()
        .filter_map(|p| {
            let (neg, body) = p.strip_prefix('!').map_or(("", p.as_str()), |b| ("!", b));
            match (tag, body.strip_prefix("tags/")) {
                (true, Some(rest)) => Some(format!("{neg}{rest}")),
                (false, None) => Some(p.clone()),
                _ => None,
            }
        })
        .collect()
}

/// Whether a run may deploy to an environment by its branch filter. An empty filter lets every run
/// through, a fork's pull request included (it still waits for reviewers, and gets no secrets).
/// A filter matches branches only, by their plain name (`main`, `release/*`), and tags only
/// through a `tags/` pattern (`tags/v*`). A kind with no pattern of its own matches nothing. A
/// filter lets no fork pull request through.
pub fn branch_allowed(patterns: &[String], name: &str, tag: bool, fork: bool) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let own = patterns_for(patterns, tag);
    !fork && !own.is_empty() && ch_workflow::glob::filter(&own, name)
}

/// Decides what happens to a job that reached its environment. Creates the gate on the first
/// call. Called inside the run's own transaction from `advance_run`.
pub(crate) async fn check_gate(conn: &mut PgConnection, run: &RunRow, meta: &RunMeta, job: &JobRow, env_name: &str) -> Result<Gate> {
    let Some(env) = find_in(conn, run.repo_id, env_name).await? else { return Ok(Gate::Open) };
    let (name, tag) = deploy_name(run);
    if !branch_allowed(&env.branches, &name, tag, meta.fork) {
        let why = if meta.fork {
            "a pull request from a fork cannot deploy to it".to_string()
        } else if tag {
            format!("the tag `{name}` is not allowed (tags need a `tags/` pattern in its filter)")
        } else {
            format!("`{name}` is not one of the branches it allows")
        };
        return Ok(Gate::Fail(format!("The environment {} does not accept this run: {why}.", env.name)));
    }
    let row: Option<(String, bool)> = sqlx::query_as("SELECT state, release_at <= now() FROM deployment_gates WHERE job_id = $1")
        .bind(job.id)
        .fetch_optional(&mut *conn)
        .await?;
    let (state, released) = match row {
        Some(r) => r,
        None => {
            let reviewers: i64 = sqlx::query_scalar("SELECT count(*) FROM environment_reviewers WHERE environment_id = $1")
                .bind(env.id)
                .fetch_one(&mut *conn)
                .await?;
            let state = if reviewers > 0 { "pending" } else { "approved" };
            let released: bool = sqlx::query_scalar(
                "INSERT INTO deployment_gates (job_id, environment_id, state, release_at)
                 VALUES ($1, $2, $3, now() + make_interval(mins => $4)) ON CONFLICT (job_id) DO UPDATE SET job_id = EXCLUDED.job_id
                 RETURNING release_at <= now()",
            )
            .bind(job.id)
            .bind(env.id)
            .bind(state)
            .bind(env.wait_minutes)
            .fetch_one(&mut *conn)
            .await?;
            (state.to_string(), released)
        }
    };
    Ok(match state.as_str() {
        "rejected" => Gate::Fail(format!("The deployment to {} was rejected.", env.name)),
        "approved" if released => Gate::Open,
        _ => Gate::Hold,
    })
}

/// Runs whose jobs sat out a wait timer that has now run out. Their runs are advanced again, so
/// the job starts. Returns how many runs were advanced.
pub async fn release_due(ctx: &Ctx) -> Result<usize> {
    let runs: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT j.run_id FROM deployment_gates g JOIN workflow_jobs j ON j.id = g.job_id
          WHERE g.state = 'approved' AND g.release_at <= now() AND j.status = 'waiting'",
    )
    .fetch_all(&ctx.db)
    .await?;
    for r in &runs {
        advance_run(ctx, *r).await?;
    }
    Ok(runs.len())
}

fn aad(env: i64, name: &str) -> Vec<u8> {
    format!("ci-env-secret:{env}:{name}").into_bytes()
}

/// The environment's secrets a job may read: those it names (all, when it reads the whole `secrets`
/// object), and only when the job is past its gate. Never for a pull request from a fork.
pub async fn secrets_for_job(
    ctx: &Ctx,
    job: &JobRow,
    env_name: &str,
    names: &BTreeSet<String>,
    all: bool,
    fork: bool,
) -> Result<BTreeMap<String, String>> {
    if fork || (!all && names.is_empty()) {
        return Ok(BTreeMap::new());
    }
    let mut conn = ctx.db.acquire().await?;
    let Some(env) = find_in(&mut conn, job.repo_id, env_name).await? else { return Ok(BTreeMap::new()) };
    let gate: Option<String> =
        sqlx::query_scalar("SELECT state FROM deployment_gates WHERE job_id = $1").bind(job.id).fetch_optional(&mut *conn).await?;
    let guarded: bool = env.wait_minutes > 0
        || sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM environment_reviewers WHERE environment_id = $1)")
            .bind(env.id)
            .fetch_one(&mut *conn)
            .await?;
    // A protected environment hands its secrets over only behind an approved gate.
    if gate.as_deref().is_some_and(|s| s != "approved") || (guarded && gate.is_none()) {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT name, value_sealed FROM environment_secrets WHERE environment_id = $1")
        .bind(env.id)
        .fetch_all(&mut *conn)
        .await?;
    let mut out = BTreeMap::new();
    for (name, sealed) in rows {
        if !all && !names.contains(&name) {
            continue;
        }
        let plain = ctx.key()?.open(&sealed, &aad(env.id, &name))?;
        let value = String::from_utf8(plain).map_err(|_| Error::internal(anyhow::anyhow!("secret {name} is not text")))?;
        out.insert(name, value);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------- settings

/// One environment as the settings page shows it. No secret values.
#[derive(Debug, Clone)]
pub struct EnvironmentView {
    pub name: String,
    pub wait_minutes: i32,
    pub branches: Vec<String>,
    pub reviewers: Vec<(UserId, String)>,
    pub secrets: Vec<(String, DateTime<Utc>)>,
}

fn invalid(field: &str, msg: impl Into<String>) -> Error {
    Error::Validation(vec![FieldError::custom("Environment", field, msg.into())])
}

pub fn validate_name(name: &str) -> Result<()> {
    if ch_workflow::parse::valid_environment_name(name) {
        Ok(())
    } else {
        Err(invalid(
            "name",
            "A name starts with a letter or digit and holds at most 100 letters, digits, spaces, dots, dashes and underscores.",
        ))
    }
}

pub async fn list(ctx: &Ctx, repo: RepoId) -> Result<Vec<EnvironmentView>> {
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name::text FROM environments WHERE repo_id = $1 ORDER BY name").bind(repo.0).fetch_all(&ctx.db).await?;
    let mut out = Vec::new();
    for n in names {
        out.push(get(ctx, repo, &n).await?);
    }
    Ok(out)
}

pub async fn get(ctx: &Ctx, repo: RepoId, name: &str) -> Result<EnvironmentView> {
    let mut conn = ctx.db.acquire().await?;
    let env = find_in(&mut conn, repo.0, name).await?.ok_or(Error::NotFound)?;
    let reviewers: Vec<(i64, String)> = sqlx::query_as(
        "SELECT u.id, u.username::text FROM environment_reviewers r JOIN users u ON u.id = r.user_id WHERE r.environment_id = $1 ORDER BY u.username",
    )
    .bind(env.id)
    .fetch_all(&mut *conn)
    .await?;
    let secrets: Vec<(String, DateTime<Utc>)> =
        sqlx::query_as("SELECT name, updated_at FROM environment_secrets WHERE environment_id = $1 ORDER BY name")
            .bind(env.id)
            .fetch_all(&mut *conn)
            .await?;
    Ok(EnvironmentView {
        name: env.name,
        wait_minutes: env.wait_minutes,
        branches: env.branches,
        reviewers: reviewers.into_iter().map(|(i, n)| (UserId(i), n)).collect(),
        secrets,
    })
}

/// Creates the environment or replaces its rules. Secrets are left as they are.
pub async fn save(ctx: &Ctx, repo: RepoId, name: &str, wait_minutes: i32, branches: &[String], reviewers: &[UserId]) -> Result<bool> {
    validate_name(name)?;
    if !(0..=MAX_WAIT_MINUTES).contains(&wait_minutes) {
        return Err(invalid("wait_minutes", format!("The wait timer is between 0 and {MAX_WAIT_MINUTES} minutes.")));
    }
    let branches: Vec<String> = branches.iter().map(|b| b.trim().to_string()).filter(|b| !b.is_empty()).collect();
    if branches.len() > MAX_BRANCH_PATTERNS {
        return Err(invalid("branches", format!("At most {MAX_BRANCH_PATTERNS} branch patterns are allowed.")));
    }
    for b in &branches {
        if b.len() > 200 || b.chars().any(char::is_control) {
            return Err(invalid("branches", "A branch pattern is at most 200 characters, with no control characters."));
        }
        ch_workflow::glob::check(b).map_err(|e| invalid("branches", format!("`{b}`: {e}")))?;
    }
    let mut ids: Vec<i64> = reviewers.iter().map(|u| u.0).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() > MAX_REVIEWERS {
        return Err(invalid("reviewers", format!("At most {MAX_REVIEWERS} required reviewers are allowed.")));
    }
    // A reviewer must be able to write to the repository now: someone who cannot decides nothing.
    for id in &ids {
        let who = Actor::User { id: UserId(*id), is_admin: false, has_2fa: false, via: AuthVia::Session };
        if ctx.authz.repo_permission(&who, repo).await? < Permission::Write {
            return Err(invalid("reviewers", "A required reviewer needs write access to the repository."));
        }
    }
    let mut tx = ctx.db.begin().await?;
    let existing = find_in(&mut tx, repo.0, name).await?;
    if existing.is_none() {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM environments WHERE repo_id = $1").bind(repo.0).fetch_one(&mut *tx).await?;
        if n >= MAX_ENVIRONMENTS {
            return Err(invalid("name", format!("At most {MAX_ENVIRONMENTS} environments are allowed.")));
        }
    }
    // The stored name keeps its first spelling; the lookup ignores case.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO environments (repo_id, name, wait_minutes, branches) VALUES ($1, $2, $3, $4)
         ON CONFLICT (repo_id, name) DO UPDATE SET wait_minutes = EXCLUDED.wait_minutes, branches = EXCLUDED.branches, updated_at = now()
         RETURNING id",
    )
    .bind(repo.0)
    .bind(name)
    .bind(wait_minutes)
    .bind(&branches)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM environment_reviewers WHERE environment_id = $1").bind(id).execute(&mut *tx).await?;
    for u in &ids {
        sqlx::query("INSERT INTO environment_reviewers (environment_id, user_id) VALUES ($1, $2)")
            .bind(id)
            .bind(u)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(existing.is_none())
}

/// Creates an environment with no rules. A name that is already taken (in any case) is refused
/// and the existing environment is left exactly as it is: it must never be a way to reset one.
pub async fn create(ctx: &Ctx, repo: RepoId, name: &str) -> Result<()> {
    validate_name(name)?;
    let mut tx = ctx.db.begin().await?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM environments WHERE repo_id = $1").bind(repo.0).fetch_one(&mut *tx).await?;
    if n >= MAX_ENVIRONMENTS {
        return Err(invalid("name", format!("At most {MAX_ENVIRONMENTS} environments are allowed.")));
    }
    // The name column ignores case, so the conflict check does too.
    let made: Option<i64> =
        sqlx::query_scalar("INSERT INTO environments (repo_id, name) VALUES ($1, $2) ON CONFLICT (repo_id, name) DO NOTHING RETURNING id")
            .bind(repo.0)
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
    if made.is_none() {
        return Err(invalid("name", "An environment with that name already exists. Open it to change its rules."));
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete(ctx: &Ctx, repo: RepoId, name: &str) -> Result<()> {
    let r =
        sqlx::query("DELETE FROM environments WHERE repo_id = $1 AND name = $2::citext").bind(repo.0).bind(name).execute(&ctx.db).await?;
    if r.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn put_secret(ctx: &Ctx, repo: RepoId, env_name: &str, name: &str, value: &str, by: Option<UserId>) -> Result<()> {
    crate::secrets::validate_name(name)?;
    if value.len() > crate::secrets::MAX_SECRET_BYTES {
        return Err(Error::invalid("Secret", "value", "A secret may hold at most 64 KiB."));
    }
    let mut conn = ctx.db.acquire().await?;
    let env = find_in(&mut conn, repo.0, env_name).await?.ok_or(Error::NotFound)?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM environment_secrets WHERE environment_id = $1 AND name <> $2")
        .bind(env.id)
        .bind(name)
        .fetch_one(&mut *conn)
        .await?;
    if n >= MAX_ENV_SECRETS {
        return Err(Error::invalid("Secret", "name", format!("At most {MAX_ENV_SECRETS} secrets are allowed in an environment.")));
    }
    let sealed = ctx.key()?.seal(value.as_bytes(), &aad(env.id, name))?;
    sqlx::query(
        "INSERT INTO environment_secrets (environment_id, name, value_sealed, created_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (environment_id, name) DO UPDATE SET value_sealed = EXCLUDED.value_sealed, updated_at = now()",
    )
    .bind(env.id)
    .bind(name)
    .bind(&sealed)
    .bind(by.map(|u| u.0))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn delete_secret(ctx: &Ctx, repo: RepoId, env_name: &str, name: &str) -> Result<()> {
    let r = sqlx::query(
        "DELETE FROM environment_secrets s USING environments e WHERE s.environment_id = e.id AND e.repo_id = $1 AND e.name = $2::citext AND s.name = $3",
    )
    .bind(repo.0)
    .bind(env_name)
    .bind(name)
    .execute(&ctx.db)
    .await?;
    if r.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Who could be asked to review: people with Write on the repository who are its owner, its
/// collaborators, or members of the owning organization or of a team given the repository.
pub async fn reviewer_candidates(ctx: &Ctx, repo: RepoId) -> Result<Vec<(UserId, String)>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT DISTINCT u.id, u.username::text FROM users u
          WHERE u.kind = 'user' AND u.suspended_at IS NULL AND u.id IN (
                SELECT r.owner_id FROM repositories r WHERE r.id = $1
                UNION SELECT c.user_id FROM repo_collaborators c WHERE c.repo_id = $1
                UNION SELECT m.user_id FROM repositories r JOIN org_members m ON m.org_id = r.owner_id WHERE r.id = $1
                UNION SELECT tm.user_id FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id WHERE tr.repo_id = $1)
          ORDER BY 2 LIMIT 300",
    )
    .bind(repo.0)
    .fetch_all(&ctx.db)
    .await?;
    let mut out = Vec::new();
    for (id, name) in rows {
        let who = Actor::User { id: UserId(id), is_admin: false, has_2fa: false, via: AuthVia::Session };
        if ctx.authz.repo_permission(&who, repo).await? >= Permission::Write {
            out.push((UserId(id), name));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------- review

/// A job of a run that waits at an environment.
#[derive(Debug, Clone)]
pub struct GateView {
    pub job_id: i64,
    pub job_name: String,
    pub environment: String,
    /// `pending`, `approved` (maybe still on the timer), or `rejected`.
    pub state: String,
    pub release_at: DateTime<Utc>,
    pub decided_by: Option<String>,
    pub comment: String,
    pub reviewers: Vec<String>,
}

type GateRow = (i64, String, String, String, DateTime<Utc>, Option<String>, String, i64);

pub async fn gates_of_run(ctx: &Ctx, run: i64) -> Result<Vec<GateView>> {
    let rows: Vec<GateRow> = sqlx::query_as(
        "SELECT g.job_id, j.name, e.name::text, g.state, g.release_at, (SELECT username::text FROM users WHERE id = g.decided_by), g.comment, e.id
           FROM deployment_gates g JOIN workflow_jobs j ON j.id = g.job_id JOIN environments e ON e.id = g.environment_id
          WHERE j.run_id = $1 ORDER BY g.job_id",
    )
    .bind(run)
    .fetch_all(&ctx.db)
    .await?;
    let mut out = Vec::new();
    for (job_id, job_name, environment, state, release_at, decided_by, comment, env_id) in rows {
        let reviewers: Vec<String> = sqlx::query_scalar(
            "SELECT u.username::text FROM environment_reviewers r JOIN users u ON u.id = r.user_id WHERE r.environment_id = $1 ORDER BY 1",
        )
        .bind(env_id)
        .fetch_all(&ctx.db)
        .await?;
        out.push(GateView { job_id, job_name, environment, state, release_at, decided_by, comment, reviewers });
    }
    Ok(out)
}

/// Whether `user` is a required reviewer of the environment that holds `job`.
pub async fn is_reviewer(ctx: &Ctx, job: i64, user: UserId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deployment_gates g JOIN environment_reviewers r ON r.environment_id = g.environment_id
                         WHERE g.job_id = $1 AND r.user_id = $2 AND g.state = 'pending')",
    )
    .bind(job)
    .bind(user.0)
    .fetch_one(&ctx.db)
    .await?)
}

/// A required reviewer approves or rejects a held job. The caller has checked Write on the
/// repository; the reviewer list is checked here, against the environment's rules as they are now.
#[allow(clippy::too_many_arguments)]
pub async fn decide(
    ctx: &Ctx,
    client: &ClientInfo,
    actor: &Actor,
    repo: RepoId,
    run: i64,
    job: i64,
    approve: bool,
    comment: &str,
) -> Result<()> {
    let Some(who) = actor.user_id() else { return Err(Error::forbidden("Only a person can review a deployment.")) };
    let comment: String = comment.chars().filter(|c| !c.is_control() || *c == '\n').take(MAX_COMMENT).collect();
    let mut tx = ctx.db.begin().await?;
    let gate: Option<(String, String)> = sqlx::query_as(
        "SELECT g.state, e.name::text FROM deployment_gates g JOIN workflow_jobs j ON j.id = g.job_id JOIN environments e ON e.id = g.environment_id
          WHERE g.job_id = $1 AND j.run_id = $2 AND j.repo_id = $3 FOR UPDATE OF g",
    )
    .bind(job)
    .bind(run)
    .bind(repo.0)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, env)) = gate else { return Err(Error::NotFound) };
    if state != "pending" {
        return Err(Error::conflict("This deployment was already decided."));
    }
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deployment_gates g JOIN environment_reviewers r ON r.environment_id = g.environment_id
                         WHERE g.job_id = $1 AND r.user_id = $2)",
    )
    .bind(job)
    .bind(who.0)
    .fetch_one(&mut *tx)
    .await?;
    if !allowed {
        return Err(Error::forbidden("You are not a required reviewer for this environment."));
    }
    sqlx::query("UPDATE deployment_gates SET state = $2, decided_by = $3, decided_at = now(), comment = $4 WHERE job_id = $1")
        .bind(job)
        .bind(if approve { "approved" } else { "rejected" })
        .bind(who.0)
        .bind(&comment)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let ev =
        AuditEvent::new(actor, client, if approve { action::DEPLOYMENT_APPROVED } else { action::DEPLOYMENT_REJECTED }, Outcome::Success)
            .target("workflow_job", job)
            .details(serde_json::json!({"repo_id": repo.0, "run_id": run, "environment": env}));
    if let Err(e) = ctx.audit.record_standalone(ev).await {
        tracing::error!(error = %e, "could not record a deployment review");
    }
    advance_run(ctx, run).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_filter_lets_matching_names_through_and_never_a_fork() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(branch_allowed(&[], "anything", false, false));
        assert!(branch_allowed(&[], "anything", false, true), "no filter: reviewers decide");
        assert!(branch_allowed(&p(&["main", "release/*"]), "main", false, false));
        assert!(branch_allowed(&p(&["main", "release/*"]), "release/1.2", false, false));
        assert!(!branch_allowed(&p(&["main", "release/*"]), "feature/x", false, false));
        assert!(!branch_allowed(&p(&["main"]), "main", false, true), "a fork's branch named main is not main");
    }

    #[test]
    fn a_plain_pattern_never_matches_a_tag_and_a_tags_pattern_never_matches_a_branch() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The reported case: a tag named main must not pass a filter for the branch main.
        assert!(!branch_allowed(&p(&["main"]), "main", true, false));
        assert!(branch_allowed(&p(&["main"]), "main", false, false));
        assert!(!branch_allowed(&p(&["*"]), "v1", true, false), "a plain star is branches only");
        assert!(!branch_allowed(&p(&["**"]), "v1", true, false));
        // A tags/ pattern is for tags.
        assert!(branch_allowed(&p(&["tags/v*"]), "v1", true, false));
        assert!(!branch_allowed(&p(&["tags/v*"]), "x1", true, false));
        assert!(!branch_allowed(&p(&["tags/v*"]), "v1", false, false), "a branch named v1 is not a tag");
        assert!(!branch_allowed(&p(&["tags/v*"]), "tags/v1", false, false), "nor is a branch named tags/v1");
        assert!(!branch_allowed(&p(&["tags/main"]), "main", false, false));
        // Both kinds together, with an exclusion.
        let both = p(&["main", "tags/v*", "!tags/v0*"]);
        assert!(branch_allowed(&both, "main", false, false));
        assert!(branch_allowed(&both, "v2", true, false));
        assert!(!branch_allowed(&both, "v0.1", true, false));
        assert!(!branch_allowed(&both, "dev", false, false));
        // Only exclusions of one kind start that kind out as everything, and never the other.
        assert!(branch_allowed(&p(&["!release/*"]), "main", false, false));
        assert!(!branch_allowed(&p(&["!release/*"]), "v1", true, false));
        assert!(!branch_allowed(&p(&["!tags/v0*"]), "main", false, false));
        assert!(!branch_allowed(&p(&["tags/v*"]), "v1", true, true), "never a fork");
    }
}
