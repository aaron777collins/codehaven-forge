//! Repository settings: the general page with its danger zone, collaborators, branch
//! protection rules and webhooks. Reading any of these needs Maintain access; the actions
//! that hand the repository to someone else or end it need Admin.

use super::repo_common::{RepoCtx, branch_names};
use super::{FieldErrors, Params, Query};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt;
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::model::{BranchProtection, NewWebhook, Repo, Visibility};
use ch_core::web_contract::RepoUpdate;
use ch_core::{Error, Permission, UserId, WebhookId};

const PERMS: [(&str, &str); 5] = [("read", "Read"), ("triage", "Triage"), ("write", "Write"), ("maintain", "Maintain"), ("admin", "Admin")];

const EVENTS: [(&str, &str); 12] = [
    ("push", "Push"),
    ("pull_request", "Pull request"),
    ("issues", "Issues"),
    ("issue_comment", "Issue comment"),
    ("pull_request_review", "Pull request review"),
    ("release", "Release"),
    ("workflow_run", "Workflow run"),
    ("create", "Create"),
    ("delete", "Delete"),
    ("check_run", "Check run"),
    ("status", "Status"),
    ("member", "Member"),
];

// ------------------------------------------------------------------ pure helpers

/// Topics as the form gives them: comma separated, any case, maybe repeated. They come back
/// lowercase, trimmed and deduped, or an error a person can read.
fn normalise_topics(raw: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let p = part.trim().to_lowercase();
        if p.is_empty() {
            continue;
        }
        if p.len() > 50 || !p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("{p} is not a valid topic. Topics use letters, numbers and dashes."));
        }
        if !out.iter().any(|t| t == &p) {
            out.push(p);
        }
    }
    if out.len() > 20 {
        return Err("Add at most 20 topics.".into());
    }
    Ok(out)
}

/// A new repository name: the same shape as the sign-up form allows.
fn repo_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with(['.', '-'])
        && !name.ends_with('.')
        && !name.contains("..")
}

/// A webhook address must be a full http or https address with a host. Whether the server
/// will actually deliver to it is a separate check: private and loopback addresses are
/// refused unless the administrator allowed them.
fn hook_url_ok(url: &str) -> bool {
    let u = url.trim();
    if u.len() > 2048 || u.chars().any(char::is_whitespace) {
        return false;
    }
    let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = host_port.split_once(':').map_or(host_port, |(h, _)| h);
    !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// The checked event boxes, keeping only names this site knows, in the order of the list.
fn events_from_form(form: &FormData) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in form.all("events") {
        if EVENTS.iter().any(|(id, _)| *id == v) && !out.iter().any(|e| e == v) {
            out.push(v.to_string());
        }
    }
    out
}

/// One name per line. Check names can hold commas, as in `test (ubuntu, 1.22)`.
fn split_lines(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.lines() {
        let p = part.trim();
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    }
    out
}

fn approvals_text(n: u32) -> String {
    match n {
        0 => "No approving reviews required.".into(),
        1 => "1 approving review required.".into(),
        n => format!("{n} approving reviews required."),
    }
}

fn rule_flags(p: &BranchProtection) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if p.dismiss_stale_reviews {
        v.push("Dismisses stale reviews".into());
    }
    if p.require_code_owner_review {
        v.push("Requires code owner review".into());
    }
    if p.require_resolved_threads {
        v.push("Requires resolved review threads".into());
    }
    if p.strict {
        v.push("Branch must be up to date before merging".into());
    }
    if p.block_force_push {
        v.push("Blocks force push".into());
    }
    if p.block_deletion {
        v.push("Blocks deletion".into());
    }
    if p.allow_direct_push {
        v.push("Allows direct push without a pull request".into());
    }
    v
}

/// The protection form as one rule, or field errors a person can act on.
fn protection_from_form(form: &FormData) -> Result<BranchProtection, FieldErrors> {
    let pattern = form.get("pattern").trim().to_string();
    let mut errors = FieldErrors::default();
    if pattern.is_empty() {
        errors = errors.with("pattern", "Name the branch or pattern.");
    } else if pattern.len() > 200 || pattern.chars().any(char::is_whitespace) || pattern.starts_with('/') || pattern.contains("..") {
        errors = errors.with("pattern", "Use a branch name or a pattern like release/*.");
    }
    let raw = form.get("required_approvals").trim();
    let required_approvals = if raw.is_empty() {
        0
    } else {
        match raw.parse::<u32>() {
            Ok(n) if n <= 6 => n,
            _ => {
                errors = errors.with("required_approvals", "Required approvals must be from 0 to 6.");
                0
            }
        }
    };
    let required_checks = split_lines(form.get("required_checks"));
    if required_checks.iter().any(|c| c.len() > 100) {
        errors = errors.with("required_checks", "Keep each check name to 100 characters or fewer.");
    }
    if errors.any() {
        return Err(errors);
    }
    Ok(BranchProtection {
        pattern,
        required_approvals,
        dismiss_stale_reviews: form.flag("dismiss_stale_reviews"),
        require_code_owner_review: form.flag("require_code_owner_review"),
        required_checks,
        strict: form.flag("strict"),
        require_resolved_threads: form.flag("require_resolved_threads"),
        block_force_push: form.flag("block_force_push"),
        block_deletion: form.flag("block_deletion"),
        allow_direct_push: form.flag("allow_direct_push"),
    })
}

// ------------------------------------------------------------------ general

struct BranchOption {
    name: String,
    selected: bool,
}

/// Everything the general page shows in its inputs, so a failed danger zone post can put the
/// page back the way the user left it.
struct GeneralInput {
    description: String,
    topics: String,
    default_branch: String,
    rename_name: String,
    transfer_owner: String,
    transfer_confirm: String,
    delete_confirm: String,
}

impl GeneralInput {
    fn of_repo(repo: &Repo) -> Self {
        GeneralInput {
            description: repo.description.clone(),
            topics: repo.topics.join(", "),
            default_branch: repo.default_branch.clone(),
            rename_name: String::new(),
            transfer_owner: String::new(),
            transfer_confirm: String::new(),
            delete_confirm: String::new(),
        }
    }

    fn of_form(form: &FormData) -> Self {
        GeneralInput {
            description: form.get("description").trim().to_string(),
            topics: form.get("topics").trim().to_string(),
            default_branch: form.get("default_branch").trim().to_string(),
            rename_name: form.get("new_name").trim().to_string(),
            transfer_owner: form.get("new_owner").trim().to_string(),
            transfer_confirm: form.get("confirm").trim().to_string(),
            delete_confirm: form.get("confirm").trim().to_string(),
        }
    }
}

#[derive(Template)]
#[template(path = "settings_repo/general.html")]
struct GeneralPage {
    c: Chrome,
    base: String,
    branches: Vec<BranchOption>,
    /// `private`, `internal` or `public`.
    tier: &'static str,
    /// The parent's tier for a fork, which caps the choices; `public` otherwise.
    parent_tier: &'static str,
    switch_on: bool,
    /// The account owner (or an org owner, or a site admin): the only one who may publish or
    /// change who can contribute.
    is_owner: bool,
    /// `everyone` or `grants`.
    policy: &'static str,
    text_private: &'static str,
    text_internal: &'static str,
    text_public: &'static str,
    confirm_internal: &'static str,
    description: String,
    topics: String,
    can_admin: bool,
    is_private: bool,
    archived: bool,
    /// The repository merges only through its merge queue (ch-dxk).
    merge_queue: bool,
    repo_name: String,
    full_name: String,
    rename_name: String,
    transfer_owner: String,
    transfer_confirm: String,
    delete_confirm: String,
    errors: FieldErrors,
}

/// The tier a fork may reach: its parent's. Anything else may reach public.
async fn parent_tier(ctx: &Ctx, rc: &RepoCtx) -> &'static str {
    match rc.repo.fork_parent_id {
        Some(p) => match ctx.state.repos.get(&ch_core::Actor::System, p).await {
            Ok(parent) => parent.visibility.as_str(),
            Err(_) => "private",
        },
        None => "public",
    }
}

async fn general_page(ctx: &Ctx, rc: &RepoCtx, input: GeneralInput, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let mut names = branch_names(ctx, rc.id()).await?;
    if !input.default_branch.is_empty() && !names.contains(&input.default_branch) {
        names.insert(0, input.default_branch.clone());
    }
    let branches = names.into_iter().map(|name| BranchOption { selected: name == input.default_branch, name }).collect();
    let c = rc.chrome(ctx, "Settings", "settings").await;
    let page = GeneralPage {
        c,
        base: rc.base(),
        branches,
        can_admin: rc.perm.allows(Permission::Admin),
        is_private: rc.repo.visibility == Visibility::Private,
        tier: rc.repo.visibility.as_str(),
        parent_tier: parent_tier(ctx, rc).await,
        switch_on: ctx.state.repos.public_repos_allowed().await.unwrap_or(false),
        is_owner: super::visibility::is_owner_for_page(ctx, rc).await,
        policy: rc.repo.contribution_policy.as_str(),
        text_private: super::visibility::TEXT_PRIVATE,
        text_internal: super::visibility::TEXT_INTERNAL,
        text_public: super::visibility::TEXT_PUBLIC,
        confirm_internal: super::visibility::CONFIRM_INTERNAL,
        archived: rc.repo.archived,
        merge_queue: ctx.state.pulls.merge_queue_enabled(&ctx.actor, rc.id()).await.unwrap_or(false),
        repo_name: rc.repo.name.clone(),
        full_name: rc.repo.full_name(),
        errors,
        description: input.description,
        topics: input.topics,
        rename_name: input.rename_name,
        transfer_owner: input.transfer_owner,
        transfer_confirm: input.transfer_confirm,
        delete_confirm: input.delete_confirm,
    };
    ctx.page_status(status, &page)
}

pub async fn general_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    general_page(&ctx, &rc, GeneralInput::of_repo(&rc.repo), FieldErrors::default(), StatusCode::OK).await
}

pub async fn general_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    let input = GeneralInput::of_form(&form);
    let mut errors = FieldErrors::default();
    if input.description.len() > 500 {
        errors = errors.with("description", "Keep the description to 500 characters or fewer.");
    }
    let topics = match normalise_topics(&input.topics) {
        Ok(t) => Some(t),
        Err(m) => {
            errors = errors.with("topics", &m);
            None
        }
    };
    let names = branch_names(&ctx, rc.id()).await?;
    if input.default_branch.is_empty() || (!names.contains(&input.default_branch) && input.default_branch != rc.repo.default_branch) {
        errors = errors.with("default_branch", "Pick one of the branches.");
    }
    if errors.any() {
        return general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let update = RepoUpdate {
        description: Some(input.description.clone()),
        default_branch: Some(input.default_branch.clone()),
        topics,
        ..Default::default()
    };
    match ctx.state.repos.update(&ctx.actor, rc.id(), update, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&format!("{}/settings", rc.base()), "ok", "Settings saved.")),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn archive_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let archived = match form.get("value") {
        "1" => true,
        "0" => false,
        _ => return Err(WebError::bad("Use the archive buttons on the settings page.")),
    };
    let update = RepoUpdate { archived: Some(archived), ..Default::default() };
    ctx.state.repos.update(&ctx.actor, rc.id(), update, &ctx.client).await?;
    let text = if archived {
        "Repository archived. It is read only for everyone now."
    } else {
        "Repository unarchived. Pushes and merges work again."
    };
    Ok(ctx.redirect_flash(&format!("{}/settings", rc.base()), "ok", text))
}

/// The merge queue switch (ch-dxk): while it is on, pull requests merge only through the queue.
pub async fn merge_queue_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let on = form.get("enabled") == "1";
    ctx.state.pulls.set_merge_queue(&ctx.actor, rc.id(), on, &ctx.client).await?;
    let text = if on { "Turned the merge queue on." } else { "Turned the merge queue off. Anything waiting in it was removed." };
    Ok(ctx.redirect_flash(&format!("{}/settings", rc.base()), "ok", text))
}

pub async fn rename_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let new_name = form.get("new_name").trim().to_string();
    let mut input = GeneralInput::of_repo(&rc.repo);
    input.rename_name = new_name.clone();
    if !repo_name_ok(&new_name) {
        let errors = FieldErrors::default().with("new_name", "Use letters, numbers, dashes, underscores and dots.");
        return general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    if new_name == rc.repo.name {
        return Ok(ctx.redirect_flash(&format!("{}/settings", rc.base()), "info", "The repository already has that name."));
    }
    match ctx.state.repos.rename(&ctx.actor, rc.id(), &new_name, &ctx.client).await {
        Ok(repo) => {
            let to = format!("/{}/{}", fmt::enc(&repo.owner_name), fmt::enc(&repo.name));
            Ok(ctx.redirect_flash(&format!("{to}/settings"), "ok", &format!("Renamed the repository to {}.", repo.name)))
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn transfer_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let new_owner = form.get("new_owner").trim().to_string();
    let confirm = form.get("confirm").trim().to_string();
    let mut input = GeneralInput::of_repo(&rc.repo);
    input.transfer_owner = new_owner.clone();
    input.transfer_confirm = confirm.clone();
    let mut errors = FieldErrors::default();
    if new_owner.is_empty() {
        errors = errors.with("new_owner", "Name the new owner.");
    }
    if confirm != rc.repo.name {
        errors = errors.with("transfer_confirm", &format!("Type {} to confirm.", rc.repo.name));
    }
    if errors.any() {
        return general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let target = match ctx.state.accounts.get_by_username(&ctx.actor, &new_owner).await {
        Ok(u) => u,
        Err(Error::NotFound) => {
            let errors = FieldErrors::default().with("new_owner", "No account with that name.");
            return general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
        Err(e) => return Err(e.into()),
    };
    if target.id == rc.repo.owner_id {
        return Ok(ctx.redirect_flash(&format!("{}/settings", rc.base()), "info", "That account already owns this repository."));
    }
    match ctx.state.repos.transfer(&ctx.actor, rc.id(), target.id, &ctx.client).await {
        Ok(repo) => {
            let to = format!("/{}/{}", fmt::enc(&repo.owner_name), fmt::enc(&repo.name));
            Ok(ctx.redirect_flash(&format!("{to}/settings"), "ok", &format!("Transferred the repository to {}.", repo.owner_name)))
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn delete_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let full_name = rc.repo.full_name();
    let confirm = form.get("confirm").trim().to_string();
    if confirm != full_name {
        let mut input = GeneralInput::of_repo(&rc.repo);
        input.delete_confirm = confirm;
        let errors = FieldErrors::default().with("delete_confirm", &format!("Type {full_name} to confirm."));
        return general_page(&ctx, &rc, input, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    ctx.state.repos.delete(&ctx.actor, rc.id(), &ctx.client).await?;
    Ok(ctx.redirect_flash("/", "ok", &format!("Deleted {full_name}.")))
}

// ------------------------------------------------------------------ collaborators

struct PermOption {
    value: String,
    label: String,
    selected: bool,
}

fn perm_options(current: &str) -> Vec<PermOption> {
    PERMS.iter().map(|(v, l)| PermOption { value: (*v).to_string(), label: (*l).to_string(), selected: *v == current }).collect()
}

struct CollabRow {
    user_id: i64,
    username: String,
    perm_label: String,
    options: Vec<PermOption>,
}

#[derive(Template)]
#[template(path = "settings_repo/collaborators.html")]
struct CollaboratorsPage {
    c: Chrome,
    base: String,
    can_admin: bool,
    owner_username: String,
    owner_display: String,
    rows: Vec<CollabRow>,
    perm_options: Vec<PermOption>,
    new_username: String,
    errors: FieldErrors,
}

async fn collaborators_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    new_username: String,
    new_permission: String,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let list = ctx.state.repos.collaborators(&ctx.actor, rc.id()).await?;
    let owner_display = match ctx.state.accounts.get_user(rc.repo.owner_id).await {
        Ok(u) if !u.display_name.is_empty() => u.display_name,
        _ => rc.repo.owner_name.clone(),
    };
    let rows = list
        .into_iter()
        .map(|c| CollabRow {
            user_id: c.user_id.get(),
            perm_label: PERMS
                .iter()
                .find(|(v, _)| *v == c.permission.as_str())
                .map(|(_, l)| (*l).to_string())
                .unwrap_or_else(|| c.permission.as_str().to_string()),
            options: perm_options(c.permission.as_str()),
            username: c.username,
        })
        .collect();
    let add_options = if new_permission.is_empty() { perm_options("write") } else { perm_options(&new_permission) };
    let c = rc.chrome(ctx, "Collaborators", "settings").await;
    let page = CollaboratorsPage {
        c,
        base: rc.base(),
        can_admin: rc.perm.allows(Permission::Admin),
        owner_username: rc.repo.owner_name.clone(),
        owner_display,
        rows,
        perm_options: add_options,
        new_username,
        errors,
    };
    ctx.page_status(status, &page)
}

pub async fn collaborators_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    collaborators_page(&ctx, &rc, String::new(), String::new(), FieldErrors::default(), StatusCode::OK).await
}

pub async fn collaborator_add(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let back = format!("{}/settings/collaborators", rc.base());
    // A row's permission change sends the id; the add form sends a username.
    if let Some(id) = form.int("user_id") {
        let Some(p) = Permission::parse(form.get("permission")) else { return Err(WebError::bad("Pick a permission.")) };
        if id == rc.repo.owner_id.get() {
            return Err(WebError::bad("The owner already has full access to this repository."));
        }
        ctx.state.repos.set_collaborator(&ctx.actor, rc.id(), UserId(id), p, &ctx.client).await?;
        return Ok(ctx.redirect_flash(&back, "ok", "Permission saved."));
    }
    let username = form.get("username").trim().to_string();
    let permission = form.get("permission").trim().to_string();
    let parsed = Permission::parse(&permission);
    let mut errors = FieldErrors::default();
    if username.is_empty() {
        errors = errors.with("username", "Name the account to add.");
    }
    if parsed.is_none() {
        errors = errors.with("permission", "Pick a permission.");
    }
    if errors.any() {
        return collaborators_page(&ctx, &rc, username, permission, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let user = match ctx.state.accounts.get_by_username(&ctx.actor, &username).await {
        Ok(u) => u,
        Err(Error::NotFound) => {
            let errors = FieldErrors::default().with("username", "No account with that name.");
            return collaborators_page(&ctx, &rc, username, permission, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
        Err(e) => return Err(e.into()),
    };
    let p = parsed.unwrap_or(Permission::Write);
    let label = PERMS.iter().find(|(v, _)| *v == p.as_str()).map(|(_, l)| (*l).to_string()).unwrap_or_else(|| p.as_str().to_string());
    match ctx.state.repos.set_collaborator(&ctx.actor, rc.id(), user.id, p, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Added {} with {label} access.", user.username))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => collaborators_page(&ctx, &rc, username, permission, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn collaborator_remove(
    ctx: Ctx,
    Path((owner, name, user_id)): Path<(String, String, i64)>,
    _form: FormData,
) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    if user_id == rc.repo.owner_id.get() {
        return Err(WebError::bad("The owner cannot be removed from their own repository."));
    }
    ctx.state.repos.remove_collaborator(&ctx.actor, rc.id(), UserId(user_id), &ctx.client).await?;
    let who = ctx.state.accounts.get_user(UserId(user_id)).await.map(|u| u.username).unwrap_or_else(|_| "That person".into());
    Ok(ctx.redirect_flash(&format!("{}/settings/collaborators", rc.base()), "ok", &format!("Removed {who}'s access.")))
}

// ------------------------------------------------------------------ branch protection

#[derive(Default)]
struct ProtectionForm {
    pattern: String,
    required_approvals: u32,
    dismiss_stale_reviews: bool,
    require_code_owner_review: bool,
    require_resolved_threads: bool,
    block_force_push: bool,
    block_deletion: bool,
    allow_direct_push: bool,
    strict: bool,
    required_checks: String,
}

impl ProtectionForm {
    fn of_form(form: &FormData) -> Self {
        ProtectionForm {
            pattern: form.get("pattern").trim().to_string(),
            required_approvals: form.int("required_approvals").and_then(|n| u32::try_from(n).ok()).filter(|n| *n <= 6).unwrap_or(0),
            dismiss_stale_reviews: form.flag("dismiss_stale_reviews"),
            require_code_owner_review: form.flag("require_code_owner_review"),
            require_resolved_threads: form.flag("require_resolved_threads"),
            block_force_push: form.flag("block_force_push"),
            block_deletion: form.flag("block_deletion"),
            allow_direct_push: form.flag("allow_direct_push"),
            strict: form.flag("strict"),
            required_checks: form.get("required_checks").trim().to_string(),
        }
    }

    fn of_protection(p: &BranchProtection) -> Self {
        ProtectionForm {
            pattern: p.pattern.clone(),
            required_approvals: p.required_approvals,
            dismiss_stale_reviews: p.dismiss_stale_reviews,
            require_code_owner_review: p.require_code_owner_review,
            require_resolved_threads: p.require_resolved_threads,
            block_force_push: p.block_force_push,
            block_deletion: p.block_deletion,
            allow_direct_push: p.allow_direct_push,
            strict: p.strict,
            required_checks: p.required_checks.join("\n"),
        }
    }
}

struct RuleCard {
    pattern: String,
    edit_href: String,
    approvals_text: String,
    flags: Vec<String>,
    checks: Vec<String>,
}

#[derive(Template)]
#[template(path = "settings_repo/branches.html")]
struct BranchesPage {
    c: Chrome,
    base: String,
    f: ProtectionForm,
    editing: bool,
    rules: Vec<RuleCard>,
    errors: FieldErrors,
    /// Check names seen on recent runs, to copy into the field.
    suggestions: Vec<String>,
}

/// Names of checks and statuses that reported on the last few commits CI ran on. A failure to
/// read them only means no suggestions.
async fn recent_check_names(ctx: &Ctx, rc: &RepoCtx) -> Vec<String> {
    let Ok(runs) = ctx.state.ci.runs(&ctx.actor, rc.id(), 1, 30).await else { return Vec::new() };
    let mut shas: Vec<String> = Vec::new();
    for r in runs {
        if !shas.contains(&r.head_sha) {
            shas.push(r.head_sha);
        }
        if shas.len() == 5 {
            break;
        }
    }
    let mut names: Vec<String> = Vec::new();
    for sha in &shas {
        if let Ok(list) = ctx.state.checks.check_runs_for_sha(rc.id(), sha).await {
            names.extend(list.into_iter().map(|c| c.name));
        }
        if let Ok(list) = ctx.state.checks.statuses_for_sha(rc.id(), sha).await {
            names.extend(list.into_iter().map(|s| s.context));
        }
    }
    names.retain(|n| !n.is_empty() && n.len() <= 100);
    names.sort();
    names.dedup();
    names.truncate(40);
    names
}

#[allow(clippy::too_many_arguments)]
async fn branches_view(
    ctx: &Ctx,
    rc: &RepoCtx,
    rules: Vec<BranchProtection>,
    f: ProtectionForm,
    editing: bool,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let cards = rules
        .iter()
        .map(|p| RuleCard {
            pattern: p.pattern.clone(),
            edit_href: fmt::enc(&p.pattern),
            approvals_text: approvals_text(p.required_approvals),
            flags: rule_flags(p),
            checks: p.required_checks.clone(),
        })
        .collect();
    let chosen = split_lines(&f.required_checks);
    let mut suggestions = recent_check_names(ctx, rc).await;
    suggestions.retain(|n| !chosen.contains(n));
    let c = rc.chrome(ctx, "Branch protection", "settings").await;
    ctx.page_status(status, &BranchesPage { c, base: rc.base(), f, editing, rules: cards, errors, suggestions })
}

async fn branches_again(ctx: &Ctx, rc: &RepoCtx, form: &FormData, errors: FieldErrors) -> WebResult<Response> {
    let f = ProtectionForm::of_form(form);
    let rules = ctx.state.repos.protections(&ctx.actor, rc.id()).await?;
    let editing = rules.iter().any(|r| r.pattern == f.pattern);
    branches_view(ctx, rc, rules, f, editing, errors, StatusCode::UNPROCESSABLE_ENTITY).await
}

pub async fn branches_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    let rules = ctx.state.repos.protections(&ctx.actor, rc.id()).await?;
    let mut f = ProtectionForm::default();
    let mut editing = false;
    if let Some(pat) = q.get("edit").map(String::as_str)
        && let Some(p) = rules.iter().find(|r| r.pattern == pat)
    {
        f = ProtectionForm::of_protection(p);
        editing = true;
    }
    branches_view(&ctx, &rc, rules, f, editing, FieldErrors::default(), StatusCode::OK).await
}

pub async fn branch_protect(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    let protection = match protection_from_form(&form) {
        Ok(p) => p,
        Err(errors) => return branches_again(&ctx, &rc, &form, errors).await,
    };
    let pattern = protection.pattern.clone();
    match ctx.state.repos.set_protection(&ctx.actor, rc.id(), protection, &ctx.client).await {
        Ok(()) => {
            Ok(ctx.redirect_flash(&format!("{}/settings/branches", rc.base()), "ok", &format!("Saved the protection rule for {pattern}.")))
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => branches_again(&ctx, &rc, &form, errors).await,
            None => Err(e.into()),
        },
    }
}

pub async fn branch_unprotect(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    let pattern = form.get("pattern").trim().to_string();
    if pattern.is_empty() {
        return Err(WebError::bad("Name the protection rule to remove."));
    }
    ctx.state.repos.remove_protection(&ctx.actor, rc.id(), &pattern, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/branches", rc.base()), "ok", &format!("Removed the protection rule for {pattern}.")))
}

// ------------------------------------------------------------------ webhooks

struct EventOption {
    value: String,
    label: String,
    checked: bool,
}

struct HookRow {
    id: i64,
    url: String,
    events: Vec<String>,
    active: bool,
}

#[derive(Template)]
#[template(path = "settings_repo/hooks.html")]
struct HooksPage {
    c: Chrome,
    base: String,
    rows: Vec<HookRow>,
    url: String,
    active: bool,
    events: Vec<EventOption>,
    errors: FieldErrors,
}

async fn hooks_view(
    ctx: &Ctx,
    rc: &RepoCtx,
    url: String,
    active: bool,
    chosen: Vec<String>,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let list = ctx.state.webhooks.list(&ctx.actor, Some(rc.id()), None).await?;
    let rows = list.into_iter().map(|h| HookRow { id: h.id.get(), url: h.url, events: h.events, active: h.active }).collect();
    let events = EVENTS
        .iter()
        .map(|(v, l)| EventOption { value: (*v).to_string(), label: (*l).to_string(), checked: chosen.iter().any(|c| c.as_str() == *v) })
        .collect();
    let c = rc.chrome(ctx, "Webhooks", "settings").await;
    ctx.page_status(status, &HooksPage { c, base: rc.base(), rows, url, active, events, errors })
}

pub async fn hooks_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    hooks_view(&ctx, &rc, String::new(), true, Vec::new(), FieldErrors::default(), StatusCode::OK).await
}

pub async fn hook_create(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    let url = form.get("url").trim().to_string();
    let events = events_from_form(&form);
    let mut errors = FieldErrors::default();
    if url.is_empty() {
        errors = errors.with("url", "Give the webhook an address.");
    } else if !hook_url_ok(&url) {
        errors = errors.with("url", "The address must start with http:// or https:// and name a host.");
    }
    if events.is_empty() {
        errors = errors.with("events", "Pick at least one event.");
    }
    let active = form.flag("active");
    if errors.any() {
        return hooks_view(&ctx, &rc, url, active, events, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let new = NewWebhook {
        repo_id: Some(rc.id()),
        org_id: None,
        secret: form.opt("secret").map(str::to_string),
        url: url.clone(),
        events: events.clone(),
        active,
    };
    match ctx.state.webhooks.create(&ctx.actor, new, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&format!("{}/settings/hooks", rc.base()), "ok", "Webhook added.")),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => hooks_view(&ctx, &rc, url, active, events, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn hook_delete(ctx: Ctx, Path((owner, name, id)): Path<(String, String, i64)>, _form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Maintain)?;
    ctx.state.webhooks.delete(&ctx.actor, WebhookId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/hooks", rc.base()), "ok", "Webhook deleted."))
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_trimmed_lowercased_and_deduped() {
        assert_eq!(normalise_topics(" Rust , CLI ,rust, testing ").unwrap(), vec!["rust", "cli", "testing"]);
        assert_eq!(normalise_topics("").unwrap(), Vec::<String>::new());
        assert_eq!(normalise_topics(" , , ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn topics_reject_bad_characters_and_long_lists() {
        assert!(normalise_topics("web dev").is_err());
        assert!(normalise_topics("c++").is_err());
        let many = (0..21).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert!(normalise_topics(&many).is_err());
        let twenty = (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert_eq!(normalise_topics(&twenty).unwrap().len(), 20);
    }

    #[test]
    fn repo_names_are_checked() {
        assert!(repo_name_ok("my-repo_1.0"));
        assert!(repo_name_ok("a"));
        assert!(!repo_name_ok(""));
        assert!(!repo_name_ok("has space"));
        assert!(!repo_name_ok("-leading"));
        assert!(!repo_name_ok("trailing."));
        assert!(!repo_name_ok("a/b"));
        assert!(!repo_name_ok(&"x".repeat(101)));
    }

    #[test]
    fn hook_addresses_need_a_scheme_and_a_host() {
        assert!(hook_url_ok("https://example.com/hook"));
        assert!(hook_url_ok("http://ci.lan:8080/hook?x=1"));
        assert!(hook_url_ok("https://user:pw@example.com"));
        assert!(hook_url_ok("  http://localhost:9000/x  "));
        assert!(!hook_url_ok("ftp://example.com"));
        assert!(!hook_url_ok("example.com/hook"));
        assert!(!hook_url_ok("https:///hook"));
        assert!(!hook_url_ok("https:// /x"));
        assert!(!hook_url_ok(""));
        assert!(!hook_url_ok(&format!("https://{}.com/x", "a".repeat(2100))));
    }

    #[test]
    fn protection_parses_from_a_form() {
        let f = FormData::parse(
            b"pattern=release%2F*&required_approvals=2&dismiss_stale_reviews=1&strict=1&required_checks=build%0D%0Atest+%28ubuntu%2C+1.22%29%0Abuild%0A+%0A",
        );
        let p = protection_from_form(&f).unwrap();
        assert_eq!(p.pattern, "release/*");
        assert_eq!(p.required_approvals, 2);
        assert!(p.dismiss_stale_reviews && p.strict);
        assert!(!p.block_deletion && !p.allow_direct_push);
        assert_eq!(p.required_checks, vec!["build", "test (ubuntu, 1.22)"], "one name per line, commas kept");
    }

    #[test]
    fn protection_rejects_bad_input() {
        assert!(protection_from_form(&FormData::parse(b"pattern=&required_approvals=1")).is_err());
        assert!(protection_from_form(&FormData::parse(b"pattern=main&required_approvals=9")).is_err());
        assert!(protection_from_form(&FormData::parse(b"pattern=has+space&required_approvals=1")).is_err());
        assert!(protection_from_form(&FormData::parse(b"pattern=main&required_approvals=7")).is_err());
        assert!(protection_from_form(&FormData::parse(b"pattern=main&required_approvals=&block_deletion=1")).is_ok());
        assert!(protection_from_form(&FormData::parse(b"pattern=%2Fetc&required_approvals=1")).is_err());
    }

    #[test]
    fn events_come_from_the_known_list_only() {
        let f = FormData::parse(b"events=push&events=pull_request&events=rm+-rf&events=push");
        assert_eq!(events_from_form(&f), vec!["push", "pull_request"]);
        assert!(events_from_form(&FormData::parse(b"url=x")).is_empty());
        assert_eq!(EVENTS.len(), 12);
        assert_eq!(PERMS.len(), 5);
    }

    #[test]
    fn approval_text_reads_naturally() {
        assert_eq!(approvals_text(0), "No approving reviews required.");
        assert_eq!(approvals_text(1), "1 approving review required.");
        assert_eq!(approvals_text(3), "3 approving reviews required.");
    }
}

// ROUTES:
// GET+POST /{owner}/{repo}/settings -> general_get, general_post
// GET+POST /{owner}/{repo}/settings/visibility and POST .../settings/contributions -> views::visibility
// POST /{owner}/{repo}/settings/archive -> archive_post
// POST /{owner}/{repo}/settings/rename -> rename_post
// POST /{owner}/{repo}/settings/transfer -> transfer_post
// POST /{owner}/{repo}/settings/delete -> delete_post
// GET /{owner}/{repo}/settings/collaborators -> collaborators_get
// POST /{owner}/{repo}/settings/collaborators -> collaborator_add
// POST /{owner}/{repo}/settings/collaborators/{user_id}/remove -> collaborator_remove
// GET /{owner}/{repo}/settings/branches -> branches_get
// POST /{owner}/{repo}/settings/branches -> branch_protect
// POST /{owner}/{repo}/settings/branches/remove -> branch_unprotect
// GET /{owner}/{repo}/settings/hooks -> hooks_get
// POST /{owner}/{repo}/settings/hooks -> hook_create
// POST /{owner}/{repo}/settings/hooks/{id}/delete -> hook_delete
