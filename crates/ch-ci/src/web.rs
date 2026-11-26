//! The Actions pages' view of CI: `ch_core::web_contract::Pipelines`.
//!
//! The web layer asks for workflows, jobs with their steps, runners, secrets, variables and
//! artifacts in the shapes its pages draw. Everything here goes through the same functions the
//! REST handlers use, and checks the same permissions: reading needs `Read`, changing secrets
//! and variables needs `Admin` on the repository, and runners are for site admins (or, for one
//! repository, its admins).

use crate::runs;
use crate::secrets::{self, Scope};
use crate::service::{CiImpl, runner_summaries};
use async_trait::async_trait;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::ClientInfo;
use ch_core::services::Ci;
use ch_core::web_contract::{
    ActionUsed, ActionsPolicy, Artifact, DeploymentGate, EnvironmentDraft, EnvironmentInfo, EnvironmentReviewer, Pipelines, Runner,
    SecretMeta, Variable, WorkflowDef, WorkflowInput, WorkflowJob, WorkflowStep,
};
use ch_core::{Actor, Error, Permission, RepoId, Result, RunId, RunnerId};
use std::collections::HashMap;

impl CiImpl {
    async fn record_environment(&self, actor: &Actor, client: &ClientInfo, act: &str, repo: RepoId, env: &str, detail: serde_json::Value) {
        let ev = AuditEvent::new(actor, client, act, Outcome::Success)
            .target("environment", env)
            .details(serde_json::json!({ "repo_id": repo.0, "environment": env, "detail": detail }));
        if let Err(e) = self.ctx.audit.record_standalone(ev).await {
            tracing::warn!(error = %e, "could not record the audit event");
        }
    }

    async fn record_secret(&self, actor: &Actor, client: &ClientInfo, act: &str, name: &str) {
        let ev = AuditEvent::new(actor, client, act, Outcome::Success)
            .target("secret", name)
            .details(serde_json::json!({ "scope": "repo", "name": name }));
        if let Err(e) = self.ctx.audit.record_standalone(ev).await {
            tracing::warn!(error = %e, "could not record the audit event");
        }
    }
}

type StepRange = (i64, i32, Option<i64>, Option<i64>);

#[async_trait]
impl Pipelines for CiImpl {
    async fn workflows(&self, actor: &Actor, repo: RepoId, git_ref: &str) -> Result<Vec<WorkflowDef>> {
        let git_ref = Some(git_ref).filter(|r| !r.trim().is_empty());
        let files = Ci::workflows(self, actor, repo, git_ref).await?;
        Ok(files
            .into_iter()
            .map(|f| WorkflowDef {
                path: f.path,
                name: f.name,
                can_dispatch: f.events.iter().any(|e| e == "workflow_dispatch") && f.errors.is_empty(),
                triggers: f.events,
                error: if f.errors.is_empty() { None } else { Some(f.errors.join(" ")) },
                inputs: f
                    .dispatch_inputs
                    .into_iter()
                    .map(|i| WorkflowInput {
                        name: i.name,
                        description: i.description,
                        default: i.default.map(|v| match v {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        }),
                        required: i.required,
                        kind: i.kind,
                        options: i.options,
                    })
                    .collect(),
            })
            .collect())
    }

    async fn jobs(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<WorkflowJob>> {
        let infos = Ci::jobs(self, actor, repo, run).await?;
        // `needs` holds job keys. The page shows names.
        let needs: Vec<(i64, Vec<String>)> =
            sqlx::query_as("SELECT id, needs FROM workflow_jobs WHERE run_id = $1").bind(run.0).fetch_all(&self.ctx.db).await?;
        let needs: HashMap<i64, Vec<String>> = needs.into_iter().collect();
        let name_of: HashMap<&str, &str> = infos.iter().map(|j| (j.key.as_str(), j.name.as_str())).collect();
        let ranges: Vec<StepRange> = sqlx::query_as(
            "SELECT s.job_id, s.number, s.log_start, s.log_end FROM workflow_steps s
             JOIN workflow_jobs j ON j.id = s.job_id WHERE j.run_id = $1",
        )
        .bind(run.0)
        .fetch_all(&self.ctx.db)
        .await?;
        let ranges: HashMap<(i64, i32), (Option<i64>, Option<i64>)> = ranges.into_iter().map(|(j, n, a, b)| ((j, n), (a, b))).collect();
        // A queued job says when no runner here can take it, and which labels the runners offer.
        let offered = if infos.iter().any(|j| j.status == "queued") { runs::runner_labels(&self.ctx, repo.0).await? } else { vec![] };
        let no_runner_minutes = self.ctx.config.ci.no_runner_minutes;
        Ok(infos
            .iter()
            .map(|j| WorkflowJob {
                id: j.id,
                run_id: j.run_id,
                name: j.name.clone(),
                status: j.status.clone(),
                conclusion: j.conclusion.clone(),
                needs: needs
                    .get(&j.id.0)
                    .map(|keys| keys.iter().map(|k| name_of.get(k.as_str()).map(|n| n.to_string()).unwrap_or_else(|| k.clone())).collect())
                    .unwrap_or_default(),
                runner_name: j.runner_name.clone(),
                queue_note: (j.status == "queued" && !runs::some_runner_fits(&j.labels, &offered)).then(|| {
                    format!(
                        "{} The job fails if no runner with these labels is added within {no_runner_minutes} minutes of it being queued.",
                        runs::no_runner_line(&j.labels, &offered)
                    )
                }),
                started_at: j.started_at,
                completed_at: j.completed_at,
                steps: j
                    .steps
                    .iter()
                    .map(|s| {
                        let (from, to) = ranges.get(&(j.id.0, s.number)).copied().unwrap_or((None, None));
                        WorkflowStep {
                            number: s.number.max(0) as u32,
                            name: s.name.clone(),
                            status: s.status.clone(),
                            conclusion: s.conclusion.clone(),
                            started_at: s.started_at,
                            completed_at: s.completed_at,
                            log_from: from.unwrap_or(0).max(0) as u64,
                            log_to: to.map(|t| t.max(0) as u64),
                        }
                    })
                    .collect(),
            })
            .collect())
    }

    async fn runners(&self, actor: &Actor, repo: Option<RepoId>) -> Result<Vec<Runner>> {
        match repo {
            Some(r) => self.ctx.authz.require(actor, r, Permission::Admin).await?,
            None if actor.is_site_admin() => {}
            None => return Err(Error::forbidden("Only a site admin can see every runner.")),
        }
        let rows = runner_summaries(&self.ctx, repo.map(|r| r.0)).await?;
        let created: Vec<(i64, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as("SELECT id, created_at FROM runners WHERE revoked_at IS NULL").fetch_all(&self.ctx.db).await?;
        let created: HashMap<i64, chrono::DateTime<chrono::Utc>> = created.into_iter().collect();
        Ok(rows
            .into_iter()
            .map(|r| Runner {
                created_at: created.get(&r.id.0).copied().unwrap_or_else(chrono::Utc::now),
                id: r.id,
                name: r.name,
                labels: r.labels,
                repo_id: r.repo_id,
                status: r.status,
                version: r.version.unwrap_or_default(),
                last_seen_at: r.last_seen_at,
            })
            .collect())
    }

    async fn revoke_runner(&self, actor: &Actor, id: RunnerId, client: &ClientInfo) -> Result<()> {
        if !actor.is_site_admin() {
            return Err(Error::forbidden("Only a site admin can remove a runner."));
        }
        let mut tx = self.ctx.db.begin().await?;
        let done =
            sqlx::query("UPDATE runners SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL").bind(id.0).execute(&mut *tx).await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.ctx
            .audit
            .record(&mut tx, AuditEvent::new(actor, client, action::RUNNER_REMOVED, Outcome::Success).target("runner", id.0))
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn token_ceiling(&self, actor: &Actor, repo: RepoId) -> Result<String> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        Ok(sqlx::query_scalar("SELECT actions_token_ceiling FROM repositories WHERE id = $1").bind(repo.0).fetch_one(&self.ctx.db).await?)
    }

    async fn set_token_ceiling(&self, actor: &Actor, repo: RepoId, value: &str, client: &ClientInfo) -> Result<()> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        if !matches!(value, "read" | "write") {
            return Err(Error::invalid("Repository", "actions_token_ceiling", "Choose read or write."));
        }
        let mut tx = self.ctx.db.begin().await?;
        sqlx::query("UPDATE repositories SET actions_token_ceiling = $2 WHERE id = $1").bind(repo.0).bind(value).execute(&mut *tx).await?;
        self.ctx
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::ACTIONS_TOKEN_CEILING, Outcome::Success)
                    .target("repo", repo.0)
                    .details(serde_json::json!({ "value": value })),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn actions_policy(&self, actor: &Actor) -> Result<ActionsPolicy> {
        if !actor.is_site_admin() {
            return Err(Error::forbidden("Only a site admin can see the Actions policy."));
        }
        let p = crate::action_fetch::load_policy(&self.ctx).await?;
        Ok(ActionsPolicy {
            mode: match p.mode {
                ch_workflow::uses::PolicyMode::None => "none",
                ch_workflow::uses::PolicyMode::Selected => "selected",
                ch_workflow::uses::PolicyMode::All => "all",
            }
            .to_string(),
            allow: p.allow,
            block: p.block,
            require_sha: p.require_sha,
            allow_codehaven_repos: p.allow_codehaven_repos,
        })
    }

    async fn set_actions_policy(&self, actor: &Actor, policy: ActionsPolicy, client: &ClientInfo) -> Result<()> {
        if !actor.is_site_admin() {
            return Err(Error::forbidden("Only a site admin can change the Actions policy."));
        }
        if !matches!(policy.mode.as_str(), "none" | "selected" | "all") {
            return Err(Error::invalid("ActionsPolicy", "mode", "Choose none, selected or all."));
        }
        let clean = |list: &[String], field: &str| -> Result<Vec<String>> {
            let mut out: Vec<String> = Vec::new();
            for p in list.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
                if !ch_workflow::uses::valid_pattern(p) {
                    return Err(Error::invalid("ActionsPolicy", field, format!("`{p}` is not owner/* or owner/repo.")));
                }
                if !out.iter().any(|o| o.eq_ignore_ascii_case(p)) {
                    out.push(p.to_string());
                }
            }
            if out.len() > 500 {
                return Err(Error::invalid("ActionsPolicy", field, "List at most 500 patterns."));
            }
            Ok(out)
        };
        let allow = clean(&policy.allow, "allow")?;
        let block = clean(&policy.block, "block")?;
        let mut tx = self.ctx.db.begin().await?;
        sqlx::query(
            "UPDATE actions_policy SET mode = $1, allow = $2, block = $3, require_sha = $4, allow_codehaven_repos = $5, updated_at = now() WHERE id",
        )
        .bind(&policy.mode)
        .bind(&allow)
        .bind(&block)
        .bind(policy.require_sha)
        .bind(policy.allow_codehaven_repos)
        .execute(&mut *tx)
        .await?;
        self.ctx
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::ACTIONS_POLICY, Outcome::Success).details(serde_json::json!({
                    "mode": policy.mode, "allow": allow, "block": block,
                    "require_sha": policy.require_sha, "allow_codehaven_repos": policy.allow_codehaven_repos
                })),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn run_actions(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<ActionUsed>> {
        self.ctx.authz.require(actor, repo, Permission::Read).await?;
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT a.host, a.name, a.git_ref, a.sha FROM action_resolutions a JOIN workflow_runs r ON r.id = a.run_id
             WHERE a.run_id = $1 AND r.repo_id = $2 ORDER BY a.created_at, a.name",
        )
        .bind(run.0)
        .bind(repo.0)
        .fetch_all(&self.ctx.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(host, name, git_ref, sha)| ActionUsed {
                name: if host == "haven" { format!("haven_{name}") } else { name },
                requested: git_ref,
                sha,
            })
            .collect())
    }

    async fn secrets(&self, actor: &Actor, repo: RepoId) -> Result<Vec<SecretMeta>> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        Ok(secrets::list_secrets(&self.ctx, Scope::Repo(repo.0))
            .await?
            .into_iter()
            .map(|(name, updated_at)| SecretMeta { name, updated_at })
            .collect())
    }

    async fn set_secret(&self, actor: &Actor, repo: RepoId, name: &str, value: &str, client: &ClientInfo) -> Result<()> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        secrets::put_secret(&self.ctx, Scope::Repo(repo.0), name, value, actor.user_id()).await?;
        self.record_secret(actor, client, action::SECRET_CHANGED, name).await;
        Ok(())
    }

    async fn delete_secret(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        if !secrets::delete_secret(&self.ctx, Scope::Repo(repo.0), name).await? {
            return Err(Error::NotFound);
        }
        self.record_secret(actor, client, action::SECRET_DELETED, name).await;
        Ok(())
    }

    async fn variables(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Variable>> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        Ok(secrets::list_variables(&self.ctx, Scope::Repo(repo.0))
            .await?
            .into_iter()
            .map(|(name, value, updated_at)| Variable { name, value, updated_at })
            .collect())
    }

    async fn set_variable(&self, actor: &Actor, repo: RepoId, name: &str, value: &str, _client: &ClientInfo) -> Result<()> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        // `put_variable` creates or replaces.
        secrets::put_variable(&self.ctx, Scope::Repo(repo.0), name, value).await?;
        Ok(())
    }

    async fn delete_variable(&self, actor: &Actor, repo: RepoId, name: &str, _client: &ClientInfo) -> Result<()> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        if !secrets::delete_variable(&self.ctx, Scope::Repo(repo.0), name).await? {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    async fn environments(&self, actor: &Actor, repo: RepoId) -> Result<Vec<EnvironmentInfo>> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        Ok(crate::environments::list(&self.ctx, repo).await?.into_iter().map(env_info).collect())
    }

    async fn environment(&self, actor: &Actor, repo: RepoId, name: &str) -> Result<EnvironmentInfo> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        Ok(env_info(crate::environments::get(&self.ctx, repo, name).await?))
    }

    async fn save_environment(
        &self,
        actor: &Actor,
        repo: RepoId,
        name: &str,
        draft: EnvironmentDraft,
        client: &ClientInfo,
    ) -> Result<bool> {
        actor.deny_app("change an environment")?;
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        let detail = serde_json::json!({"wait_minutes": draft.wait_minutes, "branches": draft.branches, "reviewers": draft.reviewers.iter().map(|u| u.0).collect::<Vec<_>>()});
        let created = crate::environments::save(&self.ctx, repo, name, draft.wait_minutes, &draft.branches, &draft.reviewers).await?;
        self.record_environment(actor, client, action::ENVIRONMENT_CHANGED, repo, name, detail).await;
        Ok(created)
    }

    async fn create_environment(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        actor.deny_app("create an environment")?;
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        crate::environments::create(&self.ctx, repo, name).await?;
        let detail = serde_json::json!({"wait_minutes": 0, "branches": [], "reviewers": []});
        self.record_environment(actor, client, action::ENVIRONMENT_CHANGED, repo, name, detail).await;
        Ok(())
    }

    async fn delete_environment(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        actor.deny_app("delete an environment")?;
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        crate::environments::delete(&self.ctx, repo, name).await?;
        self.record_environment(actor, client, action::ENVIRONMENT_DELETED, repo, name, serde_json::json!({})).await;
        Ok(())
    }

    async fn set_environment_secret(
        &self,
        actor: &Actor,
        repo: RepoId,
        environment: &str,
        name: &str,
        value: &str,
        client: &ClientInfo,
    ) -> Result<()> {
        actor.deny_app("change an environment secret")?;
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        crate::environments::put_secret(&self.ctx, repo, environment, name, value, actor.user_id()).await?;
        self.record_environment(actor, client, action::ENVIRONMENT_SECRET_CHANGED, repo, environment, serde_json::json!({"name": name}))
            .await;
        Ok(())
    }

    async fn delete_environment_secret(
        &self,
        actor: &Actor,
        repo: RepoId,
        environment: &str,
        name: &str,
        client: &ClientInfo,
    ) -> Result<()> {
        actor.deny_app("change an environment secret")?;
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        crate::environments::delete_secret(&self.ctx, repo, environment, name).await?;
        self.record_environment(actor, client, action::ENVIRONMENT_SECRET_DELETED, repo, environment, serde_json::json!({"name": name}))
            .await;
        Ok(())
    }

    async fn reviewer_candidates(&self, actor: &Actor, repo: RepoId) -> Result<Vec<EnvironmentReviewer>> {
        self.ctx.authz.require(actor, repo, Permission::Admin).await?;
        Ok(crate::environments::reviewer_candidates(&self.ctx, repo)
            .await?
            .into_iter()
            .map(|(user_id, login)| EnvironmentReviewer { user_id, login })
            .collect())
    }

    async fn run_gates(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<DeploymentGate>> {
        self.ctx.authz.require(actor, repo, Permission::Read).await?;
        Ci::get_run(self, actor, repo, run).await?;
        let may_write = self.ctx.authz.repo_permission(actor, repo).await? >= Permission::Write;
        let mut out = Vec::new();
        for g in crate::environments::gates_of_run(&self.ctx, run.0).await? {
            let can_review = match (may_write, actor.user_id()) {
                (true, Some(u)) => crate::environments::is_reviewer(&self.ctx, g.job_id, u).await?,
                _ => false,
            };
            out.push(DeploymentGate {
                job_id: g.job_id,
                job_name: g.job_name,
                environment: g.environment,
                state: g.state,
                release_at: g.release_at,
                decided_by: g.decided_by,
                comment: g.comment,
                reviewers: g.reviewers,
                can_review,
            });
        }
        Ok(out)
    }

    async fn review_deployment(
        &self,
        actor: &Actor,
        repo: RepoId,
        run: RunId,
        job: i64,
        approve: bool,
        comment: &str,
        client: &ClientInfo,
    ) -> Result<()> {
        // A review is a person's decision. A connected app never makes it.
        actor.deny_app("review a deployment")?;
        self.ctx.authz.require(actor, repo, Permission::Write).await?;
        crate::triggers::refuse_if_locked(&self.ctx, repo).await?;
        crate::environments::decide(&self.ctx, client, actor, repo, run.0, job, approve, comment).await
    }

    async fn artifacts(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<Artifact>> {
        let infos = Ci::artifacts(self, actor, repo, run).await?;
        let shas: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, blob_sha256 FROM artifacts WHERE run_id = $1").bind(run.0).fetch_all(&self.ctx.db).await?;
        let shas: HashMap<i64, String> = shas.into_iter().collect();
        Ok(infos
            .into_iter()
            .map(|a| Artifact {
                sha256: shas.get(&a.id).cloned().unwrap_or_default(),
                id: a.id,
                name: a.name,
                size: a.size.max(0) as u64,
                created_at: a.created_at,
                expired: a.expired,
            })
            .collect())
    }
}

fn env_info(e: crate::environments::EnvironmentView) -> EnvironmentInfo {
    EnvironmentInfo {
        name: e.name,
        wait_minutes: e.wait_minutes,
        branches: e.branches,
        reviewers: e.reviewers.into_iter().map(|(user_id, login)| EnvironmentReviewer { user_id, login }).collect(),
        secrets: e.secrets.into_iter().map(|(name, updated_at)| SecretMeta { name, updated_at }).collect(),
    }
}
