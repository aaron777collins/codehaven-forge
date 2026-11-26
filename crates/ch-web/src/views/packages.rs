//! Packages: the container images and NuGet packages the person signed in can pull.
//!
//! The registry decides who may read each package and answers `NotFound` for the rest, so these
//! pages never learn what they must not show. Every change (linking, unlinking, deleting) goes
//! through one endpoint whose hidden `do` field says which form was sent.

use super::FieldErrors;
use super::repo_common::RepoCtx;
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::Error;
use ch_core::packages::{PackageDetail, PackageSummary, PackageVersion};

// ------------------------------------------------------------------ view building

/// One package as the lists draw it.
pub struct PackageRow {
    pub name: String,
    pub kind_label: String,
    pub href: String,
    /// The `owner/repo` it follows, or "Not linked" / "Owner only" when there is none.
    pub repo_text: String,
    pub repo_href: Option<String>,
    pub latest: Option<String>,
    pub versions: String,
    pub size: String,
    pub when: TimeView,
}

/// One tag or version as the package page draws it.
pub struct VersionRow {
    pub label: String,
    pub digest: String,
    pub platforms: String,
    pub size: String,
    pub when: TimeView,
}

/// The whole package page: the summary plus what its owner may do with it.
pub struct PackageView {
    pub name: String,
    pub owner: String,
    pub kind_label: String,
    pub is_container: bool,
    pub repo_text: String,
    pub repo_href: Option<String>,
    /// The line under the heading about who can use it, when it has no repository.
    pub note: Option<&'static str>,
    pub latest: Option<String>,
    pub when: TimeView,
    pub can_admin: bool,
    pub linkable: Vec<String>,
    pub versions: Vec<VersionRow>,
}

pub fn kind_label(kind: &str) -> String {
    match kind {
        "container" => "Container image".into(),
        "nuget" => "NuGet".into(),
        other => other.into(),
    }
}

pub fn package_href(owner: &str, kind: &str, name: &str) -> String {
    format!("/packages/{}/{}/{}", fmt::enc(owner), fmt::enc(kind), fmt::enc_path(name))
}

/// A link to `owner/repo`, or nothing when the string is not that shape.
fn repo_href(repo: &str) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    Some(format!("/{}/{}", fmt::enc(owner), fmt::enc(name)))
}

/// What the Repository column shows: the link, or why there is none.
fn repo_cell(repo: &Option<String>, orphaned: bool) -> (String, Option<String>) {
    match (repo, orphaned) {
        (Some(r), _) => (r.clone(), repo_href(r)),
        (None, true) => ("Owner only".into(), None),
        (None, false) => ("Not linked".into(), None),
    }
}

/// What the package page says when there is no repository, by cause: never linked, unlinked by its
/// owner, its repository deleted, or its repository moved to another owner (ch-0kp). A package
/// orphaned before the cause was kept gets the wording for any of them.
fn orphan_note(repo: &Option<String>, orphaned: bool, reason: Option<&str>) -> Option<&'static str> {
    if repo.is_some() {
        return None;
    }
    Some(match (orphaned, reason) {
        (false, _) => "Not linked to a repository, so only its owner can use it.",
        (true, Some("unlinked")) => "It was unlinked from its repository, so only its owner can use it.",
        (true, Some("repo_deleted")) => "Its repository was deleted, so only its owner can use it.",
        (true, Some("repo_moved")) => "Its repository moved to another owner, so only its owner can use it.",
        (true, _) => "Its repository was deleted, moved or unlinked, so only its owner can use it.",
    })
}

fn row(s: PackageSummary) -> PackageRow {
    let (repo_text, repo_href) = repo_cell(&s.repo, s.orphaned);
    PackageRow {
        href: package_href(&s.owner, &s.kind, &s.name),
        kind_label: kind_label(&s.kind),
        repo_text,
        repo_href,
        latest: s.latest,
        versions: fmt::plural(s.versions, "version", "versions"),
        size: ch_render::human_bytes(s.size.max(0) as u64),
        when: fmt::time(s.updated_at),
        name: s.name,
    }
}

fn version_row(v: PackageVersion) -> VersionRow {
    VersionRow {
        label: v.label,
        digest: v.digest.chars().take(19).collect(),
        platforms: v.platforms.join(", "),
        size: ch_render::human_bytes(v.size.max(0) as u64),
        when: fmt::time(v.pushed_at),
    }
}

fn view(d: PackageDetail) -> PackageView {
    let (repo_text, repo_href) = repo_cell(&d.summary.repo, d.summary.orphaned);
    PackageView {
        is_container: d.summary.kind == "container",
        kind_label: kind_label(&d.summary.kind),
        repo_text,
        repo_href,
        latest: d.summary.latest,
        when: fmt::time(d.summary.updated_at),
        versions: d.versions.into_iter().map(version_row).collect(),
        name: d.summary.name,
        owner: d.summary.owner,
        note: orphan_note(&d.summary.repo, d.summary.orphaned, d.summary.orphan_reason.as_deref()),
        can_admin: d.can_admin,
        linkable: d.linkable,
    }
}

// ------------------------------------------------------------------ the lists

#[derive(Template)]
#[template(path = "packages/list.html")]
struct ListPage {
    c: Chrome,
    heading: String,
    rows: Vec<PackageRow>,
}

pub async fn index(ctx: Ctx) -> WebResult<Response> {
    ctx.user_id()?;
    let svc = ctx.state.ext.get::<ch_core::packages::PackagesHandle>().ok_or_else(WebError::not_found)?;
    let rows: Vec<PackageRow> = svc.0.list(&ctx.actor, None, None).await?.into_iter().map(row).collect();
    let c = ctx.chrome("Packages").await;
    ctx.page(&ListPage { c, heading: "Packages".into(), rows })
}

pub async fn owner(ctx: Ctx, Path(owner): Path<String>) -> WebResult<Response> {
    ctx.user_id()?;
    let svc = ctx.state.ext.get::<ch_core::packages::PackagesHandle>().ok_or_else(WebError::not_found)?;
    let rows: Vec<PackageRow> = svc.0.list(&ctx.actor, Some(&owner), None).await?.into_iter().map(row).collect();
    let c = ctx.chrome(&format!("Packages of {owner}")).await;
    ctx.page(&ListPage { c, heading: format!("Packages of {owner}"), rows })
}

#[derive(Template)]
#[template(path = "packages/repo_list.html")]
struct RepoListPage {
    c: Chrome,
    rows: Vec<PackageRow>,
}

pub async fn repo_list(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    ctx.user_id()?;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let svc = ctx.state.ext.get::<ch_core::packages::PackagesHandle>().ok_or_else(WebError::not_found)?;
    let rows: Vec<PackageRow> = svc.0.list(&ctx.actor, None, Some(rc.id())).await?.into_iter().map(row).collect();
    let c = rc.chrome(&ctx, "Packages", "packages").await;
    ctx.page(&RepoListPage { c, rows })
}

// ------------------------------------------------------------------ one package

#[derive(Template)]
#[template(path = "packages/show.html")]
struct ShowPage {
    c: Chrome,
    p: PackageView,
    host: String,
    docker_host: String,
    username: String,
    action: String,
    errors: FieldErrors,
}

/// The host docker needs: the external URL without its scheme or trailing slash, port kept. Docker
/// references and `docker login` take a host, and reject `https://host/...` as an invalid reference.
pub fn docker_host(external_url: &str) -> String {
    let t = external_url.trim();
    let lower = t.to_ascii_lowercase();
    let rest = ["https://", "http://"].iter().find(|p| lower.starts_with(**p)).map_or(t, |p| &t[p.len()..]);
    rest.trim_end_matches('/').to_string()
}

async fn package_page(ctx: &Ctx, owner: &str, kind: &str, name: &str, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let svc = ctx.state.ext.get::<ch_core::packages::PackagesHandle>().ok_or_else(WebError::not_found)?;
    let p = view(svc.0.detail(&ctx.actor, owner, kind, name).await?);
    let c = ctx.chrome("Packages").await;
    let username = c.user.as_ref().map(|u| u.username.clone()).unwrap_or_default();
    let host = ctx.state.config.server.external_url.trim_end_matches('/').to_string();
    ctx.page_status(
        status,
        &ShowPage { c, p, docker_host: docker_host(&host), host, username, action: package_href(owner, kind, name), errors },
    )
}

pub async fn show(ctx: Ctx, Path((owner, kind, name)): Path<(String, String, String)>) -> WebResult<Response> {
    ctx.user_id()?;
    package_page(&ctx, &owner, &kind, &name, FieldErrors::default(), StatusCode::OK).await
}

// ------------------------------------------------------------------ changes

#[derive(Template)]
#[template(path = "packages/confirm_link.html")]
struct ConfirmPage {
    c: Chrome,
    name: String,
    repo: String,
    audience: String,
    widens: bool,
    action: String,
    back: String,
}

/// One endpoint for every form on the package page: the hidden `do` field says which.
pub async fn act(ctx: Ctx, Path((owner, kind, name)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let svc = ctx.state.ext.get::<ch_core::packages::PackagesHandle>().ok_or_else(WebError::not_found)?;
    let back = package_href(&owner, &kind, &name);
    let repo = form.get("repo").trim().to_string();
    match form.get("do") {
        "preview_link" => match svc.0.preview_link(&ctx.actor, &owner, &kind, &name, &repo).await {
            Ok(preview) => {
                let c = ctx.chrome("Link a package").await;
                ctx.page(&ConfirmPage {
                    c,
                    name: name.clone(),
                    repo,
                    audience: preview.audience,
                    widens: preview.widens,
                    action: back.clone(),
                    back,
                })
            }
            Err(e) => failed(&ctx, &owner, &kind, &name, e).await,
        },
        "link" => {
            let confirmed = form.get("confirm") == "1";
            match svc.0.link(&ctx.actor, &owner, &kind, &name, &repo, confirmed, &ctx.client).await {
                Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Linked the package to {repo}."))),
                Err(e) => failed(&ctx, &owner, &kind, &name, e).await,
            }
        }
        "unlink" => match svc.0.unlink(&ctx.actor, &owner, &kind, &name, &ctx.client).await {
            Ok(()) => Ok(ctx.redirect_flash(&back, "ok", "Unlinked the package. Only its owner can use it now.")),
            Err(e) => failed(&ctx, &owner, &kind, &name, e).await,
        },
        "delete_version" => {
            let label = form.get("label").trim().to_string();
            let confirm_name = form.get("confirm_name");
            match svc.0.delete_version(&ctx.actor, &owner, &kind, &name, &label, confirm_name, &ctx.client).await {
                Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Deleted the version {label}."))),
                Err(e) => failed(&ctx, &owner, &kind, &name, e).await,
            }
        }
        "delete_package" => {
            let confirm_name = form.get("confirm_name");
            match svc.0.delete_package(&ctx.actor, &owner, &kind, &name, confirm_name, &ctx.client).await {
                Ok(()) => Ok(ctx.redirect_flash("/packages", "ok", &format!("Deleted the package {name}."))),
                Err(e) => failed(&ctx, &owner, &kind, &name, e).await,
            }
        }
        _ => Err(WebError::bad("That form is not one we know.")),
    }
}

/// A failed change goes back to the package page as a 422 when the form was at fault; anything
/// else becomes the error page.
async fn failed(ctx: &Ctx, owner: &str, kind: &str, name: &str, e: Error) -> WebResult<Response> {
    match FieldErrors::from_error(&e) {
        Some(errs) => {
            let msg = if errs.general.is_empty() { "Check the form and try again.".to_string() } else { errs.general };
            package_page(ctx, owner, kind, name, FieldErrors::general(&msg), StatusCode::UNPROCESSABLE_ENTITY).await
        }
        None => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{docker_host, orphan_note};

    #[test]
    fn the_page_tells_never_linked_unlinked_and_deleted_apart() {
        let none = None;
        let linked = Some("alice/app".to_string());
        assert_eq!(orphan_note(&linked, false, None), None, "a linked package says nothing");
        assert_eq!(orphan_note(&none, false, None), Some("Not linked to a repository, so only its owner can use it."));
        assert_eq!(orphan_note(&none, true, Some("unlinked")), Some("It was unlinked from its repository, so only its owner can use it."));
        assert_eq!(orphan_note(&none, true, Some("repo_deleted")), Some("Its repository was deleted, so only its owner can use it."));
        assert_eq!(
            orphan_note(&none, true, Some("repo_moved")),
            Some("Its repository moved to another owner, so only its owner can use it.")
        );
        assert_eq!(orphan_note(&none, true, None), Some("Its repository was deleted, moved or unlinked, so only its owner can use it."));
    }

    #[test]
    fn docker_gets_a_host_and_never_a_scheme() {
        assert_eq!(docker_host("https://code.example.com"), "code.example.com");
        assert_eq!(docker_host("https://code.example.com/"), "code.example.com");
        assert_eq!(docker_host("http://192.0.2.100:3200"), "192.0.2.100:3200");
        assert_eq!(docker_host("HTTPS://Code.Example.com///"), "Code.Example.com");
        assert_eq!(docker_host("code.example.com"), "code.example.com");
    }
}
