//! The site admin pages: the instance overview, invite links, accounts, CI runners and the
//! audit log. Every handler checks `site_admin` first, and a non admin gets `NotFound`, so
//! the pages leave no trace that they exist. Secrets a page here mints (invite links, one
//! time password links, runner registration tokens) are only ever in the response that
//! created them, never in a redirect.

use super::{FieldErrors, Params, People, Query};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::audit::{AuditFilter, Outcome, StoredAudit, action};
use ch_core::model::{InviteStatus, LinkPurpose, NewInvite};
use ch_core::web_contract::{Runner, SiteStats};
use ch_core::{Error, InviteId, RunnerId, UserId};
use std::net::IpAddr;

/// Only a site admin may open these pages. An admin who has not turned two-factor on gets
/// told why; everyone else gets the same `NotFound` as any address that does not exist.
fn site_admin(ctx: &Ctx) -> WebResult<()> {
    if ctx.actor.is_site_admin() {
        Ok(())
    } else if ctx.actor.is_admin_without_2fa() {
        Err(WebError::Core(Error::forbidden("Turn on two-factor sign-in in Settings before using the admin pages.")))
    } else {
        Err(WebError::not_found())
    }
}

// ---------------------------------------------------------------- pure helpers

/// The audit `action` constant in words. Unknown actions pass through unchanged.
fn friendly_action(action: &str) -> &str {
    match action {
        action::SIGN_IN => "Signed in",
        action::SIGN_IN_FAILED => "Sign in failed",
        action::TWO_FACTOR_FAILED => "Two-factor code failed",
        action::LOCKOUT => "Locked out after failed sign-ins",
        action::UNLOCK => "Lockout lifted",
        action::SIGN_OUT => "Signed out",
        action::PASSWORD_SET => "Password set",
        action::PASSWORD_CHANGED => "Password changed",
        action::PASSWORD_RESET => "Password reset",
        action::TWO_FACTOR_ON => "Two-factor turned on",
        action::TWO_FACTOR_OFF => "Two-factor turned off",
        action::RECOVERY_CODE_USED => "Recovery code used",
        action::PASSKEY_ADDED => "Passkey added",
        action::PASSKEY_REMOVED => "Passkey removed",
        action::PASSKEY_USED => "Signed in with a passkey",
        action::INVITE_CREATED => "Invite created",
        action::INVITE_REVOKED => "Invite revoked",
        action::INVITE_ACCEPTED => "Invite accepted",
        action::INVITE_REFUSED => "Invite refused",
        action::USER_CREATED => "Account created",
        action::USER_SUSPENDED => "Account suspended",
        action::USER_ADMIN_GRANTED => "Admin access granted",
        action::USER_CI_CHANGED => "Can run CI changed",
        action::SESSION_REVOKED => "Session revoked",
        action::TOKEN_CREATED => "Access token created",
        action::TOKEN_DELETED => "Access token deleted",
        action::SSH_KEY_ADDED => "SSH key added",
        action::SSH_KEY_REMOVED => "SSH key removed",
        action::RUNNER_TOKEN_ISSUED => "Runner token issued",
        action::RUNNER_REGISTERED => "Runner registered",
        action::RUNNER_REMOVED => "Runner removed",
        action::SECRET_CHANGED => "Secret changed",
        action::SECRET_DELETED => "Secret deleted",
        action::REPO_VISIBILITY => "Visibility changed",
        action::REPO_TRANSFERRED => "Repository transferred",
        action::REPO_DELETED => "Repository deleted",
        action::BRANCH_PROTECTION => "Branch protection changed",
        other => other,
    }
}

/// The chip an invite gets: CSS class and word.
fn invite_chip(status: InviteStatus) -> (&'static str, &'static str) {
    match status {
        InviteStatus::Pending => ("chip-ok", "Pending"),
        InviteStatus::Used => ("chip-muted", "Used"),
        InviteStatus::Revoked => ("chip-danger", "Revoked"),
        InviteStatus::Expired => ("chip-warn", "Expired"),
    }
}

/// The chip a runner status gets. Anything unknown reads as offline.
fn runner_chip(status: &str) -> (&'static str, &'static str) {
    match status {
        "online" => ("chip-ok", "Online"),
        "busy" => ("chip-warn", "Busy"),
        _ => ("chip-muted", "Offline"),
    }
}

/// Success is green and failure red, except failed sign-ins and lockouts. Those are someone
/// trying a password that did not work, so they read amber, not red.
fn outcome_chip(outcome: Outcome, action: &str) -> (&'static str, &'static str) {
    match outcome {
        Outcome::Success => ("chip-ok", "success"),
        Outcome::Failure => match action {
            action::SIGN_IN_FAILED | action::TWO_FACTOR_FAILED | action::LOCKOUT => ("chip-warn", "denied"),
            _ => ("chip-danger", "failed"),
        },
    }
}

/// One choice in the invite expiry `<select>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiryOpt {
    pub hours: u32,
    pub label: &'static str,
    pub selected: bool,
}

const EXPIRY_CHOICES: [(u32, &str); 4] = [(1, "1 hour"), (24, "1 day"), (168, "7 days"), (720, "30 days")];

/// The expiry choices inside `[min_hours, max_hours]`, with 7 days selected when the
/// configuration allows it and the first remaining choice otherwise. An empty list can only
/// come from a config that allows none of the fixed choices.
fn expiry_options(min_hours: u32, max_hours: u32) -> Vec<ExpiryOpt> {
    let mut out: Vec<ExpiryOpt> = EXPIRY_CHOICES
        .into_iter()
        .filter(|(h, _)| min_hours <= *h && *h <= max_hours)
        .map(|(h, label)| ExpiryOpt { hours: h, label, selected: false })
        .collect();
    if let Some(seven_days) = out.iter_mut().find(|o| o.hours == 168) {
        seven_days.selected = true;
    } else if let Some(first) = out.first_mut() {
        first.selected = true;
    }
    out
}

/// The audit filter from the query string. A blank or unparseable value simply drops that
/// filter, it is never an error page.
fn audit_filter(actor: &str, action: &str, ip: &str, before: &str) -> AuditFilter {
    let actor_id = actor.trim().parse::<i64>().ok().map(UserId);
    let trimmed = action.trim();
    let action = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
    AuditFilter {
        actor_id,
        action,
        client_ip: ip.trim().parse::<IpAddr>().ok(),
        since: None,
        limit: Some(51),
        before_id: before.trim().parse::<i64>().ok(),
    }
}

/// Disk use for the overview card. `used` is what is taken, never more than `total`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskView {
    pub used_bytes: u64,
    pub used: String,
    pub total: String,
    pub percent: u64,
}

fn disk(total: u64, free: u64) -> DiskView {
    let used = total.saturating_sub(free);
    let percent = if total == 0 { 0 } else { ((u128::from(used.min(total)) * 100) / u128::from(total)) as u64 };
    DiskView { used_bytes: used, used: ch_render::human_bytes(used), total: ch_render::human_bytes(total), percent }
}

/// A query value that is present and not blank, trimmed. `Params` is a plain map, so this
/// stands in for the `opt` that `FormData` has.
fn opt<'a>(q: &'a Params, key: &str) -> Option<&'a str> {
    q.get(key).map(String::as_str).map(str::trim).filter(|v| !v.is_empty())
}

/// Back to the users list, keeping the search when there was one.
fn users_url(q: &str) -> String {
    let q = q.trim();
    if q.is_empty() { "/admin/users".to_string() } else { format!("/admin/users?q={}", fmt::enc(q)) }
}

/// The next page of audit rows: the filters that are set, plus the id to read before.
fn audit_query(actor: &str, action: &str, ip: &str, before_id: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !actor.is_empty() {
        parts.push(format!("actor={}", fmt::enc(actor)));
    }
    if !action.is_empty() {
        parts.push(format!("action={}", fmt::enc(action)));
    }
    if !ip.is_empty() {
        parts.push(format!("ip={}", fmt::enc(ip)));
    }
    parts.push(format!("before_id={before_id}"));
    format!("/admin/audit?{}", parts.join("&"))
}

// ---------------------------------------------------------------- overview

#[derive(Debug, Clone)]
struct StatsView {
    users: i64,
    orgs: i64,
    repos: i64,
    runners_online: i64,
    queued_jobs: i64,
    database_ok: bool,
    version: String,
    disk: DiskView,
}

fn stats_view(s: &SiteStats) -> StatsView {
    StatsView {
        users: s.users,
        orgs: s.orgs,
        repos: s.repos,
        runners_online: s.runners_online,
        queued_jobs: s.queued_jobs,
        database_ok: s.database_ok,
        version: s.version.clone(),
        disk: disk(s.data_dir_total_bytes, s.data_dir_free_bytes),
    }
}

#[derive(Template)]
#[template(path = "admin/overview.html")]
struct OverviewPage {
    c: Chrome,
    section: &'static str,
    stats: StatsView,
}

pub async fn overview(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    let stats = stats_view(&ctx.state.people.site_stats(&ctx.actor).await?);
    let c = ctx.chrome("Overview").await.nav("admin");
    ctx.page(&OverviewPage { c, section: "overview", stats })
}

// ---------------------------------------------------------------- invites

/// What the create form held, kept across a failed POST so the page re-renders with it.
#[derive(Debug, Clone, Default)]
struct InviteForm {
    email: String,
    note: String,
    org: String,
    team: String,
    grants_admin: bool,
    grants_ci: bool,
    hours: Option<u32>,
}

impl InviteForm {
    fn from_form(form: &FormData) -> Self {
        InviteForm {
            email: form.get("email").trim().to_string(),
            note: form.get("note").trim().to_string(),
            org: form.get("org").trim().trim_start_matches('@').to_string(),
            team: form.get("team").trim().to_string(),
            grants_admin: form.flag("grants_admin"),
            grants_ci: form.flag("grants_ci"),
            hours: form.int("expires_in_hours").map(|h| h as u32),
        }
    }
}

struct InviteRow {
    id: i64,
    who: String,
    note: String,
    chip_class: &'static str,
    chip_label: &'static str,
    pending: bool,
    expires: TimeView,
    created: TimeView,
    used_by: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/invites.html")]
struct InvitesPage {
    c: Chrome,
    section: &'static str,
    rows: Vec<InviteRow>,
    form: InviteForm,
    options: Vec<ExpiryOpt>,
    errors: FieldErrors,
}

async fn invites_page(ctx: &Ctx, form: InviteForm, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let invites = ctx.state.invites.list(&ctx.actor).await?;
    let now = chrono::Utc::now();
    let mut people = People::new(&ctx.state);
    let mut rows = Vec::with_capacity(invites.len());
    for inv in &invites {
        let status = inv.status(now);
        let (chip_class, chip_label) = invite_chip(status);
        rows.push(InviteRow {
            id: inv.id.0,
            who: inv.email.clone().unwrap_or_else(|| "anyone with the link".to_string()),
            note: inv.note.clone().unwrap_or_default(),
            chip_class,
            chip_label,
            pending: status == InviteStatus::Pending,
            expires: fmt::time(inv.expires_at),
            created: fmt::time(inv.created_at),
            used_by: match inv.used_by {
                Some(id) => Some(people.get(id).await.display),
                None => None,
            },
        });
    }
    let mut options = expiry_options(ctx.state.config.invites.min_expiry_hours, ctx.state.config.invites.max_expiry_hours);
    if let Some(hours) = form.hours {
        for o in &mut options {
            o.selected = o.hours == hours;
        }
    }
    let c = ctx.chrome("Invites").await.nav("admin");
    let page = InvitesPage { c, section: "invites", rows, form, options, errors };
    ctx.page_status(status, &page)
}

pub async fn invites_get(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    invites_page(&ctx, InviteForm::default(), FieldErrors::default(), StatusCode::OK).await
}

pub async fn invites_create(ctx: Ctx, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let held = InviteForm::from_form(&form);
    // An organization and a team by name, as `codehaven admin invite create --org --team` takes them.
    let (mut org_id, mut team_id) = (None, None);
    if !held.org.is_empty() {
        match ctx.state.accounts.get_by_username(&ctx.actor, &held.org).await {
            Ok(u) if u.kind == ch_core::model::UserKind::Org => org_id = Some(ch_core::OrgId(u.id.0)),
            _ => {
                let errors = FieldErrors::default().with("org", "There is no organization with that name.");
                return invites_page(&ctx, held, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
            }
        }
    }
    if !held.team.is_empty() {
        let Some(org) = org_id else {
            let errors = FieldErrors::default().with("team", "Name the organization the team is in.");
            return invites_page(&ctx, held, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
        };
        match ctx.state.orgs.get_team(&ctx.actor, org, &held.team).await {
            Ok(t) => team_id = Some(t.id),
            Err(_) => {
                let errors = FieldErrors::default().with("team", "That organization has no team with that name.");
                return invites_page(&ctx, held, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
            }
        }
    }
    let new = NewInvite {
        email: form.opt("email").map(str::to_string),
        grants_admin: held.grants_admin,
        grants_ci: held.grants_ci,
        org_id,
        team_id,
        note: form.opt("note").map(str::to_string),
        expires_in_hours: held.hours,
    };
    match ctx.state.invites.create(&ctx.actor, new, &ctx.client).await {
        Ok(created) => {
            let c = ctx.chrome("Invite created").await.nav("admin");
            // An invite made for an address is also mailed to it, when mail is on (ch-bfa).
            let emailed = match created.invite.email.clone() {
                Some(to) if ctx.state.mail.enabled() => {
                    let body = invite_mail(&c.instance, &created.url, created.invite.expires_at);
                    let subject = format!("You are invited to {}", c.instance);
                    match ctx.state.mail.send(&to, &subject, &body).await {
                        Ok(()) => Some(to),
                        Err(e) => {
                            tracing::warn!(error = %e, "an invite mail was not sent");
                            None
                        }
                    }
                }
                _ => None,
            };
            let page =
                InviteCreatedPage { c, section: "invites", url: created.url, expires: fmt::time(created.invite.expires_at), emailed };
            ctx.page(&page)
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => invites_page(&ctx, held, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

/// The response of the POST that created an invite. The full link is here and nowhere else.
#[derive(Template)]
#[template(path = "admin/invite_created.html")]
struct InviteCreatedPage {
    c: Chrome,
    section: &'static str,
    url: String,
    expires: TimeView,
    /// The address the link was mailed to, when it was.
    emailed: Option<String>,
}

/// The invite mail. Plain text, the link on a line of its own.
pub fn invite_mail(instance: &str, url: &str, expires: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "You are invited to make an account on {instance}, a private Git server.\n\nOpen this link to choose a username and password:\n\n{url}\n\nThe link works once, until {}. If you were not expecting this, ignore this mail.\n",
        expires.format("%-d %B %Y %H:%M UTC")
    )
}

pub async fn invite_revoke(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    ctx.state.invites.revoke(&ctx.actor, InviteId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash("/admin/invites", "ok", "Invite revoked."))
}

// ---------------------------------------------------------------- users

struct UserRow {
    id: i64,
    username: String,
    href: String,
    display: Option<String>,
    email: Option<String>,
    is_admin: bool,
    suspended: bool,
    can_run_ci: bool,
    has_2fa: bool,
    is_self: bool,
    joined: TimeView,
}

#[derive(Template)]
#[template(path = "admin/users.html")]
struct UsersPage {
    c: Chrome,
    section: &'static str,
    rows: Vec<UserRow>,
    q: String,
}

pub async fn users_get(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    site_admin(&ctx)?;
    let q_text = opt(&q, "q").unwrap_or("");
    let users = ctx.state.people.list_users(&ctx.actor, opt(&q, "q"), 100).await?;
    let me = ctx.user_id().ok();
    let rows = users
        .iter()
        .map(|u| UserRow {
            id: u.id.0,
            username: u.username.clone(),
            href: format!("/{}", fmt::enc(&u.username)),
            display: if u.display_name.is_empty() { None } else { Some(u.display_name.clone()) },
            email: u.email.clone(),
            is_admin: u.is_admin,
            suspended: u.suspended,
            can_run_ci: u.can_run_ci,
            has_2fa: u.has_2fa,
            is_self: me == Some(u.id),
            joined: fmt::time(u.created_at),
        })
        .collect();
    let c = ctx.chrome("Users").await.nav("admin");
    ctx.page(&UsersPage { c, section: "users", rows, q: q_text.to_string() })
}

pub async fn user_suspend(ctx: Ctx, Path(id): Path<i64>, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    ctx.state.accounts.suspend(&ctx.actor, UserId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash(&users_url(form.get("q")), "ok", "Account suspended."))
}

pub async fn user_ci(ctx: Ctx, Path(id): Path<i64>, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let on = form.get("value") == "1";
    ctx.state.accounts.set_can_run_ci(&ctx.actor, UserId(id), on, &ctx.client).await?;
    Ok(ctx.redirect_flash(&users_url(form.get("q")), "ok", if on { "Can run CI is now on." } else { "Can run CI is now off." }))
}

pub async fn user_unlock(ctx: Ctx, Path(id): Path<i64>, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    ctx.state.accounts.unlock(&ctx.actor, UserId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash(&users_url(form.get("q")), "ok", "Account unlocked."))
}

pub async fn user_signout(ctx: Ctx, Path(id): Path<i64>, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    ctx.state.accounts.sign_out_everywhere(&ctx.actor, UserId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash(&users_url(form.get("q")), "ok", "Signed out everywhere."))
}

/// The response of the POST that minted a password reset link. The link is here and nowhere
/// else. It works for one hour.
#[derive(Template)]
#[template(path = "admin/reset_link.html")]
struct ResetLinkPage {
    c: Chrome,
    section: &'static str,
    username: String,
    url: String,
    expires: TimeView,
}

pub async fn user_reset_password(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let link = ctx.state.accounts.issue_link(&ctx.actor, UserId(id), LinkPurpose::ResetPassword, 60, &ctx.client).await?;
    let username = ctx.state.accounts.get_user(UserId(id)).await.map(|u| u.username).unwrap_or_default();
    let c = ctx.chrome("Password reset link").await.nav("admin");
    let page = ResetLinkPage {
        c,
        section: "users",
        username,
        url: format!("{}{}", ctx.state.config.server.external_url, link.path),
        expires: fmt::time(link.expires_at),
    };
    ctx.page(&page)
}

// ---------------------------------------------------------------- runners

struct RunnerRow {
    id: i64,
    name: String,
    labels: Vec<String>,
    chip_class: &'static str,
    chip_label: &'static str,
    version: String,
    last_seen: Option<TimeView>,
    scope: String,
}

fn runner_row(r: &Runner) -> RunnerRow {
    let (chip_class, chip_label) = runner_chip(&r.status);
    RunnerRow {
        id: r.id.0,
        name: r.name.clone(),
        labels: r.labels.clone(),
        chip_class,
        chip_label,
        version: r.version.clone(),
        last_seen: fmt::time_opt(r.last_seen_at),
        scope: match r.repo_id {
            Some(id) => format!("Repository #{}", id.0),
            None => "All repositories".to_string(),
        },
    }
}

#[derive(Template)]
#[template(path = "admin/runners.html")]
struct RunnersPage {
    c: Chrome,
    section: &'static str,
    rows: Vec<RunnerRow>,
}

pub async fn runners_get(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    let rows = ctx.state.pipelines.runners(&ctx.actor, None).await?.iter().map(runner_row).collect();
    let c = ctx.chrome("Runners").await.nav("admin");
    ctx.page(&RunnersPage { c, section: "runners", rows })
}

pub async fn runner_revoke(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    ctx.state.pipelines.revoke_runner(&ctx.actor, RunnerId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash("/admin/runners", "ok", "Runner removed."))
}

/// The response of the POST that minted a runner registration token. The token is here and
/// nowhere else.
#[derive(Template)]
#[template(path = "admin/runner_token.html")]
struct RunnerTokenPage {
    c: Chrome,
    section: &'static str,
    token: String,
    expires: TimeView,
    commands: String,
}

pub async fn runner_token(ctx: Ctx, _form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let t = ctx.state.ci.create_runner_token(&ctx.actor, None, &ctx.client).await?;
    let commands = format!("haven-runner register --url {} --token {}\nhaven-runner run", ctx.state.config.server.external_url, t.token);
    let c = ctx.chrome("Runner registration token").await.nav("admin");
    ctx.page(&RunnerTokenPage { c, section: "runners", token: t.token, expires: fmt::time(t.expires_at), commands })
}

// ---------------------------------------------------------------- actions policy

#[derive(Template)]
#[template(path = "admin/actions.html")]
struct ActionsPolicyPage {
    c: Chrome,
    section: &'static str,
    mode: String,
    allow: String,
    block: String,
    require_sha: bool,
    allow_codehaven_repos: bool,
    error: String,
}

async fn actions_page(
    ctx: &Ctx,
    p: ch_core::web_contract::ActionsPolicy,
    error: &str,
    status: axum::http::StatusCode,
) -> WebResult<Response> {
    let c = ctx.chrome("Actions").await.nav("admin");
    ctx.page_status(
        status,
        &ActionsPolicyPage {
            c,
            section: "actions",
            mode: p.mode,
            allow: p.allow.join("\n"),
            block: p.block.join("\n"),
            require_sha: p.require_sha,
            allow_codehaven_repos: p.allow_codehaven_repos,
            error: error.to_string(),
        },
    )
}

pub async fn actions_get(ctx: Ctx) -> WebResult<Response> {
    site_admin(&ctx)?;
    let p = ctx.state.pipelines.actions_policy(&ctx.actor).await?;
    actions_page(&ctx, p, "", axum::http::StatusCode::OK).await
}

pub async fn actions_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    site_admin(&ctx)?;
    let lines = |s: &str| -> Vec<String> { s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect() };
    let p = ch_core::web_contract::ActionsPolicy {
        mode: form.get("mode").to_string(),
        allow: lines(form.get("allow")),
        block: lines(form.get("block")),
        require_sha: form.get("require_sha") == "1",
        allow_codehaven_repos: form.get("allow_codehaven_repos") == "1",
    };
    match ctx.state.pipelines.set_actions_policy(&ctx.actor, p.clone(), &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash("/admin/actions", "ok", "Saved the Actions policy.")),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errs) => {
                // Say which pattern or choice was wrong, not only that something was.
                let msg = [errs.general.clone(), errs.get("allow"), errs.get("block"), errs.get("mode")]
                    .into_iter()
                    .find(|m| !m.is_empty())
                    .unwrap_or_else(|| "Check the form and try again.".to_string());
                actions_page(&ctx, p, &msg, axum::http::StatusCode::UNPROCESSABLE_ENTITY).await
            }
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------- audit log

struct AuditRow {
    at: TimeView,
    actor: String,
    action_label: String,
    target: String,
    ip: String,
    chip_class: &'static str,
    chip_label: &'static str,
    details: String,
}

fn audit_row(s: &StoredAudit) -> AuditRow {
    let e = &s.event;
    let (chip_class, chip_label) = outcome_chip(e.outcome, &e.action);
    let target = match (&e.target_type, &e.target_id) {
        (Some(t), Some(i)) => format!("{t} #{i}"),
        (Some(t), None) => t.clone(),
        (None, Some(i)) => format!("#{i}"),
        (None, None) => "-".to_string(),
    };
    AuditRow {
        at: fmt::time(s.at),
        actor: e.actor_label.clone(),
        action_label: friendly_action(&e.action).to_string(),
        target,
        ip: e.client_ip.map(|ip| ip.to_string()).unwrap_or_else(|| "-".to_string()),
        chip_class,
        chip_label,
        details: if e.details.is_null() {
            String::new()
        } else {
            serde_json::to_string_pretty(&e.details).unwrap_or_else(|_| e.details.to_string())
        },
    }
}

#[derive(Template)]
#[template(path = "admin/audit.html")]
struct AuditPage {
    c: Chrome,
    section: &'static str,
    rows: Vec<AuditRow>,
    actor: String,
    action: String,
    ip: String,
    older_href: Option<String>,
}

pub async fn audit_get(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    site_admin(&ctx)?;
    let actor = opt(&q, "actor").unwrap_or("");
    let action = opt(&q, "action").unwrap_or("");
    let ip = opt(&q, "ip").unwrap_or("");
    let before = match q.get("before_id") {
        Some(v) if !v.trim().is_empty() => v.as_str(),
        _ => q.get("before").map(String::as_str).unwrap_or(""),
    };
    let mut stored = ctx.state.audit.query(&ctx.actor, audit_filter(actor, action, ip, before)).await?;
    // One more row than the page shows, to know whether older rows exist.
    let has_more = stored.len() > 50;
    if has_more {
        stored.truncate(50);
    }
    let older_href = if has_more { stored.last().map(|last| audit_query(actor, action, ip, last.id.0)) } else { None };
    let c = ctx.chrome("Audit log").await.nav("admin");
    let page = AuditPage {
        c,
        section: "audit",
        rows: stored.iter().map(audit_row).collect(),
        actor: actor.to_string(),
        action: action.to_string(),
        ip: ip.to_string(),
        older_href,
    };
    ctx.page(&page)
}

// ROUTES:
// GET /admin -> overview
// GET /admin/invites -> invites_get
// POST /admin/invites -> invites_create
// POST /admin/invites/{id}/revoke -> invite_revoke
// GET /admin/users -> users_get
// POST /admin/users/{id}/suspend -> user_suspend
// POST /admin/users/{id}/ci -> user_ci
// POST /admin/users/{id}/unlock -> user_unlock
// POST /admin/users/{id}/signout -> user_signout
// POST /admin/users/{id}/reset-password -> user_reset_password
// GET /admin/runners -> runners_get
// POST /admin/runners/{id}/revoke -> runner_revoke
// POST /admin/runners/token -> runner_token
// GET /admin/audit -> audit_get

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn at(y: i32, m: u32, d: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn friendly_action_names_the_known_constants() {
        assert_eq!(friendly_action(action::SIGN_IN), "Signed in");
        assert_eq!(friendly_action(action::SIGN_IN_FAILED), "Sign in failed");
        assert_eq!(friendly_action(action::INVITE_CREATED), "Invite created");
        assert_eq!(friendly_action(action::USER_SUSPENDED), "Account suspended");
        assert_eq!(friendly_action(action::RUNNER_REMOVED), "Runner removed");
        assert_eq!(friendly_action("something.new"), "something.new");
        assert_eq!(friendly_action(""), "");
    }

    #[test]
    fn invite_chips_match_the_status() {
        assert_eq!(invite_chip(InviteStatus::Pending), ("chip-ok", "Pending"));
        assert_eq!(invite_chip(InviteStatus::Used), ("chip-muted", "Used"));
        assert_eq!(invite_chip(InviteStatus::Revoked), ("chip-danger", "Revoked"));
        assert_eq!(invite_chip(InviteStatus::Expired), ("chip-warn", "Expired"));
    }

    #[test]
    fn runner_chips_match_the_status() {
        assert_eq!(runner_chip("online"), ("chip-ok", "Online"));
        assert_eq!(runner_chip("busy"), ("chip-warn", "Busy"));
        assert_eq!(runner_chip("offline"), ("chip-muted", "Offline"));
        assert_eq!(runner_chip("weird"), ("chip-muted", "Offline"));
    }

    #[test]
    fn failed_sign_ins_read_as_denied_not_failed() {
        assert_eq!(outcome_chip(Outcome::Success, action::SIGN_IN), ("chip-ok", "success"));
        assert_eq!(outcome_chip(Outcome::Failure, action::SIGN_IN_FAILED), ("chip-warn", "denied"));
        assert_eq!(outcome_chip(Outcome::Failure, action::TWO_FACTOR_FAILED), ("chip-warn", "denied"));
        assert_eq!(outcome_chip(Outcome::Failure, action::LOCKOUT), ("chip-warn", "denied"));
        assert_eq!(outcome_chip(Outcome::Failure, action::INVITE_REVOKED), ("chip-danger", "failed"));
    }

    #[test]
    fn expiry_options_filter_and_default_to_seven_days() {
        let opts = expiry_options(1, 720);
        assert_eq!(opts.iter().map(|o| o.hours).collect::<Vec<_>>(), vec![1, 24, 168, 720]);
        let selected: Vec<u32> = opts.iter().filter(|o| o.selected).map(|o| o.hours).collect();
        assert_eq!(selected, vec![168]);
        // A range without 7 days selects the first remaining choice.
        let narrow = expiry_options(20, 100);
        assert_eq!(narrow.iter().map(|o| o.hours).collect::<Vec<_>>(), vec![24]);
        assert!(narrow[0].selected);
        // A single choice is still selected.
        let one = expiry_options(1, 1);
        assert_eq!(one.len(), 1);
        assert!(one[0].selected);
        // Nothing in range gives nothing.
        assert!(expiry_options(200, 300).is_empty());
    }

    #[test]
    fn audit_filter_drops_blank_and_bad_values() {
        let f = audit_filter(" 7 ", " invite.created ", " 10.0.0.1 ", "42");
        assert_eq!(f.actor_id, Some(UserId(7)));
        assert_eq!(f.action.as_deref(), Some("invite.created"));
        assert_eq!(f.client_ip, Some("10.0.0.1".parse::<IpAddr>().unwrap()));
        assert_eq!(f.before_id, Some(42));
        assert_eq!(f.limit, Some(51));
        assert_eq!(f.since, None);
        let blank = audit_filter("", "  ", "", "");
        assert_eq!(blank.actor_id, None);
        assert_eq!(blank.action, None);
        assert_eq!(blank.client_ip, None);
        assert_eq!(blank.before_id, None);
        let bad = audit_filter("abc", "x", "not-an-ip", "abc");
        assert_eq!(bad.actor_id, None);
        assert_eq!(bad.action.as_deref(), Some("x"));
        assert_eq!(bad.client_ip, None);
        assert_eq!(bad.before_id, None);
    }

    #[test]
    fn disk_shows_use_and_never_exceeds_total() {
        let d = disk(2000, 500);
        assert_eq!(d.used_bytes, 1500);
        assert_eq!(d.used, ch_render::human_bytes(1500));
        assert_eq!(d.total, ch_render::human_bytes(2000));
        assert_eq!(d.percent, 75);
        assert_eq!(disk(0, 0).percent, 0);
        assert_eq!(disk(100, 500).used_bytes, 0);
        assert_eq!(disk(100, 500).percent, 0);
    }

    #[test]
    fn query_helpers_trim_and_keep_what_was_asked() {
        assert_eq!(users_url(""), "/admin/users");
        assert_eq!(users_url("   "), "/admin/users");
        assert_eq!(users_url(" ali ce "), "/admin/users?q=ali%20ce");
        assert_eq!(audit_query("", "", "", 9), "/admin/audit?before_id=9");
        assert_eq!(audit_query("7", "auth.sign_in", "", 12), "/admin/audit?actor=7&action=auth.sign_in&before_id=12");
        assert_eq!(audit_query("", "", "10.0.0.1", 3), "/admin/audit?ip=10.0.0.1&before_id=3");
    }

    #[test]
    fn opt_takes_trimmed_non_blank_values() {
        let mut q = Params::new();
        assert_eq!(opt(&q, "q"), None);
        q.insert("q".to_string(), "  alice  ".to_string());
        assert_eq!(opt(&q, "q"), Some("alice"));
        q.insert("q".to_string(), "   ".to_string());
        assert_eq!(opt(&q, "q"), None);
    }

    #[test]
    fn stats_view_carries_the_counts_and_disk() {
        let s = SiteStats { users: 3, database_ok: true, data_dir_total_bytes: 1000, data_dir_free_bytes: 250, ..Default::default() };
        let v = stats_view(&s);
        assert_eq!(v.users, 3);
        assert_eq!(v.orgs, 0);
        assert!(v.database_ok);
        assert_eq!(v.disk.used_bytes, 750);
        assert_eq!(v.disk.percent, 75);
    }

    #[test]
    fn runner_rows_read_the_scope_and_status() {
        let r = Runner {
            id: RunnerId(5),
            name: "box".into(),
            labels: vec!["linux".into()],
            repo_id: Some(ch_core::RepoId(9)),
            status: "online".into(),
            version: "1.2".into(),
            last_seen_at: None,
            created_at: at(2026, 1, 2),
        };
        let row = runner_row(&r);
        assert_eq!(row.id, 5);
        assert_eq!(row.name, "box");
        assert_eq!(row.labels, vec!["linux".to_string()]);
        assert_eq!(row.chip_class, "chip-ok");
        assert_eq!(row.chip_label, "Online");
        assert_eq!(row.scope, "Repository #9");
        assert!(row.last_seen.is_none());
        let mut shared = r;
        shared.repo_id = None;
        shared.status = "offline".into();
        let row = runner_row(&shared);
        assert_eq!(row.scope, "All repositories");
        assert_eq!(row.chip_label, "Offline");
    }

    #[test]
    fn audit_rows_shape_target_outcome_and_details() {
        let client = ch_core::model::ClientInfo { ip: "10.1.2.3".parse().unwrap(), user_agent: None };
        let event = ch_core::audit::AuditEvent::new(&ch_core::Actor::System, &client, action::SIGN_IN, Outcome::Success)
            .target("user", 4)
            .details(serde_json::json!({ "user": "alice" }));
        let s = StoredAudit { id: ch_core::AuditId(1), at: at(2026, 1, 2), event };
        let row = audit_row(&s);
        assert_eq!(row.action_label, "Signed in");
        assert_eq!(row.actor, "System (cli)");
        assert_eq!(row.target, "user #4");
        assert_eq!(row.ip, "10.1.2.3");
        assert_eq!(row.chip_class, "chip-ok");
        assert!(row.details.contains("\"alice\""));

        let denied = ch_core::audit::AuditEvent::new(&ch_core::Actor::System, &client, action::LOCKOUT, Outcome::Failure);
        let s = StoredAudit { id: ch_core::AuditId(2), at: at(2026, 1, 2), event: denied };
        let row = audit_row(&s);
        assert_eq!(row.target, "-");
        assert_eq!(row.chip_class, "chip-warn");
        assert_eq!(row.chip_label, "denied");
        assert_eq!(row.details, "");
    }
}
