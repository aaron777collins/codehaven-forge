//! The Actions pages' view of CI: `ch_core::web_contract::Pipelines`.
//!
//! The web layer asks for workflows, jobs with their steps, runners, secrets, variables and
//! artifacts in the shapes its pages draw. Everything here goes through the same functions the
//! REST handlers use, and checks the same permissions: reading needs `Read`, changing secrets
//! and variables needs `Admin` on the repository, and runners are for site admins (or, for one
//! repository, its admins).

use crate::secrets::{self, Scope};
use crate::service::{CiImpl, runner_summaries};
use async_trait::async_trait;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::ClientInfo;
use ch_core::services::Ci;
use ch_core::web_contract::{Artifact, Pipelines, Runner, SecretMeta, Variable, WorkflowDef, WorkflowInput, WorkflowJob, WorkflowStep};
use ch_core::{Actor, Error, Permission, RepoId, Result, RunId, RunnerId};
use std::collections::HashMap;

impl CiImpl {
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
