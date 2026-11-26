//! The deployment environment settings pages: the list, and one environment's rules and secrets.

use super::FieldErrors;
use super::actions::{SecretRow, TabLink, secret_name, settings_tabs};
use super::repo_common::RepoCtx;
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt;
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::web_contract::EnvironmentDraft;
use ch_core::{Permission, RunId, UserId};

/// What the rules form submitted, kept to refill the page after a refusal.
struct RuleInput {
    wait: String,
    branches: String,
    checked: Vec<i64>,
}

pub struct EnvRow {
    pub name: String,
    pub href: String,
    pub reviewers: String,
    pub wait: String,
    pub branches: String,
}

pub struct ReviewerOption {
    pub id: i64,
    pub login: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "actions/environments.html")]
struct EnvironmentsPage {
    c: Chrome,
    tabs: Vec<TabLink>,
    action: String,
    error: String,
    name_value: String,
    rows: Vec<EnvRow>,
}

#[derive(Template)]
#[template(path = "actions/environment.html")]
struct EnvironmentPage {
    c: Chrome,
    tabs: Vec<TabLink>,
    action: String,
    env: String,
    error: String,
    wait_minutes: String,
    branches: String,
    candidates: Vec<ReviewerOption>,
    secrets: Vec<SecretRow>,
    delete_action: String,
}

async fn environments_page(ctx: &Ctx, rc: &RepoCtx, error: &str, name_value: &str, status: StatusCode) -> WebResult<Response> {
    let list = ctx.state.pipelines.environments(&ctx.actor, rc.id()).await?;
    let base = rc.base();
    let c = rc.chrome(ctx, "Environments", "settings").await;
    ctx.page_status(
        status,
        &EnvironmentsPage {
            c,
            tabs: settings_tabs(&base, "environments"),
            action: format!("{base}/settings/environments"),
            rows: list
                .iter()
                .map(|e| EnvRow {
                    name: e.name.clone(),
                    href: format!("{base}/settings/environments/{}", fmt::enc(&e.name)),
                    reviewers: if e.reviewers.is_empty() {
                        "none".to_string()
                    } else {
                        e.reviewers.iter().map(|r| r.login.as_str()).collect::<Vec<_>>().join(", ")
                    },
                    wait: if e.wait_minutes > 0 { format!("{} min", e.wait_minutes) } else { "none".to_string() },
                    branches: if e.branches.is_empty() { "any".to_string() } else { e.branches.join(", ") },
                })
                .collect(),
            error: error.to_string(),
            name_value: name_value.to_string(),
        },
    )
}

/// `input` refills the rules form with what was submitted instead of what is stored.
async fn environment_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    env: &str,
    error: &str,
    status: StatusCode,
    input: Option<RuleInput>,
) -> WebResult<Response> {
    let info = ctx.state.pipelines.environment(&ctx.actor, rc.id(), env).await?;
    let candidates = ctx.state.pipelines.reviewer_candidates(&ctx.actor, rc.id()).await?;
    let checked: Vec<i64> = match &input {
        Some(i) => i.checked.clone(),
        None => info.reviewers.iter().map(|r| r.user_id.get()).collect(),
    };
    let base = rc.base();
    let c = rc.chrome(ctx, &format!("Environment {}", info.name), "settings").await;
    ctx.page_status(
        status,
        &EnvironmentPage {
            c,
            tabs: settings_tabs(&base, "environments"),
            action: format!("{base}/settings/environments/{}", fmt::enc(env)),
            env: info.name.clone(),
            wait_minutes: match &input {
                Some(i) => i.wait.clone(),
                None => info.wait_minutes.to_string(),
            },
            branches: match &input {
                Some(i) => i.branches.clone(),
                None => info.branches.join("\n"),
            },
            candidates: candidates
                .iter()
                .map(|r| ReviewerOption { id: r.user_id.get(), login: r.login.clone(), checked: checked.contains(&r.user_id.get()) })
                .collect(),
            secrets: info
                .secrets
                .iter()
                .map(|s| SecretRow {
                    name: s.name.clone(),
                    when: fmt::time(s.updated_at),
                    delete_action: format!("{base}/settings/environments/{}/secrets/{}/delete", fmt::enc(env), fmt::enc(&s.name)),
                })
                .collect(),
            error: error.to_string(),
            delete_action: format!("{base}/settings/environments/{}/delete", fmt::enc(env)),
        },
    )
}

pub async fn environments_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    environments_page(&ctx, &rc, "", "", StatusCode::OK).await
}

pub async fn environments_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let name_in = form.get("name").trim().to_string();
    match ctx.state.pipelines.create_environment(&ctx.actor, rc.id(), &name_in, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(
            &format!("{}/settings/environments/{}", rc.base(), fmt::enc(&name_in)),
            "ok",
            &format!("Created the environment {name_in}."),
        )),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errs) => {
                let msg = [errs.general.clone(), errs.get("name")].into_iter().find(|m| !m.is_empty());
                environments_page(
                    &ctx,
                    &rc,
                    msg.as_deref().unwrap_or("Check the form and try again."),
                    &name_in,
                    StatusCode::UNPROCESSABLE_ENTITY,
                )
                .await
            }
            None => Err(e.into()),
        },
    }
}

pub async fn environment_get(ctx: Ctx, Path((owner, name, env)): Path<(String, String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    environment_page(&ctx, &rc, &env, "", StatusCode::OK, None).await
}

/// One endpoint for both forms on the page: the hidden `kind` field says which.
pub async fn environment_post(ctx: Ctx, Path((owner, name, env)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let kind = form.get("kind");
    if !matches!(kind, "rules" | "secret") {
        return Err(WebError::bad("That form is not one we know."));
    }
    if kind == "secret" {
        let name_in = form.get("name").trim().to_string();
        let value = form.get("value");
        let Some(n) = secret_name(&name_in) else {
            let msg = "Secret names use upper case letters, digits and underscores, and start with a letter or underscore.";
            return environment_page(&ctx, &rc, &env, msg, StatusCode::UNPROCESSABLE_ENTITY, None).await;
        };
        if value.trim().is_empty() {
            return environment_page(&ctx, &rc, &env, "Put a value in.", StatusCode::UNPROCESSABLE_ENTITY, None).await;
        }
        match ctx.state.pipelines.set_environment_secret(&ctx.actor, rc.id(), &env, &n, value, &ctx.client).await {
            Ok(()) => Ok(ctx.redirect_flash(
                &format!("{}/settings/environments/{}", rc.base(), fmt::enc(&env)),
                "ok",
                &format!("Saved the secret {n}."),
            )),
            Err(e) => match FieldErrors::from_error(&e) {
                Some(errs) => {
                    environment_page(
                        &ctx,
                        &rc,
                        &env,
                        if errs.general.is_empty() { "Check the form and try again." } else { &errs.general },
                        StatusCode::UNPROCESSABLE_ENTITY,
                        None,
                    )
                    .await
                }
                None => Err(e.into()),
            },
        }
    } else {
        let reviewers: Vec<UserId> = form.all("reviewer").into_iter().filter_map(|v| v.parse::<i64>().ok()).map(UserId).collect();
        let input = RuleInput {
            wait: form.get("wait_minutes").trim().to_string(),
            branches: form.get("branches").to_string(),
            checked: reviewers.iter().map(|u| u.get()).collect(),
        };
        let Ok(wait_minutes) = input.wait.parse::<i32>() else {
            return environment_page(
                &ctx,
                &rc,
                &env,
                "The wait must be a whole number of minutes.",
                StatusCode::UNPROCESSABLE_ENTITY,
                Some(input),
            )
            .await;
        };
        let branches: Vec<String> = input.branches.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        let draft = EnvironmentDraft { wait_minutes, branches, reviewers };
        match ctx.state.pipelines.save_environment(&ctx.actor, rc.id(), &env, draft, &ctx.client).await {
            Ok(_) => Ok(ctx.redirect_flash(&format!("{}/settings/environments/{}", rc.base(), fmt::enc(&env)), "ok", "Saved the rules.")),
            Err(e) => match FieldErrors::from_error(&e) {
                Some(errs) => {
                    environment_page(
                        &ctx,
                        &rc,
                        &env,
                        if errs.general.is_empty() { "Check the form and try again." } else { &errs.general },
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Some(input),
                    )
                    .await
                }
                None => Err(e.into()),
            },
        }
    }
}

pub async fn environment_delete(ctx: Ctx, Path((owner, name, env)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    let _ = form;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    ctx.state.pipelines.delete_environment(&ctx.actor, rc.id(), &env, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/environments", rc.base()), "ok", &format!("Deleted the environment {env}.")))
}

pub async fn environment_secret_delete(
    ctx: Ctx,
    Path((owner, name, env, secret)): Path<(String, String, String, String)>,
    form: FormData,
) -> WebResult<Response> {
    let _ = form;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    ctx.state.pipelines.delete_environment_secret(&ctx.actor, rc.id(), &env, &secret, &ctx.client).await?;
    Ok(ctx.redirect_flash(
        &format!("{}/settings/environments/{}", rc.base(), fmt::enc(&env)),
        "ok",
        &format!("Deleted the secret {secret}."),
    ))
}

/// A required reviewer approves or rejects a job held at a deployment environment (ch-bj5).
pub async fn deployment_review(
    ctx: Ctx,
    Path((owner, name, run, job)): Path<(String, String, i64, i64)>,
    form: FormData,
) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let decision = form.get("decision");
    if decision != "approve" && decision != "reject" {
        return Err(WebError::bad("That choice is not one we know."));
    }
    let approve = decision == "approve";
    ctx.state.pipelines.review_deployment(&ctx.actor, rc.id(), RunId(run), job, approve, form.get("comment"), &ctx.client).await?;
    Ok(ctx.redirect_flash(
        &format!("{}/actions/runs/{run}", rc.base()),
        "ok",
        if approve { "Approved the deployment." } else { "Rejected the deployment." },
    ))
}
