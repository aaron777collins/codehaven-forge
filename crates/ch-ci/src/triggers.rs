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
    /// Who approved this exact commit of a pull request from a fork, when that let it run.
    pub approved_by: Option<UserId>,
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
            approved_by: t.approved_by,
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
        DomainEvent::Push { repo_id, git_ref, before, after, pusher, forced, app, .. } => {
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
                approved_by: None,
                base_ref: String::new(),
                head_ref: String::new(),
                trigger_id,
                head_branch: git_ref.strip_prefix("refs/heads/").map(String::from),
                sync_schedules: *git_ref == default_ref,
            };
            fire(ctx, &t, None).await
        }
        DomainEvent::PrOpened { repo_id, pull_id, number, app, head_sha, pusher, .. } => {
            let origin = HeadOrigin { sha: head_sha.clone(), known: true, pusher: *pusher, app: *app, built_on: None };
            pull_request(ctx, *repo_id, pull_id.0, *number, "opened", trigger_id, origin).await
        }
        DomainEvent::PrSynchronized { repo_id, pull_id, number, app, pusher, head_sha, origin_known, built_on } => {
            let origin =
                HeadOrigin { sha: Some(head_sha.clone()), known: *origin_known, pusher: *pusher, app: *app, built_on: built_on.clone() };
            pull_request(ctx, *repo_id, pull_id.0, *number, "synchronize", trigger_id, origin).await
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
                approved_by: None,
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

async fn has_write(ctx: &Ctx, user: UserId, repo: RepoId) -> bool {
    let who = Actor::User { id: user, is_admin: false, has_2fa: false, via: AuthVia::Session };
    ctx.authz.repo_permission(&who, repo).await.unwrap_or(Permission::None) >= Permission::Write
}

/// Whether someone approved this commit of a pull request from a fork: `Some(approver)` when
/// they did (`Some(None)` when that account is gone since), `None` when nobody has.
async fn fork_approval(ctx: &Ctx, pull_id: i64, sha: &str) -> Result<Option<Option<UserId>>> {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT approved_by FROM fork_run_approvals WHERE pull_id = $1 AND head_sha = $2 AND approved_at IS NOT NULL")
            .bind(pull_id)
            .bind(sha)
            .fetch_optional(&ctx.db)
            .await?;
    Ok(row.map(|(by,)| by.map(UserId)))
}

/// Whether any workflow at `sha` listens to the event. Reads the files and nothing else: no
/// workflow row, run or expression is touched.
async fn any_workflow_listens(ctx: &Ctx, info: &RepoCi, repo: RepoId, sha: &str, ev: &EventInfo) -> Result<bool> {
    let found = discovery::discover(ctx, repo, sha, info.read_github_workflows).await?;
    Ok(found.iter().any(|d| d.workflow.as_ref().is_ok_and(|w| w.matches(ev))))
}

/// Who put a pull request's head commit there, as the event that names it says (ch-x12).
struct HeadOrigin {
    /// The commit the event is about. When the pull request's head is another commit by the
    /// time the event is handled, `pusher` and `app` say nothing about it, and the event is
    /// skipped: the later commit's own event judges it. None uses the stored head.
    sha: Option<String>,
    /// False when nobody can be named for the commit: CI then waits for a person, fork or not.
    known: bool,
    pusher: Option<UserId>,
    app: Option<ch_core::AppOrigin>,
    /// Set when the server wrote the commit for `pusher` on top of this one (Update branch, a
    /// suggestion, a web edit). The pusher vouches for their own change only, so they count
    /// only if this commit was itself let run or approved for the pull request.
    built_on: Option<String>,
}

/// The reason a pull request's run waits when nobody can be named for its commit.
const UNKNOWN_PUSHER: &str = "It is not known who pushed this commit";
/// The reason it waits when the server built it on a commit nobody let run.
const BUILT_ON_HELD: &str = "It builds on a commit that waits for approval";

/// Whether CI let this commit of the pull request run: at once, because its author and pusher
/// could write, or after someone with write access approved it.
async fn cleared(ctx: &Ctx, pull_id: i64, sha: &str) -> Result<bool> {
    let hit: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM pull_cleared_commits WHERE pull_id = $1 AND head_sha = $2
         UNION ALL
         SELECT 1 FROM fork_run_approvals WHERE pull_id = $1 AND head_sha = $2 AND approved_at IS NOT NULL
         LIMIT 1",
    )
    .bind(pull_id)
    .bind(sha)
    .fetch_optional(&ctx.db)
    .await?;
    Ok(hit.is_some())
}

async fn pull_request(
    ctx: &Ctx,
    repo: RepoId,
    pull_id: i64,
    number: i64,
    action: &str,
    trigger_id: String,
    origin: HeadOrigin,
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
    if origin.sha.as_deref().is_some_and(|s| s != head_sha) {
        // The head moved on after this event. Running the stored head on it would count this
        // event's pusher for a commit someone else may have pushed (ch-x12).
        return Ok(Vec::new());
    }
    let HeadOrigin { known, pusher, app, built_on, .. } = origin;
    // A commit the server built for the pusher holds the commit under it too. Unless that one
    // was let run or approved, nobody can be named for all of this code, so CI waits (ch-x12).
    let built_on_held = match &built_on {
        Some(under) => !cleared(ctx, pull_id, under).await?,
        None => false,
    };
    let known = known && !built_on_held;
    // A branch of this repository is stored with no head repository (NULL), as ch-collab does;
    // that is not a fork. `from_fork` stays set after a fork is deleted and its head_repo_id
    // goes to NULL, so such a pull request still runs as a fork (migration 0122).
    let fork = from_fork || head_repo.is_some_and(|h| h != repo.0);
    let author_id = UserId(author);
    let login = user_login(ctx, author_id).await?.unwrap_or_default();
    // A pull request opened or pushed to by a connected app without the workflows permission
    // waits for a person.
    let app_unallowed = app.is_some_and(|a| !a.ci_allowed);
    // A fork's code runs at once only when both the pull request's author and whoever pushed
    // this commit have write access here, and no app without the workflows permission pushed
    // it. An unknown pusher counts as not having write access (ch-x12).
    let fork_held = fork
        && (app_unallowed
            || !known
            || !(has_write(ctx, author_id, repo).await
                && match pusher {
                    Some(p) if p == author_id => true,
                    Some(p) => has_write(ctx, p, repo).await,
                    None => false,
                }));
    let changed = ctx.source.changed_paths(repo, &base_sha, &head_sha).await.unwrap_or(None);
    let base_branch = base_ref.strip_prefix("refs/heads/").unwrap_or(&base_ref).to_string();
    let head_branch = head_ref.strip_prefix("refs/heads/").unwrap_or(&head_ref).to_string();
    let ev = EventInfo::pull_request(action, &base_branch, changed);
    let mut approved_by = None;
    if fork_held {
        match fork_approval(ctx, pull_id, &head_sha).await? {
            Some(by) => approved_by = by,
            None => {
                // Nothing exists until someone with write access approves this exact commit: no
                // run, job or check row, no expression evaluated with the repository's
                // variables, no concurrency group joined. Only the request is recorded, so the
                // pull request can show it and a person can approve it (ch-x12).
                if any_workflow_listens(ctx, &info, repo, &head_sha, &ev).await? {
                    let mut tx = ctx.db.begin().await?;
                    sqlx::query(
                        "INSERT INTO fork_run_approvals (repo_id, pull_id, head_sha, action, trigger_id) VALUES ($1, $2, $3, $4, $5)
                         ON CONFLICT (pull_id, head_sha) DO NOTHING",
                    )
                    .bind(repo.0)
                    .bind(pull_id)
                    .bind(&head_sha)
                    .bind(action)
                    .bind(&trigger_id)
                    .execute(&mut *tx)
                    .await?;
                    // An older commit that was never approved no longer can be.
                    sqlx::query("DELETE FROM fork_run_approvals WHERE pull_id = $1 AND head_sha <> $2 AND approved_at IS NULL")
                        .bind(pull_id)
                        .bind(&head_sha)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                }
                return Ok(Vec::new());
            }
        }
    }
    // Outside a fork, an app's run is made and held until a person approves it, and so is a
    // run for a commit nobody can be named for, since an app may have pushed it. A fork's commit
    // was approved above, before anything was made.
    let needs_approval = !fork && (app_unallowed || !known);
    let hold_reason = match (needs_approval, known) {
        (false, _) => None,
        (true, true) => Some("Started by a connected app".to_string()),
        (true, false) if built_on_held => Some(BUILT_ON_HELD.to_string()),
        (true, false) => Some(UNKNOWN_PUSHER.to_string()),
    };
    if !needs_approval {
        // This commit runs without asking anyone (a fork's only once approved), so a commit the
        // server later builds on it may count its own pusher.
        sqlx::query("INSERT INTO pull_cleared_commits (pull_id, head_sha) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(pull_id)
            .bind(&head_sha)
            .execute(&ctx.db)
            .await?;
    }
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
        info: ev,
        actor: Some((author_id, login)),
        title,
        payload,
        inputs: Json::Null,
        fork,
        needs_approval,
        hold_reason,
        approved_by,
        base_ref: base_branch,
        head_ref: head_branch,
        trigger_id,
        head_branch: None,
        sync_schedules: false,
    };
    fire(ctx, &t, None).await
}

/// The latest commit of an open pull request from a fork when it waits for someone with write
/// access to approve it.
pub async fn pending_fork_approval(ctx: &Ctx, repo: RepoId, number: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT a.head_sha FROM fork_run_approvals a
           JOIN pull_requests pr ON pr.issue_id = a.pull_id AND pr.head_sha = a.head_sha
           JOIN issues i ON i.id = pr.issue_id
          WHERE pr.repo_id = $1 AND i.number = $2 AND i.open AND a.approved_at IS NULL",
    )
    .bind(repo.0)
    .bind(number)
    .fetch_optional(&ctx.db)
    .await?)
}

/// A person with write access approves one commit of a pull request from a fork: its runs are
/// created now, from the event that asked for them. Only the pull request's latest commit can be
/// approved, once; a later commit needs its own approval.
pub async fn approve_fork_commit(ctx: &Ctx, repo: RepoId, number: i64, sha: &str, approver: UserId) -> Result<Vec<RunId>> {
    let mut tx = ctx.db.begin().await?;
    let pull: Option<(i64, String, bool)> = sqlx::query_as(
        "SELECT pr.issue_id, pr.head_sha, i.open FROM pull_requests pr JOIN issues i ON i.id = pr.issue_id
          WHERE pr.repo_id = $1 AND i.number = $2 FOR UPDATE OF pr",
    )
    .bind(repo.0)
    .bind(number)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((pull_id, head, open)) = pull else { return Err(Error::NotFound) };
    if !open {
        return Err(Error::conflict("This pull request is closed."));
    }
    if head != sha.trim() {
        return Err(Error::invalid("Approval", "head_sha", "That is not the pull request's latest commit. Approve the latest one."));
    }
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT action, trigger_id, approved_at IS NOT NULL FROM fork_run_approvals WHERE pull_id = $1 AND head_sha = $2 FOR UPDATE",
    )
    .bind(pull_id)
    .bind(&head)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((action, trigger_id, done)) = row else {
        return Err(Error::conflict("Nothing is waiting for approval on this commit."));
    };
    if done {
        return Err(Error::conflict("This commit is already approved."));
    }
    sqlx::query("UPDATE fork_run_approvals SET approved_by = $3, approved_at = now() WHERE pull_id = $1 AND head_sha = $2")
        .bind(pull_id)
        .bind(&head)
        .bind(approver.0)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // The approval is for this exact commit; nobody is named as its pusher.
    let origin = HeadOrigin { sha: Some(head), known: false, pusher: None, app: None, built_on: None };
    pull_request(ctx, repo, pull_id, number, &action, trigger_id, origin).await
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
