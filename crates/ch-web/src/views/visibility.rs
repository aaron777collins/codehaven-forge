//! Who can read a repository and who can contribute to it (epic ch-4pn): the three choices on
//! the settings page, the confirm page for making a repository public, the contribution setting
//! and the site admin's switch for public repositories.
//!
//! Every rule lives in the service. These handlers only collect the person's answers: the typed
//! name, the box about secrets and the snapshot of what the page showed.

use super::FieldErrors;
use super::repo_common::RepoCtx;
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::model::{ContributionPolicy, PublishSummary, UserKind, Visibility};
use ch_core::services::OrgRole;
use ch_core::{Error, OrgId, Permission};

pub const TEXT_PRIVATE: &str = "Only people you give access to can see it.";
pub const TEXT_INTERNAL: &str = "Anyone signed in to this Codehaven can read it. They can open issues, comment and send pull requests unless the owner limits that. Pushing needs access.";
pub const TEXT_PUBLIC: &str = "Anyone on the internet can read it without an account. People signed in here can open issues, comment and send pull requests unless the owner limits that. Pushing needs access.";
pub const CONFIRM_INTERNAL: &str = "Everyone with an account here will be able to read it, and to open issues, comment and send pull requests unless the owner limits that.";
pub const PUBLIC_WARNING: &str = "Anyone on the internet will be able to read and copy this repository without an account. Its history becomes public: every commit on every branch, tag and pull request, files deleted in later commits, and the names and email addresses inside commits. So do its issues, pull requests, comments and the usernames on them, releases and the packages listed below. People signed in here can open issues, comment and send pull requests unless you limit that. The owner's name and display name, and an organization's description, become visible. Earlier Actions runs, logs, artifacts, checks and statuses do not. Making it private later cannot take back copies. If a secret was ever committed, change that secret first.";
pub const CHECKED_HISTORY: &str = "I have checked the code and its history for secrets and personal information.";
pub const SWITCH_TEXT: &str = "Public repositories can be read by anyone on the internet without an account. Turning this on does not make any repository public. New accounts still need an invite.";

/// The chip text and title for a tier, the same everywhere a repository is named.
pub fn chip(v: Visibility) -> (&'static str, &'static str) {
    match v {
        Visibility::Private => ("Private", TEXT_PRIVATE),
        Visibility::Internal => ("Internal", TEXT_INTERNAL),
        Visibility::Public => ("Public", TEXT_PUBLIC),
    }
}

/// May this person make the repository public? The account owner, an owner of the owning
/// organization, or a site admin with two-factor. The service decides again.
pub async fn is_owner_for_page(ctx: &Ctx, rc: &RepoCtx) -> bool {
    is_account_owner(ctx, rc).await
}

async fn is_account_owner(ctx: &Ctx, rc: &RepoCtx) -> bool {
    if ctx.actor.is_site_admin() || ctx.user_id().ok() == Some(rc.repo.owner_id) {
        return true;
    }
    let Ok(owner) = ctx.state.accounts.get_user(rc.repo.owner_id).await else { return false };
    owner.kind == UserKind::Org && matches!(ctx.state.authz.org_role(&ctx.actor, OrgId(owner.id.0)).await, Ok(Some(OrgRole::Owner)))
}

// ------------------------------------------------------------------ the tier

#[derive(Template)]
#[template(path = "settings_repo/publish.html")]
pub struct PublishPage {
    pub c: Chrome,
    pub base: String,
    pub full_name: String,
    pub tier: &'static str,
    pub summary: PublishSummary,
    pub switch_on: bool,
    pub is_owner: bool,
    pub warning: &'static str,
    pub checked_text: &'static str,
    pub confirm: String,
    pub errors: FieldErrors,
}

async fn publish_page(ctx: &Ctx, rc: &RepoCtx, confirm: &str, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let summary = ctx.state.repos.publish_summary(&ctx.actor, rc.id()).await?;
    let switch_on = ctx.state.repos.public_repos_allowed().await?;
    let is_owner = is_account_owner(ctx, rc).await;
    let c = rc.chrome(ctx, "Make public", "settings").await;
    let page = PublishPage {
        c,
        base: rc.base(),
        full_name: rc.repo.full_name(),
        tier: rc.repo.visibility.as_str(),
        summary,
        switch_on,
        is_owner,
        warning: PUBLIC_WARNING,
        checked_text: CHECKED_HISTORY,
        confirm: confirm.to_string(),
        errors,
    };
    ctx.page_status(status, &page)
}

/// `GET /{owner}/{repo}/settings/visibility`: the confirm page for making a repository public.
pub async fn visibility_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    publish_page(&ctx, &rc, "", FieldErrors::default(), StatusCode::OK).await
}

/// `POST /{owner}/{repo}/settings/visibility`: `value` is `private`, `internal` or `public`.
/// Internal needs the `confirm_internal` box. Public needs the typed name, the box about
/// secrets and the snapshot the page showed.
pub async fn visibility_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let settings = format!("{}/settings", rc.base());
    let to = Visibility::parse(form.get("value")).ok_or_else(|| WebError::bad("Use the visibility choices on the settings page."))?;
    if to == Visibility::Public && form.get("snapshot").is_empty() {
        // Picked on the settings page: the confirm page comes first.
        return Ok(ctx.redirect(&format!("{}/settings/visibility", rc.base())));
    }
    if to == Visibility::Public {
        let full = rc.repo.full_name();
        let mut errors = FieldErrors::default();
        if form.get("confirm").trim() != full {
            errors = errors.with("confirm", &format!("Type {full} to confirm."));
        }
        if !form.flag("checked_history") {
            errors = errors.with("checked_history", "Tick the box once you have checked.");
        }
        let now = ctx.state.repos.publish_summary(&ctx.actor, rc.id()).await?;
        if form.get("snapshot") != now.snapshot {
            errors.general = "The repository changed since this page was loaded. Check the totals again.".into();
        }
        if errors.any() || !errors.general.is_empty() {
            return publish_page(&ctx, &rc, form.get("confirm"), errors, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
        return match ctx.state.repos.set_tier(&ctx.actor, rc.id(), Visibility::Public, &ctx.client).await {
            Ok(_) => Ok(ctx.redirect_flash(&settings, "ok", "This repository is public now. Anyone can read it without an account.")),
            Err(e @ (Error::Forbidden(_) | Error::Validation(_) | Error::Conflict(_) | Error::Unavailable(_))) => {
                let errors = FieldErrors { general: e.to_string(), ..Default::default() };
                publish_page(&ctx, &rc, form.get("confirm"), errors, StatusCode::UNPROCESSABLE_ENTITY).await
            }
            Err(e) => Err(e.into()),
        };
    }
    if to > rc.repo.visibility && !form.flag("confirm_internal") {
        return Ok(ctx.redirect_flash(&settings, "error", "Tick the box to confirm who will be able to read it."));
    }
    ctx.state.repos.set_tier(&ctx.actor, rc.id(), to, &ctx.client).await?;
    let text = match to {
        Visibility::Private => "This repository is private now. Only people with access can see it.",
        _ => "This repository is internal now. Everyone signed in here can read it.",
    };
    Ok(ctx.redirect_flash(&settings, "ok", text))
}

// ------------------------------------------------------------------ who can contribute

/// `POST /{owner}/{repo}/settings/contributions`: `policy` is `everyone` or `grants`.
pub async fn contributions_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let policy = ContributionPolicy::parse(form.get("policy")).ok_or_else(|| WebError::bad("Pick one of the two choices."))?;
    ctx.state.repos.set_contribution_policy(&ctx.actor, rc.id(), policy, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings", rc.base()), "ok", "Saved."))
}

// ------------------------------------------------------------------ the instance switch

#[derive(Template)]
#[template(path = "admin/settings.html")]
pub struct AdminSettingsPage {
    pub c: Chrome,
    pub section: &'static str,
    pub on: bool,
    pub public_count: usize,
    pub text: &'static str,
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

/// `GET /admin/settings`.
pub async fn admin_settings_get(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    let on = ctx.state.repos.public_repos_allowed().await?;
    let public_count = ctx.state.repos.list_accessible(&ch_core::Actor::Anonymous, 1, 100).await.map(|v| v.len()).unwrap_or(0);
    let c = ctx.chrome("Settings").await.nav("admin");
    ctx.page(&AdminSettingsPage { c, section: "settings", on, public_count, text: SWITCH_TEXT })
}

/// `POST /admin/settings`: `allow_public_repos` is `1` to turn the switch on, anything else off.
pub async fn admin_settings_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let on = form.get("allow_public_repos") == "1";
    let lowered = ctx.state.repos.set_public_repos_allowed(&ctx.actor, on, &ctx.client).await?;
    let text = if on {
        "Public repositories are allowed. No repository was made public.".to_string()
    } else if lowered == 1 {
        "Public repositories are off. 1 public repository is internal now.".to_string()
    } else {
        format!("Public repositories are off. {lowered} public repositories are internal now.")
    };
    Ok(ctx.redirect_flash("/admin/settings", "ok", &text))
}

#[derive(Template)]
#[template(path = "admin/workspace.html")]
pub struct AdminWorkspacePage {
    pub c: Chrome,
    pub section: &'static str,
    pub allowed: bool,
    pub editor: bool,
    pub terminal_allowed: bool,
    pub terminal: bool,
}

/// `GET /admin/workspace`.
pub async fn admin_workspace_get(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    let s = ctx.state.repos.workspace_settings().await?;
    let c = ctx.chrome("Workspace").await.nav("admin");
    ctx.page(&AdminWorkspacePage {
        c,
        section: "workspace",
        allowed: ctx.state.config.workspace.enabled,
        editor: s.editor,
        terminal_allowed: ctx.state.config.workspace.terminal,
        terminal: s.terminal,
    })
}

/// `POST /admin/workspace`: `editor` or `terminal` is `1` to turn that switch on, `0` to turn it
/// off. A field the form does not carry leaves its switch as it was, so one card's form never
/// moves the other's switch.
pub async fn admin_workspace_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let mut settings = ctx.state.repos.workspace_settings().await?;
    let editor = !form.all("editor").is_empty();
    let terminal = !form.all("terminal").is_empty();
    if editor {
        settings.editor = form.get("editor") == "1";
    }
    if terminal {
        settings.terminal = form.get("terminal") == "1";
    }
    ctx.state.repos.set_workspace_settings(&ctx.actor, settings, &ctx.client).await?;
    let text = if terminal {
        if settings.terminal { "The terminal is on." } else { "The terminal is off. Open terminals stop at their next request." }
    } else if settings.editor {
        "The editor is on."
    } else {
        "The editor is off. Open workspaces stop at their next request."
    };
    Ok(ctx.redirect_flash("/admin/workspace", "ok", text))
}
