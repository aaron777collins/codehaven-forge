//! Turning events into runs: the `ci-triggers` consumer of the event outbox, and the same
//! machinery for scheduled and manual runs.

use crate::ctx::Ctx;
use crate::discovery::{self, Discovered, RepoCi, WorkflowRow, repo_ci};
use crate::runs::{NewRun, create_run};
use ch_core::events::{DomainEvent, StoredEvent};
use ch_core::model::CiAvailability;
use ch_core::{Actor, AuthVia, Error, Permission, RepoId, Result, RunId, UserId};
use ch_workflow::trigger::EventInfo;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

pub const CONSUMER: &str = "ci-triggers";

/// What happened, with everything needed to start runs for it.
pub struct Trigger {
    pub repo: RepoId,
    /// The commit to build, and to read workflow files from.
    pub sha: String,
    pub git_ref: String,
    pub info: EventInfo,
    pub actor: Option<(UserId, String)>,
    pub title: String,
    pub payload: Json,
    pub inputs: Json,
    pub fork: bool,
    pub needs_approval: bool,
    pub hold_reason: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
    pub trigger_id: String,
    pub head_branch: Option<String>,
    /// Keep the `schedules` table in step with the default branch while here.
    pub sync_schedules: bool,
}

pub async fn user_login(ctx: &Ctx, id: UserId) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT username::text FROM users WHERE id = $1").bind(id.0).fetch_optional(&ctx.db).await?)
}

fn zero_sha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b'0')
}

/// Starts a run for every workflow that listens to the event. Returns the runs created.
pub async fn fire(ctx: &Ctx, t: &Trigger, only: Option<&str>) -> Result<Vec<RunId>> {
    let info = repo_ci(ctx, t.repo).await?;
    if info.availability() != CiAvailability::Enabled {
        tracing::debug!(repo = t.repo.0, availability = ?info.availability(), "workflows are off here");
        return Ok(Vec::new());
    }
    let found = discovery::discover(ctx, t.repo, &t.sha, info.read_github_workflows).await?;
    let rows = discovery::sync_rows(ctx, t.repo, &found).await?;
    if t.sync_schedules {
        discovery::sync_schedules(ctx, t.repo, &found, &rows, ctx.now()).await?;
    }
    let mut made = Vec::new();
    for d in &found {
        if only.is_some_and(|p| p != d.path) {
            continue;
        }
        let Ok(wf) = &d.workflow else { continue };
        let Some(row) = rows.get(&d.path) else { continue };
        if row.state != "active" || !wf.matches(&t.info) {
            continue;
        }
        let new = NewRun {
            repo_id: t.repo,
            workflow_id: Some(row.id.0),
            workflow_path: d.path.clone(),
            workflow_name: d.name.clone(),
            workflow: wf.clone(),
            event: t.info.name.clone(),
            git_ref: t.git_ref.clone(),
            head_sha: t.sha.clone(),
            head_branch: t.head_branch.clone(),
            actor: t.actor.clone(),
            title: if t.title.is_empty() { d.name.clone() } else { t.title.clone() },
            event_payload: t.payload.clone(),
            inputs: t.inputs.clone(),
            fork: t.fork,
            needs_approval: t.needs_approval,
            hold_reason: t.hold_reason.clone(),
            trigger_id: Some(t.trigger_id.clone()),
            base_ref: t.base_ref.clone(),
            head_ref: t.head_ref.clone(),
        };
        match create_run(ctx, new).await {
            Ok(c) => {
                if !c.already_existed {
                    made.push(c.run_id);
                }
            }
            // One bad workflow (a matrix that expands wrongly) must not stop the others.
            Err(Error::Validation(v)) => {
                tracing::warn!(repo = t.repo.0, workflow = %d.path, problems = ?v, "workflow could not be planned")
            }
            Err(e) => return Err(e),
        }
    }
    Ok(made)
}

fn repo_json(info: &RepoCi) -> Json {
    json!({ "full_name": info.full_name(), "name": info.name, "owner": { "login": info.owner_name }, "default_branch": info.default_branch })
}

/// Handles one event from the outbox. Events this consumer has no use for do nothing.
pub async fn handle_event(ctx: &Ctx, ev: &StoredEvent) -> Result<Vec<RunId>> {
    let trigger_id = format!("evt:{}", ev.id);
    match &ev.event {
        DomainEvent::Push { repo_id, git_ref, before, after, pusher, forced, app } => {
            if zero_sha(after) {
                return Ok(Vec::new());
            }
            let info = repo_ci(ctx, *repo_id).await?;
            if info.availability() != CiAvailability::Enabled {
                return Ok(Vec::new());
            }
            let is_branch = git_ref.starts_with("refs/heads/");
            let default_ref = format!("refs/heads/{}", info.default_branch);
            let changed = if is_branch { ctx.source.changed_paths(*repo_id, before, after).await.unwrap_or(None) } else { None };
            let head = ctx.source.commit(*repo_id, after).await.unwrap_or(None);
            let login = match pusher {
                Some(u) => user_login(ctx, *u).await?.unwrap_or_default(),
                None => String::new(),
            };
            let payload = json!({
                "ref": git_ref, "before": before, "after": after, "created": zero_sha(before), "deleted": false, "forced": forced,
                "repository": repo_json(&info),
                "pusher": { "name": login },
                "sender": { "login": login },
                "head_commit": head.as_ref().map(|c| json!({ "id": c.id, "message": c.message, "author": { "name": c.author_name, "email": c.author_email } })),
            });
            let title = head.as_ref().and_then(|c| c.message.lines().next()).unwrap_or("").to_string();
            let t = Trigger {
                repo: *repo_id,
                sha: after.clone(),
                git_ref: git_ref.clone(),
                info: EventInfo::push(git_ref, changed),
                actor: pusher.map(|u| (u, login)),
                title,
                payload,
                inputs: Json::Null,
                fork: false,
                // A push made by a connected app without the workflows permission starts a run
                // that waits for a person, because the code it would run was written by the app.
                needs_approval: app.is_some_and(|a| !a.ci_allowed),
                hold_reason: app.filter(|a| !a.ci_allowed).map(|_| "Started by a connected app".to_string()),
                base_ref: String::new(),
                head_ref: String::new(),
                trigger_id,
                head_branch: git_ref.strip_prefix("refs/heads/").map(String::from),
                sync_schedules: *git_ref == default_ref,
            };
            fire(ctx, &t, None).await
        }
        DomainEvent::PrOpened { repo_id, pull_id, number, app, .. } => {
            pull_request(ctx, *repo_id, pull_id.0, *number, "opened", trigger_id, *app).await
        }
        DomainEvent::PrSynchronized { repo_id, pull_id, number, app, .. } => {
            pull_request(ctx, *repo_id, pull_id.0, *number, "synchronize", trigger_id, *app).await
        }
        DomainEvent::ReleasePublished { repo_id, tag, actor, .. } => {
            let info = repo_ci(ctx, *repo_id).await?;
            if info.availability() != CiAvailability::Enabled {
                return Ok(Vec::new());
            }
            let sha = match ctx.source.resolve(*repo_id, &format!("refs/tags/{tag}")).await {
                Ok(s) => s,
                Err(Error::NotFound) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            let login = user_login(ctx, *actor).await?.unwrap_or_default();
            let mut ei = EventInfo::simple("release");
            ei.action = Some("published".into());
            let t = Trigger {
                repo: *repo_id,
                sha,
                git_ref: format!("refs/tags/{tag}"),
                info: ei,
                actor: Some((*actor, login.clone())),
                title: format!("Release {tag}"),
                payload: json!({ "action": "published", "release": { "tag_name": tag }, "repository": repo_json(&info), "sender": { "login": login } }),
                inputs: Json::Null,
                fork: false,
                needs_approval: false,
                hold_reason: None,
                base_ref: String::new(),
                head_ref: String::new(),
                trigger_id,
                head_branch: None,
                sync_schedules: false,
            };
            fire(ctx, &t, None).await
        }
        _ => Ok(Vec::new()),
    }
}

type PullRow = (String, i64, bool, Option<i64>, bool, String, String, String, String, bool);

async fn pull_request(
    ctx: &Ctx,
    repo: RepoId,
    pull_id: i64,
    number: i64,
    action: &str,
    trigger_id: String,
    app: Option<ch_core::AppOrigin>,
) -> Result<Vec<RunId>> {
    let info = repo_ci(ctx, repo).await?;
    if info.availability() != CiAvailability::Enabled {
        return Ok(Vec::new());
    }
    let row: Option<PullRow> = sqlx::query_as(
        "SELECT i.title, i.author_id, i.open, pr.head_repo_id, pr.from_fork, pr.head_ref, pr.head_sha, pr.base_ref, pr.base_sha, pr.draft
         FROM pull_requests pr JOIN issues i ON i.id = pr.issue_id WHERE pr.issue_id = $1 AND pr.repo_id = $2",
    )
    .bind(pull_id)
    .bind(repo.0)
    .fetch_optional(&ctx.db)
    .await?;
    let Some((title, author, open, head_repo, from_fork, head_ref, head_sha, base_ref, base_sha, draft)) = row else {
        return Ok(Vec::new());
    };
    if !open {
        return Ok(Vec::new());
    }
    // A branch of this repository is stored with no head repository (NULL), as ch-collab does;
    // that is not a fork. `from_fork` stays set after a fork is deleted and its head_repo_id
    // goes to NULL, so such a pull request still runs as a fork (migration 0122).
    let fork = from_fork || head_repo.is_some_and(|h| h != repo.0);
    let author_id = UserId(author);
    let login = user_login(ctx, author_id).await?.unwrap_or_default();
    // A fork's code runs only after someone with write access says so, unless its author
    // already has write access here.
    let fork_held = fork && {
        let who = Actor::User { id: author_id, is_admin: false, has_2fa: false, via: AuthVia::Session };
        ctx.authz.repo_permission(&who, repo).await.unwrap_or(Permission::None) < Permission::Write
    };
    // A pull request opened or pushed to by a connected app without the workflows permission waits
    // for a person too.
    let app_held = app.is_some_and(|a| !a.ci_allowed);
    let needs_approval = fork_held || app_held;
    let hold_reason = if app_held {
        Some("Started by a connected app".to_string())
    } else if fork_held {
        Some("Pull request from a fork".to_string())
    } else {
        None
    };
    let changed = ctx.source.changed_paths(repo, &base_sha, &head_sha).await.unwrap_or(None);
    let base_branch = base_ref.strip_prefix("refs/heads/").unwrap_or(&base_ref).to_string();
    let head_branch = head_ref.strip_prefix("refs/heads/").unwrap_or(&head_ref).to_string();
    let head_repo_name = match head_repo {
        Some(h) if h != repo.0 => repo_ci(ctx, RepoId(h)).await.map(|i| i.full_name()).unwrap_or_default(),
        _ => info.full_name(),
    };
    let payload = json!({
        "action": action, "number": number,
        "pull_request": {
            "number": number, "title": title, "state": "open", "draft": draft, "user": { "login": login },
            "head": { "ref": head_branch, "sha": head_sha, "repo": { "full_name": head_repo_name } },
            "base": { "ref": base_branch, "sha": base_sha, "repo": { "full_name": info.full_name() } },
        },
        "repository": repo_json(&info),
        "sender": { "login": login },
    });
    let t = Trigger {
        repo,
        sha: head_sha,
        git_ref: format!("refs/pull/{number}/merge"),
        info: EventInfo::pull_request(action, &base_branch, changed),
        actor: Some((author_id, login)),
        title,
        payload,
        inputs: Json::Null,
        fork,
        needs_approval,
        hold_reason,
        base_ref: base_branch,
        head_ref: head_branch,
        trigger_id,
        head_branch: None,
        sync_schedules: false,
    };
    fire(ctx, &t, None).await
}

/// Reads and handles a batch of events. An event that keeps failing is skipped after three
/// tries, with an error in the log, so one bad event cannot block every later one.
pub async fn process_batch(ctx: &Ctx, failures: &mut BTreeMap<i64, u32>) -> Result<usize> {
    let events = ctx.events.poll(CONSUMER, 50).await?;
    let n = events.len();
    for ev in events {
        match handle_event(ctx, &ev).await {
            Ok(runs) => {
                if !runs.is_empty() {
                    tracing::info!(event = ev.id, kind = ev.event.kind(), runs = runs.len(), "started workflow runs");
                }
            }
            Err(e) => {
                let c = failures.entry(ev.id).or_insert(0);
                *c += 1;
                if *c < 3 {
                    tracing::warn!(event = ev.id, attempt = *c, error = %e, "could not handle event, will retry");
                    return Ok(0);
                }
                tracing::error!(event = ev.id, error = %e, "giving up on event");
            }
        }
        failures.remove(&ev.id);
        ctx.events.ack(CONSUMER, ev.id).await?;
    }
    Ok(n)
}

/// A workflow found by name, path, id or file name, for manual runs.
pub fn find_workflow<'a>(found: &'a [Discovered], rows: &BTreeMap<String, WorkflowRow>, which: &str) -> Option<&'a Discovered> {
    let w = which.trim();
    found.iter().find(|d| d.path == w || d.path.rsplit('/').next() == Some(w) || rows.get(&d.path).is_some_and(|r| r.id.0.to_string() == w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_shas() {
        assert!(zero_sha(&"0".repeat(40)));
        assert!(!zero_sha(&format!("{}1", "0".repeat(39))));
        assert!(!zero_sha(""));
    }
}
