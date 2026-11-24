//! Codehaven Pages in the app (`docs/design/pages.md`): the repository's Pages settings, the
//! signed-in hop that makes a viewing token, and the site admin's full mode switch. Every rule
//! lives in the service; these handlers collect answers and show results.

use super::FieldErrors;
use super::repo_common::RepoCtx;
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::{Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use ch_core::model::Visibility;
use ch_core::pages::{DeployState, PagesConfigInput, PagesFolder, PagesSettings, PagesSite};
use ch_core::{Error, Permission};
use std::collections::HashMap;

pub const FULL_MODE_WARNING: &str = "Every account on this instance can then publish pages that run with full browser features on a subdomain of the pages host. Their pages can set cookies for the parent domain and keep their own storage. Only turn this on if you trust every account. The proxy also needs a wildcard certificate for the subdomains.";

pub struct AudienceOption {
    pub value: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

pub struct BranchChoice {
    pub name: String,
    pub selected: bool,
}

pub struct CopyRow {
    pub id: i64,
    pub sha: String,
    pub branch: String,
    pub folder: &'static str,
    pub state: &'static str,
    pub chip: &'static str,
    pub error: String,
    pub files: String,
    pub when: TimeView,
    pub live: bool,
    pub can_serve: bool,
}

#[derive(Template)]
#[template(path = "settings_repo/pages.html")]
pub struct PagesPage {
    pub c: Chrome,
    pub base: String,
    pub available: bool,
    pub can_admin: bool,
    pub enabled: bool,
    pub url: String,
    pub serving: &'static str,
    pub branches: Vec<BranchChoice>,
    pub root_selected: bool,
    pub audiences: Vec<AudienceOption>,
    pub copies: Vec<CopyRow>,
    pub working: bool,
    pub errors: FieldErrors,
}

fn audience_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Public => "Anyone, with no account",
        Visibility::Internal => "Anyone signed in here",
        Visibility::Private => "People with access to this repository",
    }
}

/// The choices up to the repository's tier, widest first.
fn audiences(repo: Visibility, chosen: Visibility) -> Vec<AudienceOption> {
    [Visibility::Public, Visibility::Internal, Visibility::Private]
        .into_iter()
        .filter(|v| *v <= repo)
        .map(|v| AudienceOption { value: v.as_str(), label: audience_label(v), selected: v == chosen.min(repo) })
        .collect()
}

fn mb(bytes: i64) -> String {
    if bytes < 1024 * 1024 { format!("{} KB", (bytes + 1023) / 1024) } else { format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)) }
}

fn copy_rows(site: &PagesSite) -> Vec<CopyRow> {
    site.deployments
        .iter()
        .map(|d| CopyRow {
            id: d.id,
            sha: fmt::short_sha(&d.commit_sha),
            branch: d.branch.clone(),
            folder: d.folder.label(),
            state: match d.state {
                DeployState::Queued => "Waiting",
                DeployState::Building => "Publishing",
                DeployState::Ready => "Ready",
                DeployState::Failed => "Failed",
            },
            chip: match d.state {
                DeployState::Ready => "",
                DeployState::Failed => "chip-warn",
                _ => "",
            },
            error: d.error.clone().unwrap_or_default(),
            files: if d.state == DeployState::Ready {
                format!("{}, {}", fmt::plural(d.file_count, "file", "files"), mb(d.total_bytes))
            } else {
                String::new()
            },
            when: fmt::time(d.finished_at.unwrap_or(d.created_at)),
            live: d.live,
            can_serve: d.state == DeployState::Ready && !d.live,
        })
        .collect()
}

async fn branch_list(ctx: &Ctx, rc: &RepoCtx) -> WebResult<Vec<String>> {
    let refs = ctx.state.git.refs(rc.id(), Some("refs/heads/")).await?;
    Ok(refs.into_iter().filter_map(|r| r.name.strip_prefix("refs/heads/").map(str::to_string)).collect())
}

async fn render(ctx: &Ctx, rc: &RepoCtx, input: Option<PagesConfigInput>, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let available = ctx.state.pages.enabled();
    let can_admin = rc.perm.allows(Permission::Admin);
    let site = if available && can_admin { ctx.state.pages.site(&ctx.actor, rc.id()).await? } else { None };
    let tier = rc.repo.visibility;
    let (branch, folder, audience) = match (&input, &site) {
        (Some(i), _) => (i.branch.clone(), i.folder, i.audience),
        (None, Some(s)) => (s.branch.clone(), s.folder, s.audience),
        (None, None) => (rc.repo.default_branch.clone(), PagesFolder::Root, tier),
    };
    let mut names = if can_admin && available { branch_list(ctx, rc).await? } else { Vec::new() };
    if !branch.is_empty() && !names.contains(&branch) {
        names.insert(0, branch.clone());
    }
    let copies = site.as_ref().map(copy_rows).unwrap_or_default();
    let page = PagesPage {
        c: rc.chrome(ctx, "Pages", "settings").await,
        base: rc.base(),
        available,
        can_admin,
        enabled: site.is_some(),
        url: site.as_ref().map(|s| s.url.clone()).unwrap_or_default(),
        serving: site.as_ref().and_then(|s| s.serving_tier).map(audience_label).unwrap_or(""),
        branches: names.into_iter().map(|name| BranchChoice { selected: name == branch, name }).collect(),
        root_selected: folder == PagesFolder::Root,
        audiences: audiences(tier, audience),
        working: site
            .as_ref()
            .is_some_and(|s| s.deployments.iter().any(|d| matches!(d.state, DeployState::Queued | DeployState::Building))),
        copies,
        errors,
    };
    ctx.page_status(status, &page)
}

/// `GET /{owner}/{repo}/settings/pages`.
pub async fn settings_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    render(&ctx, &rc, None, FieldErrors::default(), StatusCode::OK).await
}

/// `POST /{owner}/{repo}/settings/pages`: turn on, or change the branch, folder or audience.
pub async fn settings_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let mut errors = FieldErrors::default();
    let folder = PagesFolder::parse(form.get("folder"));
    let audience = Visibility::parse(form.get("audience"));
    if folder.is_none() {
        errors = errors.with("folder", "Pick / or /docs.");
    }
    if audience.is_none() {
        errors = errors.with("audience", "Pick who can see the site.");
    }
    let input = PagesConfigInput {
        branch: form.get("branch").trim().to_string(),
        folder: folder.unwrap_or(PagesFolder::Root),
        audience: audience.unwrap_or(Visibility::Private),
    };
    if errors.any() {
        return render(&ctx, &rc, Some(input), errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let was_on = ctx.state.pages.site(&ctx.actor, rc.id()).await?.is_some();
    match ctx.state.pages.configure(&ctx.actor, rc.id(), input.clone(), &ctx.client).await {
        Ok(_) => {
            let text = if was_on { "Saved. A new copy is being published." } else { "Pages is on. The first copy is being published." };
            Ok(ctx.redirect_flash(&format!("{}/settings/pages", rc.base()), "ok", text))
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => render(&ctx, &rc, Some(input), errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

/// `POST /{owner}/{repo}/settings/pages/deploy`.
pub async fn deploy_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, _form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    ctx.state.pages.deploy(&ctx.actor, rc.id(), &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/pages", rc.base()), "ok", "A new copy is being published."))
}

/// `POST /{owner}/{repo}/settings/pages/rollback`: `deployment` is the copy to serve.
pub async fn rollback_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let id: i64 = form.get("deployment").parse().map_err(|_| WebError::not_found())?;
    ctx.state.pages.rollback(&ctx.actor, rc.id(), id, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/pages", rc.base()), "ok", "That copy is live now."))
}

/// `POST /{owner}/{repo}/settings/pages/disable`.
pub async fn disable_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, _form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    ctx.state.pages.disable(&ctx.actor, rc.id(), &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/pages", rc.base()), "ok", "Pages is off. Every copy was deleted."))
}

/// `GET /{owner}/{repo}/pages/open?path=`: for a signed-in person who may see the site, a
/// fresh token for it and a redirect there. Anything else is the ordinary not found.
pub async fn open_get(
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> WebResult<Response> {
    let path = q.get("path").map(String::as_str).unwrap_or("/");
    let to = ctx.state.pages.open(&ctx.actor, &owner, &name, path).await.map_err(|e| match e {
        Error::Validation(_) | Error::Forbidden(_) | Error::Unauthorized(_) => WebError::not_found(),
        e => WebError::Core(e),
    })?;
    let mut r = ctx.redirect(&to);
    // The address carries a token: never cached, never sent on as a referrer.
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    Ok(r)
}

#[derive(Template)]
#[template(path = "admin/pages.html")]
pub struct AdminPagesPage {
    pub c: Chrome,
    pub section: &'static str,
    pub available: bool,
    pub full_mode: bool,
    pub warning: &'static str,
}

fn site_admin(ctx: &Ctx) -> WebResult<()> {
    if ctx.actor.is_site_admin() {
        Ok(())
    } else if ctx.actor.is_admin_without_2fa() {
        Err(WebError::Core(Error::forbidden("Turn on two-factor sign-in in Settings before using the admin pages.")))
    } else {
        Err(WebError::not_found())
    }
}

/// `GET /admin/pages`.
pub async fn admin_get(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    let s = ctx.state.pages.settings().await?;
    let c = ctx.chrome("Pages").await.nav("admin");
    ctx.page(&AdminPagesPage {
        c,
        section: "pages",
        available: ctx.state.pages.enabled(),
        full_mode: s.full_mode,
        warning: FULL_MODE_WARNING,
    })
}

/// `POST /admin/pages`: `full_mode` is `1` to turn full mode on, anything else off.
pub async fn admin_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let on = form.get("full_mode") == "1";
    ctx.state.pages.set_settings(&ctx.actor, PagesSettings { full_mode: on }, &ctx.client).await?;
    let text = if on {
        "Full mode is on. Sites also open at their subdomain."
    } else {
        "Full mode is off. Sites open sandboxed at the pages host."
    };
    Ok(ctx.redirect_flash("/admin/pages", "ok", text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audiences_never_go_past_the_repository() {
        let v: Vec<&str> = audiences(Visibility::Internal, Visibility::Public).iter().map(|a| a.value).collect();
        assert_eq!(v, ["internal", "private"]);
        let chosen: Vec<&str> =
            audiences(Visibility::Internal, Visibility::Public).iter().filter(|a| a.selected).map(|a| a.value).collect();
        assert_eq!(chosen, ["internal"], "a wider saved choice shows as the repository's tier");
        assert_eq!(audiences(Visibility::Private, Visibility::Private).len(), 1);
    }
}
