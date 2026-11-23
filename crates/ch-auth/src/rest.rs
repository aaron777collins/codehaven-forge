//! REST endpoints for accounts, orgs and the site admin (ARCHITECTURE.md section 11).
//! Paths and field names follow GitHub's v3 where they mean the same thing.
//!
//! Every route here is private: the gate rejects a signed-out request before a handler runs.
//! The site admin routes need a signed-in browser session with two-factor, so a token can
//! never reach them.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use ch_api_types::common::SimpleUser;
use ch_api_types::users as dto;
use ch_core::access::{Client, CurrentActor};
use ch_core::audit::AuditFilter;
use ch_core::error::{Error, FieldError, Result};
use ch_core::identity::{Profile, UserListFilter};
use ch_core::ids::*;
use ch_core::model::{NewInvite, User, UserKind};
use ch_core::services::OrgRole;
use ch_core::state::AppState;
use ch_core::{Actor, Permission};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

type Res<T> = Result<T>;

/// Mounted under `/api/v3`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/user", get(get_me).patch(patch_me))
        .route("/users/{login}", get(get_user))
        .route("/users/{login}/keys", get(user_keys))
        .route("/users/{login}/suspended", put(suspend).delete(unsuspend))
        .route("/users/{login}/site_admin", put(grant_admin).delete(revoke_admin))
        .route("/user/keys", get(list_keys).post(create_key))
        .route("/user/keys/{id}", get(get_key).delete(delete_key))
        .route("/user/emails", get(list_emails).post(add_emails).delete(remove_emails))
        .route("/user/tokens", get(list_tokens).post(create_token))
        .route("/user/tokens/{id}", delete(delete_token))
        .route("/user/orgs", get(my_orgs).post(create_org))
        .route("/orgs/{org}", get(get_org).patch(patch_org).delete(delete_org))
        .route("/orgs/{org}/members", get(org_members))
        .route("/orgs/{org}/members/{login}", delete(remove_org_member))
        .route("/orgs/{org}/memberships/{login}", put(set_org_member))
        .route("/orgs/{org}/teams", get(list_teams).post(create_team))
        .route("/orgs/{org}/teams/{slug}", get(get_team).patch(update_team).delete(delete_team))
        .route("/orgs/{org}/teams/{slug}/members", get(team_members))
        .route("/orgs/{org}/teams/{slug}/memberships/{login}", put(add_team_member).delete(remove_team_member))
        .route("/orgs/{org}/teams/{slug}/repos/{owner}/{repo}", put(set_team_repo).delete(remove_team_repo))
        .route("/admin/users", get(admin_users))
        .route("/admin/users/{login}/unlock", post(admin_unlock))
        .route("/admin/users/{login}/ci", put(ci_on).delete(ci_off))
        .route("/admin/invites", get(list_invites).post(create_invite))
        .route("/admin/invites/{id}", delete(revoke_invite))
        .route("/admin/audit", get(audit_log))
}

fn base(s: &AppState) -> String {
    s.config.server.external_url.trim_end_matches('/').to_string()
}

fn kind_name(k: UserKind) -> &'static str {
    match k {
        UserKind::User => "User",
        UserKind::Org => "Organization",
        UserKind::Bot => "Bot",
    }
}

async fn profile_dto(s: &AppState, u: &User, me: bool) -> Res<dto::User> {
    let (bio, updated_at): (String, DateTime<Utc>) =
        sqlx::query_as("SELECT description, updated_at FROM users WHERE id = $1").bind(u.id.0).fetch_one(&s.db).await?;
    let b = base(s);
    Ok(dto::User {
        login: u.username.clone(),
        id: u.id.0,
        avatar_url: ch_api_types::avatar_url(&b, &u.username),
        url: format!("{b}/api/v3/users/{}", u.username),
        html_url: format!("{b}/{}", u.username),
        kind: kind_name(u.kind).into(),
        site_admin: u.is_admin,
        name: Some(u.display_name.clone()).filter(|n| !n.is_empty()),
        email: if me { u.email.clone() } else { None },
        bio: Some(bio).filter(|n| !n.is_empty()),
        company: None,
        location: None,
        created_at: u.created_at,
        updated_at,
        two_factor_authentication: if me { Some(u.has_2fa) } else { None },
        suspended_at: None,
    })
}

fn require_user(actor: &Actor) -> Res<UserId> {
    crate::core::acting_user(actor, None)
}

fn login_not_found(e: Error) -> Error {
    e
}

// ---------------------------------------------------------------- profile

async fn get_me(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<dto::User>> {
    let me = s.accounts.get_user(require_user(&actor)?).await?;
    Ok(Json(profile_dto(&s, &me, true).await?))
}

async fn patch_me(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(body): Json<dto::UpdateUser>,
) -> Res<Json<dto::User>> {
    let id = require_user(&actor)?;
    let me = s.directory.update_profile(&actor, id, Profile { display_name: body.name, description: body.bio }, &client).await?;
    Ok(Json(profile_dto(&s, &me, true).await?))
}

/// Signed out, an account exists only while it owns a public repository (the gate checked, and
/// this checks again). It shows the name and display name and nothing else.
async fn public_owner(s: &AppState, actor: &Actor, login: &str) -> Res<Option<User>> {
    if !actor.is_anonymous() {
        return Ok(None);
    }
    let public: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users u JOIN repositories r ON r.owner_id = u.id WHERE u.username = $1::citext AND r.access_tier = 'public') \
         AND (SELECT allow_public_repos FROM visibility_settings)",
    )
    .bind(login)
    .fetch_one(&s.db)
    .await?;
    if public != Some(true) {
        return Err(Error::unauthorized());
    }
    Ok(Some(s.accounts.get_by_username(&Actor::System, login).await.map_err(|_| Error::unauthorized())?))
}

/// The epoch, where a signed-out answer must carry a date it may not show.
fn hidden_date() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default()
}

async fn get_user(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(login): Path<String>) -> Res<Json<dto::User>> {
    if let Some(u) = public_owner(&s, &actor, &login).await? {
        let b = base(&s);
        return Ok(Json(dto::User {
            login: u.username.clone(),
            id: u.id.0,
            avatar_url: ch_api_types::avatar_url(&b, &u.username),
            url: format!("{b}/api/v3/users/{}", u.username),
            html_url: format!("{b}/{}", u.username),
            kind: kind_name(u.kind).into(),
            site_admin: false,
            name: Some(u.display_name.clone()).filter(|n| !n.is_empty()),
            email: None,
            bio: None,
            company: None,
            location: None,
            created_at: hidden_date(),
            updated_at: hidden_date(),
            two_factor_authentication: None,
            suspended_at: None,
        }));
    }
    let u = s.accounts.get_by_username(&actor, &login).await.map_err(login_not_found)?;
    let me = actor.user_id() == Some(u.id);
    Ok(Json(profile_dto(&s, &u, me).await?))
}

// ---------------------------------------------------------------- keys

async fn key_dto(s: &AppState, k: &ch_core::model::SshKey) -> Res<dto::PublicKey> {
    let key: String = sqlx::query_scalar("SELECT public_key FROM ssh_keys WHERE id = $1").bind(k.id.0).fetch_one(&s.db).await?;
    Ok(dto::PublicKey {
        id: k.id.0,
        key,
        title: k.title.clone(),
        created_at: k.created_at,
        last_used_at: k.last_used_at,
        read_only: false,
        verified: true,
        url: format!("{}/api/v3/user/keys/{}", base(s), k.id.0),
    })
}

async fn user_keys(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(login): Path<String>,
) -> Res<Json<Vec<dto::PublicKey>>> {
    let u = s.accounts.get_by_username(&actor, &login).await?;
    let keys = s.accounts.list_ssh_keys(&actor, u.id).await?;
    let mut out = Vec::new();
    for k in &keys {
        out.push(key_dto(&s, k).await?);
    }
    Ok(Json(out))
}

async fn list_keys(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<Vec<dto::PublicKey>>> {
    let id = require_user(&actor)?;
    let keys = s.accounts.list_ssh_keys(&actor, id).await?;
    let mut out = Vec::new();
    for k in &keys {
        out.push(key_dto(&s, k).await?);
    }
    Ok(Json(out))
}

async fn get_key(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(id): Path<i64>) -> Res<Json<dto::PublicKey>> {
    let me = require_user(&actor)?;
    let keys = s.accounts.list_ssh_keys(&actor, me).await?;
    let k = keys.iter().find(|k| k.id.0 == id).ok_or(Error::NotFound)?;
    Ok(Json(key_dto(&s, k).await?))
}

async fn create_key(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(body): Json<dto::CreateKey>,
) -> Res<(StatusCode, Json<dto::PublicKey>)> {
    let me = require_user(&actor)?;
    let k = s.accounts.add_ssh_key(&actor, me, &body.title, &body.key, &client).await?;
    Ok((StatusCode::CREATED, Json(key_dto(&s, &k).await?)))
}

async fn delete_key(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(id): Path<i64>,
) -> Res<StatusCode> {
    require_user(&actor)?;
    s.accounts.delete_ssh_key(&actor, SshKeyId(id), &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- emails

async fn list_emails(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<Vec<dto::Email>>> {
    let me = require_user(&actor)?;
    Ok(Json(
        s.directory
            .emails(&actor, me)
            .await?
            .into_iter()
            .map(|e| dto::Email { email: e.email, primary: e.primary, verified: e.verified, visibility: None })
            .collect(),
    ))
}

async fn add_emails(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(body): Json<dto::AddEmails>,
) -> Res<(StatusCode, Json<Vec<dto::Email>>)> {
    let me = require_user(&actor)?;
    let mut out = Vec::new();
    for e in &body.emails {
        let a = s.directory.add_email(&actor, me, e, &client).await?;
        out.push(dto::Email { email: a.email, primary: a.primary, verified: a.verified, visibility: None });
    }
    Ok((StatusCode::CREATED, Json(out)))
}

async fn remove_emails(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(body): Json<dto::AddEmails>,
) -> Res<StatusCode> {
    let me = require_user(&actor)?;
    for e in &body.emails {
        s.directory.remove_email(&actor, me, e, &client).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- tokens

fn token_dto(t: &ch_core::model::AccessToken, secret: Option<String>) -> dto::PersonalToken {
    dto::PersonalToken {
        id: t.id.0,
        name: t.name.clone(),
        scopes: t.scopes.clone(),
        token_last_eight: t.prefix.clone(),
        expires_at: t.expires_at,
        created_at: t.created_at,
        last_used_at: t.last_used_at,
        token: secret,
    }
}

async fn list_tokens(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<Vec<dto::PersonalToken>>> {
    let me = require_user(&actor)?;
    Ok(Json(s.accounts.list_tokens(&actor, me).await?.iter().map(|t| token_dto(t, None)).collect()))
}

async fn create_token(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(body): Json<dto::CreateToken>,
) -> Res<(StatusCode, Json<dto::PersonalToken>)> {
    let me = require_user(&actor)?;
    let (t, secret) = s.accounts.create_token(&actor, me, &body.name, body.scopes, body.expires_in_days.unwrap_or(0), &client).await?;
    Ok((StatusCode::CREATED, Json(token_dto(&t, Some(secret)))))
}

async fn delete_token(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(id): Path<i64>,
) -> Res<StatusCode> {
    require_user(&actor)?;
    s.accounts.delete_token(&actor, TokenId(id), &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- orgs

fn org_dto(s: &AppState, o: &ch_core::identity::Org) -> dto::Organization {
    let b = base(s);
    dto::Organization {
        login: o.name.clone(),
        id: o.id.0,
        url: format!("{b}/api/v3/orgs/{}", o.name),
        html_url: format!("{b}/orgs/{}", o.name),
        avatar_url: ch_api_types::avatar_url(&b, &o.name),
        description: Some(o.description.clone()).filter(|d| !d.is_empty()),
        name: Some(o.display_name.clone()).filter(|d| !d.is_empty()),
        created_at: o.created_at,
    }
}

/// GitHub's word for a permission level: `read` is `pull` and `write` is `push`.
fn github_permission(p: Permission) -> &'static str {
    match p {
        Permission::Read => "pull",
        Permission::Write => "push",
        other => other.as_str(),
    }
}

fn team_dto(s: &AppState, org: &str, t: &ch_core::identity::Team) -> dto::Team {
    let b = base(s);
    dto::Team {
        id: t.id.0,
        name: t.name.clone(),
        slug: t.name.to_lowercase(),
        description: Some(t.description.clone()).filter(|d| !d.is_empty()),
        permission: github_permission(t.permission).into(),
        url: format!("{b}/api/v3/orgs/{org}/teams/{}", t.name.to_lowercase()),
        html_url: format!("{b}/orgs/{org}/teams/{}", t.id.0),
        members_count: t.member_count,
        repos_count: t.repo_count,
        organization: None,
    }
}

async fn my_orgs(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<Vec<dto::Organization>>> {
    let me = crate::core::acting_user(&actor, Some("read:org"))?;
    Ok(Json(s.orgs.list_for_user(&actor, me).await?.iter().map(|o| org_dto(&s, o)).collect()))
}

async fn create_org(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(body): Json<dto::CreateOrg>,
) -> Res<(StatusCode, Json<dto::Organization>)> {
    let o = s.orgs.create(&actor, &body.name, body.description.as_deref().unwrap_or(""), &client).await?;
    Ok((StatusCode::CREATED, Json(org_dto(&s, &o))))
}

async fn get_org(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(org): Path<String>) -> Res<Json<dto::Organization>> {
    if public_owner(&s, &actor, &org).await?.is_some() {
        let o = s.orgs.get(&Actor::System, &org).await.map_err(|_| Error::unauthorized())?;
        let mut d = org_dto(&s, &o);
        d.created_at = hidden_date();
        return Ok(Json(d));
    }
    Ok(Json(org_dto(&s, &s.orgs.get(&actor, &org).await?)))
}

#[derive(Deserialize)]
struct PatchOrg {
    name: Option<String>,
    description: Option<String>,
    default_repository_permission: Option<String>,
}

async fn patch_org(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(org): Path<String>,
    Json(body): Json<PatchOrg>,
) -> Res<Json<dto::Organization>> {
    let o = s.orgs.get(&actor, &org).await?;
    let base_perm = match body.default_repository_permission.as_deref() {
        Some(p) => Some(Permission::parse(p).ok_or_else(|| {
            Error::invalid("Organization", "default_repository_permission", "Use none, read, triage, write, maintain or admin.")
        })?),
        None => None,
    };
    Ok(Json(org_dto(&s, &s.orgs.update(&actor, o.id, body.name, body.description, base_perm, &client).await?)))
}

async fn delete_org(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(org): Path<String>,
) -> Res<StatusCode> {
    let o = s.orgs.get(&actor, &org).await?;
    s.orgs.delete(&actor, o.id, &client).await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Serialize)]
struct MemberDto {
    #[serde(flatten)]
    user: SimpleUser,
    role: &'static str,
}

fn member_dto(s: &AppState, m: &ch_core::identity::OrgMember) -> MemberDto {
    let b = base(s);
    MemberDto {
        user: SimpleUser {
            login: m.username.clone(),
            id: m.user_id.0,
            avatar_url: ch_api_types::avatar_url(&b, &m.username),
            url: format!("{b}/api/v3/users/{}", m.username),
            html_url: format!("{b}/{}", m.username),
            kind: "User".into(),
            site_admin: false,
        },
        role: if m.role == OrgRole::Owner { "admin" } else { "member" },
    }
}

async fn org_members(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(org): Path<String>) -> Res<Json<Vec<MemberDto>>> {
    let o = s.orgs.get(&actor, &org).await?;
    Ok(Json(s.orgs.members(&actor, o.id).await?.iter().map(|m| member_dto(&s, m)).collect()))
}

#[derive(Deserialize, Default)]
struct RoleBody {
    role: Option<String>,
}

async fn set_org_member(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, login)): Path<(String, String)>,
    body: Option<Json<RoleBody>>,
) -> Res<Json<serde_json::Value>> {
    let o = s.orgs.get(&actor, &org).await?;
    let u = s.accounts.get_by_username(&actor, &login).await?;
    let role = match body.and_then(|b| b.0.role).as_deref() {
        None | Some("member") => OrgRole::Member,
        Some("admin") | Some("owner") => OrgRole::Owner,
        Some(_) => return Err(Error::invalid("Membership", "role", "Use member or admin.")),
    };
    s.orgs.set_member(&actor, o.id, u.id, role, &client).await?;
    Ok(Json(serde_json::json!({"state": "active", "role": if role == OrgRole::Owner { "admin" } else { "member" }})))
}

async fn remove_org_member(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, login)): Path<(String, String)>,
) -> Res<StatusCode> {
    let o = s.orgs.get(&actor, &org).await?;
    let u = s.accounts.get_by_username(&actor, &login).await?;
    s.orgs.remove_member(&actor, o.id, u.id, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_teams(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(org): Path<String>) -> Res<Json<Vec<dto::Team>>> {
    let o = s.orgs.get(&actor, &org).await?;
    Ok(Json(s.orgs.teams(&actor, o.id).await?.iter().map(|t| team_dto(&s, &o.name, t)).collect()))
}

async fn create_team(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(org): Path<String>,
    Json(body): Json<dto::CreateTeam>,
) -> Res<(StatusCode, Json<dto::Team>)> {
    let o = s.orgs.get(&actor, &org).await?;
    let perm = match body.permission.as_deref() {
        None => Permission::Read,
        Some(p) => Permission::parse(p).filter(|p| *p != Permission::None).ok_or_else(|| {
            Error::Validation(vec![FieldError::custom("Team", "permission", "Use pull, triage, push, maintain or admin.")])
        })?,
    };
    let t = s.orgs.create_team(&actor, o.id, &body.name, body.description.as_deref().unwrap_or(""), perm, &client).await?;
    Ok((StatusCode::CREATED, Json(team_dto(&s, &o.name, &t))))
}

async fn get_team(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((org, slug)): Path<(String, String)>,
) -> Res<Json<dto::Team>> {
    let o = s.orgs.get(&actor, &org).await?;
    Ok(Json(team_dto(&s, &o.name, &s.orgs.get_team(&actor, o.id, &slug).await?)))
}

async fn update_team(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, slug)): Path<(String, String)>,
    Json(body): Json<dto::UpdateTeam>,
) -> Res<Json<dto::Team>> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    if body.name.as_deref().is_some_and(|n| n != t.name) {
        return Err(Error::Validation(vec![FieldError::custom(
            "Team",
            "name",
            "Renaming a team is not supported. Create a new team instead.",
        )]));
    }
    if body.description.as_deref().is_some_and(|d| d != t.description) {
        return Err(Error::Validation(vec![FieldError::custom("Team", "description", "Changing a team's description is not supported.")]));
    }
    if let Some(p) = body.permission.as_deref() {
        let perm = Permission::parse(p).filter(|p| *p != Permission::None).ok_or_else(|| {
            Error::Validation(vec![FieldError::custom("Team", "permission", "Use pull, triage, push, maintain or admin.")])
        })?;
        s.orgs.set_team_permission(&actor, t.id, perm, &client).await?;
    }
    Ok(Json(team_dto(&s, &o.name, &s.orgs.get_team(&actor, o.id, &slug).await?)))
}

async fn delete_team(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, slug)): Path<(String, String)>,
) -> Res<StatusCode> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    s.orgs.delete_team(&actor, t.id, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn team_members(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((org, slug)): Path<(String, String)>,
) -> Res<Json<Vec<MemberDto>>> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    Ok(Json(s.orgs.team_members(&actor, t.id).await?.iter().map(|m| member_dto(&s, m)).collect()))
}

async fn add_team_member(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, slug, login)): Path<(String, String, String)>,
) -> Res<Json<serde_json::Value>> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    let u = s.accounts.get_by_username(&actor, &login).await?;
    s.orgs.add_team_member(&actor, t.id, u.id, &client).await?;
    Ok(Json(serde_json::json!({"state": "active", "role": "member"})))
}

async fn remove_team_member(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, slug, login)): Path<(String, String, String)>,
) -> Res<StatusCode> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    let u = s.accounts.get_by_username(&actor, &login).await?;
    s.orgs.remove_team_member(&actor, t.id, u.id, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct PermBody {
    permission: Option<String>,
}

async fn repo_id(s: &AppState, owner: &str, repo: &str) -> Res<RepoId> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT r.id FROM repositories r JOIN users u ON u.id = r.owner_id WHERE u.username = $1::citext AND r.name = $2::citext",
    )
    .bind(owner)
    .bind(repo)
    .fetch_optional(&s.db)
    .await?;
    id.map(RepoId).ok_or(Error::NotFound)
}

async fn set_team_repo(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, slug, owner, repo)): Path<(String, String, String, String)>,
    body: Option<Json<PermBody>>,
) -> Res<StatusCode> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    let rid = repo_id(&s, &owner, &repo).await?;
    let perm = match body.and_then(|b| b.0.permission) {
        Some(p) => Permission::parse(&p).filter(|p| *p != Permission::None).ok_or_else(|| {
            Error::Validation(vec![FieldError::custom("Team", "permission", "Use pull, triage, push, maintain or admin.")])
        })?,
        // No body means the team's own permission, the GitHub behaviour.
        None => t.permission,
    };
    s.orgs.set_team_repo(&actor, t.id, rid, perm, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_team_repo(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, slug, owner, repo)): Path<(String, String, String, String)>,
) -> Res<StatusCode> {
    let o = s.orgs.get(&actor, &org).await?;
    let t = s.orgs.get_team(&actor, o.id, &slug).await?;
    let rid = repo_id(&s, &owner, &repo).await?;
    s.orgs.remove_team_repo(&actor, t.id, rid, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- site admin

#[derive(Deserialize)]
struct UsersQuery {
    q: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

async fn admin_users(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<UsersQuery>,
) -> Res<Json<Vec<dto::User>>> {
    let users = s
        .directory
        .list_users(&actor, UserListFilter { query: q.q, page: q.page.unwrap_or(1), per_page: q.per_page.unwrap_or(30) })
        .await?;
    let mut out = Vec::new();
    for u in &users {
        let mut d = profile_dto(&s, u, false).await?;
        d.email = u.email.clone();
        d.two_factor_authentication = Some(u.has_2fa);
        // Suspending touches `updated_at`, so it dates the suspension.
        d.suspended_at = if u.suspended { Some(d.updated_at) } else { None };
        out.push(d);
    }
    Ok(Json(out))
}

async fn target(s: &AppState, actor: &Actor, login: &str) -> Res<User> {
    crate::core::require_site_admin(actor)?;
    s.accounts.get_by_username(actor, login).await
}

/// The id whose `can_run_ci` the admin is changing: a person, or an organization by name.
async fn ci_target(s: &AppState, actor: &Actor, login: &str) -> Res<UserId> {
    crate::core::require_site_admin(actor)?;
    match s.accounts.get_by_username(actor, login).await {
        Ok(u) => Ok(u.id),
        Err(Error::NotFound) => Ok(UserId(s.orgs.get(actor, login).await?.id.0)),
        Err(e) => Err(e),
    }
}

async fn suspend(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let u = target(&s, &actor, &login).await?;
    s.accounts.suspend(&actor, u.id, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unsuspend(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let u = target(&s, &actor, &login).await?;
    s.directory.unsuspend(&actor, u.id, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn grant_admin(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let u = target(&s, &actor, &login).await?;
    s.directory.set_admin(&actor, u.id, true, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_admin(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let u = target(&s, &actor, &login).await?;
    s.directory.set_admin(&actor, u.id, false, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_unlock(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let u = target(&s, &actor, &login).await?;
    s.accounts.unlock(&actor, u.id, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ci_on(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let id = ci_target(&s, &actor, &login).await?;
    s.accounts.set_can_run_ci(&actor, id, true, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ci_off(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(login): Path<String>,
) -> Res<StatusCode> {
    let id = ci_target(&s, &actor, &login).await?;
    s.accounts.set_can_run_ci(&actor, id, false, &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct InviteDto {
    id: i64,
    status: String,
    email: Option<String>,
    grants_admin: bool,
    grants_ci: bool,
    org_id: Option<i64>,
    team_id: Option<i64>,
    note: Option<String>,
    created_by: Option<i64>,
    used_by: Option<i64>,
    expires_at: DateTime<Utc>,
    used_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    /// Only in the response to creating one. Shown once.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

fn invite_dto(i: &ch_core::model::Invite, url: Option<String>) -> InviteDto {
    let status = format!("{:?}", i.status(Utc::now())).to_lowercase();
    InviteDto {
        id: i.id.0,
        status,
        email: i.email.clone(),
        grants_admin: i.grants_admin,
        grants_ci: i.grants_ci,
        org_id: i.org_id.map(|o| o.0),
        team_id: i.team_id.map(|t| t.0),
        note: i.note.clone(),
        created_by: i.created_by.map(|u| u.0),
        used_by: i.used_by.map(|u| u.0),
        expires_at: i.expires_at,
        used_at: i.used_at,
        created_at: i.created_at,
        url,
    }
}

async fn list_invites(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<Vec<InviteDto>>> {
    Ok(Json(s.invites.list(&actor).await?.iter().map(|i| invite_dto(i, None)).collect()))
}

#[derive(Deserialize)]
struct CreateInviteBody {
    email: Option<String>,
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    can_run_ci: bool,
    org_id: Option<i64>,
    team_id: Option<i64>,
    note: Option<String>,
    expires_in_hours: Option<u32>,
}

async fn create_invite(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Json(b): Json<CreateInviteBody>,
) -> Res<(StatusCode, Json<InviteDto>)> {
    let created = s
        .invites
        .create(
            &actor,
            NewInvite {
                email: b.email,
                grants_admin: b.admin,
                grants_ci: b.can_run_ci,
                org_id: b.org_id.map(OrgId),
                team_id: b.team_id.map(TeamId),
                note: b.note,
                expires_in_hours: b.expires_in_hours,
            },
            &client,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(invite_dto(&created.invite, Some(created.url)))))
}

async fn revoke_invite(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(id): Path<i64>,
) -> Res<StatusCode> {
    s.invites.revoke(&actor, InviteId(id), &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AuditQuery {
    action: Option<String>,
    actor_id: Option<i64>,
    ip: Option<std::net::IpAddr>,
    before_id: Option<i64>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct AuditDto {
    id: i64,
    at: DateTime<Utc>,
    actor: String,
    actor_id: Option<i64>,
    ip: Option<String>,
    user_agent: Option<String>,
    action: String,
    target_type: Option<String>,
    target_id: Option<String>,
    outcome: &'static str,
    details: serde_json::Value,
}

async fn audit_log(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Query(q): Query<AuditQuery>) -> Res<Json<Vec<AuditDto>>> {
    let rows = s
        .audit
        .query(
            &actor,
            AuditFilter {
                actor_id: q.actor_id.map(UserId),
                action: q.action,
                client_ip: q.ip,
                since: None,
                limit: q.limit,
                before_id: q.before_id,
            },
        )
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AuditDto {
                id: r.id.0,
                at: r.at,
                actor: r.event.actor_label,
                actor_id: r.event.actor_id,
                ip: r.event.client_ip.map(|i| i.to_string()),
                user_agent: r.event.user_agent,
                action: r.event.action,
                target_type: r.event.target_type,
                target_id: r.event.target_id,
                outcome: r.event.outcome.as_str(),
                details: r.event.details,
            })
            .collect(),
    ))
}
