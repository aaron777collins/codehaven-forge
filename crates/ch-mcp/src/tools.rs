//! The 46 tools. Each one calls the same services the web pages and the REST API call, as the
//! connected app's `Actor`, so every permission rule lives in a service and no tool can bypass
//! it. A tool does three things on its own: it shapes results for a model, it checks the
//! revision it was given, and it keeps what other people wrote under `untrusted`.

use crate::registry::Args;
use crate::shape;
use ch_core::apps::{Apps, GrantView};
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::services::*;
use ch_core::state::AppState;
use ch_core::{Actor, Error, NotificationId, RepoId, RunId};
use serde_json::{Value, json};
use std::sync::Arc;

/// The services a tool may use. Deliberately narrow: nothing about accounts, sign-in, hooks,
/// organizations or the directory is reachable from here.
#[derive(Clone)]
pub struct ToolServices {
    pub repos: Arc<dyn Repos>,
    pub git: Arc<dyn GitService>,
    pub issues: Arc<dyn Issues>,
    pub pulls: Arc<dyn Pulls>,
    pub reviews: Arc<dyn Reviews>,
    pub search: Arc<dyn Search>,
    pub notifications: Arc<dyn Notifications>,
    pub checks: Arc<dyn Checks>,
    pub ci: Arc<dyn Ci>,
    pub authz: Arc<dyn Authz>,
    pub apps: Arc<dyn Apps>,
    pub audit: Arc<dyn Audit>,
    /// The instance origin, for the noreply address on commits the connector makes.
    pub external_url: String,
}

impl ToolServices {
    pub fn from_state(s: &AppState) -> Self {
        Self {
            repos: s.repos.clone(),
            git: s.git.clone(),
            issues: s.issues.clone(),
            pulls: s.pulls.clone(),
            reviews: s.reviews.clone(),
            search: s.search.clone(),
            notifications: s.notifications.clone(),
            checks: s.checks.clone(),
            ci: s.ci.clone(),
            authz: s.authz.clone(),
            apps: s.apps.clone(),
            audit: s.audit.clone(),
            external_url: s.config.server.external_url.clone(),
        }
    }
}

/// What a tool returns: the server's own words and the words of other people, kept apart.
pub struct Output {
    pub meta: Value,
    pub untrusted: Value,
    /// For the audit row: the repository the call touched, and a number or id.
    pub repo_id: Option<RepoId>,
    pub item: Option<i64>,
}

/// A failure with only words that are safe to show.
#[derive(Debug)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(m: impl Into<String>) -> Self {
        ToolError(m.into())
    }
}

const WRITE_HINT: &str = " This connection may not have been given that. Open Settings, Connected apps on the Codehaven site, and connect again with more access.";

/// Turns a service error into a message for the model. Internal errors say nothing about the
/// cause, and "not found" stays plain so a repository that cannot be read looks like one that
/// does not exist.
pub fn public_error(e: &Error, actor: &Actor) -> ToolError {
    let app = actor.is_app();
    ToolError(match e {
        Error::NotFound => "Not found.".to_string(),
        Error::Forbidden(m) => {
            if app {
                format!("{m}{WRITE_HINT}")
            } else {
                m.clone()
            }
        }
        Error::Validation(errs) => {
            let msg =
                errs.iter().map(|f| f.message.clone().unwrap_or_else(|| format!("{} {}", f.field, f.code))).collect::<Vec<_>>().join(" ");
            format!("That was not accepted: {msg}")
        }
        Error::Conflict(m) => m.clone(),
        Error::MergeBlocked(reasons) => format!("This cannot be merged yet: {}", reasons.join("; ")),
        Error::RateLimited { retry_after_secs } => format!("Too many requests. Try again in {retry_after_secs} seconds."),
        Error::Unauthorized(_) => "This connection is no longer signed in. Connect again.".to_string(),
        Error::Unavailable(_) => "That is not available right now.".to_string(),
        Error::Internal(_) => "Something went wrong on the server.".to_string(),
    })
}

type TR = Result<Output, ToolError>;

fn out(meta: Value, untrusted: Value) -> TR {
    Ok(Output { meta, untrusted, repo_id: None, item: None })
}

pub struct Ctx {
    pub svc: ToolServices,
    pub actor: Actor,
    pub client: ClientInfo,
}

fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn page_of(a: &Args) -> (u32, u32) {
    (a.int("page").unwrap_or(1).max(1) as u32, a.int("per_page").unwrap_or(30).clamp(1, 50) as u32)
}

fn next_page(count: usize, page: u32, per: u32) -> Value {
    if count as u32 >= per { json!(page + 1) } else { Value::Null }
}

fn user_json(u: &UserRef) -> Value {
    json!({"username": u.username, "display_name": u.display_name})
}

fn issue_json(v: &IssueView) -> Value {
    let i = &v.issue;
    json!({
        "number": i.number, "is_pull_request": i.is_pull, "title": i.title, "body": i.body,
        "state": if i.open { "open" } else { "closed" }, "state_reason": i.state_reason,
        "labels": v.labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>(),
        "author": user_json(&v.author), "assignees": v.assignees.iter().map(user_json).collect::<Vec<_>>(),
        "comments": i.comments, "locked": i.locked, "created_at": i.created_at, "updated_at": i.updated_at, "closed_at": i.closed_at,
        "repository": v.repo_full_name,
    })
}

fn commit_json(c: &CommitInfo) -> Value {
    // Names and dates only: an email address of a committer is never returned.
    json!({
        "sha": c.sha, "parents": c.parents, "message": c.message,
        "author": {"name": c.author.name, "date": c.author.at}, "committer": {"name": c.committer.name, "date": c.committer.at},
    })
}

fn diff_files(d: &Diff, with_patch: bool, budget: usize) -> (Vec<Value>, bool) {
    let mut left = budget;
    let mut truncated = false;
    let mut files = Vec::new();
    for f in &d.files {
        let mut o = json!({
            "status": f.status, "path": f.new_path.clone().or(f.old_path.clone()), "old_path": f.old_path,
            "additions": f.additions, "deletions": f.deletions, "binary": f.binary,
        });
        if with_patch && !f.binary && !f.collapsed {
            let mut patch = String::new();
            'h: for h in &f.hunks {
                patch.push_str(&h.header);
                patch.push('\n');
                for l in &h.lines {
                    let p = match l.kind {
                        LineKind::Added => '+',
                        LineKind::Removed => '-',
                        LineKind::Context => ' ',
                    };
                    patch.push(p);
                    patch.push_str(&l.text);
                    patch.push('\n');
                    if patch.len() > left.min(shape::MAX_FILE_CHARS) {
                        break 'h;
                    }
                }
            }
            let (p, cut) = shape::clamp_chars(&patch, left.min(shape::MAX_FILE_CHARS));
            left = left.saturating_sub(p.len());
            truncated |= cut;
            o["patch"] = json!(p);
            o["patch_truncated"] = json!(cut);
        } else if with_patch {
            o["patch_omitted"] = json!(if f.binary { "binary file" } else { "too large to show" });
        }
        files.push(o);
    }
    (files, truncated)
}

impl Ctx {
    async fn repo(&self, a: &Args) -> Result<Repo, ToolError> {
        self.svc.repos.get_by_name(&self.actor, a.req("owner"), a.req("repo")).await.map_err(|e| public_error(&e, &self.actor))
    }

    fn err(&self, e: Error) -> ToolError {
        public_error(&e, &self.actor)
    }

    /// Turns what the caller wrote into a commit of this repository: a branch, a tag, or a full
    /// commit id that a branch or tag reaches. Nothing else is accepted, so a commit that exists
    /// only in a sibling fork, a tree, a blob or `rev:path` can never be read through a tool.
    async fn rev(&self, repo: &Repo, rev: Option<&str>) -> Result<String, ToolError> {
        let bad = || ToolError::new("Use a branch name, a tag name or a full commit id.");
        if repo.is_empty {
            return Err(ToolError::new("This repository has no commits yet."));
        }
        let name = rev.unwrap_or(&repo.default_branch);
        if name.is_empty() || name.len() > 255 {
            return Err(bad());
        }
        if is_full_sha(name) {
            return match self.svc.git.reachable(repo.id, &name.to_ascii_lowercase()).await {
                Ok(true) => Ok(name.to_ascii_lowercase()),
                Ok(false) => Err(ToolError::new("Not found.")),
                Err(e) => Err(self.err(e)),
            };
        }
        for prefix in ["refs/heads/", "refs/tags/"] {
            let full = format!("{prefix}{name}");
            let refs = match self.svc.git.refs(repo.id, Some(&full)).await {
                Ok(r) => r,
                // A name git would not accept as a ref (`..`, `:`, `^`, a leading dash) is not one.
                Err(Error::Validation(_)) => return Err(bad()),
                Err(e) => return Err(self.err(e)),
            };
            if let Some(r) = refs.into_iter().find(|r| r.name == full) {
                return Ok(r.sha);
            }
        }
        Err(ToolError::new("Not found."))
    }

    /// Gives the repository back with its id set on the output, for the audit trail.
    fn with(&self, repo: &Repo, item: Option<i64>, o: TR) -> TR {
        o.map(|mut o| {
            o.repo_id = Some(repo.id);
            o.item = item;
            o
        })
    }

    pub async fn call(&self, name: &str, a: &Args) -> TR {
        match name {
            // ---- connection and repositories
            "get_connection" => self.get_connection().await,
            "list_repositories" => self.list_repositories(a).await,
            "search_repositories" => self.search_repositories(a).await,
            "get_repository" => self.get_repository(a).await,
            // ---- files
            "list_files" => self.list_files(a).await,
            "get_file" => self.get_file(a).await,
            "search_code" => self.search_code(a).await,
            // ---- refs and commits
            "list_branches" => self.list_refs(a, "refs/heads/", "branches").await,
            "list_tags" => self.list_refs(a, "refs/tags/", "tags").await,
            "compare_refs" => self.compare_refs(a).await,
            "list_commits" => self.list_commits(a).await,
            "get_commit" => self.get_commit(a).await,
            // ---- issues
            "list_issues" => self.list_issues(a).await,
            "search_issues" => self.search_issues(a).await,
            "get_issue" => self.get_issue(a).await,
            "list_issue_comments" => self.list_issue_comments(a).await,
            "list_labels" => self.list_labels(a).await,
            // ---- pulls
            "list_pull_requests" => self.list_pull_requests(a).await,
            "get_pull_request" => self.get_pull_request(a).await,
            "get_pull_request_diff" => self.get_pull_request_diff(a).await,
            "list_pull_request_commits" => self.list_pull_request_commits(a).await,
            "get_pull_request_checks" => self.get_pull_request_checks(a).await,
            "list_reviews" => self.list_reviews(a).await,
            "list_review_threads" => self.list_review_threads(a).await,
            // ---- CI
            "get_commit_status" => self.get_commit_status(a).await,
            "list_workflows" => self.list_workflows(a).await,
            "list_workflow_runs" => self.list_workflow_runs(a).await,
            "get_workflow_run" => self.get_workflow_run(a).await,
            "get_job_log" => self.get_job_log(a).await,
            "list_notifications" => self.list_notifications(a).await,
            // ---- additive writes
            "create_repository" => self.create_repository(a).await,
            "create_branch" => self.create_branch(a).await,
            "create_issue" => self.create_issue(a).await,
            "add_issue_comment" => self.add_issue_comment(a).await,
            "create_pull_request" => self.create_pull_request(a).await,
            "add_review_comment" => self.add_review_comment(a).await,
            // ---- changing writes
            "commit_files" => self.commit_files(a).await,
            "update_issue" => self.update_issue(a).await,
            "update_pull_request" => self.update_pull_request(a).await,
            "submit_review" => self.submit_review(a).await,
            "update_pull_request_branch" => self.update_pull_request_branch(a).await,
            "merge_pull_request" => self.merge_pull_request(a).await,
            "mark_notification_read" => self.mark_notification_read(a).await,
            // ---- workflows
            "dispatch_workflow" => self.dispatch_workflow(a).await,
            "rerun_workflow_run" => self.rerun_workflow_run(a).await,
            "cancel_workflow_run" => self.cancel_workflow_run(a).await,
            _ => Err(ToolError::new("Unknown tool.")),
        }
    }

    // ================================================================ connection and repositories

    async fn get_connection(&self) -> TR {
        let g: GrantView = self.svc.apps.grant_of(&self.actor).await.map_err(|e| self.err(e))?;
        let repos: Vec<Value> =
            g.repos.iter().map(|r| json!({"repository": r.full_name, "write": r.can_write, "workflows": r.can_workflows})).collect();
        out(
            json!({"username": g.username, "app": g.client_host, "scopes": g.scopes, "read_all": g.read_all, "write_all": g.write_all, "workflows_all": g.workflows_all, "expires_at": g.expires_at}),
            json!({"repositories": repos}),
        )
    }

    async fn list_repositories(&self, a: &Args) -> TR {
        let (page, per) = page_of(a);
        let repos = self.svc.repos.list_accessible(&self.actor, page, per).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = repos.iter().map(repo_json).collect();
        out(json!({"count": items.len(), "page": page, "next_page": next_page(items.len(), page, per)}), json!({"repositories": items}))
    }

    async fn search_repositories(&self, a: &Args) -> TR {
        let limit = a.int("limit").unwrap_or(20).clamp(1, 50) as u32;
        let hits = self.svc.search.repos(&self.actor, a.req("query"), limit).await.map_err(|e| self.err(e))?;
        out(json!({"count": hits.len()}), json!({"results": hits_json(&hits)}))
    }

    async fn get_repository(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        self.with(&repo, None, out(json!({}), json!({"repository": repo_json(&repo)})))
    }

    // ================================================================ files

    async fn list_files(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let sha = self.rev(&repo, a.str("ref")).await?;
        let path = a.str("path").unwrap_or("").trim_matches('/');
        let entries = self.svc.git.tree(repo.id, &sha, path).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = entries.iter().map(|e| json!({"name": e.name, "path": e.path, "type": e.kind, "size": e.size})).collect();
        let n = items.len();
        self.with(&repo, None, out(json!({"revision": sha, "count": n}), json!({"entries": items})))
    }

    async fn get_file(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let sha = self.rev(&repo, a.str("ref")).await?;
        let path = a.req("path").trim_matches('/');
        let blob = self.svc.git.blob(repo.id, &sha, path, 1 << 20).await.map_err(|e| self.err(e))?;
        if blob.is_binary {
            return self.with(
                &repo,
                None,
                out(json!({"revision": sha, "size": blob.size, "binary": true, "sha": blob.sha}), json!({"path": path})),
            );
        }
        let text = String::from_utf8_lossy(&blob.content).into_owned();
        let lines: Vec<&str> = text.split('\n').collect();
        let total = if text.ends_with('\n') { lines.len() - 1 } else { lines.len() };
        let start = a.int("start_line").unwrap_or(1).max(1) as usize;
        let end = a.int("end_line").map(|e| e as usize).unwrap_or(usize::MAX);
        if start > total.max(1) {
            return Err(ToolError::new(format!("The file has {total} lines.")));
        }
        let end = end.min(total).max(start);
        let window = lines[(start - 1)..end.min(lines.len())].join("\n");
        let (shown, cut) = shape::clamp_chars(&window, shape::MAX_FILE_CHARS);
        let shown_lines = shown.matches('\n').count() + usize::from(!shown.is_empty());
        let last = if cut { start + shown_lines.saturating_sub(1) } else { end };
        let next = if cut || end < total { Some(last + 1) } else { None };
        self.with(
            &repo,
            None,
            out(
                json!({"revision": sha, "size": blob.size, "sha": blob.sha, "total_lines": total, "start_line": start, "end_line": last, "next_start_line": next, "truncated": cut || blob.truncated}),
                json!({"path": path, "content": shown}),
            ),
        )
    }

    async fn search_code(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let sha = self.rev(&repo, a.str("ref")).await?;
        let limit = a.int("limit").unwrap_or(20).clamp(1, 50) as u32;
        let hits = self.svc.git.grep(repo.id, &sha, a.req("query"), limit).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = hits.iter().map(|h| json!({"path": h.title, "line": h.id, "text": h.snippet})).collect();
        self.with(&repo, None, out(json!({"revision": sha, "count": items.len()}), json!({"matches": items})))
    }

    // ================================================================ refs and commits

    async fn list_refs(&self, a: &Args, prefix: &str, key: &str) -> TR {
        let repo = self.repo(a).await?;
        let refs = self.svc.git.refs(repo.id, Some(prefix)).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> =
            refs.iter().map(|r| json!({"name": r.name.strip_prefix(prefix).unwrap_or(&r.name), "sha": r.sha})).collect();
        let n = items.len();
        self.with(&repo, None, out(json!({"count": n, "default_branch": repo.default_branch}), json!({ key: items })))
    }

    async fn compare_refs(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let base = self.rev(&repo, Some(a.req("base"))).await?;
        let head = self.rev(&repo, Some(a.req("head"))).await?;
        let c = self.svc.git.compare(repo.id, &base, &head).await.map_err(|e| self.err(e))?;
        let (files, cut) = diff_files(&c.diff, false, 0);
        let commits: Vec<Value> = c.commits.iter().take(30).map(commit_json).collect();
        self.with(
            &repo,
            None,
            out(
                json!({"base_sha": c.base_sha, "head_sha": c.head_sha, "merge_base": c.merge_base, "ahead_by": c.ahead_by, "behind_by": c.behind_by, "commit_count": c.commits.len(), "file_count": files.len(), "truncated": cut || c.commits.len() > 30}),
                json!({"commits": commits, "files": files}),
            ),
        )
    }

    async fn list_commits(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let sha = self.rev(&repo, a.str("ref")).await?;
        let (page, per) = page_of(a);
        let path = a.str("path").filter(|p| !p.is_empty());
        let commits = self.svc.git.log(repo.id, &sha, path, (page - 1) * per, per).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = commits.iter().map(commit_json).collect();
        let n = items.len();
        self.with(
            &repo,
            None,
            out(json!({"revision": sha, "count": n, "page": page, "next_page": next_page(n, page, per)}), json!({"commits": items})),
        )
    }

    async fn get_commit(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let sha = self.rev(&repo, Some(a.req("sha"))).await?;
        let c = self.svc.git.commit(repo.id, &sha).await.map_err(|e| self.err(e))?;
        let d = self.svc.git.commit_diff(repo.id, &c.sha).await.map_err(|e| self.err(e))?;
        let (files, _) = diff_files(&d, false, 0);
        self.with(&repo, None, out(json!({"file_count": files.len()}), json!({"commit": commit_json(&c), "files": files})))
    }

    // ================================================================ issues

    async fn list_issues(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let (page, per) = page_of(a);
        let open = match a.str("state").unwrap_or("open") {
            "open" => Some(true),
            "closed" => Some(false),
            _ => None,
        };
        let labels: Vec<String> =
            a.str("labels").unwrap_or("").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let f = IssueFilter { open, labels, page, per_page: per, ..Default::default() };
        let views = self.svc.issues.views(&self.actor, repo.id, f).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = views.iter().filter(|v| !v.issue.is_pull).map(issue_json).collect();
        let n = items.len();
        self.with(
            &repo,
            None,
            out(json!({"count": n, "page": page, "next_page": next_page(views.len(), page, per)}), json!({"issues": items})),
        )
    }

    async fn search_issues(&self, a: &Args) -> TR {
        let limit = a.int("limit").unwrap_or(20).clamp(1, 50) as u32;
        let hits = self.svc.search.issues(&self.actor, a.req("query"), limit).await.map_err(|e| self.err(e))?;
        out(json!({"count": hits.len()}), json!({"results": hits_json(&hits)}))
    }

    async fn get_issue(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let v = self.svc.issues.view(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({}), json!({"issue": issue_json(&v)})))
    }

    async fn list_issue_comments(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let cs = self.svc.issues.comment_views(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = cs
            .iter()
            .map(
                |c| json!({"id": c.comment.id, "author": user_json(&c.author), "body": c.comment.body, "created_at": c.comment.created_at}),
            )
            .collect();
        let count = items.len();
        self.with(&repo, Some(n), out(json!({"count": count}), json!({"comments": items})))
    }

    async fn list_labels(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let ls = self.svc.issues.labels(&self.actor, repo.id).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = ls.iter().map(|l| json!({"name": l.name, "description": l.description, "color": l.color})).collect();
        let n = items.len();
        self.with(&repo, None, out(json!({"count": n}), json!({"labels": items})))
    }

    // ================================================================ pull requests

    /// The head repository, hidden when this connection cannot read it.
    async fn visible_head(&self, repo: &Repo, v: &PullView) -> Value {
        match &v.head_repo {
            Some(h) if h.id == repo.id => json!(h.full_name()),
            Some(h) => match self.svc.repos.get(&self.actor, h.id).await {
                Ok(_) => json!(h.full_name()),
                Err(_) => Value::Null,
            },
            None => Value::Null,
        }
    }

    async fn pull_json(&self, repo: &Repo, v: &PullView) -> Value {
        let p = &v.pull;
        let mut j = issue_json(&v.issue);
        let head_repo = self.visible_head(repo, v).await;
        let head_hidden = v.head_repo.as_ref().is_some_and(|h| h.id != repo.id) && head_repo.is_null();
        j["pull"] = json!({
            "head_ref": if head_hidden { Value::Null } else { json!(p.head_ref) },
            "head_repository": head_repo,
            "head_sha": p.head_sha, "base_ref": p.base_ref, "base_sha": p.base_sha, "draft": p.draft,
            "mergeable": p.mergeable, "merged": p.merged_at.is_some(), "merged_at": p.merged_at,
            "additions": v.stats.additions, "deletions": v.stats.deletions, "changed_files": v.stats.changed_files, "commits": v.stats.commits,
        });
        j
    }

    async fn list_pull_requests(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let (page, per) = page_of(a);
        let open = match a.str("state").unwrap_or("open") {
            "open" => Some(true),
            "closed" => Some(false),
            _ => None,
        };
        let f = PullFilter { issue: IssueFilter { open, page, per_page: per, ..Default::default() }, ..Default::default() };
        let views = self.svc.pulls.views(&self.actor, repo.id, f).await.map_err(|e| self.err(e))?;
        let mut items = Vec::new();
        for v in &views {
            items.push(self.pull_json(&repo, v).await);
        }
        let n = items.len();
        self.with(
            &repo,
            None,
            out(json!({"count": n, "page": page, "next_page": next_page(n, page, per)}), json!({"pull_requests": items})),
        )
    }

    async fn get_pull_request(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let v = self.svc.pulls.view(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let gate = self.svc.pulls.merge_gate(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let pj = self.pull_json(&repo, &v).await;
        self.with(
            &repo,
            Some(n),
            out(
                json!({"mergeable": gate.mergeable}),
                json!({"pull_request": pj, "merge_gate": {"mergeable": gate.mergeable, "reasons": gate.reasons}}),
            ),
        )
    }

    async fn get_pull_request_diff(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let page = a.int("page").unwrap_or(1).max(1) as usize;
        let d = self.svc.pulls.files(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let total = d.files.len();
        let slice = Diff { files: d.files.into_iter().skip((page - 1) * 100).take(100).collect() };
        let (files, cut) = diff_files(&slice, true, shape::MAX_RESULT_CHARS - 10_000);
        let shown = files.len();
        let more = total > page * 100;
        self.with(&repo, Some(n), out(json!({"total_files": total, "page": page, "files_in_page": shown, "next_page": if more { json!(page + 1) } else { Value::Null }, "truncated": cut}), json!({"files": files})))
    }

    async fn list_pull_request_commits(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let cs = self.svc.pulls.commits(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = cs.iter().take(100).map(commit_json).collect();
        self.with(&repo, Some(n), out(json!({"count": cs.len(), "truncated": cs.len() > 100}), json!({"commits": items})))
    }

    async fn checks_for(&self, repo: &Repo, sha: &str) -> TR {
        // Only what this person may see of the commit's checks (ch-4pn.12).
        let seen = self.svc.checks.seen_for_sha(&self.actor, repo.id, sha).await.map_err(|e| self.err(e))?;
        let combined = seen.state.unwrap_or(ch_core::model::CombinedState::Pending);
        let (statuses, runs) = (seen.statuses, seen.runs);
        let st: Vec<Value> =
            statuses.iter().map(|s| json!({"context": s.context, "state": s.state, "description": s.description})).collect();
        let cr: Vec<Value> = runs.iter().map(|r| json!({"name": r.name, "status": r.status, "conclusion": r.conclusion})).collect();
        out(json!({"sha": sha, "combined": combined}), json!({"statuses": st, "check_runs": cr}))
    }

    async fn get_pull_request_checks(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let v = self.svc.pulls.view(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let r = self.checks_for(&repo, &v.pull.head_sha).await;
        self.with(&repo, Some(n), r)
    }

    async fn list_reviews(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let rs = self.svc.reviews.views(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = rs.iter().map(|r| json!({"id": r.review.id, "author": user_json(&r.author), "state": r.review.state, "body": r.review.body, "commit_sha": r.review.commit_sha, "submitted_at": r.review.submitted_at, "comments": r.comment_count})).collect();
        let c = items.len();
        self.with(&repo, Some(n), out(json!({"count": c}), json!({"reviews": items})))
    }

    async fn list_review_threads(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let ts = self.svc.reviews.threads(&self.actor, repo.id, n).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = ts
            .iter()
            .map(|t| {
                json!({
                    "id": t.id, "path": t.path, "resolved": t.resolved, "outdated": t.outdated,
                    "comments": t.comments.iter().map(|c| json!({"id": c.comment.id, "author": user_json(&c.author), "line": c.comment.line, "side": c.comment.side, "body": c.comment.body})).collect::<Vec<_>>(),
                })
            })
            .collect();
        let c = items.len();
        self.with(&repo, Some(n), out(json!({"count": c}), json!({"threads": items})))
    }

    // ================================================================ CI

    async fn get_commit_status(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let sha = self.rev(&repo, Some(a.req("sha"))).await?;
        let r = self.checks_for(&repo, &sha).await;
        self.with(&repo, None, r)
    }

    async fn list_workflows(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let ws = self.svc.ci.workflows(&self.actor, repo.id, a.str("ref")).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> =
            ws.iter().map(|w| json!({"path": w.path, "name": w.name, "state": w.state, "events": w.events, "errors": w.errors})).collect();
        let n = items.len();
        self.with(&repo, None, out(json!({"count": n}), json!({"workflows": items})))
    }

    async fn list_workflow_runs(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let (page, per) = page_of(a);
        let runs = self.svc.ci.runs(&self.actor, repo.id, page, per).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = runs.iter().map(run_json).collect();
        let n = items.len();
        self.with(&repo, None, out(json!({"count": n, "page": page, "next_page": next_page(n, page, per)}), json!({"runs": items})))
    }

    async fn get_workflow_run(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let id = a.int("run_id").unwrap_or(0);
        let run = self.svc.ci.get_run(&self.actor, repo.id, RunId(id)).await.map_err(|e| self.err(e))?;
        let jobs = self.svc.ci.jobs(&self.actor, repo.id, RunId(id)).await.map_err(|e| self.err(e))?;
        let js: Vec<Value> = jobs.iter().map(|j| json!({"id": j.id, "name": j.name, "status": j.status, "conclusion": j.conclusion, "started_at": j.started_at, "completed_at": j.completed_at})).collect();
        self.with(&repo, Some(id), out(json!({}), json!({"run": run_json(&run), "jobs": js})))
    }

    async fn get_job_log(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let job = ch_core::JobId(a.int("job_id").unwrap_or(0));
        let size = self.svc.ci.log_size(&self.actor, repo.id, job).await.map_err(|e| self.err(e))?;
        let max = shape::MAX_LOG_CHARS as u64;
        // By default the tail. A cursor reads forward from a byte position.
        let from = match a.int("cursor") {
            Some(c) => (c as u64).min(size),
            None => size.saturating_sub(max),
        };
        let bytes = self
            .svc
            .ci
            .log_range(&self.actor, repo.id, job, LogRange { from, to: Some((from + max).min(size)) })
            .await
            .map_err(|e| self.err(e))?;
        let end = from + bytes.len() as u64;
        let text = shape::strip_ansi(&String::from_utf8_lossy(&bytes));
        let (text, cut) =
            if a.has("cursor") { shape::clamp_chars(&text, shape::MAX_LOG_CHARS) } else { shape::tail_chars(&text, shape::MAX_LOG_CHARS) };
        let next = if end < size { json!(end) } else { Value::Null };
        self.with(
            &repo,
            Some(job.0),
            out(
                json!({"log_bytes": size, "from": from, "to": end, "next_cursor": next, "truncated": cut || from > 0 || end < size}),
                json!({"log": text}),
            ),
        )
    }

    async fn list_notifications(&self, a: &Args) -> TR {
        let (page, per) = page_of(a);
        let unread = a.bool("unread_only");
        let ns = self.svc.notifications.inbox(&self.actor, unread, page, per).await.map_err(|e| self.err(e))?;
        let count = self.svc.notifications.unread_count(&self.actor).await.map_err(|e| self.err(e))?;
        let items: Vec<Value> = ns.iter().map(|n| json!({"id": n.id, "repo_id": n.repo_id, "reason": n.reason, "type": n.subject_type, "subject_id": n.subject_id, "title": n.title, "unread": n.unread, "updated_at": n.updated_at})).collect();
        let c = items.len();
        out(json!({"count": c, "unread_total": count, "page": page, "next_page": next_page(c, page, per)}), json!({"notifications": items}))
    }

    // ================================================================ additive writes

    async fn create_repository(&self, a: &Args) -> TR {
        let owner = self.actor.author_id().ok_or_else(|| ToolError::new("Not available."))?;
        // The service forces private and refuses anything but a personal namespace for an app.
        let new = NewRepo {
            owner_id: owner,
            name: a.req("name").to_string(),
            description: a.str("description").unwrap_or("").to_string(),
            visibility: Visibility::Private,
            default_branch: None,
            auto_init: true,
        };
        let r = self.svc.repos.create(&self.actor, new, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&r, None, out(json!({"created": true}), json!({"repository": repo_json(&r)})))
    }

    async fn create_branch(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let from = self.rev(&repo, a.str("from")).await?;
        let name = a.req("name");
        self.svc.git.create_ref(&self.actor, repo.id, name, &from, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, None, out(json!({"created": true, "from_sha": from}), json!({"branch": name})))
    }

    async fn create_issue(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let new = NewIssue {
            repo_id: repo.id,
            title: a.req("title").to_string(),
            body: a.str("body").unwrap_or("").to_string(),
            labels: a.list("labels"),
            assignees: vec![],
            milestone_id: None,
        };
        let i = self.svc.issues.create(&self.actor, new, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(i.number), out(json!({"created": true, "number": i.number}), json!({"title": i.title})))
    }

    async fn add_issue_comment(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let c = self.svc.issues.add_comment(&self.actor, repo.id, n, a.req("body"), &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({"created": true, "comment_id": c.id}), json!({})))
    }

    async fn create_pull_request(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let base = a.str("base").unwrap_or(&repo.default_branch).to_string();
        let new = NewPull {
            repo_id: repo.id,
            title: a.req("title").to_string(),
            body: a.str("body").unwrap_or("").to_string(),
            head_repo_id: repo.id,
            head_ref: a.req("head").to_string(),
            base_ref: base,
            draft: a.bool("draft"),
        };
        let p = self.svc.pulls.create(&self.actor, new, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(p.issue.number), out(json!({"created": true, "number": p.issue.number}), json!({"title": p.issue.title})))
    }

    async fn add_review_comment(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let c = NewReviewComment {
            path: a.req("path").to_string(),
            side: a.str("side").unwrap_or("right").to_string(),
            line: a.int("line").unwrap_or(1) as u32,
            start_line: None,
            body: a.req("body").to_string(),
            in_reply_to: None,
        };
        let r = self.svc.reviews.add_comment(&self.actor, repo.id, n, c, None, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({"created": true, "comment_id": r.comment.id}), json!({})))
    }

    // ================================================================ changing writes

    async fn commit_files(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let branch = a.req("branch");
        let expected = a.req("expected_head").to_ascii_lowercase();
        if !is_full_sha(&expected) {
            return Err(ToolError::new("expected_head must be a full commit id."));
        }
        // The branch must point at the expected head right now, so nothing is overwritten.
        let head = self.rev(&repo, Some(branch)).await?;
        if head.is_empty() || head != expected {
            return Err(ToolError::new(format!("The branch has moved. It now points at {head}. Read it again and retry.")));
        }
        let me = self.svc.apps.grant_of(&self.actor).await.map_err(|e| self.err(e))?;
        let author = Signature {
            name: me.display_name.clone(),
            // The person behind the app. Account emails never go into history.
            email: match &self.actor {
                Actor::App { user_id, .. } => ch_core::noreply::noreply_email(user_id.0, &me.username, &self.svc.external_url),
                _ => format!("{}@users.noreply.invalid", me.username),
            },
            at: chrono::Utc::now(),
        };
        let mut changes: Vec<FileChange> = Vec::new();
        let mut total = 0usize;
        let mut seen = std::collections::HashSet::new();
        for op in a.array("changes") {
            let o = op.as_object().ok_or_else(|| ToolError::new("Each change must be an object."))?;
            let get = |k: &str| o.get(k).and_then(|v| v.as_str());
            let kind = get("op").unwrap_or("");
            let path = get("path").unwrap_or("").trim_matches('/').to_string();
            if path.is_empty() || !seen.insert(path.clone()) {
                return Err(ToolError::new("Each change needs its own path."));
            }
            for k in o.keys() {
                if !matches!(k.as_str(), "op" | "path" | "content" | "old_text" | "new_text" | "from") {
                    return Err(ToolError::new(format!("Unknown field '{k}' in a change.")));
                }
            }
            match kind {
                "create" => {
                    let content = get("content").ok_or_else(|| ToolError::new("create needs content."))?;
                    if self.svc.git.blob(repo.id, &head, &path, 16).await.is_ok() {
                        return Err(ToolError::new(format!("{path} already exists. Use update or replace_text.")));
                    }
                    total += content.len();
                    changes.push(FileChange { path, content: Some(content.as_bytes().to_vec()), renamed_from: None });
                }
                "update" => {
                    let content = get("content").ok_or_else(|| ToolError::new("update needs content."))?;
                    let cur = self
                        .svc
                        .git
                        .blob(repo.id, &head, &path, 1 << 20)
                        .await
                        .map_err(|_| ToolError::new(format!("{path} does not exist. Use create.")))?;
                    let cur_text = String::from_utf8_lossy(&cur.content);
                    // Replacing a whole file the caller could not have been shown whole would
                    // corrupt it, so it is refused and replace_text is the way.
                    if cur.is_binary
                        || cur.truncated
                        || cur_text.chars().count() > shape::MAX_FILE_CHARS
                        || cur_text.chars().any(shape::is_hidden)
                    {
                        return Err(ToolError::new(format!(
                            "{path} is binary, too large to have been returned whole, or contains characters that are shown as escapes. Use replace_text instead of update."
                        )));
                    }
                    total += content.len();
                    changes.push(FileChange { path, content: Some(content.as_bytes().to_vec()), renamed_from: None });
                }
                "replace_text" => {
                    let (old, new) = (get("old_text").unwrap_or(""), get("new_text").unwrap_or(""));
                    if old.is_empty() {
                        return Err(ToolError::new("replace_text needs old_text."));
                    }
                    let cur = self
                        .svc
                        .git
                        .blob(repo.id, &head, &path, 1 << 20)
                        .await
                        .map_err(|_| ToolError::new(format!("{path} does not exist.")))?;
                    if cur.is_binary || cur.truncated {
                        return Err(ToolError::new(format!("{path} is binary or too large to edit.")));
                    }
                    let text = String::from_utf8(cur.content).map_err(|_| ToolError::new(format!("{path} is not valid UTF-8 text.")))?;
                    match text.matches(old).count() {
                        1 => {}
                        0 => return Err(ToolError::new(format!("old_text was not found in {path}."))),
                        n => {
                            return Err(ToolError::new(format!(
                                "old_text matches {n} places in {path}. Make it longer so it matches exactly one."
                            )));
                        }
                    }
                    let updated = text.replacen(old, new, 1);
                    total += updated.len();
                    changes.push(FileChange { path, content: Some(updated.into_bytes()), renamed_from: None });
                }
                "delete" => {
                    self.svc.git.blob(repo.id, &head, &path, 16).await.map_err(|_| ToolError::new(format!("{path} does not exist.")))?;
                    changes.push(FileChange { path, content: None, renamed_from: None });
                }
                "rename" => {
                    let from = get("from").unwrap_or("").trim_matches('/').to_string();
                    if from.is_empty() || from == path {
                        return Err(ToolError::new("rename needs a different from path."));
                    }
                    let cur = self
                        .svc
                        .git
                        .blob(repo.id, &head, &from, 1 << 20)
                        .await
                        .map_err(|_| ToolError::new(format!("{from} does not exist.")))?;
                    if cur.truncated {
                        return Err(ToolError::new(format!("{from} is too large to rename here.")));
                    }
                    total += cur.content.len();
                    changes.push(FileChange { path, content: Some(cur.content), renamed_from: Some(from) });
                }
                _ => return Err(ToolError::new("op must be create, replace_text, update, delete or rename.")),
            }
        }
        if total > 768 * 1024 {
            return Err(ToolError::new("The changes add up to more than 768 KiB. Split them into several commits."));
        }
        let req = CommitFileChange {
            repo_id: repo.id,
            branch: branch.to_string(),
            new_branch: a.str("new_branch").map(str::to_string),
            expected_sha: Some(expected),
            changes,
            message: a.req("message").to_string(),
            author,
        };
        let sha = self.svc.git.commit_file_change(&self.actor, req, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, None, out(json!({"committed": true, "commit": sha, "branch": a.str("new_branch").unwrap_or(branch)}), json!({})))
    }

    async fn update_issue(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let patch = IssuePatch {
            title: a.str("title").map(str::to_string),
            body: a.str("body").map(str::to_string),
            open: a.str("state").map(|s| s == "open"),
            state_reason: None,
            labels: if a.has("labels") { Some(a.list("labels")) } else { None },
            assignees: None,
            milestone: None,
        };
        let v = self.svc.issues.update(&self.actor, repo.id, n, patch, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({"updated": true}), json!({"issue": issue_json(&v)})))
    }

    async fn update_pull_request(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        // Title and text only. The base branch is never changed through a tool.
        let patch = PullPatch { title: a.str("title").map(str::to_string), body: a.str("body").map(str::to_string), base: None };
        if patch.title.is_some() || patch.body.is_some() {
            self.svc.pulls.update(&self.actor, repo.id, n, patch, &self.client).await.map_err(|e| self.err(e))?;
        }
        match a.str("state") {
            Some("closed") => {
                self.svc.pulls.close(&self.actor, repo.id, n, &self.client).await.map_err(|e| self.err(e))?;
            }
            Some("open") => {
                self.svc.pulls.reopen(&self.actor, repo.id, n, &self.client).await.map_err(|e| self.err(e))?;
            }
            _ => {}
        }
        self.with(&repo, Some(n), out(json!({"updated": true}), json!({})))
    }

    async fn submit_review(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let state = if a.req("kind") == "request_changes" { ReviewState::ChangesRequested } else { ReviewState::Commented };
        let r = self
            .svc
            .reviews
            .submit(&self.actor, repo.id, n, state, a.str("body").unwrap_or(""), vec![], &self.client)
            .await
            .map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({"submitted": true, "review_id": r.id}), json!({})))
    }

    async fn update_pull_request_branch(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let v = self.svc.pulls.update_branch(&self.actor, repo.id, n, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({"updated": true, "head_sha": v.pull.head_sha}), json!({})))
    }

    async fn merge_pull_request(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let n = a.int("number").unwrap_or(0);
        let method = a.str("method").map(|m| match m {
            "squash" => MergeMethod::Squash,
            "rebase" => MergeMethod::Rebase,
            _ => MergeMethod::Merge,
        });
        // The head branch is never deleted from here: that path deletes as the person.
        let opts = MergeOptions {
            method,
            title: a.str("title").map(str::to_string),
            message: a.str("message").map(str::to_string),
            expected_head_sha: Some(a.req("expected_head").to_string()),
            delete_branch: Some(false),
        };
        let v = self.svc.pulls.merge_with(&self.actor, repo.id, n, opts, &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(n), out(json!({"merged": v.pull.merged_at.is_some(), "merge_commit": v.pull.merge_commit_sha}), json!({})))
    }

    async fn mark_notification_read(&self, a: &Args) -> TR {
        let id = a.int("id").unwrap_or(0);
        self.svc.notifications.mark_read(&self.actor, Some(NotificationId(id))).await.map_err(|e| self.err(e))?;
        Ok(Output { meta: json!({"marked_read": true}), untrusted: json!({}), repo_id: None, item: Some(id) })
    }

    // ================================================================ workflows

    async fn dispatch_workflow(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let inputs = Value::Object(a.object("inputs"));
        let id = self
            .svc
            .ci
            .dispatch(&self.actor, repo.id, a.req("workflow"), a.req("ref"), inputs, &self.client)
            .await
            .map_err(|e| self.err(e))?;
        self.with(&repo, Some(id.0), out(json!({"started": true, "run_id": id}), json!({})))
    }

    async fn rerun_workflow_run(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let id = a.int("run_id").unwrap_or(0);
        let new = self.svc.ci.rerun(&self.actor, repo.id, RunId(id), a.bool("failed_only"), &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(id), out(json!({"started": true, "run_id": new}), json!({})))
    }

    async fn cancel_workflow_run(&self, a: &Args) -> TR {
        let repo = self.repo(a).await?;
        let id = a.int("run_id").unwrap_or(0);
        self.svc.ci.cancel(&self.actor, repo.id, RunId(id), &self.client).await.map_err(|e| self.err(e))?;
        self.with(&repo, Some(id), out(json!({"cancelled": true}), json!({})))
    }
}

fn repo_json(r: &Repo) -> Value {
    json!({
        "full_name": format!("{}/{}", r.owner_name, r.name), "description": r.description, "private": r.visibility == Visibility::Private,
        "default_branch": r.default_branch, "archived": r.archived, "empty": r.is_empty, "topics": r.topics, "pushed_at": r.pushed_at,
    })
}

fn hits_json(hits: &[SearchHit]) -> Vec<Value> {
    hits.iter().map(|h| json!({"kind": h.kind, "id": h.id, "title": h.title, "snippet": h.snippet})).collect()
}

fn run_json(r: &WorkflowRun) -> Value {
    json!({"id": r.id, "number": r.run_number, "event": r.event, "head_sha": r.head_sha, "head_branch": r.head_branch, "status": r.status, "conclusion": r.conclusion, "created_at": r.created_at})
}
