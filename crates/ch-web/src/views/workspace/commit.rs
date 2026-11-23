//! `commit` and `branches`: writing from the browser (spec section 9).
//!
//! A commit carries many files and lands as one commit and one push event, through the same
//! `commit_file_change` as the web editor, so permission, compare-and-swap, protection, audit
//! and events stay one code path. It is all or nothing.

use super::ctx::{JsonData, Need, WorkspaceCtx, WsError, WsResult, json};
use super::files::{head_of, valid_branch, valid_new_branch, valid_sha};
use super::limits::Kind;
use crate::ctx::Ctx;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use base64::Engine;
use ch_core::model::{CommitFileChange, FileChange};
use serde::{Deserialize, Deserializer};
use std::collections::HashSet;

/// Changes in one commit.
pub const MAX_CHANGES: usize = 200;
/// Bytes of one file in a commit.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Bytes of all files in a commit.
pub const MAX_TOTAL_BYTES: usize = 10 * 1024 * 1024;
/// The request body: the files above in base64, plus the rest.
pub const MAX_BODY_BYTES: usize = 14 * 1024 * 1024;
/// A commit message.
const MAX_MESSAGE: usize = 64 * 1024;

/// A field that must be present and may be `null`. Plain `Option` would treat a missing field
/// as `null`, and `expected_head` must never be left out by accident.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitBody {
    pub branch: String,
    #[serde(deserialize_with = "present")]
    pub expected_head: Option<String>,
    #[serde(default)]
    pub new_branch: Option<String>,
    pub message: String,
    #[serde(default)]
    pub workflow_confirmed: bool,
    pub changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, tag = "op", rename_all = "lowercase")]
pub enum Change {
    Put { path: String, content_b64: String },
    Rename { from: String, path: String, content_b64: String },
    Delete { path: String },
}

/// A path under a directory whose files CI runs. Compared without case, because the
/// filesystem a runner checks out onto may fold it.
pub fn is_workflow_path(p: &str) -> bool {
    let l = p.trim_start_matches('/').to_ascii_lowercase();
    l.starts_with(".codehaven/workflows/") || l.starts_with(".github/workflows/")
}

/// A path the browser may create: a plain relative path with no control characters. git and
/// the server check the rest (`.git` spellings, folders, links).
fn check_new_path(field: &str, p: &str) -> WsResult<()> {
    let bad = p.is_empty()
        || p.len() > 4096
        || p.starts_with('/')
        || p.ends_with('/')
        || p.contains('\\')
        || p.chars().any(|c| c.is_control())
        || p.split('/').any(|s| s.is_empty() || s == "." || s == "..");
    if bad { Err(WsError::invalid(field, "That file name is not allowed.")) } else { Ok(()) }
}

/// Turns the request into the service's changes, with every cap and rule that needs no git.
pub fn changes_of(body: &CommitBody) -> WsResult<Vec<FileChange>> {
    if body.changes.is_empty() {
        return Err(WsError::invalid("changes", "There is nothing to commit."));
    }
    if body.changes.len() > MAX_CHANGES {
        return Err(WsError::too_large(format!("Commit at most {MAX_CHANGES} files at once.")));
    }
    let message = body.message.trim();
    if message.is_empty() {
        return Err(WsError::invalid("message", "Write a commit message."));
    }
    if body.message.len() > MAX_MESSAGE {
        return Err(WsError::too_large("That commit message is too long."));
    }
    let mut targets = HashSet::new();
    let mut total = 0usize;
    let mut workflow = false;
    let mut out = Vec::with_capacity(body.changes.len());
    let decode = |b64: &str| -> WsResult<Vec<u8>> {
        // Checked before decoding, so a huge string is never expanded.
        if b64.len() > MAX_FILE_BYTES.div_ceil(3) * 4 {
            return Err(WsError::too_large("A file is over 2 MB. Commit it with git instead."));
        }
        base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| WsError::invalid("content_b64", "A file's content is not valid."))
    };
    for c in &body.changes {
        let (path, from, content) = match c {
            Change::Put { path, content_b64 } => (path, None, Some(decode(content_b64)?)),
            Change::Rename { from, path, content_b64 } => (path, Some(from), Some(decode(content_b64)?)),
            Change::Delete { path } => (path, None, None),
        };
        check_new_path("path", path)?;
        if let Some(f) = from {
            check_new_path("from", f)?;
            if f == path {
                return Err(WsError::invalid("from", "A file cannot be renamed to itself."));
            }
            workflow |= is_workflow_path(f);
            if !targets.insert(f.clone()) {
                return Err(WsError::invalid("changes", format!("{f} is changed twice in this commit.")));
            }
        }
        workflow |= is_workflow_path(path);
        if !targets.insert(path.clone()) {
            return Err(WsError::invalid("changes", format!("{path} is changed twice in this commit.")));
        }
        if let Some(bytes) = &content {
            if bytes.len() > MAX_FILE_BYTES {
                return Err(WsError::too_large("A file is over 2 MB. Commit it with git instead."));
            }
            total += bytes.len();
            if total > MAX_TOTAL_BYTES {
                return Err(WsError::too_large("These files are over 10 MB together. Commit fewer at once."));
            }
        }
        out.push(FileChange { path: path.clone(), content, renamed_from: from.cloned() });
    }
    if workflow && !body.workflow_confirmed {
        return Err(WsError::Code {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "workflow_confirm",
            message: "This commit changes a workflow. Confirm that you mean to change what CI runs.".into(),
            field: Some("workflow_confirmed".into()),
            retry_after: None,
        });
    }
    Ok(out)
}

/// What `commit_file_change` said, as the page's codes.
fn commit_error(e: ch_core::Error) -> WsError {
    use ch_core::Error as E;
    match e {
        E::Conflict(m) if m.contains("file and a folder") => WsError::invalid("changes", m),
        E::Conflict(m) => WsError::code(StatusCode::CONFLICT, "head_moved", m),
        E::NotFound => WsError::code(StatusCode::CONFLICT, "branch_gone", "That branch no longer exists."),
        E::Forbidden(m) => WsError::code(StatusCode::FORBIDDEN, "protected", m),
        other => other.into(),
    }
}

pub async fn commit(ctx: Ctx, Path((owner, name)): Path<(String, String)>, headers: HeaderMap, body: Body) -> WsResult<Response> {
    let ws = WorkspaceCtx::load_post(ctx, &owner, &name, Need::Write, &headers).await?;
    let _permit = ws.limit(Kind::Commit)?;
    let JsonData(req) = JsonData::<CommitBody>::read(&ws, &headers, body, MAX_BODY_BYTES).await?;
    if !valid_branch(&req.branch) {
        return Err(WsError::invalid("branch", "That is not a branch name."));
    }
    if let Some(nb) = &req.new_branch
        && !valid_new_branch(nb)
    {
        return Err(WsError::invalid("new_branch", "That is not a branch name."));
    }
    let changes = changes_of(&req)?;
    let repo = ws.rc.id();
    let git = &ws.ctx.state.git;

    // The head this commit starts from. Null only for a repository with no commits, and then
    // the branch must not exist.
    let current = git.workspace_branch_head(repo, &req.branch).await?;
    let expected = match (&req.expected_head, &current) {
        (None, None) if ws.rc.repo.is_empty && req.new_branch.is_none() => None,
        (None, _) => return Err(WsError::code(StatusCode::CONFLICT, "head_moved", "The branch has commits now. Reload it.")),
        (Some(_), None) => return Err(WsError::code(StatusCode::CONFLICT, "branch_gone", "That branch no longer exists.")),
        (Some(e), Some(h)) => {
            if !valid_sha(e) {
                return Err(WsError::invalid("expected_head", "That is not a commit id."));
            }
            // Straight to the branch, the head must not have moved. To a new branch, any commit
            // in its history will do.
            let ok = e == h || (req.new_branch.is_some() && git.workspace_is_ancestor(repo, e, h).await?);
            if !ok {
                return Err(WsError::code(StatusCode::CONFLICT, "head_moved", "The branch moved since you started. Your drafts are kept."));
            }
            Some(e.clone())
        }
    };

    // Protection, before any object is written. The service checks again when the ref moves.
    let target = req.new_branch.clone().unwrap_or_else(|| req.branch.clone());
    if let Some(nb) = &req.new_branch {
        if git.workspace_branch_head(repo, nb).await?.is_some() {
            return Err(WsError::code(StatusCode::CONFLICT, "branch_exists", "A branch with that name exists. Choose another name."));
        }
    } else if current.is_some() && !git.workspace_direct_commit_allowed(&ws.ctx.actor, repo, &req.branch).await? {
        return Err(WsError::code(
            StatusCode::FORBIDDEN,
            "protected",
            "This branch is protected. Commit to a new branch and open a pull request.",
        ));
    }

    let bytes: u64 = changes.iter().map(|c| c.content.as_ref().map_or(0, |b| b.len() as u64)).sum();
    ws.charge_commit_bytes(bytes)?;
    let user = ws.ctx.state.accounts.get_user(ws.user).await?;
    let author = crate::views::repo::signature(&ws.ctx, &user);
    let req = CommitFileChange {
        repo_id: repo,
        branch: req.branch.clone(),
        new_branch: req.new_branch.clone(),
        expected_sha: expected,
        changes,
        message: req.message.clone(),
        author,
    };
    let sha = git.commit_file_change(&ws.ctx.actor, req, &ws.ctx.client).await.map_err(commit_error)?;
    Ok(json(&serde_json::json!({ "sha": sha, "branch": target })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchBody {
    /// The branch the new one starts from.
    pub branch: String,
    /// A commit in that branch's history.
    pub sha: String,
    pub name: String,
}

pub async fn branches(ctx: Ctx, Path((owner, name)): Path<(String, String)>, headers: HeaderMap, body: Body) -> WsResult<Response> {
    let ws = WorkspaceCtx::load_post(ctx, &owner, &name, Need::Write, &headers).await?;
    let _permit = ws.limit(Kind::Commit)?;
    let JsonData(req) = JsonData::<BranchBody>::read(&ws, &headers, body, 4096).await?;
    if !valid_new_branch(&req.name) {
        return Err(WsError::invalid("name", "That is not a branch name."));
    }
    let Some(head) = head_of(&ws, &req.branch).await? else {
        return Err(WsError::code(StatusCode::CONFLICT, "branch_gone", "That branch no longer exists."));
    };
    let git = &ws.ctx.state.git;
    if !valid_sha(&req.sha) || (req.sha != head && !git.workspace_is_ancestor(ws.rc.id(), &req.sha, &head).await?) {
        return Err(WsError::code(StatusCode::CONFLICT, "head_moved", "The branch moved. Reload it."));
    }
    if git.workspace_branch_head(ws.rc.id(), &req.name).await?.is_some() {
        return Err(WsError::code(StatusCode::CONFLICT, "branch_exists", "A branch with that name exists. Choose another name."));
    }
    match git.create_ref(&ws.ctx.actor, ws.rc.id(), &format!("refs/heads/{}", req.name), &req.sha, &ws.ctx.client).await {
        Ok(()) => Ok(json(&serde_json::json!({ "branch": req.name, "sha": req.sha }))),
        Err(ch_core::Error::Validation(v)) if v.iter().any(|f| f.code == "already_exists") => {
            Err(WsError::code(StatusCode::CONFLICT, "branch_exists", "A branch with that name exists. Choose another name."))
        }
        Err(e) => Err(commit_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(changes: serde_json::Value, confirmed: bool) -> CommitBody {
        serde_json::from_value(serde_json::json!({
            "branch": "main", "expected_head": null, "message": "m", "workflow_confirmed": confirmed, "changes": changes
        }))
        .unwrap()
    }

    #[test]
    fn expected_head_must_be_present_and_unknown_fields_are_refused() {
        let missing = serde_json::json!({"branch": "main", "message": "m", "changes": []});
        assert!(serde_json::from_value::<CommitBody>(missing).is_err());
        let extra = serde_json::json!({"branch": "main", "expected_head": null, "message": "m", "changes": [], "author": "x"});
        assert!(serde_json::from_value::<CommitBody>(extra).is_err());
        let bad_op =
            serde_json::json!({"branch": "main", "expected_head": null, "message": "m", "changes": [{"op": "chmod", "path": "a"}]});
        assert!(serde_json::from_value::<CommitBody>(bad_op).is_err());
    }

    #[test]
    fn workflow_paths_need_confirming_in_any_letter_case() {
        for p in [".codehaven/workflows/ci.yml", ".GitHub/Workflows/x.yml", ".CODEHAVEN/WORKFLOWS/a/b.yaml"] {
            let c = serde_json::json!([{"op": "put", "path": p, "content_b64": "eA=="}]);
            assert!(matches!(changes_of(&body(c.clone(), false)), Err(WsError::Code { code: "workflow_confirm", .. })), "{p}");
            assert!(changes_of(&body(c, true)).is_ok(), "{p}");
        }
        // A delete and a rename out of the folder count too.
        let d = serde_json::json!([{"op": "delete", "path": ".github/workflows/x.yml"}]);
        assert!(changes_of(&body(d, false)).is_err());
        let r = serde_json::json!([{"op": "rename", "from": ".github/workflows/x.yml", "path": "x.yml", "content_b64": ""}]);
        assert!(changes_of(&body(r, false)).is_err());
        assert!(!is_workflow_path(".github/workflowsx/a"));
    }

    #[test]
    fn duplicates_controls_and_caps_are_refused() {
        let dup = serde_json::json!([{"op": "put", "path": "a", "content_b64": ""}, {"op": "delete", "path": "a"}]);
        assert!(changes_of(&body(dup, false)).is_err());
        let ren = serde_json::json!([{"op": "rename", "from": "a", "path": "b", "content_b64": ""}, {"op": "put", "path": "a", "content_b64": ""}]);
        assert!(changes_of(&body(ren, false)).is_err(), "a rename's source is a target too");
        for p in ["a\nb", "a\rb", "tab\there", "/abs", "a/../b", "a//b", "a\\b", "", "dir/"] {
            let c = serde_json::json!([{"op": "put", "path": p, "content_b64": ""}]);
            assert!(changes_of(&body(c, false)).is_err(), "{p:?}");
        }
        let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_FILE_BYTES + 1]);
        let c = serde_json::json!([{"op": "put", "path": "big", "content_b64": big}]);
        assert!(matches!(changes_of(&body(c, false)), Err(WsError::Code { code: "too_large", .. })));
        let many: Vec<_> = (0..=MAX_CHANGES).map(|i| serde_json::json!({"op": "delete", "path": format!("f{i}")})).collect();
        assert!(changes_of(&body(serde_json::Value::Array(many), false)).is_err());
        let ok = serde_json::json!([{"op": "put", "path": "absent tree", "content_b64": "aGk="}, {"op": "put", "path": "__proto__", "content_b64": ""}]);
        assert_eq!(changes_of(&body(ok, false)).unwrap().len(), 2);
    }
}
