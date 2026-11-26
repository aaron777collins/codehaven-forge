//! The JSON a workflow reads as `github.event` (ch-ctk.7, ch-ctk.10): the repository, the sender,
//! a pull request and a release in the shape GitHub's webhooks use, so an action that reads
//! `context.payload.release.id` or `github.event.pull_request.base.sha` finds it.
//!
//! Only what any reader of the repository may already see goes in: names, ids, the pull request's
//! text and a release's assets by name and digest. No e-mail address of any account, and no
//! secret, is ever written here.

use crate::ctx::Ctx;
use ch_core::{Error, ReleaseId, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use serde_json::{Value as Json, json};
use sqlx::FromRow;

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A user the way webhooks show one.
pub fn user_object(base: &str, id: i64, login: &str, kind: &str) -> Json {
    json!({
        "login": login,
        "id": id,
        "type": if kind == "user" { "User" } else if kind == "org" { "Organization" } else { "Bot" },
        "site_admin": false,
        "html_url": format!("{base}/{}", enc(login)),
        "url": format!("{base}/api/v3/users/{}", enc(login)),
        "avatar_url": format!("{base}/avatar/{}", enc(login)),
    })
}

#[derive(FromRow)]
struct UserRow {
    id: i64,
    username: String,
    kind: String,
}

/// An account by id. A missing one is the empty object, not an error: a deleted author must not
/// stop a run from starting.
pub async fn user_by_id(ctx: &Ctx, id: UserId) -> Result<Json> {
    let row: Option<UserRow> =
        sqlx::query_as("SELECT id, username::text AS username, kind FROM users WHERE id = $1").bind(id.0).fetch_optional(&ctx.db).await?;
    Ok(row.map(|u| user_object(&ctx.base_url(), u.id, &u.username, &u.kind)).unwrap_or_else(|| json!({})))
}

#[derive(FromRow)]
struct RepoRow {
    id: i64,
    owner_id: i64,
    owner_name: String,
    owner_kind: String,
    name: String,
    description: String,
    visibility: String,
    default_branch: String,
    archived: bool,
    fork: bool,
    created_at: DateTime<Utc>,
}

/// The repository object. `private` is true for anything that is not public, as it is on GitHub
/// for internal repositories.
pub async fn repository_object(ctx: &Ctx, repo: RepoId) -> Result<Json> {
    let r: RepoRow = sqlx::query_as(
        "SELECT r.id, r.owner_id, u.username::text AS owner_name, u.kind AS owner_kind, r.name::text AS name,
                COALESCE(r.description, '') AS description, r.access_tier::text AS visibility, r.default_branch, r.archived,
                (r.fork_root_id IS NOT NULL) AS fork, r.created_at
         FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(repo.0)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or(Error::NotFound)?;
    let base = ctx.base_url();
    let full = format!("{}/{}", r.owner_name, r.name);
    Ok(json!({
        "id": r.id,
        "name": r.name,
        "full_name": full,
        "private": r.visibility != "public",
        "visibility": r.visibility,
        "owner": user_object(&base, r.owner_id, &r.owner_name, &r.owner_kind),
        "html_url": format!("{base}/{full}"),
        "url": format!("{base}/api/v3/repos/{full}"),
        "clone_url": format!("{base}/{full}.git"),
        "description": if r.description.is_empty() { Json::Null } else { json!(r.description) },
        "fork": r.fork,
        "archived": r.archived,
        "default_branch": r.default_branch,
        "created_at": stamp(r.created_at),
    }))
}

#[derive(FromRow)]
struct ReleaseRow {
    id: i64,
    tag_name: String,
    target_commitish: String,
    name: String,
    body: String,
    draft: bool,
    prerelease: bool,
    author_id: i64,
    created_at: DateTime<Utc>,
    published_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct AssetRow {
    id: i64,
    name: String,
    label: String,
    content_type: String,
    size: i64,
    blob_sha256: String,
    download_count: i64,
    uploader_id: Option<i64>,
    created_at: DateTime<Utc>,
}

/// The release object with its assets, as the REST API gives it. `full_name` is `owner/repo`.
pub async fn release_object(ctx: &Ctx, repo: RepoId, full_name: &str, release: ReleaseId) -> Result<Json> {
    let r: ReleaseRow = sqlx::query_as(
        "SELECT id, tag_name, target_commitish, name, body, draft, prerelease, author_id, created_at, published_at
         FROM releases WHERE id = $1 AND repo_id = $2",
    )
    .bind(release.0)
    .bind(repo.0)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or(Error::NotFound)?;
    let assets: Vec<AssetRow> = sqlx::query_as(
        "SELECT id, name, label, content_type, size, blob_sha256, download_count, uploader_id, created_at
         FROM release_assets WHERE release_id = $1 ORDER BY name",
    )
    .bind(release.0)
    .fetch_all(&ctx.db)
    .await?;
    let base = ctx.base_url();
    let api = format!("{base}/api/v3/repos/{full_name}");
    let tag = enc(&r.tag_name);
    let author = user_by_id(ctx, UserId(r.author_id)).await?;
    let mut out_assets = Vec::with_capacity(assets.len());
    for a in &assets {
        let uploader = match a.uploader_id {
            Some(u) => user_by_id(ctx, UserId(u)).await?,
            None => Json::Null,
        };
        out_assets.push(json!({
            "id": a.id,
            "name": a.name,
            "label": if a.label.is_empty() { Json::Null } else { json!(a.label) },
            "content_type": a.content_type,
            "state": "uploaded",
            "size": a.size,
            "digest": format!("sha256:{}", a.blob_sha256),
            "download_count": a.download_count,
            "created_at": stamp(a.created_at),
            "updated_at": stamp(a.created_at),
            "uploader": uploader,
            "url": format!("{api}/releases/assets/{}", a.id),
            "browser_download_url": format!("{base}/{full_name}/releases/download/{tag}/{}", enc(&a.name)),
        }));
    }
    Ok(json!({
        "id": r.id,
        "tag_name": r.tag_name,
        "target_commitish": r.target_commitish,
        "name": if r.name.is_empty() { Json::Null } else { json!(r.name) },
        "body": if r.body.is_empty() { Json::Null } else { json!(r.body) },
        "draft": r.draft,
        "prerelease": r.prerelease,
        "created_at": stamp(r.created_at),
        "published_at": r.published_at.map(stamp),
        "author": author,
        "assets": out_assets,
        "url": format!("{api}/releases/{}", r.id),
        "html_url": format!("{base}/{full_name}/releases/tag/{tag}"),
        "assets_url": format!("{api}/releases/{}/assets", r.id),
        "upload_url": format!("{api}/releases/{}/assets{{?name,label}}", r.id),
        "tarball_url": format!("{api}/tarball/{tag}"),
        "zipball_url": format!("{api}/zipball/{tag}"),
    }))
}

#[derive(FromRow)]
struct PullRow {
    id: i64,
    title: String,
    body: String,
    open: bool,
    locked: bool,
    author_id: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
    merged_at: Option<DateTime<Utc>>,
    merge_commit_sha: Option<String>,
    draft: bool,
    head_ref: String,
    head_sha: String,
    base_ref: String,
    base_sha: String,
    head_repo_id: Option<i64>,
}

/// One side of a pull request.
async fn pull_side(ctx: &Ctx, repo: RepoId, git_ref: &str, sha: &str, full_name: &str) -> Result<Json> {
    let owner = full_name.split('/').next().unwrap_or("");
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
    Ok(json!({
        "label": format!("{owner}:{branch}"),
        "ref": branch,
        "sha": sha,
        "repo": repository_object(ctx, repo).await?,
        "user": json!({ "login": owner }),
    }))
}

/// The pull request object, from the stored rows. `number` is the pull request's number in `repo`.
pub async fn pull_object(ctx: &Ctx, repo: RepoId, full_name: &str, number: i64) -> Result<Json> {
    let p: PullRow = sqlx::query_as(
        "SELECT i.id, i.title, i.body, i.open, i.locked, i.author_id, i.created_at, i.updated_at, i.closed_at,
                pr.merged_at, pr.merge_commit_sha, pr.draft, pr.head_ref, pr.head_sha, pr.base_ref, pr.base_sha, pr.head_repo_id
         FROM pull_requests pr JOIN issues i ON i.id = pr.issue_id WHERE pr.repo_id = $1 AND i.number = $2",
    )
    .bind(repo.0)
    .bind(number)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or(Error::NotFound)?;
    let base = ctx.base_url();
    let api = format!("{base}/api/v3/repos/{full_name}");
    let html = format!("{base}/{full_name}/pulls/{number}");
    let labels: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT l.id, l.name::text, l.color FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = $1 ORDER BY l.name",
    )
    .bind(p.id)
    .fetch_all(&ctx.db)
    .await
    .unwrap_or_default();
    let head_repo = RepoId(p.head_repo_id.unwrap_or(repo.0));
    let head_full: String = sqlx::query_scalar(
        "SELECT u.username::text || '/' || r.name::text FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(head_repo.0)
    .fetch_optional(&ctx.db)
    .await?
    .unwrap_or_else(|| full_name.to_string());
    let state = if p.open { "open" } else { "closed" };
    Ok(json!({
        "id": p.id,
        "number": number,
        "state": state,
        "locked": p.locked,
        "title": p.title,
        "body": if p.body.is_empty() { Json::Null } else { json!(p.body) },
        "user": user_by_id(ctx, UserId(p.author_id)).await?,
        "labels": labels.iter().map(|(id, name, color)| json!({ "id": id, "name": name, "color": color })).collect::<Vec<_>>(),
        "draft": p.draft,
        "merged": p.merged_at.is_some(),
        "merged_at": p.merged_at.map(stamp),
        "merge_commit_sha": p.merge_commit_sha,
        "created_at": stamp(p.created_at),
        "updated_at": stamp(p.updated_at),
        "closed_at": p.closed_at.map(stamp),
        "html_url": html,
        "url": format!("{api}/pulls/{number}"),
        "issue_url": format!("{api}/issues/{number}"),
        "diff_url": format!("{html}.diff"),
        "patch_url": format!("{html}.patch"),
        "head": pull_side(ctx, head_repo, &p.head_ref, &p.head_sha, &head_full).await?,
        "base": pull_side(ctx, repo, &p.base_ref, &p.base_sha, full_name).await?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_object_has_names_and_links_and_no_email() {
        let u = user_object("https://h.example", 7, "ada lovelace", "user");
        assert_eq!(u["login"], "ada lovelace");
        assert_eq!(u["type"], "User");
        assert_eq!(u["html_url"], "https://h.example/ada%20lovelace");
        assert!(u.get("email").is_none());
        assert_eq!(user_object("https://h.example", 8, "codehaven-actions", "bot")["type"], "Bot");
    }
}
