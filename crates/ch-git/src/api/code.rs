//! Commits, compare, contents, readme, archives and the git data endpoints.

use super::convert::*;
use super::{Api, ApiJson, PageQuery, header_str, paged, trim_page};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ch_api_types::repos as dto;
use ch_core::model::*;
use ch_core::services::ArchiveFormat;
use ch_core::state::AppState;
use ch_core::{Error, Permission, Result};
use serde::Deserialize;
use serde_json::json;

/// Largest file the contents API returns inline. Anything bigger is a download.
const MAX_INLINE_BYTES: u64 = 10 * 1024 * 1024;
/// The most a raw contents answer carries, the same as `/{owner}/{repo}/raw/...` on the web.
pub const MAX_RAW_BYTES: u64 = 50_000_000;
/// Largest file the contents API accepts, as base64 in JSON.
const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{repo}/commits", get(list_commits))
        .route("/repos/{owner}/{repo}/commits/{ref}", get(get_commit))
        .route("/repos/{owner}/{repo}/compare/{basehead}", get(compare))
        .route("/repos/{owner}/{repo}/contents", get(contents_root))
        .route(
            "/repos/{owner}/{repo}/contents/{*path}",
            get(contents).put(put_contents).delete(delete_contents).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES * 4 / 3 + 4096)),
        )
        .route("/repos/{owner}/{repo}/readme", get(readme))
        .route("/repos/{owner}/{repo}/zipball", get(zipball_default))
        .route("/repos/{owner}/{repo}/zipball/{ref}", get(zipball))
        .route("/repos/{owner}/{repo}/tarball", get(tarball_default))
        .route("/repos/{owner}/{repo}/tarball/{ref}", get(tarball))
        .route("/repos/{owner}/{repo}/git/refs", get(git_refs).post(git_create_ref))
        .route("/repos/{owner}/{repo}/git/matching-refs/{*prefix}", get(git_matching_refs))
        .route("/repos/{owner}/{repo}/git/ref/{*name}", get(git_ref))
        .route("/repos/{owner}/{repo}/git/refs/{*name}", get(git_ref).patch(git_update_ref).delete(git_delete_ref))
        .route("/repos/{owner}/{repo}/git/trees/{sha}", get(git_tree))
        .route("/repos/{owner}/{repo}/git/blobs/{sha}", get(git_blob))
        .route("/repos/{owner}/{repo}/git/commits/{sha}", get(get_commit_by_sha))
}

#[derive(Deserialize, Default)]
struct RefQuery {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
}

fn empty_repo() -> Error {
    Error::conflict("Git Repository is empty.")
}

// ------------------------------------------------------------------------ commits

#[derive(Deserialize)]
struct CommitsQuery {
    sha: Option<String>,
    path: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

async fn list_commits(api: Api, Path((owner, name)): Path<(String, String)>, Query(q): Query<CommitsQuery>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    if repo.is_empty {
        return Err(empty_repo());
    }
    let pq = PageQuery { page: q.page, per_page: q.per_page };
    let rev = q.sha.clone().unwrap_or_else(|| repo.default_branch.clone());
    let rows = api.core.read_log(repo.id, &rev, q.path.as_deref(), pq.skip(), pq.per_page() + 1).await?;
    let (rows, more) = trim_page(rows, pq);
    let out = commits(&api, &repo, &rows).await?;
    Ok(paged(&format!("{}/repos/{owner}/{name}/commits", api_url(api.base())), pq, out, more))
}

async fn get_commit(api: Api, Path((owner, name, git_ref)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let info = api.core.read_commit(repo.id, &git_ref).await?;
    let diff = api.core.commit_diff(repo.id, &info.sha).await?;
    Ok(Json(commit(&api, &repo, &info, Some(&diff)).await?).into_response())
}

async fn get_commit_by_sha(api: Api, Path((owner, name, sha)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let info = api.core.read_commit(repo.id, &sha).await?;
    Ok(Json(commit(&api, &repo, &info, None).await?).into_response())
}

async fn compare(api: Api, Path((owner, name, basehead)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let (base, head) = basehead
        .split_once("...")
        .or_else(|| basehead.split_once(".."))
        .ok_or_else(|| Error::invalid("Compare", "basehead", "use base...head"))?;
    let c = api.core.read_compare(repo.id, base, head).await?;
    let mb = api.core.read_commit(repo.id, &c.merge_base).await?;
    let base_url = api_url(api.base());
    let body = dto::Compare {
        url: format!("{base_url}/repos/{owner}/{name}/compare/{basehead}"),
        html_url: format!("{}/{owner}/{name}/compare/{basehead}", api.base()),
        status: compare_status(c.ahead_by, c.behind_by),
        ahead_by: c.ahead_by as i64,
        behind_by: c.behind_by as i64,
        total_commits: c.ahead_by as i64,
        commits: commits(&api, &repo, &c.commits).await?,
        files: commit_files(&c.diff),
        merge_base_commit: commit(&api, &repo, &mb, None).await?,
    };
    Ok(Json(body).into_response())
}

// ------------------------------------------------------------------------ contents

fn rev_of(repo: &Repo, q: &RefQuery) -> String {
    q.git_ref.clone().filter(|r| !r.is_empty()).unwrap_or_else(|| repo.default_branch.clone())
}

fn wants_raw(h: &HeaderMap) -> bool {
    header_str(h, "accept").is_some_and(|a| a.contains(".raw"))
}

async fn contents_root(
    api: Api,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<RefQuery>,
    headers: HeaderMap,
) -> Result<Response> {
    contents_at(api, owner, name, String::new(), q, headers).await
}

async fn contents(
    api: Api,
    Path((owner, name, path)): Path<(String, String, String)>,
    Query(q): Query<RefQuery>,
    headers: HeaderMap,
) -> Result<Response> {
    contents_at(api, owner, name, path, q, headers).await
}

async fn contents_at(api: Api, owner: String, name: String, path: String, q: RefQuery, headers: HeaderMap) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    if repo.is_empty {
        return Err(Error::NotFound);
    }
    let rev = rev_of(&repo, &q);
    let clean = crate::command::check_path(&path)?.to_string();
    let (kind, size) = if clean.is_empty() {
        ("tree".to_string(), 0)
    } else {
        let (_, kind, size) = api.core.object_info(repo.id, &format!("{rev}:{clean}")).await?;
        (kind, size)
    };
    if kind == "tree" {
        let entries = api.core.read_tree(repo.id, &rev, &clean).await?;
        let out: Vec<dto::ContentEntry> = entries.iter().map(|e| content_entry(api.base(), &repo, &rev, e, None)).collect();
        return Ok(Json(out).into_response());
    }
    if size > MAX_INLINE_BYTES && !wants_raw(&headers) {
        return Err(Error::forbidden("This file is too large to return as JSON. Ask for the raw media type or download an archive."));
    }
    if size > MAX_RAW_BYTES {
        return Err(Error::forbidden("This file is too large to return here. Clone the repository or download an archive."));
    }
    // A raw answer is read into memory, so it has a ceiling too: the same one as a raw file on
    // the web (ch-ulq). Larger files come from a clone or an archive.
    let blob = api.core.read_blob(repo.id, &rev, &clean, if wants_raw(&headers) { MAX_RAW_BYTES } else { MAX_INLINE_BYTES }).await?;
    if blob.truncated {
        return Err(Error::forbidden("This file is too large to return here. Clone the repository or download an archive."));
    }
    if wants_raw(&headers) {
        let mut r = Response::new(Body::from(blob.content));
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        return Ok(r);
    }
    let parent = clean.rsplit_once('/').map(|(_, n)| n).unwrap_or(&clean).to_string();
    let entry = TreeEntry {
        name: parent,
        path: clean.clone(),
        kind: EntryKind::Blob,
        mode: "100644".into(),
        sha: blob.sha.clone(),
        size: Some(blob.size),
    };
    Ok(Json(content_entry(api.base(), &repo, &rev, &entry, Some(&blob))).into_response())
}

#[derive(Deserialize)]
struct PutBody {
    #[serde(flatten)]
    body: dto::PutContent,
}

fn identity_of(body_committer: Option<&dto::GitIdentity>, fallback: (String, String)) -> Signature {
    match body_committer {
        Some(c) => Signature { name: c.name.clone(), email: c.email.clone(), at: chrono::Utc::now() },
        None => Signature { name: fallback.0, email: fallback.1, at: chrono::Utc::now() },
    }
}

async fn put_contents(
    api: Api,
    Path((owner, name, path)): Path<(String, String, String)>,
    ApiJson(PutBody { body }): ApiJson<PutBody>,
) -> Result<Response> {
    let repo = api.repo_with(&owner, &name, Permission::Write).await?;
    let user = api.actor.user_id().ok_or_else(|| Error::forbidden("Editing files needs a person's account."))?;
    let branch = body.branch.clone().unwrap_or_else(|| repo.default_branch.clone());
    let clean = super::super::write::check_edit_path(&path)?;
    let bytes = decode_base64(&body.content).ok_or_else(|| Error::invalid("File", "content", "content must be base64"))?;
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(Error::invalid("File", "content", "files over 100 MB cannot be uploaded this way"));
    }
    let existing = if repo.is_empty { None } else { api.core.object_info(repo.id, &format!("refs/heads/{branch}:{clean}")).await.ok() };
    let is_update = match (&existing, &body.sha) {
        (Some((current, kind, _)), Some(given)) if kind == "blob" => {
            if current != given {
                return Err(Error::conflict("The file has changed since you read it. Reload and try again."));
            }
            true
        }
        (Some(_), None) => return Err(Error::invalid("File", "sha", "that file already exists, so its current sha is required")),
        (Some(_), Some(_)) => return Err(Error::invalid("File", "path", "that path is a folder")),
        (None, _) => false,
    };
    let author = identity_of(body.committer.as_ref(), api.core.commit_identity(user).await?);
    let req = CommitFileChange {
        repo_id: repo.id,
        branch: branch.clone(),
        new_branch: None,
        expected_sha: None,
        changes: vec![FileChange { path: clean.clone(), content: Some(bytes.clone()), renamed_from: None }],
        message: body.message.clone(),
        author,
    };
    let sha = api.core.do_commit_file_change(&api.actor, req, &api.client).await?;
    let info = api.core.read_commit(repo.id, &sha).await?;
    let (blob_sha, ..) = api.core.object_info(repo.id, &format!("{sha}:{clean}")).await?;
    let entry = TreeEntry {
        name: clean.rsplit('/').next().unwrap_or(&clean).to_string(),
        path: clean,
        kind: EntryKind::Blob,
        mode: "100644".into(),
        sha: blob_sha,
        size: Some(bytes.len() as u64),
    };
    let resp = dto::PutContentResponse {
        content: Some(content_entry(api.base(), &repo, &branch, &entry, None)),
        commit: commit(&api, &repo, &info, None).await?,
    };
    Ok((if is_update { StatusCode::OK } else { StatusCode::CREATED }, Json(resp)).into_response())
}

async fn delete_contents(
    api: Api,
    Path((owner, name, path)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<dto::DeleteContent>,
) -> Result<Response> {
    let repo = api.repo_with(&owner, &name, Permission::Write).await?;
    let user = api.actor.user_id().ok_or_else(|| Error::forbidden("Editing files needs a person's account."))?;
    let branch = body.branch.clone().unwrap_or_else(|| repo.default_branch.clone());
    let clean = super::super::write::check_edit_path(&path)?;
    let (current, kind, _) = api.core.object_info(repo.id, &format!("refs/heads/{branch}:{clean}")).await?;
    if kind != "blob" {
        return Err(Error::NotFound);
    }
    if current != body.sha {
        return Err(Error::conflict("The file has changed since you read it. Reload and try again."));
    }
    let author = identity_of(None, api.core.commit_identity(user).await?);
    let req = CommitFileChange {
        repo_id: repo.id,
        branch,
        new_branch: None,
        expected_sha: None,
        changes: vec![FileChange { path: clean, content: None, renamed_from: None }],
        message: body.message.clone(),
        author,
    };
    let sha = api.core.do_commit_file_change(&api.actor, req, &api.client).await?;
    let info = api.core.read_commit(repo.id, &sha).await?;
    Ok(Json(json!({ "content": null, "commit": commit(&api, &repo, &info, None).await? })).into_response())
}

async fn readme(api: Api, Path((owner, name)): Path<(String, String)>, Query(q): Query<RefQuery>, headers: HeaderMap) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    if repo.is_empty {
        return Err(Error::NotFound);
    }
    let rev = rev_of(&repo, &q);
    let root = api.core.read_tree(repo.id, &rev, "").await?;
    let found = pick_readme(&root).ok_or(Error::NotFound)?;
    let path = found.path.clone();
    contents_at(api, owner, name, path, q, headers).await
}

/// The README of a folder listing: `README.md` first, then other markup, then plain `README`.
pub fn pick_readme(entries: &[TreeEntry]) -> Option<&TreeEntry> {
    let rank = |n: &str| -> Option<u8> {
        let l = n.to_ascii_lowercase();
        match l.as_str() {
            "readme.md" | "readme.markdown" => Some(0),
            "readme.rst" | "readme.adoc" | "readme.org" => Some(1),
            "readme.txt" => Some(2),
            "readme" => Some(3),
            _ => None,
        }
    };
    entries
        .iter()
        .filter(|e| e.kind == EntryKind::Blob || e.kind == EntryKind::Symlink)
        .filter_map(|e| rank(&e.name).map(|r| (r, e)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, e)| e)
}

// ------------------------------------------------------------------------ archives

async fn archive(api: Api, owner: String, name: String, rev: Option<String>, format: ArchiveFormat) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    if repo.is_empty {
        return Err(Error::NotFound);
    }
    let rev = rev.unwrap_or_else(|| repo.default_branch.clone());
    let stream = api.core.read_archive(repo.id, &rev, format).await?;
    let (ext, ct) = match format {
        ArchiveFormat::Zip => ("zip", "application/zip"),
        ArchiveFormat::TarGz => ("tar.gz", "application/gzip"),
    };
    let safe: String = rev.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '-' }).collect();
    let mut r = Response::new(Body::from_stream(stream));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    if let Ok(v) = HeaderValue::from_str(&ch_core::disposition::attachment(&format!("{}-{safe}.{ext}", repo.name))) {
        r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
}

async fn zipball(api: Api, Path((o, n, r)): Path<(String, String, String)>) -> Result<Response> {
    archive(api, o, n, Some(r), ArchiveFormat::Zip).await
}
async fn zipball_default(api: Api, Path((o, n)): Path<(String, String)>) -> Result<Response> {
    archive(api, o, n, None, ArchiveFormat::Zip).await
}
async fn tarball(api: Api, Path((o, n, r)): Path<(String, String, String)>) -> Result<Response> {
    archive(api, o, n, Some(r), ArchiveFormat::TarGz).await
}
async fn tarball_default(api: Api, Path((o, n)): Path<(String, String)>) -> Result<Response> {
    archive(api, o, n, None, ArchiveFormat::TarGz).await
}

// ------------------------------------------------------------------------ git data

fn ref_dto(api: &Api, repo: &Repo, name: &str, sha: &str, kind: &str) -> dto::GitRef {
    let base = api_url(api.base());
    let full = format!("{base}/repos/{}/{}", repo.owner_name, repo.name);
    dto::GitRef {
        git_ref: name.to_string(),
        url: format!("{full}/git/{name}"),
        object: dto::GitRefObject {
            sha: sha.to_string(),
            kind: kind.to_string(),
            url: format!("{full}/git/{}s/{sha}", if kind == "tag" { "tag" } else { "commit" }),
        },
    }
}

async fn refs_under(api: &Api, repo: &Repo, prefixes: &[&str]) -> Result<Vec<dto::GitRef>> {
    if repo.is_empty {
        return Ok(Vec::new());
    }
    let mut raw = api.core.read_refs_raw(repo.id, prefixes).await?;
    raw.sort();
    Ok(raw.iter().map(|(n, s, t)| ref_dto(api, repo, n, s, t)).collect())
}

async fn git_refs(api: Api, Path((owner, name)): Path<(String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    Ok(Json(refs_under(&api, &repo, &["refs/heads/", "refs/tags/"]).await?).into_response())
}

async fn git_matching_refs(api: Api, Path((owner, name, prefix)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let full = format!("refs/{}", prefix.trim_start_matches("refs/"));
    if !(full.starts_with("refs/heads") || full.starts_with("refs/tags")) {
        return Ok(Json(Vec::<dto::GitRef>::new()).into_response());
    }
    let all = refs_under(&api, &repo, &["refs/heads/", "refs/tags/"]).await?;
    Ok(Json(all.into_iter().filter(|r| r.git_ref.starts_with(&full)).collect::<Vec<_>>()).into_response())
}

fn full_ref(name: &str) -> String {
    format!("refs/{}", name.trim_start_matches("refs/"))
}

async fn git_ref(api: Api, Path((owner, name, r)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let full = full_ref(&r);
    let found =
        refs_under(&api, &repo, &["refs/heads/", "refs/tags/"]).await?.into_iter().find(|x| x.git_ref == full).ok_or(Error::NotFound)?;
    Ok(Json(found).into_response())
}

async fn git_create_ref(api: Api, Path((owner, name)): Path<(String, String)>, ApiJson(body): ApiJson<dto::CreateRef>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    if !body.git_ref.starts_with("refs/heads/") && !body.git_ref.starts_with("refs/tags/") {
        return Err(Error::invalid("Reference", "ref", "only refs/heads/ and refs/tags/ can be created"));
    }
    api.core.do_create_ref(&api.actor, repo.id, &body.git_ref, &body.sha, &api.client).await?;
    let found = refs_under(&api, &repo, &["refs/heads/", "refs/tags/"])
        .await?
        .into_iter()
        .find(|x| x.git_ref == body.git_ref)
        .ok_or(Error::NotFound)?;
    Ok((StatusCode::CREATED, Json(found)).into_response())
}

async fn git_update_ref(
    api: Api,
    Path((owner, name, r)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<dto::UpdateRef>,
) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let full = full_ref(&r);
    api.core.do_update_ref(&api.actor, repo.id, &full, &body.sha, body.force, &api.client).await?;
    let found =
        refs_under(&api, &repo, &["refs/heads/", "refs/tags/"]).await?.into_iter().find(|x| x.git_ref == full).ok_or(Error::NotFound)?;
    Ok(Json(found).into_response())
}

async fn git_delete_ref(api: Api, Path((owner, name, r)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    api.core.do_delete_ref(&api.actor, repo.id, &full_ref(&r), &api.client).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct TreeQuery {
    recursive: Option<String>,
}

async fn git_tree(api: Api, Path((owner, name, sha)): Path<(String, String, String)>, Query(q): Query<TreeQuery>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let base = api_url(api.base());
    let recursive = q.recursive.as_deref().is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
    let (entries, truncated) = if recursive {
        api.core.read_tree_recursive(repo.id, &format!("{sha}^{{tree}}"), 100_000).await?
    } else {
        (api.core.read_tree(repo.id, &format!("{sha}^{{tree}}"), "").await?, false)
    };
    let (tree_sha, ..) = api.core.object_info(repo.id, &format!("{sha}^{{tree}}")).await?;
    let items: Vec<_> = entries
        .iter()
        .map(|e| {
            let kind = match e.kind {
                EntryKind::Tree => "tree",
                EntryKind::Commit => "commit",
                _ => "blob",
            };
            let route = if e.kind == EntryKind::Tree { "trees" } else { "blobs" };
            json!({
                "path": if recursive { e.path.clone() } else { e.name.clone() },
                "mode": e.mode,
                "type": kind,
                "sha": e.sha,
                "size": e.size,
                "url": format!("{base}/repos/{owner}/{name}/git/{route}/{}", e.sha),
            })
        })
        .collect();
    Ok(Json(json!({ "sha": tree_sha, "url": format!("{base}/repos/{owner}/{name}/git/trees/{tree_sha}"), "tree": items, "truncated": truncated })).into_response())
}

async fn git_blob(api: Api, Path((owner, name, sha)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let blob = api.core.read_blob(repo.id, &sha, "", MAX_INLINE_BYTES).await?;
    if blob.truncated {
        return Err(Error::forbidden("This file is too large to return as JSON."));
    }
    let base = api_url(api.base());
    Ok(Json(json!({
        "sha": blob.sha,
        "size": blob.size,
        "encoding": "base64",
        "content": encode_base64(&blob.content),
        "url": format!("{base}/repos/{owner}/{name}/git/blobs/{}", blob.sha),
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> TreeEntry {
        TreeEntry { name: name.into(), path: name.into(), kind, mode: "100644".into(), sha: "a".repeat(40), size: Some(1) }
    }

    #[test]
    fn readme_preference() {
        let list = vec![entry("README.txt", EntryKind::Blob), entry("readme.md", EntryKind::Blob), entry("src", EntryKind::Tree)];
        assert_eq!(pick_readme(&list).unwrap().name, "readme.md");
        let list = vec![entry("README", EntryKind::Blob), entry("README.rst", EntryKind::Blob)];
        assert_eq!(pick_readme(&list).unwrap().name, "README.rst");
        assert!(pick_readme(&[entry("README", EntryKind::Tree)]).is_none());
        assert!(pick_readme(&[]).is_none());
    }

    #[test]
    fn raw_media_type() {
        let mut h = HeaderMap::new();
        h.insert("accept", HeaderValue::from_static("application/vnd.github.raw+json"));
        assert!(wants_raw(&h));
        h.insert("accept", HeaderValue::from_static("application/json"));
        assert!(!wants_raw(&h));
    }

    #[test]
    fn full_refs() {
        assert_eq!(full_ref("heads/main"), "refs/heads/main");
        assert_eq!(full_ref("refs/tags/v1"), "refs/tags/v1");
    }
}
