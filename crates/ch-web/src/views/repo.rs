//! Browsing a repository: home, trees, files, raw, blame, history, commits, branches, tags,
//! compare, archives, and the file editor.

use super::code::{self, BlameRow, DiffMode, DiffOptions};
use super::repo_common::{BranchPicker, RefKind, RefPath, RepoCtx, branch_names, resolve_ref, tag_names};
use super::{Params, People, Query};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, StatusView, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ch_core::model::{CheckRun, CommitFileChange, CommitInfo, EntryKind, FileChange, Signature, TreeEntry};
use ch_core::services::ArchiveFormat;
use ch_core::web_contract::BlameHunk;
use ch_core::{Error, Permission};
use ch_render::markdown::{self, MdContext};

const BLOB_VIEW_LIMIT: u64 = 1_000_000;
const RAW_LIMIT: u64 = 50_000_000;
const PAGE: u32 = 30;

// ------------------------------------------------------------------ shared pieces

pub struct CommitRow {
    pub title: String,
    pub author: String,
    pub author_seed: String,
    pub sha: String,
    pub short: String,
    pub href: String,
    pub when: TimeView,
    pub status: Option<StatusView>,
    pub has_body: bool,
}

pub fn status_from_runs(runs: &[CheckRun]) -> Option<StatusView> {
    if runs.is_empty() {
        return None;
    }
    let any_fail = runs.iter().any(|r| matches!(r.conclusion.as_deref(), Some("failure" | "timed_out" | "error")));
    let any_active = runs.iter().any(|r| r.status != "completed");
    let all_ok = runs.iter().all(|r| matches!(r.conclusion.as_deref(), Some("success" | "skipped" | "neutral")));
    Some(if any_fail {
        fmt::status("completed", Some("failure"))
    } else if any_active {
        fmt::status("in_progress", None)
    } else if all_ok {
        fmt::status("completed", Some("success"))
    } else {
        fmt::status("completed", Some("cancelled"))
    })
}

pub async fn commit_rows(ctx: &Ctx, rc: &RepoCtx, commits: &[CommitInfo], with_status: bool) -> Vec<CommitRow> {
    let base = rc.base();
    // Signed out shows no CI status: those records keep the audience they were made for.
    let statuses: Vec<Option<StatusView>> = if with_status && !ctx.actor.is_anonymous() {
        let futs = commits.iter().map(|c| ctx.state.checks.check_runs_for_sha(rc.id(), &c.sha));
        futures::future::join_all(futs).await.into_iter().map(|r| r.ok().and_then(|runs| status_from_runs(&runs))).collect()
    } else {
        vec![None; commits.len()]
    };
    commits
        .iter()
        .zip(statuses)
        .map(|(c, status)| CommitRow {
            title: fmt::title_of(&c.message),
            author: c.author.name.clone(),
            author_seed: fmt::enc(&c.author.name),
            sha: c.sha.clone(),
            short: fmt::short_sha(&c.sha),
            href: format!("{base}/commit/{}", c.sha),
            when: fmt::time(c.author.at),
            status,
            has_body: !fmt::body_of(&c.message).is_empty(),
        })
        .collect()
}

pub struct Crumb {
    pub name: String,
    pub href: String,
}

/// Breadcrumbs for `a/b/c`: each part links to the tree at that depth.
fn crumbs(rc: &RepoCtx, rp: &RefPath, kind: &str, path: &str) -> Vec<Crumb> {
    let mut acc = String::new();
    let mut out = Vec::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        out.push(Crumb { name: part.to_string(), href: format!("{}/{kind}/{}/{}", rc.base(), rp.enc(), fmt::enc_path(&acc)) });
    }
    out
}

fn md_ctx(rc: &RepoCtx, rp: &RefPath, dir: &str) -> MdContext {
    MdContext {
        owner: rc.repo.owner_name.clone(),
        repo: rc.repo.name.clone(),
        git_ref: rp.name.clone(),
        dir: dir.to_string(),
        link_refs: true,
        heading_ids: true,
    }
}

fn is_markdown(name: &str) -> bool {
    let l = name.to_lowercase();
    l.ends_with(".md") || l.ends_with(".markdown") || l.ends_with(".mdown")
}

fn is_image(name: &str) -> bool {
    let l = name.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".avif", ".ico", ".bmp", ".svg"].iter().any(|e| l.ends_with(e))
}

pub(crate) fn signature(ctx: &Ctx, user: &ch_core::model::User) -> Signature {
    Signature {
        name: if user.display_name.is_empty() { user.username.clone() } else { user.display_name.clone() },
        email: ch_core::noreply::noreply_email(user.id.0, &user.username, &ctx.state.config.server.external_url),
        at: chrono::Utc::now(),
    }
}

/// Commit rows without the status lookup, for pages that list many commits at once.
pub async fn commit_rows_pub(ctx: &Ctx, rc: &RepoCtx, commits: &[CommitInfo]) -> Vec<CommitRow> {
    commit_rows(ctx, rc, commits, false).await
}

// ------------------------------------------------------------------ home and tree

pub struct EntryRow {
    pub name: String,
    pub href: String,
    pub is_dir: bool,
    pub icon: &'static str,
    pub size: String,
}

pub struct ReadmeView {
    pub name: String,
    pub html: String,
}

pub struct LastCommit {
    pub row: CommitRow,
    pub count_href: String,
}

pub struct CloneBox {
    pub https: String,
    pub ssh: Option<String>,
}

pub struct ReleaseChip {
    pub tag: String,
    pub name: String,
    pub href: String,
}

#[derive(Template)]
#[template(path = "repo/tree.html")]
struct TreePage {
    c: Chrome,
    picker: BranchPicker,
    is_home: bool,
    path: String,
    crumbs: Vec<Crumb>,
    root_href: String,
    entries: Vec<EntryRow>,
    last: Option<LastCommit>,
    readme: Option<ReadmeView>,
    clone: CloneBox,
    release: Option<ReleaseChip>,
    topics: Vec<String>,
    description: String,
    can_write: bool,
    new_file_href: String,
    compare_href: String,
    is_default: bool,
    archive_zip: String,
    archive_tgz: String,
    find_href: String,
}

#[derive(Template)]
#[template(path = "repo/empty.html")]
struct EmptyPage {
    c: Chrome,
    clone: CloneBox,
    branch: String,
    can_write: bool,
}

fn sorted_entries(rc: &RepoCtx, rp: &RefPath, mut entries: Vec<TreeEntry>) -> Vec<EntryRow> {
    entries.sort_by_key(|e| (e.kind != EntryKind::Tree, e.name.to_lowercase()));
    entries
        .into_iter()
        .map(|e| {
            let is_dir = e.kind == EntryKind::Tree;
            let kind = if is_dir { "tree" } else { "blob" };
            EntryRow {
                href: format!("{}/{kind}/{}/{}", rc.base(), rp.enc(), fmt::enc_path(&e.path)),
                icon: match e.kind {
                    EntryKind::Tree => "folder",
                    EntryKind::Commit => "git-fork",
                    EntryKind::Symlink => "link",
                    EntryKind::Blob => "file",
                },
                size: if is_dir { String::new() } else { e.size.map(ch_render::human_bytes).unwrap_or_default() },
                is_dir,
                name: e.name,
            }
        })
        .collect()
}

async fn tree_view(ctx: Ctx, rc: RepoCtx, rest: &str, home: bool) -> WebResult<Response> {
    let (rp, branches, tags) = resolve_ref(&ctx, &rc, rest).await?;
    let base = rc.base();
    if rc.repo.is_empty {
        let clone = CloneBox { https: rc.clone_https(&ctx), ssh: rc.clone_ssh(&ctx) };
        let c = rc.chrome(&ctx, "", "code").await;
        return ctx.page(&EmptyPage { c, clone, branch: rc.repo.default_branch.clone(), can_write: rc.perm.allows(Permission::Write) });
    }
    let entries = ctx.state.git.tree(rc.id(), &rp.name, &rp.path).await?;
    let sha = ctx.state.git.resolve(rc.id(), &rp.name).await.ok();

    let readme = {
        let found = entries.iter().find(|e| {
            e.kind == EntryKind::Blob && {
                let l = e.name.to_lowercase();
                l == "readme" || l == "readme.md" || l == "readme.markdown" || l == "readme.txt"
            }
        });
        match found {
            Some(e) => match ctx.state.git.blob(rc.id(), &rp.name, &e.path, BLOB_VIEW_LIMIT).await {
                Ok(b) if !b.is_binary => {
                    let text = String::from_utf8_lossy(&b.content).to_string();
                    let html = if is_markdown(&e.name) {
                        markdown::render(&text, &md_ctx(&rc, &rp, &rp.path))
                    } else {
                        format!("<pre class=\"plain\">{}</pre>", ch_render::escape_html(&text))
                    };
                    Some(ReadmeView { name: e.name.clone(), html })
                }
                _ => None,
            },
            None => None,
        }
    };

    let last = match ctx.state.git.log(rc.id(), &rp.name, if rp.path.is_empty() { None } else { Some(rp.path.as_str()) }, 0, 1).await {
        Ok(v) => match v.first() {
            Some(c) => {
                let mut rows = commit_rows(&ctx, &rc, std::slice::from_ref(c), true).await;
                rows.pop().map(|row| LastCommit { row, count_href: format!("{base}/commits/{}", rp.enc()) })
            }
            None => None,
        },
        Err(_) => None,
    };

    let release = if home {
        ctx.state.releases.latest(&ctx.actor, rc.id()).await.ok().map(|r| ReleaseChip {
            href: format!("{base}/releases/tag/{}", fmt::enc(&r.tag_name)),
            name: if r.name.is_empty() { r.tag_name.clone() } else { r.name.clone() },
            tag: r.tag_name,
        })
    } else {
        None
    };

    let picker = BranchPicker::new(&rp, &branches, &tags, &rc.repo.default_branch, &format!("{base}/tree"), &rp.path, &base);
    let _ = sha;
    let c = rc.chrome(&ctx, &rp.path, "code").await;
    let page = TreePage {
        c,
        picker,
        is_home: home && rp.path.is_empty(),
        crumbs: crumbs(&rc, &rp, "tree", &rp.path),
        root_href: format!("{base}/tree/{}", rp.enc()),
        path: rp.path.clone(),
        entries: sorted_entries(&rc, &rp, entries),
        last,
        readme,
        clone: CloneBox { https: rc.clone_https(&ctx), ssh: rc.clone_ssh(&ctx) },
        release,
        topics: rc.repo.topics.clone(),
        description: rc.repo.description.clone(),
        can_write: rc.perm.allows(Permission::Write) && !rc.repo.archived && rp.kind == RefKind::Branch,
        new_file_href: format!("{base}/new/{}/{}", rp.enc(), fmt::enc_path(&rp.path)),
        compare_href: format!("{base}/compare/{}...{}", fmt::enc_path(&rc.repo.default_branch), rp.enc()),
        is_default: rp.name == rc.repo.default_branch,
        archive_zip: format!("{base}/archive/{}.zip", rp.enc()),
        archive_tgz: format!("{base}/archive/{}.tar.gz", rp.enc()),
        find_href: format!("{base}/find/{}", rp.enc()),
    };
    ctx.page(&page)
}

pub async fn home(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    tree_view(ctx, rc, "", true).await
}

pub async fn tree(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    tree_view(ctx, rc, &rest, false).await
}

// ------------------------------------------------------------------ file finder

#[derive(Template)]
#[template(path = "repo/find.html")]
struct FindPage {
    c: Chrome,
    picker: BranchPicker,
    files: Vec<(String, String)>,
    truncated: bool,
    back_href: String,
}

const FIND_LIMIT: usize = 4000;
const FIND_DIRS: usize = 600;

/// Every file in a tree, for the "go to file" page. Walks directories breadth first and stops
/// at fixed limits, so a huge repository cannot make this page expensive.
pub async fn find(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let (rp, branches, tags) = resolve_ref(&ctx, &rc, &rest).await?;
    let base = rc.base();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut queue = std::collections::VecDeque::from([String::new()]);
    let (mut dirs, mut truncated) = (0usize, false);
    while let Some(dir) = queue.pop_front() {
        dirs += 1;
        if dirs > FIND_DIRS || files.len() >= FIND_LIMIT {
            truncated = true;
            break;
        }
        for e in ctx.state.git.tree(rc.id(), &rp.name, &dir).await? {
            if e.kind == EntryKind::Tree {
                queue.push_back(e.path);
            } else if files.len() < FIND_LIMIT {
                files.push((e.path.clone(), format!("{base}/blob/{}/{}", rp.enc(), fmt::enc_path(&e.path))));
            } else {
                truncated = true;
            }
        }
    }
    files.sort();
    let picker = BranchPicker::new(&rp, &branches, &tags, &rc.repo.default_branch, &format!("{base}/find"), "", &base);
    let c = rc.chrome(&ctx, "Go to file", "code").await;
    ctx.page(&FindPage { c, picker, files, truncated, back_href: format!("{base}/tree/{}", rp.enc()) })
}

// ------------------------------------------------------------------ blob

#[derive(Template)]
#[template(path = "repo/blob.html")]
struct BlobPage {
    c: Chrome,
    picker: BranchPicker,
    crumbs: Vec<Crumb>,
    root_href: String,
    file_name: String,
    size: String,
    line_count: usize,
    language: String,
    kind: &'static str,
    code_html: String,
    markdown_html: String,
    raw_href: String,
    blame_href: String,
    history_href: String,
    edit_href: String,
    permalink: String,
    source_href: String,
    rendered_href: String,
    showing_source: bool,
    can_edit: bool,
    truncated: bool,
    binary_note: String,
    sha_short: String,
    last: Option<LastCommit>,
}

pub async fn blob(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let (rp, branches, tags) = resolve_ref(&ctx, &rc, &rest).await?;
    if rp.path.is_empty() {
        return Ok(ctx.redirect(&format!("{}/tree/{}", rc.base(), rp.enc())));
    }
    let b = ctx.state.git.blob(rc.id(), &rp.name, &rp.path, BLOB_VIEW_LIMIT).await?;
    let base = rc.base();
    let file_name = rp.path.rsplit('/').next().unwrap_or(&rp.path).to_string();
    let source = q.get("display").map(String::as_str) == Some("source");
    let sha = ctx.state.git.resolve(rc.id(), &rp.name).await.unwrap_or_else(|_| rp.name.clone());
    let enc_path = fmt::enc_path(&rp.path);

    let (mut kind, mut code_html, mut markdown_html, mut line_count, mut language, mut binary_note) =
        ("text", String::new(), String::new(), 0usize, String::new(), String::new());
    if b.is_binary {
        if is_image(&file_name) {
            kind = "image";
        } else {
            kind = "binary";
            binary_note = format!("This file is binary ({}). It cannot be shown here.", ch_render::human_bytes(b.size));
        }
    } else {
        let text = String::from_utf8_lossy(&b.content).to_string();
        let h = ch_render::highlight::highlight(&text, &rp.path);
        language = h.language.clone().unwrap_or_default();
        line_count = h.lines.len();
        if is_markdown(&file_name) && !source {
            kind = "markdown";
            markdown_html = markdown::render(&text, &md_ctx(&rc, &rp, rp.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")));
        } else {
            code_html = code::code_table(&h.lines);
        }
    }
    let can_edit = rc.perm.allows(Permission::Write) && !rc.repo.archived && rp.kind == RefKind::Branch && !b.is_binary;
    let last = match ctx.state.git.log(rc.id(), &rp.name, Some(rp.path.as_str()), 0, 1).await {
        Ok(v) => match v.first() {
            Some(c) => commit_rows(&ctx, &rc, std::slice::from_ref(c), false)
                .await
                .pop()
                .map(|row| LastCommit { row, count_href: format!("{base}/commits/{}/{enc_path}", rp.enc()) }),
            None => None,
        },
        Err(_) => None,
    };
    let picker = BranchPicker::new(&rp, &branches, &tags, &rc.repo.default_branch, &format!("{base}/blob"), &rp.path, &base);
    let mut c = rc.chrome(&ctx, &rp.path, "code").await;
    // The file page knows the file, so its workspace button opens it: on this branch, or on
    // the default branch when the page shows a tag or a commit, which the workspace cannot edit.
    if let Some(nav) = c.repo.as_mut()
        && !nav.workspace.is_empty()
    {
        nav.workspace = if rp.kind == RefKind::Branch {
            format!("{}?branch={}&path={}", nav.workspace, fmt::enc(&rp.name), fmt::enc_path(&rp.path))
        } else {
            format!("{}?path={}", nav.workspace, fmt::enc_path(&rp.path))
        };
    }
    ctx.page(&BlobPage {
        c,
        picker,
        crumbs: crumbs(&rc, &rp, "tree", &rp.path),
        root_href: format!("{base}/tree/{}", rp.enc()),
        file_name,
        size: ch_render::human_bytes(b.size),
        line_count,
        language,
        kind,
        code_html,
        markdown_html,
        raw_href: format!("{base}/raw/{}/{enc_path}", rp.enc()),
        blame_href: format!("{base}/blame/{}/{enc_path}", rp.enc()),
        history_href: format!("{base}/commits/{}/{enc_path}", rp.enc()),
        edit_href: format!("{base}/edit/{}/{enc_path}", rp.enc()),
        permalink: format!("{base}/blob/{sha}/{enc_path}"),
        source_href: format!("{base}/blob/{}/{enc_path}?display=source", rp.enc()),
        rendered_href: format!("{base}/blob/{}/{enc_path}", rp.enc()),
        showing_source: source,
        can_edit,
        truncated: b.truncated,
        binary_note,
        sha_short: fmt::short_sha(&sha),
        last,
    })
}

// ------------------------------------------------------------------ raw and archive

fn inline_type(name: &str) -> Option<&'static str> {
    let l = name.to_lowercase();
    let ext = l.rsplit('.').next().unwrap_or("");
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Raw files can never run script on our origin: a `sandbox` CSP, `nosniff`, and an
/// attachment disposition for anything that is not a plain image or plain text.
pub fn raw_response(file_name: &str, content: Vec<u8>, binary: bool) -> Response {
    let (ctype, disposition) = match inline_type(file_name) {
        Some(t) => (t.to_string(), "inline".to_string()),
        None if !binary
            && !matches!(
                file_name.rsplit('.').next().map(str::to_lowercase).as_deref(),
                Some("html" | "htm" | "svg" | "xml" | "xhtml")
            ) =>
        {
            ("text/plain; charset=utf-8".to_string(), "inline".to_string())
        }
        None => ("application/octet-stream".to_string(), ch_core::disposition::attachment(file_name)),
    };
    let mut r = (StatusCode::OK, content).into_response();
    let h = r.headers_mut();
    for (k, v) in [
        (header::CONTENT_TYPE, ctype),
        (header::CONTENT_DISPOSITION, disposition),
        (header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:".to_string()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        (header::CACHE_CONTROL, "private, max-age=60".to_string()),
    ] {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    }
    r
}

pub async fn raw(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let (rp, _, _) = resolve_ref(&ctx, &rc, &rest).await?;
    if rp.path.is_empty() {
        return Err(WebError::not_found());
    }
    let b = ctx.state.git.blob(rc.id(), &rp.name, &rp.path, RAW_LIMIT).await?;
    if b.truncated {
        return Err(WebError::Core(Error::invalid(
            "blob",
            "size",
            "This file is too large to download here. Clone the repository instead.",
        )));
    }
    let file_name = rp.path.rsplit('/').next().unwrap_or(&rp.path);
    Ok(raw_response(file_name, b.content, b.is_binary))
}

pub async fn archive(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let (stem, format, ext) = if let Some(s) = rest.strip_suffix(".tar.gz") {
        (s, ArchiveFormat::TarGz, "tar.gz")
    } else if let Some(s) = rest.strip_suffix(".zip") {
        (s, ArchiveFormat::Zip, "zip")
    } else {
        return Err(WebError::not_found());
    };
    let sha = ctx.state.git.resolve(rc.id(), stem).await?;
    let stream = ctx.state.git.archive(rc.id(), &sha, format).await?;
    let mut r = Response::new(Body::from_stream(stream));
    let ctype = if ext == "zip" { "application/zip" } else { "application/gzip" };
    let fname = format!("{}-{}.{ext}", rc.repo.name, stem.replace('/', "-"));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    if let Ok(v) = HeaderValue::from_str(&ch_core::disposition::attachment(&fname)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(r)
}

// ------------------------------------------------------------------ blame

#[derive(Template)]
#[template(path = "repo/blame.html")]
struct BlamePage {
    c: Chrome,
    crumbs: Vec<Crumb>,
    /// The branch, tag or short commit the crumbs start from, as on the file page.
    ref_name: String,
    root_href: String,
    file_name: String,
    table: String,
    blob_href: String,
    history_href: String,
    raw_href: String,
}

pub async fn blame(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let (rp, _, _) = resolve_ref(&ctx, &rc, &rest).await?;
    if rp.path.is_empty() {
        return Err(WebError::not_found());
    }
    let b = ctx.state.git.blob(rc.id(), &rp.name, &rp.path, BLOB_VIEW_LIMIT).await?;
    if b.is_binary {
        return Err(WebError::Core(Error::invalid("blob", "binary", "Blame is only available for text files.")));
    }
    let blame = ctx.state.git.blame(rc.id(), &rp.name, &rp.path).await?;
    let hunks: Vec<BlameHunk> = blame
        .ranges
        .iter()
        .map(|r| BlameHunk {
            sha: r.sha.clone(),
            author: r.author.name.clone(),
            at: r.author.at,
            summary: r.summary.clone(),
            start_line: r.start_line,
            line_count: r.line_count,
        })
        .collect();
    let text = String::from_utf8_lossy(&b.content).to_string();
    let h = ch_render::highlight::highlight(&text, &rp.path);
    let mut rows: Vec<BlameRow> = Vec::with_capacity(h.lines.len());
    for hk in &hunks {
        for i in 0..hk.line_count {
            rows.push(BlameRow {
                start: i == 0,
                sha: hk.sha.clone(),
                commit_href: format!("{}/commit/{}", rc.base(), hk.sha),
                author: hk.author.clone(),
                ago: fmt::time(hk.at).ago,
                summary: hk.summary.clone(),
            });
        }
    }
    let base = rc.base();
    let enc_path = fmt::enc_path(&rp.path);
    let c = rc.chrome(&ctx, &format!("Blame {}", rp.path), "code").await;
    ctx.page(&BlamePage {
        c,
        ref_name: if rp.name.len() == 40 && rp.name.chars().all(|ch| ch.is_ascii_hexdigit()) {
            fmt::short_sha(&rp.name)
        } else {
            rp.name.clone()
        },
        crumbs: crumbs(&rc, &rp, "tree", &rp.path),
        root_href: format!("{base}/tree/{}", rp.enc()),
        file_name: rp.path.rsplit('/').next().unwrap_or("").to_string(),
        table: code::blame_table(&h.lines, &rows),
        blob_href: format!("{base}/blob/{}/{enc_path}", rp.enc()),
        history_href: format!("{base}/commits/{}/{enc_path}", rp.enc()),
        raw_href: format!("{base}/raw/{}/{enc_path}", rp.enc()),
    })
}

// ------------------------------------------------------------------ commits

#[derive(Template)]
#[template(path = "repo/commits.html")]
struct CommitsPage {
    c: Chrome,
    picker: BranchPicker,
    rows: Vec<CommitRow>,
    pager: super::Pager,
    path: String,
}

pub async fn commits(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    commits_inner(ctx, rc, &rest, q).await
}

pub async fn commits_default(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    commits_inner(ctx, rc, "", q).await
}

async fn commits_inner(ctx: Ctx, rc: RepoCtx, rest: &str, q: Params) -> WebResult<Response> {
    let (rp, branches, tags) = resolve_ref(&ctx, &rc, rest).await?;
    let page: u32 = q.get("page").and_then(|p| p.parse().ok()).filter(|p| *p >= 1).unwrap_or(1);
    let path = if rp.path.is_empty() { None } else { Some(rp.path.as_str()) };
    let mut list =
        if rc.repo.is_empty { Vec::new() } else { ctx.state.git.log(rc.id(), &rp.name, path, (page - 1) * PAGE, PAGE + 1).await? };
    let more = list.len() as u32 > PAGE;
    list.truncate(PAGE as usize);
    let base = rc.base();
    let here =
        format!("{base}/commits/{}{}", rp.enc(), if rp.path.is_empty() { String::new() } else { format!("/{}", fmt::enc_path(&rp.path)) });
    let rows = commit_rows(&ctx, &rc, &list, true).await;
    let picker = BranchPicker::new(&rp, &branches, &tags, &rc.repo.default_branch, &format!("{base}/commits"), &rp.path, &base);
    let c = rc.chrome(&ctx, "Commits", "code").await;
    ctx.page(&CommitsPage { c, picker, rows, pager: super::Pager::new(&here, page, more), path: rp.path.clone() })
}

// ------------------------------------------------------------------ one commit

pub struct CheckLine {
    pub name: String,
    pub status: StatusView,
    pub href: String,
}

#[derive(Template)]
#[template(path = "repo/commit.html")]
struct CommitPage {
    c: Chrome,
    title: String,
    body: String,
    author: String,
    committer: Option<String>,
    when: TimeView,
    sha: String,
    short: String,
    parents: Vec<(String, String)>,
    tree_href: String,
    files: usize,
    additions: u32,
    deletions: u32,
    diff_html: String,
    mode_unified: String,
    mode_split: String,
    split: bool,
    checks: Vec<CheckLine>,
}

pub async fn commit(ctx: Ctx, Path((owner, name, sha)): Path<(String, String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let info = ctx.state.git.commit(rc.id(), &sha).await?;
    let parent = info.parents.first().cloned().unwrap_or_else(|| EMPTY_TREE.to_string());
    let diff = ctx.state.git.diff(rc.id(), &parent, &info.sha).await?;
    let mode = DiffMode::from_query(q.get("diff").map(String::as_str));
    let base = rc.base();
    let sum = code::summarize(&diff);
    let opts = DiffOptions { file_url: Some(format!("{base}/blob/{}/{{path}}", info.sha)), ..Default::default() };
    // CI records keep the audience they were made for, and signed-out Actions is not open yet.
    let anon = ctx.actor.is_anonymous();
    let runs = if anon { Vec::new() } else { ctx.state.checks.check_runs_for_sha(rc.id(), &info.sha).await.unwrap_or_default() };
    let statuses = if anon { Vec::new() } else { ctx.state.checks.statuses_for_sha(rc.id(), &info.sha).await.unwrap_or_default() };
    let mut checks: Vec<CheckLine> = runs
        .iter()
        .map(|r| CheckLine {
            name: r.name.clone(),
            status: fmt::status(&r.status, r.conclusion.as_deref()),
            href: r.details_url.clone().unwrap_or_default(),
        })
        .collect();
    checks.extend(statuses.iter().map(|s| CheckLine {
        name: s.context.clone(),
        status: fmt::status(&s.state, Some(&s.state)),
        href: s.target_url.clone().unwrap_or_default(),
    }));
    let same = info.author.name == info.committer.name && info.author.email == info.committer.email;
    let c = rc.chrome(&ctx, &fmt::title_of(&info.message), "code").await;
    ctx.page(&CommitPage {
        c,
        title: fmt::title_of(&info.message),
        body: fmt::body_of(&info.message),
        author: info.author.name.clone(),
        committer: if same { None } else { Some(info.committer.name.clone()) },
        when: fmt::time(info.author.at),
        short: fmt::short_sha(&info.sha),
        tree_href: format!("{base}/tree/{}", info.sha),
        parents: info.parents.iter().map(|p| (fmt::short_sha(p), format!("{base}/commit/{p}"))).collect(),
        files: sum.files,
        additions: sum.additions,
        deletions: sum.deletions,
        diff_html: code::diff_html(&diff, mode, &opts),
        mode_unified: format!("{base}/commit/{}", info.sha),
        mode_split: format!("{base}/commit/{}?diff=split", info.sha),
        split: mode == DiffMode::Split,
        sha: info.sha,
        checks,
    })
}

/// git's well known empty tree, the base for diffing a root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// ------------------------------------------------------------------ branches and tags

pub struct RefRow {
    pub name: String,
    pub sha_short: String,
    pub commit_href: String,
    pub tree_href: String,
    pub compare_href: String,
    pub is_default: bool,
    pub zip: String,
    pub release_href: String,
    pub delete_action: String,
}

#[derive(Template)]
#[template(path = "repo/refs.html")]
struct RefsPage {
    c: Chrome,
    kind: &'static str,
    rows: Vec<RefRow>,
    can_delete: bool,
    branches_href: String,
    tags_href: String,
}

async fn refs_page(ctx: Ctx, rc: RepoCtx, tags: bool) -> WebResult<Response> {
    let prefix = if tags { "refs/tags/" } else { "refs/heads/" };
    let mut refs = ctx.state.git.refs(rc.id(), Some(prefix)).await?;
    refs.sort_by_key(|r| (r.name.strip_prefix(prefix).unwrap_or(&r.name) != rc.repo.default_branch, std::cmp::Reverse(r.name.clone())));
    if !tags {
        refs.sort_by_key(|r| (r.name.strip_prefix(prefix).unwrap_or(&r.name) != rc.repo.default_branch, r.name.to_lowercase()));
    }
    let base = rc.base();
    let rows = refs
        .iter()
        .map(|r| {
            let n = r.name.strip_prefix(prefix).unwrap_or(&r.name).to_string();
            RefRow {
                sha_short: fmt::short_sha(&r.sha),
                commit_href: format!("{base}/commit/{}", r.sha),
                tree_href: format!("{base}/tree/{}", fmt::enc_path(&n)),
                compare_href: format!("{base}/compare/{}...{}", fmt::enc_path(&rc.repo.default_branch), fmt::enc_path(&n)),
                is_default: !tags && n == rc.repo.default_branch,
                zip: format!("{base}/archive/{}.zip", fmt::enc_path(&n)),
                release_href: format!("{base}/releases/tag/{}", fmt::enc(&n)),
                delete_action: format!("{base}/{}/delete", if tags { "tags" } else { "branches" }),
                name: n,
            }
        })
        .collect();
    let c = rc.chrome(&ctx, if tags { "Tags" } else { "Branches" }, "code").await;
    ctx.page(&RefsPage {
        c,
        kind: if tags { "tags" } else { "branches" },
        rows,
        can_delete: rc.perm.allows(Permission::Write) && !rc.repo.archived,
        branches_href: format!("{base}/branches"),
        tags_href: format!("{base}/tags"),
    })
}

pub async fn branches(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    refs_page(ctx, rc, false).await
}

pub async fn tags(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    refs_page(ctx, rc, true).await
}

async fn delete_ref(ctx: Ctx, rc: RepoCtx, form: FormData, tags: bool) -> WebResult<Response> {
    rc.require_writable(Permission::Write)?;
    let name = form.get("name").trim();
    if name.is_empty() || (!tags && name == rc.repo.default_branch) {
        return Err(WebError::bad("Pick a branch or tag other than the default branch."));
    }
    let full = format!("{}{name}", if tags { "refs/tags/" } else { "refs/heads/" });
    ctx.state.git.delete_ref(&ctx.actor, rc.id(), &full, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/{}", rc.base(), if tags { "tags" } else { "branches" }), "ok", &format!("Deleted {name}.")))
}

pub async fn branch_delete(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    delete_ref(ctx, rc, form, false).await
}

pub async fn tag_delete(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    delete_ref(ctx, rc, form, true).await
}

// ------------------------------------------------------------------ compare

#[derive(Template)]
#[template(path = "repo/compare.html")]
struct ComparePage {
    c: Chrome,
    base_ref: String,
    head_ref: String,
    branches: Vec<String>,
    /// Tags, so a release can be either side (ch-9vo).
    tags: Vec<String>,
    has_result: bool,
    same: bool,
    /// Both sides are the very same commit.
    same_commit: bool,
    /// Why there is no comparison to show: no shared history, or a name that is not here.
    problem: String,
    ahead: u32,
    behind: u32,
    rows: Vec<CommitRow>,
    files: usize,
    additions: u32,
    deletions: u32,
    diff_html: String,
    new_pull: String,
    action: String,
    split: bool,
    mode_unified: String,
    mode_split: String,
}

/// `/compare` shows the picker. With `?base=a&head=b` (the picker's own form) it jumps to the
/// pretty `/compare/a...b` address.
pub async fn compare_form(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    if let (Some(b), Some(h)) = (q.get("base"), q.get("head")) {
        return Ok(ctx.redirect(&format!("{}/compare/{}...{}", rc.base(), fmt::enc_path(b), fmt::enc_path(h))));
    }
    compare_inner(ctx, rc, "", q).await
}

pub async fn compare(ctx: Ctx, Path((owner, name, spec)): Path<(String, String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    compare_inner(ctx, rc, &spec, q).await
}

async fn compare_inner(ctx: Ctx, rc: RepoCtx, spec: &str, q: Params) -> WebResult<Response> {
    let base = rc.base();
    let branches = branch_names(&ctx, rc.id()).await?;
    let (b, h) = match spec.split_once("...") {
        Some((b, h)) => (b.to_string(), h.to_string()),
        None => (rc.repo.default_branch.clone(), q.get("head").cloned().unwrap_or_else(|| rc.repo.default_branch.clone())),
    };
    let tags = super::repo_common::tag_names(&ctx, rc.id()).await.unwrap_or_default();
    // A comparison that cannot be made says why on the page, not as a bare error (ch-9vo).
    let mut problem = String::new();
    let result = if spec.contains("...") {
        match ctx.state.git.compare(rc.id(), &b, &h).await {
            Ok(c) => Some(c),
            Err(Error::NotFound) => {
                problem = "One of these is not a branch, tag or commit in this repository.".into();
                None
            }
            Err(Error::Validation(_)) | Err(Error::Conflict(_)) => {
                problem = "These two have no history in common, so there is nothing to compare.".into();
                None
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    let mode = DiffMode::from_query(q.get("diff").map(String::as_str));
    let c = rc.chrome(&ctx, "Compare", "code").await;
    let here = format!("{base}/compare/{}...{}", fmt::enc_path(&b), fmt::enc_path(&h));
    let page = match result {
        Some(cmp) => {
            let sum = code::summarize(&cmp.diff);
            let opts = DiffOptions { file_url: Some(format!("{base}/blob/{}/{{path}}", cmp.head_sha)), ..Default::default() };
            let rows = commit_rows(&ctx, &rc, &cmp.commits, false).await;
            ComparePage {
                c,
                has_result: true,
                same: cmp.head_sha == cmp.base_sha || (cmp.ahead_by == 0 && cmp.diff.files.is_empty()),
                same_commit: cmp.head_sha == cmp.base_sha,
                problem: String::new(),
                tags: tags.clone(),
                ahead: cmp.ahead_by,
                behind: cmp.behind_by,
                rows,
                files: sum.files,
                additions: sum.additions,
                deletions: sum.deletions,
                diff_html: code::diff_html(&cmp.diff, mode, &opts),
                new_pull: format!("{base}/pulls/new?base={}&head={}", fmt::enc(&b), fmt::enc(&h)),
                mode_unified: here.clone(),
                mode_split: format!("{here}?diff=split"),
                split: mode == DiffMode::Split,
                base_ref: b,
                head_ref: h,
                branches,
                action: format!("{base}/compare"),
            }
        }
        None => ComparePage {
            c,
            has_result: false,
            same: false,
            same_commit: false,
            problem,
            tags,
            ahead: 0,
            behind: 0,
            rows: Vec::new(),
            files: 0,
            additions: 0,
            deletions: 0,
            diff_html: String::new(),
            new_pull: String::new(),
            mode_unified: String::new(),
            mode_split: String::new(),
            split: false,
            base_ref: b,
            head_ref: h,
            branches,
            action: format!("{base}/compare"),
        },
    };
    ctx.page(&page)
}

// ------------------------------------------------------------------ editing files

#[derive(Template)]
#[template(path = "repo/edit.html")]
struct EditPage {
    c: Chrome,
    mode: &'static str,
    heading: String,
    action: String,
    branch: String,
    path: String,
    content: String,
    message: String,
    new_branch: String,
    direct: bool,
    expected_sha: String,
    error: String,
    cancel_href: String,
    protected_note: bool,
}

struct EditTarget {
    rc: RepoCtx,
    rp: RefPath,
}

async fn edit_target(ctx: &Ctx, owner: &str, name: &str, rest: &str) -> WebResult<EditTarget> {
    let rc = RepoCtx::load(ctx, owner, name).await?;
    rc.require_writable(Permission::Write)?;
    let (rp, _, _) = resolve_ref(ctx, &rc, rest).await?;
    if rp.kind != RefKind::Branch {
        return Err(WebError::bad("Files can only be changed on a branch."));
    }
    Ok(EditTarget { rc, rp })
}

#[allow(clippy::too_many_arguments)]
fn blank_edit(
    c: Chrome,
    t: &EditTarget,
    mode: &'static str,
    heading: String,
    action: String,
    path: String,
    _locked: bool,
    content: String,
    cancel: String,
) -> EditPage {
    let msg = match mode {
        "new" => "Add file".to_string(),
        "delete" => format!("Delete {path}"),
        _ => format!("Update {}", path.rsplit('/').next().unwrap_or(&path)),
    };
    EditPage {
        c,
        mode,
        heading,
        action,
        branch: t.rp.name.clone(),
        path,
        content,
        message: msg,
        new_branch: String::new(),
        direct: true,
        expected_sha: String::new(),
        error: String::new(),
        cancel_href: cancel,
        protected_note: t.rp.name == t.rc.repo.default_branch,
    }
}

pub async fn edit_get(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let t = edit_target(&ctx, &owner, &name, &rest).await?;
    if t.rp.path.is_empty() {
        return Err(WebError::not_found());
    }
    let b = ctx.state.git.blob(t.rc.id(), &t.rp.name, &t.rp.path, BLOB_VIEW_LIMIT).await?;
    if b.is_binary || b.truncated {
        return Err(WebError::bad("This file cannot be edited here. Clone the repository to change it."));
    }
    let base = t.rc.base();
    let enc_path = fmt::enc_path(&t.rp.path);
    let c = t.rc.chrome(&ctx, &format!("Edit {}", t.rp.path), "code").await;
    let mut p = blank_edit(
        c,
        &t,
        "edit",
        format!("Edit {}", t.rp.path),
        format!("{base}/edit/{}/{enc_path}", t.rp.enc()),
        t.rp.path.clone(),
        true,
        String::from_utf8_lossy(&b.content).to_string(),
        format!("{base}/blob/{}/{enc_path}", t.rp.enc()),
    );
    p.expected_sha = ctx.state.git.resolve(t.rc.id(), &t.rp.name).await.unwrap_or_default();
    ctx.page(&p)
}

pub async fn new_get(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let t = edit_target(&ctx, &owner, &name, &rest).await?;
    let base = t.rc.base();
    let dir = if t.rp.path.is_empty() { String::new() } else { format!("{}/", t.rp.path.trim_end_matches('/')) };
    let c = t.rc.chrome(&ctx, "New file", "code").await;
    let mut p = blank_edit(
        c,
        &t,
        "new",
        "Create a new file".into(),
        format!("{base}/new/{}/{}", t.rp.enc(), fmt::enc_path(&t.rp.path)),
        dir,
        false,
        String::new(),
        format!("{base}/tree/{}", t.rp.enc()),
    );
    p.expected_sha = ctx.state.git.resolve(t.rc.id(), &t.rp.name).await.unwrap_or_default();
    ctx.page(&p)
}

pub async fn delete_get(ctx: Ctx, Path((owner, name, rest)): Path<(String, String, String)>) -> WebResult<Response> {
    let t = edit_target(&ctx, &owner, &name, &rest).await?;
    if t.rp.path.is_empty() {
        return Err(WebError::not_found());
    }
    let base = t.rc.base();
    let enc_path = fmt::enc_path(&t.rp.path);
    let c = t.rc.chrome(&ctx, "Delete file", "code").await;
    let mut p = blank_edit(
        c,
        &t,
        "delete",
        format!("Delete {}", t.rp.path),
        format!("{base}/delete/{}/{enc_path}", t.rp.enc()),
        t.rp.path.clone(),
        true,
        String::new(),
        format!("{base}/blob/{}/{enc_path}", t.rp.enc()),
    );
    p.expected_sha = ctx.state.git.resolve(t.rc.id(), &t.rp.name).await.unwrap_or_default();
    ctx.page(&p)
}

async fn commit_change(ctx: Ctx, owner: String, name: String, rest: String, form: FormData, mode: &'static str) -> WebResult<Response> {
    let t = edit_target(&ctx, &owner, &name, &rest).await?;
    let base = t.rc.base();
    let path = match mode {
        "new" => form.get("path").trim().trim_start_matches('/').to_string(),
        _ => t.rp.path.clone(),
    };
    let content = form.get("content").replace("\r\n", "\n");
    let message =
        form.opt("message").map(str::to_string).unwrap_or_else(|| if mode == "new" { "Add file".into() } else { format!("Update {path}") });
    let direct = form.get("target") != "branch";
    let new_branch = if direct { None } else { form.opt("new_branch").map(str::to_string) };
    let title = match mode {
        "new" => "Create a new file".to_string(),
        "delete" => format!("Delete {path}"),
        _ => format!("Edit {path}"),
    };
    let cancel =
        if mode == "new" { format!("{base}/tree/{}", t.rp.enc()) } else { format!("{base}/blob/{}/{}", t.rp.enc(), fmt::enc_path(&path)) };

    let message_form = message.clone();
    let again = |ctx: &Ctx, error: String, c: Chrome| {
        let mut p = blank_edit(
            c,
            &t,
            mode,
            title.clone(),
            format!("{base}/{mode}/{}/{}", t.rp.enc(), fmt::enc_path(&t.rp.path)),
            path.clone(),
            mode != "new",
            content.clone(),
            cancel.clone(),
        );
        p.message = message_form.clone();
        p.new_branch = form.get("new_branch").to_string();
        p.direct = direct;
        p.expected_sha = form.get("expected_sha").to_string();
        p.error = error;
        ctx.page_status(StatusCode::UNPROCESSABLE_ENTITY, &p)
    };

    if path.is_empty() || path.ends_with('/') || path.split('/').any(|s| s == ".." || s == "." || s.is_empty()) {
        let c = t.rc.chrome(&ctx, &title, "code").await;
        return again(&ctx, "Give the file a name, such as docs/notes.md.".into(), c);
    }
    if !direct && new_branch.is_none() {
        let c = t.rc.chrome(&ctx, &title, "code").await;
        return again(&ctx, "Name the new branch.".into(), c);
    }
    let user = ctx.state.accounts.get_user(ctx.user_id()?).await?;
    let change = FileChange {
        path: path.clone(),
        content: if mode == "delete" { None } else { Some(content.clone().into_bytes()) },
        renamed_from: None,
    };
    let req = CommitFileChange {
        repo_id: t.rc.id(),
        branch: t.rp.name.clone(),
        new_branch: new_branch.clone(),
        expected_sha: form.opt("expected_sha").map(str::to_string),
        changes: vec![change],
        message,
        author: signature(&ctx, &user),
    };
    match ctx.state.git.commit_file_change(&ctx.actor, req, &ctx.client).await {
        Ok(sha) => {
            let short = fmt::short_sha(&sha);
            match new_branch {
                Some(nb) => Ok(ctx.redirect_flash(
                    &format!("{base}/compare/{}...{}", fmt::enc_path(&t.rc.repo.default_branch), fmt::enc_path(&nb)),
                    "ok",
                    &format!("Committed {short} to the new branch {nb}. Open a pull request to merge it."),
                )),
                None if mode == "delete" => {
                    Ok(ctx.redirect_flash(&format!("{base}/tree/{}", t.rp.enc()), "ok", &format!("Deleted {path} in {short}.")))
                }
                None => Ok(ctx.redirect_flash(
                    &format!("{base}/blob/{}/{}", t.rp.enc(), fmt::enc_path(&path)),
                    "ok",
                    &format!("Committed {short}."),
                )),
            }
        }
        Err(e) => match e {
            Error::Conflict(m) | Error::Forbidden(m) => {
                let c = t.rc.chrome(&ctx, &title, "code").await;
                again(&ctx, m, c)
            }
            Error::Validation(list) => {
                let m = list
                    .iter()
                    .map(|f| f.message.clone().unwrap_or_else(|| format!("{} is not valid.", f.field)))
                    .collect::<Vec<_>>()
                    .join(" ");
                let c = t.rc.chrome(&ctx, &title, "code").await;
                again(&ctx, m, c)
            }
            other => Err(other.into()),
        },
    }
}

pub async fn edit_post(ctx: Ctx, Path((o, n, r)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    commit_change(ctx, o, n, r, form, "edit").await
}
pub async fn new_post(ctx: Ctx, Path((o, n, r)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    commit_change(ctx, o, n, r, form, "new").await
}
pub async fn delete_post(ctx: Ctx, Path((o, n, r)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    commit_change(ctx, o, n, r, form, "delete").await
}

/// Markdown preview for the editor tabs and comment boxes. Needs a signed in user and the
/// CSRF header like any post.
pub async fn preview(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let md = MdContext {
        owner: form.get("owner").to_string(),
        repo: form.get("repo").to_string(),
        git_ref: String::new(),
        dir: String::new(),
        link_refs: true,
        heading_ids: false,
    };
    let html = if form.get("text").trim().is_empty() {
        "<p class=\"muted\">Nothing to preview.</p>".to_string()
    } else {
        markdown::render(form.get("text"), &md)
    };
    let mut r = (StatusCode::OK, html).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

#[allow(dead_code)]
async fn _people_unused(ctx: &Ctx) {
    let _ = People::new(&ctx.state);
    let _ = tag_names(ctx, ch_core::RepoId(0)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_files_cannot_run_script_on_our_origin() {
        let r = raw_response("page.html", b"<script>alert(1)</script>".to_vec(), false);
        let h = r.headers();
        assert!(h[header::CONTENT_SECURITY_POLICY].to_str().unwrap().starts_with("sandbox"));
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment"));
        assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
        let r = raw_response("logo.svg", b"<svg/>".to_vec(), false);
        assert!(r.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment"));
    }

    #[test]
    fn text_is_plain_and_images_are_inline() {
        let r = raw_response("main.rs", b"fn main() {}".to_vec(), false);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(r.headers()[header::CONTENT_DISPOSITION], "inline");
        let r = raw_response("a.png", vec![0x89, b'P'], true);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/png");
        let r = raw_response("blob.bin", vec![0, 1, 2], true);
        assert!(r.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment"));
    }

    #[test]
    fn file_names_cannot_break_the_header() {
        let r = raw_response("a\"b\r\nX: y.bin", vec![0], true);
        let d = r.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().to_string();
        assert!(!d.contains('\n') && d.matches('"').count() == 2, "{d}");
    }

    #[test]
    fn run_status_rolls_up() {
        let run = |s: &str, c: Option<&str>| CheckRun {
            id: 1,
            repo_id: ch_core::RepoId(1),
            head_sha: "x".into(),
            name: "t".into(),
            status: s.into(),
            conclusion: c.map(str::to_string),
            details_url: None,
        };
        assert!(status_from_runs(&[]).is_none());
        assert_eq!(status_from_runs(&[run("completed", Some("success"))]).unwrap().class, "ok");
        assert_eq!(status_from_runs(&[run("completed", Some("success")), run("completed", Some("failure"))]).unwrap().class, "fail");
        assert_eq!(status_from_runs(&[run("in_progress", None)]).unwrap().class, "run");
    }
}
