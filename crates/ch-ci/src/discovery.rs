//! Finding, reading and validating the workflow files of a repository, and keeping the
//! `workflows` and `schedules` tables in step with them.

use crate::ctx::Ctx;
use ch_core::model::{CiAvailability, WorkflowFile};
use ch_core::{Error, RepoId, Result, WorkflowId};
use ch_workflow::discover::{display_name, select};
use ch_workflow::{Diagnostic, WORKFLOW_DIRS, Workflow, parse};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// The most a workflow file may weigh. The parser enforces the same limit.
const MAX_WORKFLOW_BYTES: u64 = ch_workflow::parse::MAX_FILE_BYTES as u64;
/// At most this many workflow files are read from one commit.
const MAX_WORKFLOW_FILES: usize = 100;

pub struct Discovered {
    pub path: String,
    pub name: String,
    pub workflow: std::result::Result<Workflow, Vec<Diagnostic>>,
}

/// What the repository's settings and owner say about running workflows.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RepoCi {
    pub id: i64,
    pub owner_id: i64,
    pub owner_name: String,
    pub name: String,
    pub default_branch: String,
    pub has_actions: bool,
    pub read_github_workflows: bool,
    pub archived: bool,
    pub can_run_ci: bool,
}

impl RepoCi {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner_name, self.name)
    }
    pub fn availability(&self) -> CiAvailability {
        if !self.has_actions || self.archived {
            CiAvailability::TurnedOff
        } else if !self.can_run_ci {
            CiAvailability::OwnerNotAllowed
        } else {
            CiAvailability::Enabled
        }
    }
}

pub async fn repo_ci(ctx: &Ctx, repo: RepoId) -> Result<RepoCi> {
    sqlx::query_as::<_, RepoCi>(
        "SELECT r.id, r.owner_id, u.username::text AS owner_name, r.name::text AS name, r.default_branch, r.has_actions,
                r.read_github_workflows, r.archived, u.can_run_ci
         FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(repo.0)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or(Error::NotFound)
}

/// Reads and validates every workflow file at `sha`.
pub async fn discover(ctx: &Ctx, repo: RepoId, sha: &str, read_github: bool) -> Result<Vec<Discovered>> {
    let mut listing: Vec<(String, String)> = Vec::new();
    for dir in WORKFLOW_DIRS {
        for name in ctx.source.list_dir(repo, sha, dir).await? {
            listing.push(((*dir).to_string(), name));
        }
    }
    let refs: Vec<(&str, &str)> = listing.iter().map(|(d, n)| (d.as_str(), n.as_str())).collect();
    let mut paths = select(&refs, read_github);
    paths.truncate(MAX_WORKFLOW_FILES);
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let text = match ctx.source.read(repo, sha, &path, MAX_WORKFLOW_BYTES).await {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(Error::Validation(v)) => {
                let msg = v.first().and_then(|f| f.message.clone()).unwrap_or_else(|| "the file cannot be read".into());
                out.push(Discovered { name: display_name(None, &path), path, workflow: Err(vec![Diagnostic::new(0, 0, msg)]) });
                continue;
            }
            Err(e) => return Err(e),
        };
        match parse(&text) {
            Ok(w) => out.push(Discovered { name: display_name(w.name.as_deref(), &path), path, workflow: Ok(w) }),
            Err(d) => {
                // A file that does not parse still has a name, taken from the `name:` line when one is plain.
                let n = text
                    .lines()
                    .find_map(|l| l.strip_prefix("name:"))
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|s| !s.is_empty() && !s.contains("${{"));
                out.push(Discovered { name: display_name(n.as_deref(), &path), path, workflow: Err(d) });
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct WorkflowRow {
    pub id: WorkflowId,
    pub state: String,
}

/// Records the files found in the `workflows` table, so a run can point at a stable id, and
/// returns path to row. A file that disappears keeps its row (and its run history), it is
/// just not offered any more.
pub async fn sync_rows(ctx: &Ctx, repo: RepoId, found: &[Discovered]) -> Result<BTreeMap<String, WorkflowRow>> {
    let mut out = BTreeMap::new();
    for d in found {
        let (id, state): (i64, String) = sqlx::query_as(
            "INSERT INTO workflows (repo_id, path, name) VALUES ($1, $2, $3)
             ON CONFLICT (repo_id, path) DO UPDATE SET name = EXCLUDED.name
             RETURNING id, state",
        )
        .bind(repo.0)
        .bind(&d.path)
        .bind(&d.name)
        .fetch_one(&ctx.db)
        .await?;
        out.insert(d.path.clone(), WorkflowRow { id: WorkflowId(id), state });
    }
    Ok(out)
}

/// Makes the `schedules` table match the cron lines of the default branch. Only valid, active
/// workflows schedule anything. A new schedule first fires at its next matching minute.
pub async fn sync_schedules(
    ctx: &Ctx,
    repo: RepoId,
    found: &[Discovered],
    rows: &BTreeMap<String, WorkflowRow>,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut tx = ctx.db.begin().await?;
    for d in found {
        let Some(row) = rows.get(&d.path) else { continue };
        let crons: Vec<String> = match &d.workflow {
            Ok(w) if row.state == "active" => w.triggers.schedule.iter().map(|s| s.cron.clone()).collect(),
            _ => Vec::new(),
        };
        sqlx::query("DELETE FROM schedules WHERE workflow_id = $1 AND NOT (cron = ANY($2))")
            .bind(row.id.0)
            .bind(&crons)
            .execute(&mut *tx)
            .await?;
        for cron in &crons {
            let Some(next) = ch_workflow::schedule::next_after(cron, now) else { continue };
            sqlx::query(
                "INSERT INTO schedules (workflow_id, repo_id, cron, next_run_at) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (workflow_id, cron) DO NOTHING",
            )
            .bind(row.id.0)
            .bind(repo.0)
            .bind(cron)
            .bind(next)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Turns what was discovered into what the Actions tab lists.
pub fn to_files(found: &[Discovered], rows: &BTreeMap<String, WorkflowRow>) -> Vec<WorkflowFile> {
    found
        .iter()
        .map(|d| {
            let row = rows.get(&d.path);
            let (events, errors, inputs) = match &d.workflow {
                Ok(w) => (
                    w.triggers.event_names().into_iter().map(String::from).collect(),
                    Vec::new(),
                    w.triggers
                        .workflow_dispatch
                        .as_ref()
                        .map(|dp| {
                            dp.inputs
                                .iter()
                                .map(|i| ch_core::model::DispatchInputInfo {
                                    name: i.name.clone(),
                                    description: i.description.clone(),
                                    required: i.required,
                                    kind: match i.kind {
                                        ch_workflow::InputType::String => "string",
                                        ch_workflow::InputType::Boolean => "boolean",
                                        ch_workflow::InputType::Choice => "choice",
                                        ch_workflow::InputType::Number => "number",
                                    }
                                    .into(),
                                    default: i.default.clone(),
                                    options: i.options.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
                Err(diags) => (Vec::new(), diags.iter().map(Diagnostic::to_string).collect(), Vec::new()),
            };
            WorkflowFile {
                id: row.map(|r| r.id),
                path: d.path.clone(),
                name: d.name.clone(),
                state: row.map(|r| r.state.clone()).unwrap_or_else(|| "active".into()),
                events,
                errors,
                dispatch_inputs: inputs,
            }
        })
        .collect()
}
