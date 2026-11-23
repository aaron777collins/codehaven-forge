//! `boot`, `tree`, `file`, `search` and `snapshot`: reading the repository at one pinned commit.

use super::ctx::{Key, Need, WorkspaceCtx, WsError, WsResult, json};
use super::keys::draft_key;
use crate::ctx::Ctx;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use ch_core::Permission;
use serde::Serialize;
use std::collections::HashMap;

pub type Q = Query<HashMap<String, String>>;

/// Editable file size on a desktop and on a phone.
pub const FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const FILE_BYTES_PHONE: u64 = 256 * 1024;

/// A branch name from a query: a plain ref name, never revision syntax. The server always
/// builds `refs/heads/<branch>` from it.
pub fn branch_param(q: &HashMap<String, String>, default: &str) -> WsResult<String> {
    let b = q.get("branch").map(String::as_str).filter(|b| !b.is_empty()).unwrap_or(default);
    if !valid_branch(b) {
        return Err(WsError::invalid("branch", "That is not a branch name."));
    }
    Ok(b.to_string())
}

/// What git's `check-ref-format --branch` allows, a little stricter: no revision characters.
pub fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 255
        && !b.starts_with('-')
        && !b.starts_with('/')
        && !b.ends_with('/')
        && !b.ends_with(".lock")
        && !b.ends_with('.')
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && b != "@"
        && !b.split('/').any(|c| c.starts_with('.'))
        && b.chars().all(|c| !c.is_control() && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

/// A name for a branch this page creates: `valid_branch`, and no bidi controls or invisible
/// format characters, which would make the name look like another one.
pub fn valid_new_branch(b: &str) -> bool {
    valid_branch(b)
        && !b
            .chars()
            .any(|c| matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'))
}

pub fn valid_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The branch's current head, or `branch_gone`. An empty repository has none.
pub async fn head_of(ws: &WorkspaceCtx, branch: &str) -> WsResult<Option<String>> {
    let head = ws.ctx.state.git.workspace_branch_head(ws.rc.id(), branch).await?;
    if head.is_none() && !ws.rc.repo.is_empty {
        return Err(WsError::code(StatusCode::CONFLICT, "branch_gone", "That branch no longer exists."));
    }
    Ok(head)
}

/// `sha` from the query, checked to be an ancestor of the branch's head (or the head itself).
/// Anything else is `head_moved`, with no hint whether the object exists.
pub async fn pinned(ws: &WorkspaceCtx, q: &HashMap<String, String>) -> WsResult<(String, String)> {
    let branch = branch_param(q, &ws.rc.repo.default_branch)?;
    let sha = q.get("sha").cloned().unwrap_or_default();
    let moved = || WsError::code(StatusCode::CONFLICT, "head_moved", "The branch moved. Reload it.");
    if !valid_sha(&sha) {
        return Err(moved());
    }
    let Some(head) = head_of(ws, &branch).await? else { return Err(moved()) };
    if sha != head && !ws.ctx.state.git.workspace_is_ancestor(ws.rc.id(), &sha, &head).await? {
        return Err(moved());
    }
    Ok((branch, sha))
}

#[derive(Serialize)]
struct Boot {
    repo: BootRepo,
    login: String,
    user_id: i64,
    /// The commit address the server signs browser commits with, to recognise one of ours
    /// after a lost response.
    me_email: String,
    branch: String,
    head: Option<String>,
    head_parents: Vec<String>,
    head_author_email: String,
    branches: Vec<String>,
    branches_cut: bool,
    can_write: bool,
    can_triage: bool,
    direct_commit: bool,
    suggested_branch: Option<String>,
    ci: BootCi,
    draft_key: String,
    limits: BootLimits,
    terminal: bool,
    /// The shell's pieces under this build's assets, with the integrity the page checks.
    terminal_runtime: Vec<BootRuntime>,
}

#[derive(Serialize)]
struct BootRuntime {
    url: String,
    integrity: String,
}

fn terminal_runtime(ws: &WorkspaceCtx) -> Vec<BootRuntime> {
    let assets = super::assets::assets_url(&ws.base());
    super::terminal::runtime_integrity()
        .into_iter()
        .map(|(name, integrity)| BootRuntime { url: format!("{assets}/{name}"), integrity })
        .collect()
}

#[derive(Serialize)]
struct BootRepo {
    id: i64,
    owner: String,
    name: String,
    full_name: String,
    default_branch: String,
    empty: bool,
    archived: bool,
}

#[derive(Serialize)]
struct BootCi {
    /// No run can start now: say so before anyone commits and waits.
    paused: bool,
    reason: String,
}

#[derive(Serialize)]
struct BootLimits {
    file_bytes: u64,
    file_bytes_phone: u64,
    tree_entries: usize,
    commit_changes: usize,
    commit_file_bytes: usize,
    commit_total_bytes: usize,
}

/// How many branch names `boot` lists.
const BOOT_BRANCHES: usize = 500;

/// A suggested branch for someone who cannot commit to this one: `<login>/patch-<6 hex>`.
fn suggest_branch(login: &str) -> String {
    let n: u32 = rand::random();
    format!("{login}/patch-{:06x}", n & 0x00ff_ffff)
}

pub async fn boot(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let branch = branch_param(&q, &ws.rc.repo.default_branch)?;
    let head = head_of(&ws, &branch).await?;
    let refs = ws.ctx.state.git.refs(ws.rc.id(), Some("refs/heads/")).await.unwrap_or_default();
    let mut branches: Vec<String> = refs.iter().filter_map(|r| r.name.strip_prefix("refs/heads/").map(str::to_string)).collect();
    branches.sort();
    let branches_cut = branches.len() > BOOT_BRANCHES;
    branches.truncate(BOOT_BRANCHES);
    let can_write = ws.can_write();
    let direct_commit =
        can_write && ws.ctx.state.git.workspace_direct_commit_allowed(&ws.ctx.actor, ws.rc.id(), &branch).await.unwrap_or(false);
    let me = ws.ctx.state.accounts.get_user(ws.user).await.ok();
    let login = me.as_ref().map(|u| u.username.clone()).unwrap_or_default();
    let me_email = me.as_ref().map(|u| crate::views::repo::signature(&ws.ctx, u).email).unwrap_or_default();
    let head_commit = match &head {
        Some(h) => ws.ctx.state.git.commit(ws.rc.id(), h).await.ok(),
        None => None,
    };
    let suggested_branch = (can_write && !direct_commit && !login.is_empty()).then(|| suggest_branch(&login));
    let ci = ci_state(&ws).await;
    let key = draft_key(&ws.ctx.state.config, ws.user, ws.rc.id());
    let r = &ws.rc.repo;
    let boot = Boot {
        repo: BootRepo {
            id: r.id.0,
            owner: r.owner_name.clone(),
            name: r.name.clone(),
            full_name: r.full_name(),
            default_branch: r.default_branch.clone(),
            empty: r.is_empty,
            archived: r.archived,
        },
        login,
        user_id: ws.user.0,
        me_email,
        branch,
        head_parents: head_commit.as_ref().map(|c| c.parents.clone()).unwrap_or_default(),
        head_author_email: head_commit.map(|c| c.author.email).unwrap_or_default(),
        head,
        branches,
        branches_cut,
        can_write,
        can_triage: ws.rc.perm.allows(Permission::Triage),
        direct_commit,
        suggested_branch,
        ci,
        draft_key: base64::engine::general_purpose::STANDARD.encode(key),
        limits: BootLimits {
            file_bytes: FILE_BYTES,
            file_bytes_phone: FILE_BYTES_PHONE,
            tree_entries: ch_core::model::WorkspaceTreeLimits::default().max_entries,
            commit_changes: super::commit::MAX_CHANGES,
            commit_file_bytes: super::commit::MAX_FILE_BYTES,
            commit_total_bytes: super::commit::MAX_TOTAL_BYTES,
        },
        terminal: ws.terminal_on(),
        terminal_runtime: if ws.terminal_on() { terminal_runtime(&ws) } else { Vec::new() },
    };
    Ok(json(&boot))
}

/// Whether a commit's workflows could start now, and if not, why, in plain words.
async fn ci_state(ws: &WorkspaceCtx) -> BootCi {
    use ch_core::model::CiAvailability;
    let paused = |reason: &str| BootCi { paused: true, reason: reason.to_string() };
    match ws.ctx.state.ci.availability(ws.rc.id()).await {
        Ok(CiAvailability::Enabled) => {}
        Ok(_) => return paused("Workflows are off for this repository."),
        Err(_) => return paused("Workflows are paused on this server."),
    }
    match ws.ctx.state.pipelines.runners(&ws.ctx.actor, Some(ws.rc.id())).await {
        Ok(rs) if rs.iter().any(|r| r.status == "online" || r.status == "busy") => BootCi { paused: false, reason: String::new() },
        _ => paused("Workflows are paused on this server. No runner has checked in."),
    }
}

pub async fn tree(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let (branch, sha) = pinned(&ws, &q).await?;
    let t = ws.ctx.state.git.workspace_tree(ws.rc.id(), &sha, ch_core::model::WorkspaceTreeLimits::default()).await?;
    Ok(json(&serde_json::json!({ "branch": branch, "sha": sha, "cut": t.cut, "entries": t.entries })))
}

pub async fn file(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let bad_path = || WsError::invalid("path_b64", "That is not a file path.");
    let path =
        base64::engine::general_purpose::STANDARD.decode(q.get("path_b64").map(String::as_str).unwrap_or("")).map_err(|_| bad_path())?;
    if path.is_empty() || path.len() > 4096 || path.contains(&0) {
        return Err(bad_path());
    }
    let (_, sha) = pinned(&ws, &q).await?;
    let b = ws.ctx.state.git.workspace_blob(ws.rc.id(), &sha, &path, FILE_BYTES).await?;
    let mut r = (StatusCode::OK, b.bytes).into_response();
    let h = r.headers_mut();
    super::ctx::data_headers(h);
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    if let Ok(v) = HeaderValue::from_str(&b.mode) {
        h.insert("x-file-mode", v);
    }
    h.insert("x-file-size", HeaderValue::from(b.size));
    h.insert("x-too-large", HeaderValue::from_static(if b.too_large { "1" } else { "0" }));
    Ok(r)
}

/// The `search` body: the branch and sha the page pinned, and the text to look for.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchBody {
    pub branch: String,
    pub sha: String,
    pub text: String,
}

pub async fn search(ctx: Ctx, Path((owner, name)): Path<(String, String)>, headers: HeaderMap, body: Body) -> WsResult<Response> {
    let ws = WorkspaceCtx::load_post(ctx, &owner, &name, Need::Read, &headers).await?;
    let _permit = ws.limit(super::limits::Kind::Search)?;
    let super::ctx::JsonData(req) = super::ctx::JsonData::<SearchBody>::read(&ws, &headers, body, 4096).await?;
    let mut q = HashMap::new();
    q.insert("branch".to_string(), req.branch.clone());
    q.insert("sha".to_string(), req.sha.clone());
    let (_, sha) = pinned(&ws, &q).await?;
    let text = req.text.trim();
    if text.is_empty() || text.chars().count() > 200 || text.contains('\n') || text.contains('\0') {
        return Err(WsError::invalid("text", "Search for 1 to 200 characters on one line."));
    }
    let s = ws.ctx.state.git.workspace_search(ws.rc.id(), &sha, text, ch_core::model::WorkspaceSearchLimits::default()).await?;
    Ok(json(&s))
}

/// The first bytes of a snapshot body.
pub const SNAPSHOT_MAGIC: &[u8; 8] = b"CHSNAP1\n";

/// `GET B/snapshot?branch=&sha=&path_b64=`: every regular file under one folder (all of them
/// when `path_b64` is empty) at the pinned commit, for the terminal. The body is length framed,
/// never JSON or HTML: the magic, a big-endian u32 count, then per file a flags byte (1 is
/// executable), a u32 path length, the UTF-8 path, a u32 size and the bytes. Over 2,000 files or
/// 25 MB it is `too_large` and nothing is read. Symlinks, submodules and names that are not
/// UTF-8 are left out; `x-skipped` says how many.
pub async fn snapshot(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    super::terminal::require_terminal(&ws)?;
    let _permit = ws.limit(super::limits::Kind::Snapshot)?;
    let bad_path = || WsError::invalid("path_b64", "That is not a folder.");
    let folder =
        base64::engine::general_purpose::STANDARD.decode(q.get("path_b64").map(String::as_str).unwrap_or("")).map_err(|_| bad_path())?;
    let (_, sha) = pinned(&ws, &q).await?;
    let limits = ch_core::model::WorkspaceSnapshotLimits::default();
    let snap = match ws.ctx.state.git.workspace_snapshot(ws.rc.id(), &sha, &folder, limits).await {
        Err(ch_core::Error::Validation(_)) => return Err(bad_path()),
        other => other?,
    };
    if snap.too_large {
        return Err(WsError::too_large(format!(
            "That is {} files and {} bytes. The terminal takes up to {} files and {} bytes: pick a folder.",
            snap.total_files, snap.total_bytes, limits.max_files, limits.max_bytes
        )));
    }
    let size: usize = snap.files.iter().map(|f| 9 + f.path.len() + f.bytes.len()).sum();
    let mut body = Vec::with_capacity(12 + size);
    body.extend_from_slice(SNAPSHOT_MAGIC);
    body.extend_from_slice(&(snap.files.len() as u32).to_be_bytes());
    for f in &snap.files {
        body.push(u8::from(f.executable));
        body.extend_from_slice(&(f.path.len() as u32).to_be_bytes());
        body.extend_from_slice(f.path.as_bytes());
        body.extend_from_slice(&(f.bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(&f.bytes);
    }
    let mut r = (StatusCode::OK, body).into_response();
    let h = r.headers_mut();
    super::ctx::data_headers(h);
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    h.insert("x-skipped", HeaderValue::from(snap.skipped as u64));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_branch_names_refuse_bidi_and_invisible_characters() {
        assert!(valid_new_branch("alice/patch-1"));
        assert!(!valid_new_branch("main\u{202E}"));
        assert!(!valid_new_branch("a\u{200B}b"));
        assert!(!valid_new_branch("a\u{2066}b"));
        assert!(valid_branch("main\u{202E}"), "an existing branch with such a name stays readable");
    }
}
