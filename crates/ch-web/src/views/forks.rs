//! Forks: the button, the page that makes one, and the list of a repository's forks (ch-8tj).
//!
//! The service decides who may fork, into which account and at which tier. A fork starts at most
//! as visible as its parent, and a public fork is made private first and confirmed on the
//! visibility page, the same as a new repository.

use super::people::{OwnerChoice, owner_choices, visibility_of};
use super::repo_common::RepoCtx;
use super::visibility::{TEXT_INTERNAL, TEXT_PRIVATE, TEXT_PUBLIC, chip};
use super::{FieldErrors, UserRef};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::WebResult;
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::UserId;
use ch_core::model::Visibility;

/// Shown when the parent caps the choice.
pub const FORK_CAP: &str = "A fork cannot be more visible than the repository it came from.";

#[derive(Template)]
#[template(path = "repo/fork.html")]
struct ForkPage {
    c: Chrome,
    base: String,
    full_name: String,
    owners: Vec<OwnerChoice>,
    tier: String,
    /// The parent's tier caps the choices.
    allow_internal: bool,
    allow_public: bool,
    switch_on: bool,
    text_private: &'static str,
    text_internal: &'static str,
    text_public: &'static str,
    cap_text: &'static str,
    errors: FieldErrors,
}

async fn fork_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    picked: Option<i64>,
    tier: Visibility,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    // A fork of your own repository cannot go in your own account, so the first other account
    // you can create in comes first.
    let mut owners = owner_choices(ctx, uid, picked).await;
    if picked.is_none() {
        let first = owners.iter().position(|o| o.id != rc.repo.owner_id.0).unwrap_or(0);
        if let Some(o) = owners.get_mut(first) {
            o.selected = true;
        }
    }
    let switch_on = ctx.state.repos.public_repos_allowed().await.unwrap_or(false);
    let c = rc.chrome(ctx, &format!("Fork {}", rc.repo.full_name()), "code").await;
    ctx.page_status(
        status,
        &ForkPage {
            c,
            base: rc.base(),
            full_name: rc.repo.full_name(),
            owners,
            tier: tier.as_str().into(),
            allow_internal: rc.repo.visibility >= Visibility::Internal,
            allow_public: rc.repo.visibility == Visibility::Public && switch_on,
            switch_on,
            text_private: TEXT_PRIVATE,
            text_internal: TEXT_INTERNAL,
            text_public: TEXT_PUBLIC,
            cap_text: FORK_CAP,
            errors,
        },
    )
}

pub async fn fork_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    ctx.user_id()?;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    fork_page(&ctx, &rc, None, Visibility::Private, FieldErrors::default(), StatusCode::OK).await
}

pub async fn fork_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let picked = form.int("owner_id");
    let tier = visibility_of(form.get("visibility"));
    let owners = owner_choices(&ctx, uid, picked).await;
    let Some(into) = owners.iter().find(|o| Some(o.id) == picked).map(|o| UserId(o.id)) else {
        let errors = FieldErrors::default().with("owner_id", "Pick an account you can create repositories for.");
        return fork_page(&ctx, &rc, picked, tier, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    };
    if tier > rc.repo.visibility {
        return fork_page(&ctx, &rc, picked, tier, FieldErrors::default().with("visibility", FORK_CAP), StatusCode::UNPROCESSABLE_ENTITY)
            .await;
    }
    // Nothing is created public: a public fork is made internal first and confirmed next.
    match ctx.state.repos.fork_as(&ctx.actor, rc.id(), into, tier.min(Visibility::Internal), &ctx.client).await {
        Ok(fork) => {
            let to = format!("/{}/{}", fmt::enc(&fork.owner_name), fmt::enc(&fork.name));
            Ok(if tier == Visibility::Public { ctx.redirect(&format!("{to}/settings/visibility")) } else { ctx.redirect(&to) })
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => {
                // The form has no name field: a clash reads as a sentence at the top.
                let errors = if errors.get("name").is_empty() {
                    errors
                } else {
                    FieldErrors::general(&format!("That account already has a repository named {}.", rc.repo.name))
                };
                fork_page(&ctx, &rc, picked, tier, errors, StatusCode::UNPROCESSABLE_ENTITY).await
            }
            None => Err(e.into()),
        },
    }
}

pub struct ForkRow {
    pub owner: UserRef,
    pub full_name: String,
    pub href: String,
    pub tier: &'static str,
    pub tier_title: &'static str,
    pub description: String,
    pub created: TimeView,
}

#[derive(Template)]
#[template(path = "repo/forks.html")]
struct ForksPage {
    c: Chrome,
    base: String,
    rows: Vec<ForkRow>,
    can_fork: bool,
}

pub async fn forks_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    // Only forks this viewer can read, so nothing private is named or counted.
    let forks = ctx.state.repos.forks(&ctx.actor, rc.id()).await.unwrap_or_default();
    let mut people = super::People::new(&ctx.state);
    let mut rows = Vec::with_capacity(forks.len());
    for f in forks {
        let (tier, tier_title) = chip(f.visibility);
        rows.push(ForkRow {
            owner: people.get(f.owner_id).await,
            href: format!("/{}/{}", fmt::enc(&f.owner_name), fmt::enc(&f.name)),
            full_name: f.full_name(),
            tier,
            tier_title,
            description: f.description,
            created: fmt::time(f.created_at),
        });
    }
    let c = rc.chrome(&ctx, &format!("Forks of {}", rc.repo.full_name()), "code").await;
    ctx.page(&ForksPage { c, base: rc.base(), rows, can_fork: ctx.signed_in() })
}

/// Star or unstar for the signed-in person, back to the page they were on.
pub async fn star_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let on = form.get("star") != "0";
    ctx.state.repos.set_star(&ctx.actor, rc.id(), on).await?;
    Ok(ctx.redirect_flash(&rc.base(), "ok", if on { "Starred." } else { "Unstarred." }))
}

/// How much of this repository's activity reaches the person's inbox.
pub async fn watch_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let level = form.get("level");
    let text = match level {
        "all" => "You will hear about all activity in this repository.",
        "ignore" => "You will not hear about this repository, even when mentioned.",
        "participating" => "You will hear when you take part or are mentioned.",
        _ => return Ok(ctx.redirect_flash(&rc.base(), "error", "Choose how much you want to hear.")),
    };
    ctx.state.notifications.subscribe(&ctx.actor, rc.id(), level).await?;
    Ok(ctx.redirect_flash(&rc.base(), "ok", text))
}
