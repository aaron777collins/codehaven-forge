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

fn new_repo(owner: UserId, body: dto::CreateRepo) -> NewRepo {
    NewRepo {
        owner_id: owner,
        name: body.name,
        description: body.description.unwrap_or_default(),
        // Private unless the caller says otherwise: a new repository is not shown to everyone by accident.
        visibility: if body.private == Some(false) { Visibility::Public } else { Visibility::Private },
        default_branch: body.default_branch,
        auto_init: body.auto_init.unwrap_or(false),
    }
}

async fn create_mine(api: Api, ApiJson(body): ApiJson<dto::CreateRepo>) -> Result<Response> {
    let owner = me(&api.actor)?;
    let repo = api.core.do_create_repo(&api.actor, new_repo(owner, body), &api.client).await?;
    Ok(created(repository(&api, &repo).await?))
}

async fn create_org(api: Api, Path(org): Path<String>, ApiJson(body): ApiJson<dto::CreateRepo>) -> Result<Response> {
    let owner = user_by_name(&api, &org).await?;
    let repo = api.core.do_create_repo(&api.actor, new_repo(UserId(owner.id), body), &api.client).await?;
    Ok(created(repository(&api, &repo).await?))
}

async fn get_repo(api: Api, Path((owner, name)): Path<(String, String)>) -> Result<Response> {
    let repo = api.repo(&owner, &name).await?;
    Ok(Json(repository(&api, &repo).await?).into_response())
}

async fn patch_repo(api: Api, Path((owner, name)): Path<(String, String)>, ApiJson(body): ApiJson<dto::UpdateRepo>) -> Result<Response> {
    let mut repo = api.repo(&owner, &name).await?;
    if let Some(new_name) = body.name.as_deref()
        && new_name != repo.name
    {
        repo = api.core.do_rename(&api.actor, repo.id, new_name, &api.client).await?;
    }
    let update = RepoSettingsUpdate {
        description: body.description,
        visibility: body.private.map(|p| if p { Visibility::Private } else { Visibility::Public }),
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
    let req = body.unwrap_or(dto::ForkRequest { organization: None, name: None });
    let into = match &req.organization {
        Some(org) => UserId(user_by_name(&api, org).await?.id),
        None => me(&api.actor)?,
    };
    let fork = api.core.do_fork(&api.actor, repo.id, into, req.name.as_deref(), &api.client).await?;
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
        Permission::Read => "read",
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
            Some(dto::RequiredStatusChecks { strict: p.strict, contexts: p.required_checks.clone() })
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
    let p = BranchProtection {
        pattern: branch.to_string(),
        required_approvals: body.required_pull_request_reviews.as_ref().map(|r| r.required_approving_review_count).unwrap_or(0),
        dismiss_stale_reviews: body.required_pull_request_reviews.as_ref().map(|r| r.dismiss_stale_reviews).unwrap_or(false),
        require_code_owner_review: body.required_pull_request_reviews.as_ref().map(|r| r.require_code_owner_reviews).unwrap_or(false),
        required_checks: body.required_status_checks.as_ref().map(|c| c.contexts.clone()).unwrap_or_default(),
        strict: body.required_status_checks.as_ref().map(|c| c.strict).unwrap_or(false),
        require_resolved_threads: body.required_conversation_resolution,
        block_force_push: !body.allow_force_pushes,
        block_deletion: !body.allow_deletions,
        allow_direct_push: body.allow_direct_push,
    };
    api.core.do_set_protection(&api.actor, repo.id, p.clone(), &api.client).await?;
    Ok(Json(protection_dto(&api, &repo, branch, &p)).into_response())
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
