//! Repositories, collaborators, forks, topics, branches, branch protection and tags.

use super::convert::*;
use super::{Api, ApiJson, ApiJsonOpt, PageQuery, paged, trim_page};
use axum::extract::{Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ch_api_types::common::SimpleUser;
use ch_api_types::repos as dto;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, Error, Permission, Result, UserId};
use serde::Serialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/user/repos", get(list_mine).post(create_mine))
        .route("/users/{user}/repos", get(list_user))
        .route("/orgs/{org}/repos", get(list_org).post(create_org))
        .route("/repos/{owner}/{repo}", get(get_repo).patch(patch_repo).delete(delete_repo))
        .route("/repos/{owner}/{repo}/topics", get(get_topics).put(put_topics))
        .route("/repos/{owner}/{repo}/forks", get(list_forks).post(create_fork))
        .route("/repos/{owner}/{repo}/transfer", post(transfer))
        .route("/repos/{owner}/{repo}/collaborators", get(list_collaborators))
        .route("/repos/{owner}/{repo}/collaborators/{user}", get(is_collaborator).put(add_collaborator).delete(remove_collaborator))
        .route("/repos/{owner}/{repo}/collaborators/{user}/permission", get(collaborator_permission))
        .route("/repos/{owner}/{repo}/branches", get(list_branches))
        .route("/repos/{owner}/{repo}/branches/{*rest}", get(branch_or_protection).put(put_protection).delete(delete_protection))
        .route("/repos/{owner}/{repo}/tags", get(list_tags))
}

fn created(repo: dto::Repository) -> Response {
    let loc = repo.url.clone();
    let mut r = (StatusCode::CREATED, Json(repo)).into_response();
    if let Ok(v) = HeaderValue::from_str(&loc) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

fn me(actor: &Actor) -> Result<UserId> {
    actor.user_id().ok_or_else(|| Error::forbidden("This needs a person's account, not a service credential."))
}

// ------------------------------------------------------------------------ repositories

async fn list_mine(api: Api, Query(q): Query<PageQuery>) -> Result<Response> {
    let all = api.core.accessible_all(&api.actor).await?;
    let (rows, more) = trim_page(all.into_iter().skip(q.skip() as usize).take(q.per_page() as usize + 1).collect(), q);
    let out = repositories(&api, &rows).await?;
    Ok(paged(&format!("{}/user/repos", api_url(api.base())), q, out, more))
}

async fn list_user(api: Api, Path(user): Path<String>, Query(q): Query<PageQuery>) -> Result<Response> {
    let u = user_by_name(&api, &user).await?;
    let all = api.core.do_list_for_owner(&api.actor, UserId(u.id)).await?;
    let skip = q.skip() as usize;
    let (rows, more) = trim_page(all.into_iter().skip(skip).take(q.per_page() as usize + 1).collect(), q);
    let out = repositories(&api, &rows).await?;
    Ok(paged(&format!("{}/users/{user}/repos", api_url(api.base())), q, out, more))
}

async fn list_org(api: Api, Path(org): Path<String>, q: Query<PageQuery>) -> Result<Response> {
    list_user(api, Path(org), q).await
}

/// The tier a request asks for, from `visibility` (which decides when present) or the legacy
/// `private` flag. `current` is the repository's tier now, or `None` when creating.
/// `Ok(None)` means leave it as it is.
async fn requested_tier(
    api: &Api,
    private: Option<bool>,
    visibility: Option<&str>,
    current: Option<Visibility>,
) -> Result<Option<Visibility>> {
    if let Some(s) = visibility {
        let v = Visibility::parse(s).ok_or_else(|| Error::invalid("Repository", "visibility", "use private, internal or public"))?;
        if (private == Some(false) && v != Visibility::Public) || (private == Some(true) && v == Visibility::Public) {
            return Err(Error::invalid("Repository", "visibility", "private: false does not match that visibility"));
        }
        if v == Visibility::Public {
            if current == Some(Visibility::Public) {
                return Ok(None);
            }
            if !api.core.public_allowed().await? {
                return Err(Error::forbidden(crate::tier::PUBLIC_OFF));
            }
            return Err(Error::forbidden(crate::tier::MAKE_PUBLIC_IN_BROWSER));
        }
        return Ok(if Some(v) == current { None } else { Some(v) });
    }
    if private == Some(true) {
        return Ok(if current == Some(Visibility::Private) { None } else { Some(Visibility::Private) });
    }
    if private == Some(false) {
        return Ok(match current {
            None | Some(Visibility::Private) => Some(Visibility::Internal),
            Some(Visibility::Internal | Visibility::Public) => None,
        });
    }
    Ok(None)
}

fn new_repo(owner: UserId, body: dto::CreateRepo, visibility: Visibility) -> NewRepo {
    NewRepo {
        owner_id: owner,
        name: body.name,
        description: body.description.unwrap_or_default(),
        visibility,
        default_branch: body.default_branch,
        auto_init: body.auto_init.unwrap_or(false),
    }
}

async fn create_mine(api: Api, ApiJson(body): ApiJson<dto::CreateRepo>) -> Result<Response> {
    let owner = me(&api.actor)?;
    let tier = requested_tier(&api, body.private, body.visibility.as_deref(), None).await?.unwrap_or(Visibility::Private);
    let repo = api.core.do_create_repo(&api.actor, new_repo(owner, body, tier), &api.client).await?;
    Ok(created(repository(&api, &repo).await?))
}

async fn create_org(api: Api, Path(org): Path<String>, ApiJson(body): ApiJson<dto::CreateRepo>) -> Result<Response> {
    let owner = user_by_name(&api, &org).await?;
    let tier = requested_tier(&api, body.private, body.visibility.as_deref(), None).await?.unwrap_or(Visibility::Private);
    let repo = api.core.do_create_repo(&api.actor, new_repo(UserId(owner.id), body, tier), &api.client).await?;
    Ok(created(repository(&api, &repo).await?))
}

async fn get_repo(api: Api, Path((owner, name)): Path<(String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    Ok(Json(repository(&api, &repo).await?).into_response())
}

async fn patch_repo(api: Api, Path((owner, name)): Path<(String, String)>, ApiJson(body): ApiJson<dto::UpdateRepo>) -> Result<Response> {
    let mut repo = api.repo(&owner, &name).await?;
    // Both settings are validated before anything changes.
    let tier = requested_tier(&api, body.private, body.visibility.as_deref(), Some(repo.visibility)).await?;
    let policy = match body.contribution_policy.as_deref() {
        Some(s) => Some(
            ContributionPolicy::parse(s).ok_or_else(|| Error::invalid("Repository", "contribution_policy", "use everyone or grants"))?,
        ),
        None => None,
    };
    if let Some(new_name) = body.name.as_deref()
        && new_name != repo.name
    {
        repo = api.core.do_rename(&api.actor, repo.id, new_name, &api.client).await?;
    }
    let update = RepoSettingsUpdate {
        description: body.description,
        visibility: tier,
        default_branch: body.default_branch,
        archived: body.archived,
        allow_merge_commit: body.allow_merge_commit,
        allow_squash_merge: body.allow_squash_merge,
        allow_rebase_merge: body.allow_rebase_merge,
        delete_branch_on_merge: body.delete_branch_on_merge,
        ..Default::default()
    };
    let changed = update.description.is_some()
        || update.visibility.is_some()
        || update.default_branch.is_some()
        || update.archived.is_some()
        || update.allow_merge_commit.is_some()
        || update.allow_squash_merge.is_some()
        || update.allow_rebase_merge.is_some()
        || update.delete_branch_on_merge.is_some();
    if changed {
        repo = api.core.do_update_settings(&api.actor, repo.id, update, &api.client).await?;
    }
    if let Some(policy) = policy {
        repo = api.core.do_set_contribution_policy(&api.actor, repo.id, policy, &api.client).await?;
    }
    Ok(Json(repository(&api, &repo).await?).into_response())
}

async fn delete_repo(api: Api, Path((owner, name)): Path<(String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    api.core.do_delete(&api.actor, repo.id, &api.client).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(serde::Deserialize)]
struct TransferBody {
    new_owner: String,
}

async fn transfer(api: Api, Path((owner, name)): Path<(String, String)>, ApiJson(body): ApiJson<TransferBody>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let target = user_by_name(&api, &body.new_owner).await?;
    let moved = api.core.do_transfer(&api.actor, repo.id, UserId(target.id), &api.client).await?;
    Ok((StatusCode::ACCEPTED, Json(repository(&api, &moved).await?)).into_response())
}

// ------------------------------------------------------------------------ topics

async fn get_topics(api: Api, Path((owner, name)): Path<(String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    Ok(Json(dto::Topics { names: repo.topics }).into_response())
}

async fn put_topics(api: Api, Path((owner, name)): Path<(String, String)>, ApiJson(body): ApiJson<dto::Topics>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let updated = api
        .core
        .do_update_settings(&api.actor, repo.id, RepoSettingsUpdate { topics: Some(body.names), ..Default::default() }, &api.client)
        .await?;
    Ok(Json(dto::Topics { names: updated.topics }).into_response())
}

// ------------------------------------------------------------------------ forks

async fn list_forks(api: Api, Path((owner, name)): Path<(String, String)>, Query(q): Query<PageQuery>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM repositories WHERE fork_parent_id = $1 ORDER BY created_at DESC, id DESC")
        .bind(repo.id.0)
        .fetch_all(&api.state.db)
        .await?;
    let mut visible = Vec::new();
    for (id,) in ids {
        if let Ok(r) = api.core.do_get(&api.actor, ch_core::RepoId(id)).await {
            visible.push(r);
        }
    }
    let (rows, more) = trim_page(visible.into_iter().skip(q.skip() as usize).take(q.per_page() as usize + 1).collect(), q);
    let out = repositories(&api, &rows).await?;
    Ok(paged(&format!("{}/repos/{owner}/{name}/forks", api_url(api.base())), q, out, more))
}

async fn create_fork(
    api: Api,
    Path((owner, name)): Path<(String, String)>,
    ApiJsonOpt(body): ApiJsonOpt<dto::ForkRequest>,
) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let req = body.unwrap_or(dto::ForkRequest { organization: None, name: None, private: None, visibility: None });
    let into = match &req.organization {
        Some(org) => UserId(user_by_name(&api, org).await?.id),
        None => me(&api.actor)?,
    };
    let tier = requested_tier(&api, req.private, req.visibility.as_deref(), None).await?.unwrap_or(Visibility::Private);
    let fork = api.core.do_fork_as(&api.actor, repo.id, into, req.name.as_deref(), tier, &api.client).await?;
    Ok((StatusCode::ACCEPTED, Json(repository(&api, &fork).await?)).into_response())
}

// ------------------------------------------------------------------------ collaborators

async fn list_collaborators(api: Api, Path((owner, name)): Path<(String, String)>, Query(q): Query<PageQuery>) -> Result<Response> {
    let repo = api.repo_with(&owner, &name, Permission::Write).await?;
    let mut people = vec![(repo.owner_id.0, repo.owner_name.clone(), Permission::Admin)];
    for c in api.core.do_collaborators(&api.actor, repo.id).await? {
        people.push((c.user_id.0, c.username, c.permission));
    }
    let (rows, more) = trim_page(people.into_iter().skip(q.skip() as usize).take(q.per_page() as usize + 1).collect(), q);
    let mut out = Vec::new();
    for (id, login, perm) in rows {
        let u = user_ref(&api, UserId(id)).await.unwrap_or(UserRef { id, login, kind: "user".into(), is_admin: false });
        out.push(dto::Collaborator {
            user: simple_user(api.base(), &u),
            permissions: permissions(perm),
            role_name: role_name(perm).into(),
        });
    }
    Ok(paged(&format!("{}/repos/{owner}/{name}/collaborators", api_url(api.base())), q, out, more))
}

fn role_name(p: Permission) -> &'static str {
    match p {
        Permission::None => "none",
        Permission::View => "view",
        Permission::Read => "read",
        Permission::Triage => "triage",
        Permission::Write => "write",
        Permission::Maintain => "maintain",
        Permission::Admin => "admin",
    }
}

async fn is_collaborator(api: Api, Path((owner, name, user)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo_with(&owner, &name, Permission::Write).await?;
    let target = user_by_name(&api, &user).await?;
    if target.id == repo.owner_id.0 || api.core.do_collaborators(&api.actor, repo.id).await?.iter().any(|c| c.user_id.0 == target.id) {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(Error::NotFound)
    }
}

async fn add_collaborator(
    api: Api,
    Path((owner, name, user)): Path<(String, String, String)>,
    ApiJsonOpt(body): ApiJsonOpt<dto::AddCollaborator>,
) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let target = user_by_name(&api, &user).await?;
    let word = body.map(|b| b.permission).unwrap_or_else(|| "push".into());
    let perm = Permission::parse(&word)
        .ok_or_else(|| Error::invalid("Collaborator", "permission", "use pull, triage, push, maintain or admin"))?;
    api.core.do_set_collaborator(&api.actor, repo.id, UserId(target.id), perm, &api.client).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_collaborator(api: Api, Path((owner, name, user)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let target = user_by_name(&api, &user).await?;
    api.core.do_remove_collaborator(&api.actor, repo.id, UserId(target.id), &api.client).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Serialize)]
struct PermissionBody {
    permission: &'static str,
    role_name: &'static str,
    user: SimpleUser,
}

async fn collaborator_permission(api: Api, Path((owner, name, user)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo_with(&owner, &name, Permission::Write).await?;
    let target = user_by_name(&api, &user).await?;
    let as_user = Actor::User { id: UserId(target.id), is_admin: false, has_2fa: false, via: ch_core::AuthVia::Session };
    let perm = api.state.authz.repo_permission(&as_user, repo.id).await?;
    // GitHub's legacy field has only four values.
    let legacy = match perm {
        Permission::Admin | Permission::Maintain => "admin",
        Permission::Write | Permission::Triage => "write",
        Permission::Read | Permission::View => "read",
        Permission::None => "none",
    };
    Ok(Json(PermissionBody { permission: legacy, role_name: role_name(perm), user: simple_user(api.base(), &target) }).into_response())
}

// ------------------------------------------------------------------------ branches and tags

async fn list_branches(api: Api, Path((owner, name)): Path<(String, String)>, Query(q): Query<PageQuery>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let protections = api.core.protections_of(repo.id).await?;
    let mut refs = api.core.read_refs(repo.id, Some("refs/heads/")).await?;
    refs.sort_by(|a, b| a.name.cmp(&b.name));
    let rows = refs.into_iter().skip(q.skip() as usize).take(q.per_page() as usize + 1).collect();
    let (rows, more) = trim_page(rows, q);
    let out: Vec<dto::Branch> = rows.into_iter().map(|r| branch_dto(&api, &repo, &protections, &r)).collect();
    Ok(paged(&format!("{}/repos/{owner}/{name}/branches", api_url(api.base())), q, out, more))
}

fn branch_dto(api: &Api, repo: &Repo, protections: &[BranchProtection], r: &GitRef) -> dto::Branch {
    let name = r.name.strip_prefix("refs/heads/").unwrap_or(&r.name).to_string();
    let protected = !crate::protect::matching(protections, &name).is_empty();
    dto::Branch {
        commit: dto::BranchCommit {
            sha: r.sha.clone(),
            url: format!("{}/repos/{}/{}/commits/{}", api_url(api.base()), repo.owner_name, repo.name, r.sha),
        },
        name,
        protected,
    }
}

fn protection_dto(api: &Api, repo: &Repo, branch: &str, p: &BranchProtection) -> dto::BranchProtection {
    dto::BranchProtection {
        url: format!("{}/repos/{}/{}/branches/{branch}/protection", api_url(api.base()), repo.owner_name, repo.name),
        required_status_checks: if p.required_checks.is_empty() && !p.strict {
            None
        } else {
            Some(dto::RequiredStatusChecks { strict: p.strict, contexts: p.required_checks.clone(), checks: vec![] })
        },
        required_pull_request_reviews: if p.required_approvals == 0 && !p.dismiss_stale_reviews && !p.require_code_owner_review {
            None
        } else {
            Some(dto::RequiredReviews {
                required_approving_review_count: p.required_approvals,
                dismiss_stale_reviews: p.dismiss_stale_reviews,
                require_code_owner_reviews: p.require_code_owner_review,
            })
        },
        // Nobody is exempt, administrators included.
        enforce_admins: true,
        required_conversation_resolution: p.require_resolved_threads,
        allow_force_pushes: !p.block_force_push,
        allow_deletions: !p.block_deletion,
        allow_direct_push: p.allow_direct_push,
    }
}

async fn branch_or_protection(api: Api, Path((owner, name, rest)): Path<(String, String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let protections = api.core.protections_of(repo.id).await?;
    if let Some(branch) = rest.strip_suffix("/protection") {
        let p = protections.iter().find(|p| p.pattern == branch).ok_or(Error::NotFound)?;
        return Ok(Json(protection_dto(&api, &repo, branch, p)).into_response());
    }
    let full = format!("refs/heads/{rest}");
    let r = api.core.read_refs(repo.id, Some(&full)).await?.into_iter().find(|r| r.name == full).ok_or(Error::NotFound)?;
    Ok(Json(branch_dto(&api, &repo, &protections, &r)).into_response())
}

async fn put_protection(
    api: Api,
    Path((owner, name, rest)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<dto::UpdateBranchProtection>,
) -> Result<Response> {
    let branch = rest.strip_suffix("/protection").ok_or(Error::NotFound)?;
    let repo = api.repo(&owner, &name).await?;
    let p = protection_from(branch, &body)?;
    api.core.do_set_protection(&api.actor, repo.id, p.clone(), &api.client).await?;
    Ok(Json(protection_dto(&api, &repo, branch, &p)).into_response())
}

/// The rules a PUT asks for, from Codehaven's body or GitHub's (ch-yci). What GitHub can set
/// and Codehaven cannot enforce is refused by name, never dropped: push restrictions, linear
/// history, blocking branch creation and locking the branch.
fn protection_from(branch: &str, body: &dto::UpdateBranchProtection) -> Result<BranchProtection> {
    let unsupported = |field: &str, what: &str| Error::invalid("BranchProtection", field, format!("{what} is not supported here."));
    if body.restrictions.as_ref().is_some_and(|r| !(r.users.is_empty() && r.teams.is_empty() && r.apps.is_empty())) {
        return Err(unsupported("restrictions", "Limiting who may push to a branch"));
    }
    if body.required_linear_history == Some(true) {
        return Err(unsupported("required_linear_history", "Requiring a linear history"));
    }
    if body.block_creations == Some(true) {
        return Err(unsupported("block_creations", "Blocking branch creation"));
    }
    if body.lock_branch == Some(true) {
        return Err(unsupported("lock_branch", "Locking a branch"));
    }
    let reviews = body.required_pull_request_reviews.as_ref();
    let checks = body.required_status_checks.as_ref();
    let mut required_checks: Vec<String> = checks.map(|c| c.contexts.clone()).unwrap_or_default();
    for c in checks.map(|c| c.checks.as_slice()).unwrap_or_default() {
        if !required_checks.contains(&c.context) {
            required_checks.push(c.context.clone());
        }
    }
    Ok(BranchProtection {
        pattern: branch.to_string(),
        required_approvals: reviews.map(|r| r.required_approving_review_count).unwrap_or(0),
        dismiss_stale_reviews: reviews.is_some_and(|r| r.dismiss_stale_reviews),
        require_code_owner_review: reviews.is_some_and(|r| r.require_code_owner_reviews),
        required_checks,
        strict: checks.is_some_and(|c| c.strict),
        require_resolved_threads: body.required_conversation_resolution.unwrap_or(false),
        block_force_push: !body.allow_force_pushes.unwrap_or(false),
        block_deletion: !body.allow_deletions.unwrap_or(false),
        // GitHub: with reviews required, changes go through a pull request.
        allow_direct_push: body.allow_direct_push.unwrap_or(reviews.is_none()),
    })
}

async fn delete_protection(api: Api, Path((owner, name, rest)): Path<(String, String, String)>) -> Result<Response> {
    let branch = rest.strip_suffix("/protection").ok_or(Error::NotFound)?;
    let repo = api.repo(&owner, &name).await?;
    api.core.do_remove_protection(&api.actor, repo.id, branch, &api.client).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_tags(api: Api, Path((owner, name)): Path<(String, String)>, Query(q): Query<PageQuery>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    let mut refs = api.core.read_refs(repo.id, Some("refs/tags/")).await?;
    refs.sort_by(|a, b| b.name.cmp(&a.name));
    let (rows, more) = trim_page(refs.into_iter().skip(q.skip() as usize).take(q.per_page() as usize + 1).collect(), q);
    let base = api_url(api.base());
    let out: Vec<dto::Tag> = rows
        .into_iter()
        .map(|r| {
            let tag = r.name.strip_prefix("refs/tags/").unwrap_or(&r.name).to_string();
            dto::Tag {
                commit: dto::BranchCommit { sha: r.sha.clone(), url: format!("{base}/repos/{owner}/{name}/commits/{}", r.sha) },
                zipball_url: format!("{base}/repos/{owner}/{name}/zipball/{tag}"),
                tarball_url: format!("{base}/repos/{owner}/{name}/tarball/{tag}"),
                name: tag,
            }
        })
        .collect();
    Ok(paged(&format!("{base}/repos/{owner}/{name}/tags"), q, out, more))
}
