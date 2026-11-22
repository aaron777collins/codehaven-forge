//! Releases: the list, one release by tag, the new release form and deleting one.

use super::FieldErrors;
use super::repo_common::{self, RepoCtx};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::body::Body;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use ch_core::collab::{AssetInfo, ReleaseView};
use ch_core::model::Release;
use ch_core::web_contract::NewRelease;
use ch_core::{Permission, ReleaseId};
use ch_render::markdown::{self, MdContext};
use chrono::{DateTime, Utc};

// ------------------------------------------------------------------ view building

/// One release as the list and the single page draw it.
pub struct ReleaseRow {
    pub tag: String,
    pub title: String,
    pub href: String,
    pub prerelease: bool,
    pub draft: bool,
    pub when: TimeView,
    pub body_html: String,
    pub zip_href: String,
    pub tar_href: String,
    pub assets: Vec<AssetRow>,
}

/// An uploaded file on a release.
pub struct AssetRow {
    pub name: String,
    pub size: String,
    pub downloads: String,
    pub href: String,
}

pub fn asset_href(base: &str, tag: &str, name: &str) -> String {
    format!("{base}/releases/download/{}/{}", fmt::enc(tag), fmt::enc(name))
}

fn asset_rows(base: &str, tag: &str, assets: &[AssetInfo]) -> Vec<AssetRow> {
    assets
        .iter()
        .map(|a| AssetRow {
            name: a.name.clone(),
            size: ch_render::human_bytes(a.size.max(0) as u64),
            downloads: fmt::plural(a.download_count, "download", "downloads"),
            href: asset_href(base, tag, &a.name),
        })
        .collect()
}

/// The time a release counts as: when it was published, or when it was created while still a draft.
pub fn sort_key(r: &Release) -> DateTime<Utc> {
    r.published_at.unwrap_or(r.created_at)
}

/// The heading a release shows: its name, or the tag when it has no name.
pub fn display_name(name: &str, tag: &str) -> String {
    let n = name.trim();
    if n.is_empty() { tag.to_string() } else { n.to_string() }
}

/// Newest first. Drafts are dropped unless the caller may write to the repository.
pub fn visible_sorted(releases: Vec<Release>, include_drafts: bool) -> Vec<Release> {
    let mut v: Vec<Release> = releases.into_iter().filter(|r| include_drafts || !r.draft).collect();
    v.sort_by_key(|r| std::cmp::Reverse(sort_key(r)));
    v
}

/// Release notes the way GitHub-like forges show them: references linked, no heading anchors,
/// relative links left alone because a release is not tied to one ref.
pub fn body_html(rc: &RepoCtx, body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    markdown::render(
        body,
        &MdContext {
            owner: rc.repo.owner_name.clone(),
            repo: rc.repo.name.clone(),
            git_ref: String::new(),
            dir: String::new(),
            link_refs: true,
            heading_ids: false,
        },
    )
}

pub fn tag_href(base: &str, tag: &str) -> String {
    format!("{base}/releases/tag/{}", fmt::enc(tag))
}

pub fn archive_href(base: &str, tag: &str, ext: &str) -> String {
    format!("{base}/archive/{}.{}", fmt::enc_path(tag), ext)
}

pub fn row(rc: &RepoCtx, r: &Release, assets: &[AssetInfo]) -> ReleaseRow {
    let base = rc.base();
    ReleaseRow {
        tag: r.tag_name.clone(),
        title: display_name(&r.name, &r.tag_name),
        href: tag_href(&base, &r.tag_name),
        prerelease: r.prerelease,
        draft: r.draft,
        when: fmt::time(sort_key(r)),
        body_html: body_html(rc, &r.body),
        zip_href: archive_href(&base, &r.tag_name, "zip"),
        tar_href: archive_href(&base, &r.tag_name, "tar.gz"),
        assets: asset_rows(&base, &r.tag_name, assets),
    }
}

// ------------------------------------------------------------------ list

#[derive(Template)]
#[template(path = "releases/list.html")]
struct ListPage {
    c: Chrome,
    base: String,
    rows: Vec<ReleaseRow>,
    can_write: bool,
}

pub async fn list(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let can_write = rc.perm.allows(Permission::Write);
    // The views carry each release's uploaded files. The demo has no files and answers with the plain list.
    let views = match ctx.state.releases.views(&ctx.actor, rc.id()).await {
        Ok(v) => v,
        Err(_) => ctx
            .state
            .releases
            .list(&ctx.actor, rc.id())
            .await?
            .into_iter()
            .map(|release| ReleaseView {
                target_commitish: String::new(),
                author: ch_core::collab::UserRef {
                    id: release.author_id,
                    username: String::new(),
                    display_name: String::new(),
                    kind: ch_core::model::UserKind::User,
                },
                assets: Vec::new(),
                release,
            })
            .collect(),
    };
    let by_id: std::collections::HashMap<i64, Vec<AssetInfo>> = views.iter().map(|v| (v.release.id.0, v.assets.clone())).collect();
    let releases: Vec<Release> = views.into_iter().map(|v| v.release).collect();
    let rows: Vec<ReleaseRow> =
        visible_sorted(releases, can_write).iter().map(|r| row(&rc, r, by_id.get(&r.id.0).map(Vec::as_slice).unwrap_or(&[]))).collect();
    let c = rc.chrome(&ctx, "Releases", "releases").await;
    ctx.page(&ListPage { c, base: rc.base(), rows, can_write })
}

// ------------------------------------------------------------------ one release

#[derive(Template)]
#[template(path = "releases/show.html")]
struct ShowPage {
    c: Chrome,
    row: ReleaseRow,
    can_write: bool,
    delete_action: String,
}

pub async fn show(ctx: Ctx, Path((owner, name, tag)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let can_write = rc.perm.allows(Permission::Write);
    let (release, assets) = match ctx.state.releases.view_by_tag(&ctx.actor, rc.id(), &tag).await {
        Ok(v) => (v.release, v.assets),
        Err(_) => (ctx.state.releases.get_by_tag(&ctx.actor, rc.id(), &tag).await?, Vec::new()),
    };
    if release.draft && !can_write {
        return Err(WebError::not_found());
    }
    let r = row(&rc, &release, &assets);
    let c = rc.chrome(&ctx, &r.title, "releases").await;
    ctx.page(&ShowPage { c, row: r, can_write, delete_action: format!("{}/releases/{}/delete", rc.base(), release.id.0) })
}

// ------------------------------------------------------------------ a download

/// `/{owner}/{repo}/releases/download/{tag}/{name}`: the link the API gives for an uploaded
/// file. It needs the same read access as the release, counts the download, and is always sent
/// as an attachment so a browser never renders an uploaded page.
pub async fn download(ctx: Ctx, Path((owner, name, tag, file)): Path<(String, String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let (info, stream) = ctx.state.releases.open_asset(&ctx.actor, rc.id(), &tag, &file).await?;
    let safe: String = info.name.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').collect();
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&info.size.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(r)
}

// ------------------------------------------------------------------ new release

/// What the new release form collected, kept so a failed post can be shown again.
pub struct NewForm {
    pub tag: String,
    pub target: String,
    pub title: String,
    pub notes: String,
    pub prerelease: bool,
    pub draft: bool,
}

pub struct BranchItem {
    pub name: String,
    pub on: bool,
}

#[derive(Template)]
#[template(path = "releases/new.html")]
struct NewPage {
    c: Chrome,
    owner: String,
    repo: String,
    action: String,
    cancel_href: String,
    input: NewForm,
    branches: Vec<BranchItem>,
    errors: FieldErrors,
}

async fn new_page(ctx: &Ctx, rc: &RepoCtx, input: &NewForm, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let mut branches = repo_common::branch_names(ctx, rc.id()).await.unwrap_or_default();
    if branches.is_empty() {
        branches.push(rc.repo.default_branch.clone());
    }
    branches.sort_by_key(|b| (b.as_str() != rc.repo.default_branch, b.to_lowercase()));
    let picked = if input.target.is_empty() { rc.repo.default_branch.clone() } else { input.target.clone() };
    let items = branches.into_iter().map(|b| BranchItem { on: b == picked, name: b }).collect();
    let c = rc.chrome(ctx, "New release", "releases").await;
    ctx.page_status(
        status,
        &NewPage {
            c,
            owner: rc.repo.owner_name.clone(),
            repo: rc.repo.name.clone(),
            action: format!("{}/releases/new", rc.base()),
            cancel_href: format!("{}/releases", rc.base()),
            input: NewForm {
                tag: input.tag.clone(),
                target: picked,
                title: input.title.clone(),
                notes: input.notes.clone(),
                prerelease: input.prerelease,
                draft: input.draft,
            },
            branches: items,
            errors,
        },
    )
}

pub async fn new_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let input =
        NewForm { tag: String::new(), target: String::new(), title: String::new(), notes: String::new(), prerelease: false, draft: false };
    new_page(&ctx, &rc, &input, FieldErrors::default(), StatusCode::OK).await
}

pub async fn new_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let input = NewForm {
        tag: form.get("tag").trim().to_string(),
        target: form.get("target").trim().to_string(),
        title: form.get("title").trim().to_string(),
        notes: form.get("notes").to_string(),
        prerelease: form.flag("prerelease"),
        draft: form.flag("draft"),
    };
    if !valid_tag(&input.tag) {
        let msg = if input.tag.is_empty() {
            "Give the release a tag name, like v1.2.3."
        } else {
            "That tag name is not valid. Use something like v1.2.3."
        };
        return new_page(&ctx, &rc, &input, FieldErrors::default().with("tag", msg), StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let new = NewRelease {
        repo_id: rc.id(),
        tag_name: input.tag.clone(),
        target: if input.target.is_empty() { rc.repo.default_branch.clone() } else { input.target.clone() },
        name: display_name(&input.title, &input.tag),
        body: input.notes.clone(),
        draft: input.draft,
        prerelease: input.prerelease,
    };
    match ctx.state.tracker.create_release(&ctx.actor, new, &ctx.client).await {
        Ok(release) => Ok(ctx.redirect(&tag_href(&rc.base(), &release.tag_name))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => new_page(&ctx, &rc, &input, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

/// The plain checks before the service sees the tag. Git has more rules; the tracker enforces those.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 200
        && !tag.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
        && !tag.starts_with('.')
        && !tag.starts_with('-')
        && !tag.ends_with('.')
        && !tag.ends_with(".lock")
        && !tag.contains("..")
}

// ------------------------------------------------------------------ delete

pub async fn delete(ctx: Ctx, Path((owner, name, id)): Path<(String, String, i64)>, _form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    ctx.state.tracker.delete_release(&ctx.actor, rc.id(), ReleaseId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/releases", rc.base()), "ok", "Deleted the release."))
}

// ROUTES:
// GET /{owner}/{repo}/releases -> list
// GET /{owner}/{repo}/releases/tag/{tag} -> show
// GET+POST /{owner}/{repo}/releases/new -> new_get, new_post
// POST /{owner}/{repo}/releases/{id}/delete -> delete

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::{RepoId, UserId};
    use chrono::TimeZone;

    fn release(tag: &str, name: &str, draft: bool, published: Option<DateTime<Utc>>, created: DateTime<Utc>) -> Release {
        Release {
            id: ReleaseId(1),
            repo_id: RepoId(1),
            tag_name: tag.into(),
            name: name.into(),
            body: String::new(),
            draft,
            prerelease: false,
            author_id: UserId(1),
            created_at: created,
            published_at: published,
        }
    }

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn newest_first_and_drafts_only_for_writers() {
        let rs = vec![
            release("v1.0", "", false, Some(at(2026, 1, 10)), at(2026, 1, 9)),
            release("v2.0", "", false, Some(at(2026, 3, 1)), at(2026, 2, 28)),
            release("v3.0", "", true, None, at(2026, 4, 2)),
            release("v0.9", "", false, Some(at(2025, 12, 1)), at(2025, 11, 30)),
        ];
        let tags = |v: &[Release]| v.iter().map(|r| r.tag_name.clone()).collect::<Vec<_>>();
        assert_eq!(tags(&visible_sorted(rs.clone(), true)), ["v3.0", "v2.0", "v1.0", "v0.9"]);
        assert_eq!(tags(&visible_sorted(rs, false)), ["v2.0", "v1.0", "v0.9"]);
    }

    #[test]
    fn an_unpublished_release_sorts_by_when_it_was_created() {
        let r = release("v1.0", "", true, None, at(2026, 5, 5));
        assert_eq!(sort_key(&r), at(2026, 5, 5));
    }

    #[test]
    fn the_title_falls_back_to_the_tag() {
        assert_eq!(display_name("Version 2", "v2.0"), "Version 2");
        assert_eq!(display_name("  ", "v2.0"), "v2.0");
    }

    #[test]
    fn tag_names_are_checked_before_the_service_sees_them() {
        assert!(valid_tag("v1.2.3"));
        assert!(valid_tag("release/2026-04_fix"));
        assert!(!valid_tag(""));
        assert!(!valid_tag("v1 2"));
        assert!(!valid_tag("v^1"));
        assert!(!valid_tag("v:1"));
        assert!(!valid_tag(".hidden"));
        assert!(!valid_tag("-dash"));
        assert!(!valid_tag("v1..2"));
        assert!(!valid_tag("v1.lock"));
        assert!(!valid_tag("v1."));
        assert!(!valid_tag(&"x".repeat(201)));
    }

    #[test]
    fn links_encode_the_tag() {
        assert_eq!(tag_href("/a/b", "v1 2"), "/a/b/releases/tag/v1%202");
        assert_eq!(archive_href("/a/b", "v1/2", "zip"), "/a/b/archive/v1/2.zip");
        assert_eq!(archive_href("/a/b", "v1.2.3", "tar.gz"), "/a/b/archive/v1.2.3.tar.gz");
    }
}
