//! The `Ci` service: what the web pages and collab call.

use crate::ctx::Ctx;
use crate::discovery::{self, repo_ci, to_files};
use crate::logs::read_range;
use crate::runs::{self, JobRow, RunRow, fetch_run};
use crate::source::{CHOOSE_REF, branch_or_tag, is_branch_or_tag};
use crate::triggers::{self, Trigger, find_workflow, user_login};
use async_trait::async_trait;
use bytes::Bytes;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::*;
use ch_core::services::Ci;
use ch_core::{Actor, Error, JobId, Permission, RepoId, Result, RunId, RunnerId, WorkflowId};
use ch_db::tokens::{hash_token, new_token};
use ch_workflow::inputs;
use ch_workflow::trigger::EventInfo;
use serde_json::{Value as Json, json};

pub struct CiImpl {
    pub ctx: Ctx,
}

pub fn to_run(r: &RunRow) -> ch_core::model::WorkflowRun {
    ch_core::model::WorkflowRun {
        id: RunId(r.id),
        repo_id: RepoId(r.repo_id),
        workflow_id: r.workflow_id.map(WorkflowId),
        run_number: r.run_number,
        attempt: r.attempt,
        title: r.title.clone(),
        workflow_name: crate::spec::split_payload(&r.event_payload).1.workflow_name,
        event: r.event.clone(),
        head_sha: r.head_sha.clone(),
        head_branch: r.head_branch.clone(),
        status: r.status.clone(),
        conclusion: r.conclusion.clone(),
        created_at: r.created_at,
    }
}

type StepRow = (i32, String, String, Option<String>, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>);

pub async fn steps_for(ctx: &Ctx, job: i64) -> Result<Vec<StepInfo>> {
    let rows: Vec<StepRow> = sqlx::query_as(
        "SELECT number, name, status, conclusion, started_at, completed_at FROM workflow_steps WHERE job_id = $1 ORDER BY number",
    )
    .bind(job)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(number, name, status, conclusion, started_at, completed_at)| StepInfo {
            number,
            name,
            status,
            conclusion,
            started_at,
            completed_at,
        })
        .collect())
}

pub async fn to_job_info(ctx: &Ctx, j: &JobRow) -> Result<JobInfo> {
    let runner_name: Option<String> = match j.runner_id {
        Some(r) => sqlx::query_scalar("SELECT name FROM runners WHERE id = $1").bind(r).fetch_optional(&ctx.db).await?,
        None => None,
    };
    Ok(JobInfo {
        id: JobId(j.id),
        run_id: RunId(j.run_id),
        repo_id: RepoId(j.repo_id),
        key: j.job_key.clone(),
        name: j.name.clone(),
        status: j.status.clone(),
        conclusion: j.conclusion.clone(),
        labels: j.runs_on.clone(),
        runner_id: j.runner_id.map(RunnerId),
        runner_name,
        created_at: j.created_at,
        started_at: j.started_at,
        completed_at: j.completed_at,
        steps: steps_for(ctx, j.id).await?,
    })
}

impl CiImpl {
    /// A run of this repository that the actor may see, by the Actions history rule
    /// (ch-4pn.12): View on the repository, and below Read a recorded tier that covers them.
    async fn seen_run(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<RunRow> {
        let tiers = crate::history::tiers(&self.ctx, actor, repo).await?;
        let r = self.run_in_repo(repo, run).await?;
        crate::history::run_visible(&self.ctx, r.id, tiers).await?;
        Ok(r)
    }

    /// A job of this repository whose run the actor may see.
    async fn seen_job(&self, actor: &Actor, repo: RepoId, job: JobId) -> Result<JobRow> {
        let tiers = crate::history::tiers(&self.ctx, actor, repo).await?;
        let j = self.job_in_repo(repo, job).await?;
        crate::history::run_visible(&self.ctx, j.run_id, tiers).await?;
        Ok(j)
    }

    async fn run_in_repo(&self, repo: RepoId, run: RunId) -> Result<RunRow> {
        let r = fetch_run(&self.ctx, run.0).await?;
        if r.repo_id != repo.0 {
            return Err(Error::NotFound);
        }
        Ok(r)
    }

    async fn job_in_repo(&self, repo: RepoId, job: JobId) -> Result<JobRow> {
        let j = sqlx::query_as::<_, JobRow>(&format!("SELECT {} FROM workflow_jobs WHERE id = $1", runs::JOB_COLS))
            .bind(job.0)
            .fetch_optional(&self.ctx.db)
            .await?
            .ok_or(Error::NotFound)?;
        if j.repo_id != repo.0 {
            return Err(Error::NotFound);
        }
        Ok(j)
    }
}

#[async_trait]
impl Ci for CiImpl {
    async fn runs(&self, actor: &Actor, repo: RepoId, page: u32, per_page: u32) -> Result<Vec<ch_core::model::WorkflowRun>> {
        let tiers = crate::history::tiers(&self.ctx, actor, repo).await?;
        let per = i64::from(per_page.clamp(1, 100));
        let off = i64::from(page.max(1) - 1) * per;
        // One row per run: a re-run replaces its earlier attempts, as on GitHub (ch-zwo). Below
        // Read, only attempts recorded at a tier that covers the viewer count (ch-4pn.12).
        let rows = sqlx::query_as::<_, RunRow>(&format!(
            "SELECT {} FROM workflow_runs r WHERE repo_id = $1 AND ($4::text[] IS NULL OR r.recorded_tier = ANY($4))
               AND NOT EXISTS (SELECT 1 FROM workflow_runs n WHERE n.repo_id = r.repo_id AND n.run_number = r.run_number AND n.attempt > r.attempt
                               AND ($4::text[] IS NULL OR n.recorded_tier = ANY($4)))
             ORDER BY run_number DESC, id DESC LIMIT $2 OFFSET $3",
            runs::RUN_COLS
        ))
        .bind(repo.0)
        .bind(per)
        .bind(off)
        .bind(crate::history::bind(tiers))
        .fetch_all(&self.ctx.db)
        .await?;
        Ok(rows.iter().map(to_run).collect())
    }

    async fn get_run(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<ch_core::model::WorkflowRun> {
        Ok(to_run(&self.seen_run(actor, repo, run).await?))
    }

    async fn dispatch(
        &self,
        actor: &Actor,
        repo: RepoId,
        workflow: &str,
        git_ref: &str,
        inputs_in: Json,
        _client: &ClientInfo,
    ) -> Result<RunId> {
        self.ctx.authz.require(actor, repo, Permission::Write).await?;
        require_ci(actor, repo)?;
        let info = repo_ci(&self.ctx, repo).await?;
        match info.availability() {
            CiAvailability::Enabled => {}
            CiAvailability::OwnerNotAllowed => {
                return Err(Error::forbidden("Workflows are off for this account. Ask an admin to turn them on."));
            }
            CiAvailability::TurnedOff => return Err(Error::forbidden("Actions is turned off for this repository.")),
        }
        let wanted = if git_ref.trim().is_empty() { info.default_branch.clone() } else { git_ref.trim().to_string() };
        let candidates = if wanted.starts_with("refs/") {
            if !is_branch_or_tag(&wanted) {
                return Err(Error::invalid("Dispatch", "ref", CHOOSE_REF));
            }
            vec![wanted.clone()]
        } else {
            // Prefer a branch, then a tag, so `main` means the branch even if a tag shares the name.
            vec![format!("refs/heads/{wanted}"), format!("refs/tags/{wanted}")]
        };
        let mut named = None;
        for full in candidates {
            if let Some(sha) = branch_or_tag(self.ctx.source.as_ref(), repo, &full).await? {
                named = Some((full, sha));
                break;
            }
        }
        let Some((full_ref, sha)) = named else {
            return Err(Error::invalid("Dispatch", "ref", format!("No branch or tag named `{wanted}`.")));
        };
        let found = discovery::discover(&self.ctx, repo, &sha, info.read_github_workflows).await?;
        let rows = discovery::sync_rows(&self.ctx, repo, &found).await?;
        let d = find_workflow(&found, &rows, workflow).ok_or(Error::NotFound)?;
        let wf = d
            .workflow
            .as_ref()
            .map_err(|e| Error::Validation(e.iter().map(|x| ch_core::FieldError::custom("Workflow", &d.path, x.to_string())).collect()))?;
        if rows.get(&d.path).map(|r| r.state.as_str()) == Some("disabled") {
            return Err(Error::invalid("Workflow", "state", "This workflow is disabled."));
        }
        let Some(dispatch) = &wf.triggers.workflow_dispatch else {
            return Err(Error::invalid("Workflow", "on", "This workflow does not have a workflow_dispatch trigger."));
        };
        let given = match inputs_in {
            Json::Object(o) => o,
            Json::Null => Default::default(),
            _ => return Err(Error::invalid("Dispatch", "inputs", "Inputs must be an object.")),
        };
        let resolved = inputs::resolve(dispatch, &given)
            .map_err(|errs| Error::Validation(errs.into_iter().map(|m| ch_core::FieldError::custom("Dispatch", "inputs", m)).collect()))?;
        let login = match actor.author_id() {
            Some(u) => user_login(&self.ctx, u).await?.unwrap_or_default(),
            None => String::new(),
        };
        let payload =
            json!({ "inputs": resolved, "ref": full_ref, "repository": { "full_name": info.full_name() }, "sender": { "login": login } });
        let t = Trigger {
            repo,
            sha,
            git_ref: full_ref.clone(),
            info: EventInfo::simple("workflow_dispatch"),
            actor: actor.author_id().map(|u| (u, login)),
            title: d.name.clone(),
            payload,
            inputs: Json::Object(resolved),
            fork: false,
            needs_approval: false,
            hold_reason: None,
            approved_by: None,
            base_ref: String::new(),
            head_ref: String::new(),
            // A manual run is always new: there is no event to deduplicate against.
            trigger_id: format!("dispatch:{}", unique_id()),
            head_branch: full_ref.strip_prefix("refs/heads/").map(String::from),
            sync_schedules: false,
        };
        let made = triggers::fire(&self.ctx, &t, Some(&d.path)).await?;
        made.into_iter().next().ok_or_else(|| Error::invalid("Workflow", "on", "The workflow did not start a run."))
    }

    async fn cancel(&self, actor: &Actor, repo: RepoId, run: RunId, _client: &ClientInfo) -> Result<()> {
        self.ctx.authz.require(actor, repo, Permission::Write).await?;
        require_ci(actor, repo)?;
        self.run_in_repo(repo, run).await?;
        runs::cancel_run(&self.ctx, run.0).await
    }

    async fn rerun(&self, actor: &Actor, repo: RepoId, run: RunId, failed_only: bool, _client: &ClientInfo) -> Result<RunId> {
        self.ctx.authz.require(actor, repo, Permission::Write).await?;
        require_ci(actor, repo)?;
        self.run_in_repo(repo, run).await?;
        runs::rerun(&self.ctx, run.0, failed_only, actor.author_id()).await
    }

    /// `range` is in bytes of the job's log file, the same offsets the live stream reports.
    async fn log_range(&self, actor: &Actor, repo: RepoId, job: JobId, range: LogRange) -> Result<Bytes> {
        let j = self.seen_job(actor, repo, job).await?;
        read_range(&self.ctx, j.run_id, j.id, range.from, range.to).await
    }

    async fn log_size(&self, actor: &Actor, repo: RepoId, job: JobId) -> Result<u64> {
        let j = self.seen_job(actor, repo, job).await?;
        crate::logs::log_len(&self.ctx, j.run_id, j.id).await
    }

    async fn create_runner_token(&self, actor: &Actor, repo: Option<RepoId>, client: &ClientInfo) -> Result<RunnerRegistrationToken> {
        if !actor.is_site_admin() {
            return Err(if actor.is_admin_without_2fa() {
                Error::forbidden("Turn on two-factor sign-in before using the admin pages.")
            } else {
                Error::forbidden("Only a site admin can issue a runner registration token.")
            });
        }
        if let Some(r) = repo {
            repo_ci(&self.ctx, r).await?;
        }
        let token = new_token(ch_db::ci_auth::RUNNER_REGISTRATION_PREFIX, 40);
        let ttl = self.ctx.config.ci.registration_token_ttl_minutes.clamp(1, 24 * 60) as i32;
        let mut tx = self.ctx.db.begin().await?;
        let (id, expires_at): (i64, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
            "INSERT INTO runner_registration_tokens (token_hash, created_by, repo_id, expires_at) VALUES ($1, $2, $3, now() + make_interval(mins => $4))
             RETURNING id, expires_at",
        )
        .bind(hash_token(&token))
        .bind(actor.user_id().map(|u| u.0))
        .bind(repo.map(|r| r.0))
        .bind(ttl)
        .fetch_one(&mut *tx)
        .await?;
        self.ctx
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::RUNNER_TOKEN_ISSUED, Outcome::Success)
                    .target("runner_token", id)
                    .details(json!({"repo_id": repo.map(|r| r.0), "expires_at": expires_at})),
            )
            .await?;
        tx.commit().await?;
        Ok(RunnerRegistrationToken { token, expires_at })
    }

    async fn availability(&self, repo: RepoId) -> Result<CiAvailability> {
        Ok(repo_ci(&self.ctx, repo).await?.availability())
    }

    async fn workflows(&self, actor: &Actor, repo: RepoId, git_ref: Option<&str>) -> Result<Vec<WorkflowFile>> {
        // The files are part of the repository, so View reads them. Only Read and up refresh the
        // table: a signed-out or no-grant read writes nothing (spec section 4, "Web and REST").
        let reader = crate::history::tiers(&self.ctx, actor, repo).await?.is_none();
        let info = repo_ci(&self.ctx, repo).await?;
        let rev = git_ref.map(str::trim).filter(|r| !r.is_empty()).unwrap_or(&info.default_branch);
        let is_default = rev == info.default_branch || rev == format!("refs/heads/{}", info.default_branch);
        let sha = match self.ctx.source.resolve(repo, rev).await {
            Ok(s) => s,
            // An empty repository has no commits, and so no workflows.
            Err(Error::NotFound) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let found = discovery::discover(&self.ctx, repo, &sha, info.read_github_workflows).await?;
        // Reading must not write: a connected app only reads the rows that exist.
        let rows = if reader && is_default && info.availability() == CiAvailability::Enabled && !actor.is_app() {
            let r = discovery::sync_rows(&self.ctx, repo, &found).await?;
            discovery::sync_schedules(&self.ctx, repo, &found, &r, self.ctx.now()).await?;
            r
        } else {
            // Another ref, or a repo that cannot run workflows: read the rows that exist, write nothing.
            let paths: Vec<String> = found.iter().map(|d| d.path.clone()).collect();
            let existing: Vec<(i64, String, String)> =
                sqlx::query_as("SELECT id, path, state FROM workflows WHERE repo_id = $1 AND path = ANY($2)")
                    .bind(repo.0)
                    .bind(&paths)
                    .fetch_all(&self.ctx.db)
                    .await?;
            existing.into_iter().map(|(id, path, state)| (path, discovery::WorkflowRow { id: WorkflowId(id), state })).collect()
        };
        Ok(to_files(&found, &rows))
    }

    async fn jobs(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<JobInfo>> {
        self.seen_run(actor, repo, run).await?;
        let jobs = runs::fetch_jobs(&self.ctx, run.0).await?;
        let mut out = Vec::with_capacity(jobs.len());
        for j in &jobs {
            out.push(to_job_info(&self.ctx, j).await?);
        }
        Ok(out)
    }

    async fn job(&self, actor: &Actor, repo: RepoId, job: JobId) -> Result<JobInfo> {
        let j = self.seen_job(actor, repo, job).await?;
        to_job_info(&self.ctx, &j).await
    }

    async fn artifacts(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<ArtifactInfo>> {
        self.seen_run(actor, repo, run).await?;
        let rows = crate::store::artifacts_for_run(&self.ctx, run.0).await?;
        let now = chrono::Utc::now();
        Ok(rows
            .into_iter()
            .map(|a| ArtifactInfo {
                id: a.id,
                run_id: RunId(a.run_id),
                name: a.name,
                size: a.size,
                expired: a.expires_at <= now,
                created_at: a.created_at,
                expires_at: a.expires_at,
            })
            .collect())
    }

    async fn run_hold_reason(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Option<String>> {
        self.seen_run(actor, repo, run).await?;
        Ok(sqlx::query_scalar("SELECT event_payload->'meta'->>'hold_reason' FROM workflow_runs WHERE id = $1 AND status = 'waiting'")
            .bind(run.0)
            .fetch_optional(&self.ctx.db)
            .await?
            .flatten())
    }

    async fn badge(&self, actor: &Actor, repo: RepoId, file: &str, branch: Option<&str>, event: Option<&str>) -> Result<WorkflowBadge> {
        self.ctx.authz.require(actor, repo, Permission::Read).await?;
        let info = repo_ci(&self.ctx, repo).await?;
        // The file name alone, as GitHub's badge URL has it. `.codehaven/workflows` wins over
        // `.github/workflows` when both hold the name, as discovery does.
        let wf: Option<(i64, String)> = sqlx::query_as(
            "SELECT id, name FROM workflows WHERE repo_id = $1 AND path IN ('.codehaven/workflows/' || $2, '.github/workflows/' || $2)
             ORDER BY (path LIKE '.codehaven/%') DESC LIMIT 1",
        )
        .bind(repo.0)
        .bind(file)
        .fetch_optional(&self.ctx.db)
        .await?;
        let Some((id, workflow_name)) = wf else { return Err(Error::NotFound) };
        let branch = branch.filter(|b| !b.trim().is_empty()).unwrap_or(&info.default_branch);
        // Runs of pull requests name the head branch, which may be a fork's of the same name, so
        // they count only when asked for by event.
        let conclusion: Option<Option<String>> = sqlx::query_scalar(
            "SELECT conclusion FROM workflow_runs
             WHERE repo_id = $1 AND workflow_id = $2 AND head_branch = $3 AND status = 'completed'
               AND (($4::text IS NULL AND event <> 'pull_request') OR event = $4)
               AND NOT COALESCE((event_payload->'meta'->>'fork')::boolean, false)
             ORDER BY run_number DESC, attempt DESC LIMIT 1",
        )
        .bind(repo.0)
        .bind(id)
        .bind(branch)
        .bind(event.filter(|e| !e.trim().is_empty()))
        .fetch_optional(&self.ctx.db)
        .await?;
        Ok(WorkflowBadge { workflow_name, conclusion: conclusion.flatten() })
    }

    async fn run_problem(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Option<String>> {
        let r = self.seen_run(actor, repo, run).await?;
        Ok(crate::spec::split_payload(&r.event_payload).1.problem)
    }

    async fn approve_run(&self, actor: &Actor, repo: RepoId, run: RunId, client: &ClientInfo) -> Result<()> {
        // Approval is a person's decision. A connected app never makes it, whatever it was given.
        actor.deny_app("approve a workflow run")?;
        self.ctx.authz.require(actor, repo, Permission::Write).await?;
        self.run_in_repo(repo, run).await?;
        let who = actor.user_id().ok_or_else(|| Error::forbidden("Only a person can approve a run."))?;
        runs::approve_run(&self.ctx, run.0, who).await?;
        let ev = AuditEvent::new(actor, client, action::RUN_APPROVED, Outcome::Success)
            .target("workflow_run", run.0)
            .details(json!({"repo_id": repo.0}));
        if let Err(e) = self.ctx.audit.record_standalone(ev).await {
            tracing::error!(error = %e, "could not record a run approval");
        }
        Ok(())
    }

    async fn pending_fork_approval(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Option<String>> {
        self.ctx.authz.require(actor, repo, Permission::Read).await?;
        triggers::pending_fork_approval(&self.ctx, repo, number).await
    }

    async fn approve_fork_commit(&self, actor: &Actor, repo: RepoId, number: i64, sha: &str, client: &ClientInfo) -> Result<Vec<RunId>> {
        // Approval is a person's decision. A connected app never makes it, whatever it was given.
        actor.deny_app("approve a workflow run")?;
        self.ctx.authz.require(actor, repo, Permission::Write).await?;
        let who = actor.user_id().ok_or_else(|| Error::forbidden("Only a person can approve a run."))?;
        let runs = triggers::approve_fork_commit(&self.ctx, repo, number, sha, who).await?;
        let ev = AuditEvent::new(actor, client, action::RUN_APPROVED, Outcome::Success)
            .target("pull_request", number)
            .details(json!({"repo_id": repo.0, "head_sha": sha, "runs": runs.iter().map(|r| r.0).collect::<Vec<_>>()}));
        if let Err(e) = self.ctx.audit.record_standalone(ev).await {
            tracing::error!(error = %e, "could not record a fork commit approval");
        }
        Ok(runs)
    }

    async fn runners(&self, actor: &Actor) -> Result<Vec<RunnerSummary>> {
        if !actor.is_site_admin() {
            return Err(Error::forbidden("Only a site admin can see runners."));
        }
        runner_summaries(&self.ctx, None).await
    }
}

/// A connected app runs, reruns or cancels workflows only on a repository where the person gave it
/// the workflows permission, and a personal access token only with the `workflow` scope.
fn require_ci(actor: &Actor, repo: RepoId) -> Result<()> {
    match actor {
        // `contents: write` is about the code. A job never starts, stops or reruns a run.
        Actor::Job { .. } => Err(Error::forbidden("A workflow job cannot run or stop workflows.")),
        Actor::App { grant, .. } if !grant.workflows.has(repo) => {
            Err(Error::forbidden("This connection was not given permission to run workflows on this repository"))
        }
        // The scope the token settings page describes as "Update workflow files and trigger
        // runs" (ch-s7r).
        Actor::Token { scopes, .. } if !scopes.has("workflow") => Err(Error::forbidden("This token needs the workflow scope")),
        _ => Ok(()),
    }
}

/// Unique enough to tell two manual runs apart. Not a secret.
fn unique_id() -> String {
    ch_db::tokens::random_hex_256()
}

type RunnerRow =
    (i64, String, Vec<String>, Option<String>, Option<String>, Option<String>, Option<i64>, Option<chrono::DateTime<chrono::Utc>>, bool);

pub async fn runner_summaries(ctx: &Ctx, repo: Option<i64>) -> Result<Vec<RunnerSummary>> {
    let rows: Vec<RunnerRow> = sqlx::query_as(
        "SELECT r.id, r.name, r.labels, r.os, r.arch, r.version, r.repo_id, r.last_seen_at,
                EXISTS (SELECT 1 FROM workflow_jobs j WHERE j.runner_id = r.id AND j.status = 'in_progress') AS busy
         FROM runners r WHERE r.revoked_at IS NULL AND ($1::bigint IS NULL OR r.repo_id IS NULL OR r.repo_id = $1) ORDER BY r.id",
    )
    .bind(repo)
    .fetch_all(&ctx.db)
    .await?;
    let now = chrono::Utc::now();
    Ok(rows
        .into_iter()
        .map(|(id, name, labels, os, arch, version, repo_id, last_seen_at, busy)| {
            let online = last_seen_at.is_some_and(|t| now - t < chrono::Duration::seconds(90));
            RunnerSummary {
                id: RunnerId(id),
                name,
                labels,
                os,
                arch,
                version,
                repo_id: repo_id.map(RepoId),
                status: if busy {
                    "busy"
                } else if online {
                    "online"
                } else {
                    "offline"
                }
                .into(),
                last_seen_at,
            }
        })
        .collect())
}
