//! Webhook payloads. Field names follow GitHub's for the fields common tools read: the
//! repository, the sender, and the issue, pull request, comment, review, release or status
//! the event is about. Every payload is built from the database when the event is queued, so
//! a delivery shows what was true at that moment.

use ch_api_types::common::SimpleUser;
use ch_api_types::repos::Repository;
use ch_core::error::{Error, Result};
use ch_core::events::DomainEvent;
use ch_core::ids::*;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

/// A payload ready to queue.
#[derive(Debug, Clone)]
pub struct Built {
    pub repo_id: Option<RepoId>,
    /// The hook event name, such as `issues`.
    pub event: &'static str,
    pub action: Option<String>,
    pub body: Value,
}

pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

pub async fn user_json(pool: &PgPool, base: &str, id: i64) -> Result<SimpleUser> {
    let row: Option<(String, String, bool)> =
        sqlx::query_as("SELECT username::text, kind, is_admin FROM users WHERE id = $1").bind(id).fetch_optional(pool).await?;
    let (login, kind, admin) = row.ok_or(Error::NotFound)?;
    Ok(SimpleUser {
        avatar_url: ch_api_types::avatar_url(base, &login),
        url: format!("{base}/api/v3/users/{login}"),
        html_url: format!("{base}/{login}"),
        kind: match kind.as_str() {
            "org" => "Organization",
            "bot" => "Bot",
            _ => "User",
        }
        .into(),
        site_admin: admin,
        login,
        id,
    })
}

#[derive(sqlx::FromRow)]
struct RepoRow {
    id: i64,
    owner_id: i64,
    owner: String,
    name: String,
    description: String,
    visibility: String,
    contribution_policy: String,
    default_branch: String,
    archived: bool,
    is_empty: bool,
    fork: bool,
    topics: Vec<String>,
    has_issues: bool,
    size_kb: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    pushed_at: Option<DateTime<Utc>>,
    allow_squash_merge: bool,
    allow_merge_commit: bool,
    allow_rebase_merge: bool,
    delete_branch_on_merge: bool,
    stars: i64,
    watchers: i64,
    forks: i64,
    open_issues: i64,
}

pub async fn repo_json(pool: &PgPool, base: &str, ssh: &str, repo: i64) -> Result<Repository> {
    let r: RepoRow = sqlx::query_as(
        "SELECT r.id, r.owner_id, o.username::text AS owner, r.name::text AS name, r.description, r.access_tier AS visibility, r.contribution_policy, r.default_branch, r.archived, r.is_empty,
                (r.fork_parent_id IS NOT NULL) AS fork, r.topics, r.has_issues, r.size_kb, r.created_at, r.updated_at, r.pushed_at,
                r.allow_squash_merge, r.allow_merge_commit, r.allow_rebase_merge, r.delete_branch_on_merge,
                (SELECT count(*) FROM stars s WHERE s.repo_id = r.id) AS stars,
                (SELECT count(*) FROM watches w WHERE w.repo_id = r.id AND w.level <> 'ignore') AS watchers,
                (SELECT count(*) FROM repositories f WHERE f.fork_parent_id = r.id) AS forks,
                (SELECT count(*) FROM issues i WHERE i.repo_id = r.id AND i.open AND NOT i.is_pull) AS open_issues
         FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = $1",
    )
    .bind(repo)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::NotFound)?;
    let full = format!("{}/{}", r.owner, r.name);
    Ok(Repository {
        id: r.id,
        name: r.name.clone(),
        full_name: full.clone(),
        owner: user_json(pool, base, r.owner_id).await?,
        private: r.visibility != "public",
        html_url: format!("{base}/{full}"),
        description: Some(r.description).filter(|d| !d.is_empty()),
        fork: r.fork,
        url: format!("{base}/api/v3/repos/{full}"),
        clone_url: format!("{base}/{full}.git"),
        ssh_url: format!("ssh://git@{ssh}/{full}.git"),
        default_branch: r.default_branch,
        visibility: r.visibility,
        contribution_policy: r.contribution_policy,
        archived: r.archived,
        disabled: false,
        topics: r.topics,
        has_issues: r.has_issues,
        created_at: r.created_at,
        updated_at: r.updated_at,
        pushed_at: r.pushed_at,
        size: r.size_kb,
        stargazers_count: r.stars,
        watchers_count: r.watchers,
        forks_count: r.forks,
        open_issues_count: r.open_issues,
        permissions: None,
        parent: None,
        empty: r.is_empty,
        allow_squash_merge: r.allow_squash_merge,
        allow_merge_commit: r.allow_merge_commit,
        allow_rebase_merge: r.allow_rebase_merge,
        delete_branch_on_merge: r.delete_branch_on_merge,
    })
}

#[derive(sqlx::FromRow)]
struct IssueRow {
    id: i64,
    number: i64,
    is_pull: bool,
    author_id: i64,
    title: String,
    body: String,
    open: bool,
    state_reason: Option<String>,
    locked: bool,
    comments_count: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
    labels: Vec<String>,
    assignees: Vec<i64>,
}

async fn issue_row(pool: &PgPool, issue: i64) -> Result<IssueRow> {
    sqlx::query_as(
        "SELECT i.id, i.number, i.is_pull, i.author_id, i.title, i.body, i.open, i.state_reason, i.locked, i.comments_count, i.created_at, i.updated_at, i.closed_at,
                COALESCE((SELECT array_agg(l.name::text ORDER BY l.name) FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id), '{}') AS labels,
                COALESCE((SELECT array_agg(a.user_id) FROM issue_assignees a WHERE a.issue_id = i.id), '{}') AS assignees
         FROM issues i WHERE i.id = $1",
    )
    .bind(issue)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::NotFound)
}

pub async fn issue_json(pool: &PgPool, base: &str, full_name: &str, issue: i64) -> Result<Value> {
    let i = issue_row(pool, issue).await?;
    let user = user_json(pool, base, i.author_id).await?;
    let mut assignees = Vec::new();
    for a in &i.assignees {
        assignees.push(user_json(pool, base, *a).await?);
    }
    let path = if i.is_pull { "pull" } else { "issues" };
    let mut v = json!({
        "id": i.id, "number": i.number, "title": i.title, "body": i.body,
        "state": if i.open { "open" } else { "closed" },
        "state_reason": i.state_reason, "locked": i.locked, "comments": i.comments_count,
        "user": user, "labels": i.labels.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
        "assignees": assignees, "assignee": assignees.first(),
        "created_at": i.created_at, "updated_at": i.updated_at, "closed_at": i.closed_at,
        "url": format!("{base}/api/v3/repos/{full_name}/issues/{}", i.number),
        "html_url": format!("{base}/{full_name}/{path}/{}", i.number),
    });
    if i.is_pull {
        v["pull_request"] = json!({"url": format!("{base}/api/v3/repos/{full_name}/pulls/{}", i.number), "html_url": format!("{base}/{full_name}/pull/{}", i.number)});
    }
    Ok(v)
}

pub async fn pull_json(pool: &PgPool, base: &str, full_name: &str, issue: i64) -> Result<Value> {
    let mut v = issue_json(pool, base, full_name, issue).await?;
    #[derive(sqlx::FromRow)]
    struct P {
        head_ref: String,
        head_sha: String,
        base_ref: String,
        base_sha: String,
        head_full: Option<String>,
        merged_at: Option<DateTime<Utc>>,
        merge_commit_sha: Option<String>,
        merged_by: Option<i64>,
        draft: bool,
        mergeable_state: String,
    }
    let p: P = sqlx::query_as(
        "SELECT p.head_ref, p.head_sha, p.base_ref, p.base_sha,
                (SELECT o.username::text || '/' || r.name::text FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = p.head_repo_id) AS head_full,
                p.merged_at, p.merge_commit_sha, p.merged_by, p.draft, p.mergeable_state
         FROM pull_requests p WHERE p.issue_id = $1",
    )
    .bind(issue)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::NotFound)?;
    let merged_by = match p.merged_by {
        Some(u) => Some(user_json(pool, base, u).await?),
        None => None,
    };
    v["head"] = json!({"ref": p.head_ref, "sha": p.head_sha, "label": format!("{}:{}", p.head_full.as_deref().unwrap_or(full_name), p.head_ref), "full_name": p.head_full});
    v["base"] = json!({"ref": p.base_ref, "sha": p.base_sha, "label": format!("{full_name}:{}", p.base_ref), "full_name": full_name});
    v["merged"] = json!(p.merged_at.is_some());
    v["merged_at"] = json!(p.merged_at);
    v["merged_by"] = json!(merged_by);
    v["merge_commit_sha"] = json!(p.merge_commit_sha);
    v["draft"] = json!(p.draft);
    v["mergeable_state"] = json!(p.mergeable_state);
    Ok(v)
}

fn ref_type(git_ref: &str) -> (&'static str, String) {
    if let Some(b) = git_ref.strip_prefix("refs/heads/") {
        ("branch", b.to_string())
    } else if let Some(t) = git_ref.strip_prefix("refs/tags/") {
        ("tag", t.to_string())
    } else {
        ("branch", git_ref.to_string())
    }
}

pub struct Ctx<'a> {
    pub pool: &'a PgPool,
    pub base: &'a str,
    pub ssh: &'a str,
}

/// Builds the payload for an event, or `None` when the event is not one hooks can see.
pub async fn build(cx: &Ctx<'_>, ev: &DomainEvent) -> Result<Option<Built>> {
    use DomainEvent::*;
    let (pool, base) = (cx.pool, cx.base);
    let repo_of = |r: RepoId| async move { repo_json(cx.pool, cx.base, cx.ssh, r.0).await };
    let built = match ev {
        Push { repo_id, git_ref, before, after, pusher, forced, .. } => {
            let repository = repo_of(*repo_id).await?;
            let sender = match pusher {
                Some(u) => user_json(pool, base, u.0).await?,
                None => system_user(base),
            };
            let (rt, short) = ref_type(git_ref);
            let _ = rt;
            let compare =
                format!("{}/compare/{}...{}", repository.html_url, &before[..before.len().min(12)], &after[..after.len().min(12)]);
            Some(Built {
                repo_id: Some(*repo_id),
                event: "push",
                action: None,
                body: json!({
                    "ref": git_ref, "before": before, "after": after,
                    "created": before == ZERO_SHA, "deleted": after == ZERO_SHA, "forced": forced,
                    "base_ref": null, "compare": compare, "commits": [], "head_commit": null,
                    "repository": repository, "pusher": {"name": sender.login, "email": null}, "sender": sender, "branch": short,
                }),
            })
        }
        RefCreated { repo_id, git_ref, actor, .. } | RefDeleted { repo_id, git_ref, actor, .. } => {
            let created = matches!(ev, RefCreated { .. });
            let repository = repo_of(*repo_id).await?;
            let sender = match actor {
                Some(u) => user_json(pool, base, u.0).await?,
                None => system_user(base),
            };
            let (rt, short) = ref_type(git_ref);
            Some(Built {
                repo_id: Some(*repo_id),
                event: if created { "create" } else { "delete" },
                action: None,
                body: json!({"ref": short, "ref_type": rt, "master_branch": repository.default_branch, "description": repository.description, "repository": repository, "sender": sender}),
            })
        }
        IssueOpened { repo_id, issue_id, actor, .. }
        | IssueClosed { repo_id, issue_id, actor, .. }
        | IssueReopened { repo_id, issue_id, actor, .. }
        | IssueEdited { repo_id, issue_id, actor, .. }
        | IssueLabeled { repo_id, issue_id, actor, .. }
        | IssueAssigned { repo_id, issue_id, actor, .. } => {
            let repository = repo_of(*repo_id).await?;
            let issue = issue_json(pool, base, &repository.full_name, issue_id.0).await?;
            let mut body = json!({"issue": issue, "repository": repository, "sender": user_json(pool, base, actor.0).await?});
            let action = match ev {
                IssueOpened { .. } => "opened",
                IssueClosed { .. } => "closed",
                IssueReopened { .. } => "reopened",
                IssueEdited { .. } => "edited",
                IssueLabeled { label, .. } => {
                    body["label"] = json!({"name": label});
                    "labeled"
                }
                IssueAssigned { assignee, .. } => {
                    body["assignee"] = json!(user_json(pool, base, assignee.0).await?);
                    "assigned"
                }
                _ => "updated",
            };
            body["action"] = json!(action);
            Some(Built { repo_id: Some(*repo_id), event: "issues", action: Some(action.into()), body })
        }
        CommentCreated { repo_id, issue_id, comment_id, actor, .. } => {
            let repository = repo_of(*repo_id).await?;
            let issue = issue_json(pool, base, &repository.full_name, issue_id.0).await?;
            let c: Option<(String, i64, DateTime<Utc>, DateTime<Utc>)> =
                sqlx::query_as("SELECT body, author_id, created_at, updated_at FROM issue_comments WHERE id = $1")
                    .bind(comment_id.0)
                    .fetch_optional(pool)
                    .await?;
            let Some((text, author, created, updated)) = c else { return Ok(None) };
            let number = issue["number"].as_i64().unwrap_or(0);
            let comment = json!({"id": comment_id.0, "body": text, "user": user_json(pool, base, author).await?, "created_at": created, "updated_at": updated,
                "html_url": format!("{}/issues/{number}#issuecomment-{}", repository.html_url, comment_id.0)});
            Some(Built {
                repo_id: Some(*repo_id),
                event: "issue_comment",
                action: Some("created".into()),
                body: json!({"action": "created", "issue": issue, "comment": comment, "repository": repository, "sender": user_json(pool, base, actor.0).await?}),
            })
        }
        PrOpened { repo_id, pull_id, actor, .. } => pr_event(cx, *repo_id, *pull_id, "opened", Some(*actor)).await?,
        PrSynchronized { repo_id, pull_id, .. } => pr_event(cx, *repo_id, *pull_id, "synchronize", None).await?,
        PrClosed { repo_id, pull_id, actor, .. } => pr_event(cx, *repo_id, *pull_id, "closed", *actor).await?,
        PrMerged { repo_id, pull_id, actor, .. } => pr_event(cx, *repo_id, *pull_id, "closed", Some(*actor)).await?,
        ReviewSubmitted { repo_id, pull_id, review_id, actor, .. } => {
            let repository = repo_of(*repo_id).await?;
            let pr = pull_json(pool, base, &repository.full_name, pull_id.0).await?;
            #[allow(clippy::type_complexity)]
            let r: Option<(String, String, String, i64, Option<DateTime<Utc>>)> =
                sqlx::query_as("SELECT state, body, commit_sha, author_id, submitted_at FROM reviews WHERE id = $1")
                    .bind(review_id.0)
                    .fetch_optional(pool)
                    .await?;
            let Some((state, text, sha, author, at)) = r else { return Ok(None) };
            let review = json!({"id": review_id.0, "state": state, "body": text, "commit_id": sha, "user": user_json(pool, base, author).await?, "submitted_at": at,
                "html_url": format!("{}/pull/{}#pullrequestreview-{}", repository.html_url, pr["number"], review_id.0)});
            Some(Built {
                repo_id: Some(*repo_id),
                event: "pull_request_review",
                action: Some("submitted".into()),
                body: json!({"action": "submitted", "review": review, "pull_request": pr, "repository": repository, "sender": user_json(pool, base, actor.0).await?}),
            })
        }
        ReviewCommentCreated { repo_id, pull_id, comment_id, actor, .. } => {
            let repository = repo_of(*repo_id).await?;
            let pr = pull_json(pool, base, &repository.full_name, pull_id.0).await?;
            #[allow(clippy::type_complexity)]
            let c: Option<(String, String, Option<i32>, String, String, i64, DateTime<Utc>)> =
                sqlx::query_as("SELECT path, side, line, diff_hunk, body, author_id, created_at FROM review_comments WHERE id = $1")
                    .bind(comment_id.0)
                    .fetch_optional(pool)
                    .await?;
            let Some((path, side, line, hunk, text, author, at)) = c else { return Ok(None) };
            let comment = json!({"id": comment_id.0, "path": path, "side": side.to_uppercase(), "line": line, "diff_hunk": hunk, "body": text, "user": user_json(pool, base, author).await?, "created_at": at});
            Some(Built {
                repo_id: Some(*repo_id),
                event: "pull_request_review_comment",
                action: Some("created".into()),
                body: json!({"action": "created", "comment": comment, "pull_request": pr, "repository": repository, "sender": user_json(pool, base, actor.0).await?}),
            })
        }
        ReleasePublished { repo_id, release_id, tag, actor } => {
            let repository = repo_of(*repo_id).await?;
            #[allow(clippy::type_complexity)]
            let r: Option<(String, String, String, bool, bool, Option<DateTime<Utc>>)> =
                sqlx::query_as("SELECT target_commitish, name, body, draft, prerelease, published_at FROM releases WHERE id = $1")
                    .bind(release_id.0)
                    .fetch_optional(pool)
                    .await?;
            let Some((target, name, text, draft, pre, at)) = r else { return Ok(None) };
            let release = json!({"id": release_id.0, "tag_name": tag, "target_commitish": target, "name": name, "body": text, "draft": draft, "prerelease": pre, "published_at": at,
                "html_url": format!("{}/releases/tag/{tag}", repository.html_url)});
            Some(Built {
                repo_id: Some(*repo_id),
                event: "release",
                action: Some("published".into()),
                body: json!({"action": "published", "release": release, "repository": repository, "sender": user_json(pool, base, actor.0).await?}),
            })
        }
        StatusCreated { repo_id, sha, context, .. } => {
            let repository = repo_of(*repo_id).await?;
            #[allow(clippy::type_complexity)]
            let s: Option<(i64, String, Option<String>, Option<String>, Option<i64>, DateTime<Utc>)> = sqlx::query_as(
                "SELECT id, state, description, target_url, creator_id, created_at FROM commit_statuses WHERE repo_id = $1 AND sha = $2 AND context = $3 ORDER BY id DESC LIMIT 1",
            )
            .bind(repo_id.0)
            .bind(sha)
            .bind(context)
            .fetch_optional(pool)
            .await?;
            let Some((id, state, description, target_url, creator, at)) = s else { return Ok(None) };
            let sender = match creator {
                Some(u) => user_json(pool, base, u).await?,
                None => system_user(base),
            };
            Some(Built {
                repo_id: Some(*repo_id),
                event: "status",
                action: None,
                body: json!({"id": id, "sha": sha, "name": repository.full_name, "context": context, "description": description, "state": state, "target_url": target_url,
                    "created_at": at, "updated_at": at, "repository": repository, "sender": sender}),
            })
        }
        MemberAdded { repo_id: Some(repo_id), user_id, actor, .. } => {
            let repository = repo_of(*repo_id).await?;
            let sender = match actor {
                Some(u) => user_json(pool, base, u.0).await?,
                None => system_user(base),
            };
            Some(Built {
                repo_id: Some(*repo_id),
                event: "member",
                action: Some("added".into()),
                body: json!({"action": "added", "member": user_json(pool, base, user_id.0).await?, "repository": repository, "sender": sender}),
            })
        }
        WorkflowRunUpdated { repo_id, run_id, status, conclusion } => {
            let repository = repo_of(*repo_id).await?;
            #[allow(clippy::type_complexity)]
            let r: Option<(i64, String, String, Option<String>, i64, Option<i64>, DateTime<Utc>)> = sqlx::query_as(
                "SELECT run_number, event, head_sha, head_branch, id, actor_id, created_at FROM workflow_runs WHERE id = $1",
            )
            .bind(run_id.0)
            .fetch_optional(pool)
            .await?;
            let Some((number, trigger, sha, branch, id, actor, at)) = r else { return Ok(None) };
            let sender = match actor {
                Some(u) => user_json(pool, base, u).await?,
                None => system_user(base),
            };
            let action = if status == "completed" {
                "completed"
            } else if status == "in_progress" {
                "in_progress"
            } else {
                "requested"
            };
            Some(Built {
                repo_id: Some(*repo_id),
                event: "workflow_run",
                action: Some(action.into()),
                body: json!({"action": action, "workflow_run": {"id": id, "run_number": number, "event": trigger, "head_sha": sha, "head_branch": branch, "status": status, "conclusion": conclusion,
                    "created_at": at, "html_url": format!("{}/actions/runs/{id}", repository.html_url)}, "repository": repository, "sender": sender}),
            })
        }
        WorkflowJobUpdated { repo_id, run_id, job_id, status, conclusion } => {
            let repository = repo_of(*repo_id).await?;
            let j: Option<(String, String)> =
                sqlx::query_as("SELECT job_key, name FROM workflow_jobs WHERE id = $1").bind(job_id.0).fetch_optional(pool).await?;
            let Some((key, name)) = j else { return Ok(None) };
            let action = if status == "completed" {
                "completed"
            } else if status == "in_progress" {
                "in_progress"
            } else {
                "queued"
            };
            Some(Built {
                repo_id: Some(*repo_id),
                event: "workflow_job",
                action: Some(action.into()),
                body: json!({"action": action, "workflow_job": {"id": job_id.0, "run_id": run_id.0, "name": name, "job_key": key, "status": status, "conclusion": conclusion}, "repository": repository, "sender": system_user(base)}),
            })
        }
        CheckRunUpdated { repo_id, sha, name, status, conclusion } => {
            let repository = repo_of(*repo_id).await?;
            let action = if status == "completed" { "completed" } else { "created" };
            Some(Built {
                repo_id: Some(*repo_id),
                event: "check_run",
                action: Some(action.into()),
                body: json!({"action": action, "check_run": {"name": name, "head_sha": sha, "status": status, "conclusion": conclusion}, "repository": repository, "sender": system_user(base)}),
            })
        }
        _ => None,
    };
    Ok(built)
}

async fn pr_event(cx: &Ctx<'_>, repo: RepoId, pull: PullId, action: &'static str, actor: Option<UserId>) -> Result<Option<Built>> {
    let repository = repo_json(cx.pool, cx.base, cx.ssh, repo.0).await?;
    let pr = pull_json(cx.pool, cx.base, &repository.full_name, pull.0).await?;
    let sender_id = match actor {
        Some(a) => a.0,
        None => pr["user"]["id"].as_i64().unwrap_or(0),
    };
    let sender = match user_json(cx.pool, cx.base, sender_id).await {
        Ok(u) => u,
        Err(_) => system_user(cx.base),
    };
    Ok(Some(Built {
        repo_id: Some(repo),
        event: "pull_request",
        action: Some(action.into()),
        body: json!({"action": action, "number": pr["number"], "pull_request": pr, "repository": repository, "sender": sender}),
    }))
}

fn system_user(base: &str) -> SimpleUser {
    SimpleUser {
        login: "codehaven".into(),
        id: 0,
        avatar_url: format!("{base}/favicon.svg"),
        url: format!("{base}/api/v3/users/codehaven"),
        html_url: base.to_string(),
        kind: "Bot".into(),
        site_admin: false,
    }
}
