//! The authorize request, the consent decision and the person's list of connected apps.

use super::cimd::{self, ClientDoc};
use super::*;
use ch_core::audit::action;
use ch_core::{Actor, Permission};
use ch_db::tokens as dbt;
use chrono::Utc;
use serde_json::json;
use sqlx::Row;

fn refused(msg: &str) -> Result<AuthorizeOutcome> {
    Ok(AuthorizeOutcome::Refused(msg.to_string()))
}

fn plain_ascii(s: &str, max: usize) -> bool {
    s.len() <= max && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

fn is_s256_challenge(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(super) async fn begin_authorize(
    svc: &AppsService,
    actor: &Actor,
    session_cookie: &str,
    p: AuthorizeParams,
    _client: &ClientInfo,
) -> Result<AuthorizeOutcome> {
    // 1. A signed-in person with two-factor on. Nothing is fetched or stored before this.
    let (user, has_2fa) = require_session_user(actor)?;
    if !has_2fa {
        return Ok(AuthorizeOutcome::NeedsTwoFactor);
    }
    // 2. Oversized parameters. (Duplicates are refused by the page that parses the query.)
    if p.client_id.len() > 512
        || p.redirect_uri.len() > 512
        || p.code_challenge.len() > 128
        || p.state.as_deref().is_some_and(|s| s.len() > 2048)
    {
        return refused("The request is too large.");
    }
    // 3. The client id.
    let trusted = &svc.core.config.mcp.trusted_client_hosts;
    let id_url = match cimd::check_client_id(&p.client_id, trusted) {
        Ok(u) => u,
        Err(m) => return refused(&m),
    };
    // 4. The client document, from the cache or the network. Only a real fetch counts against
    //    the person's ten a minute.
    let doc: ClientDoc = match svc.cache.get(&p.client_id) {
        Some(d) => d,
        None => {
            if svc.fetch_limit.hit(&user.0.to_string()).is_err() {
                return refused("Too many connection attempts. Wait a minute and try again.");
            }
            match svc.fetcher.fetch(&p.client_id).await {
                Ok(f) => {
                    svc.cache.put(&f);
                    f.doc
                }
                Err(m) => return refused(&m),
            }
        }
    };
    // 5. The document must describe this exact client id.
    if doc.client_id != p.client_id {
        return refused("The client document names a different client id.");
    }
    // 6. The redirect, by exact match, with the loopback port exception.
    if !cimd::redirect_allowed(&id_url, &doc.redirect_uris, &p.redirect_uri) {
        return refused("That redirect address is not one this app registered, so you were not sent back to it.");
    }
    // 7 and 8. Everything about the request itself. These also render a page, never a redirect.
    if p.response_type != "code" {
        return refused("Only response_type=code is supported.");
    }
    if p.code_challenge_method != "S256" || !is_s256_challenge(&p.code_challenge) {
        return refused("This server needs PKCE with the S256 method.");
    }
    if let Some(s) = &p.state
        && !plain_ascii(s, 2048)
    {
        return refused("The state value is not plain text.");
    }
    let mut requested = Vec::new();
    for s in p.scope.as_deref().unwrap_or("").split(' ').filter(|s| !s.is_empty()) {
        if !matches!(s, SCOPE_READ | SCOPE_WRITE | SCOPE_WORKFLOWS | SCOPE_OFFLINE) {
            return refused("The app asked for a permission this server does not know.");
        }
        if !requested.iter().any(|r| r == s) {
            requested.push(s.to_string());
        }
    }
    let Some(resource) = p.resource.as_deref() else {
        return refused("The app did not say which server it wants to use (the resource parameter).");
    };
    if !resource_matches(&svc.core.config.mcp_resource(), resource) {
        return refused("The app asked for a different server than this one.");
    }
    // 9. Store the request, bound to this person and this browser session.
    let id = dbt::random_base62(43);
    let mut tx = svc.core.db.begin().await?;
    // At most 20 live requests: forget the oldest so a person who opened many tabs is not stuck.
    sqlx::query(
        "DELETE FROM oauth_requests WHERE id IN (
           SELECT id FROM oauth_requests WHERE user_id = $1 AND used_at IS NULL AND expires_at > now() ORDER BY id DESC OFFSET $2)",
    )
    .bind(user.0)
    .bind(MAX_LIVE_REQUESTS - 1)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO oauth_requests (request_hash, session_hash, user_id, client_id, client_name, redirect_uri, state, scopes, resource, code_challenge, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now() + make_interval(secs => $11))",
    )
    .bind(dbt::hash_token(&id))
    .bind(crate::core::hash_cookie(session_cookie))
    .bind(user.0)
    .bind(&p.client_id)
    .bind(&doc.client_name)
    .bind(&p.redirect_uri)
    .bind(&p.state)
    .bind(&requested)
    .bind(svc.core.config.mcp_resource())
    .bind(&p.code_challenge)
    .bind(REQUEST_TTL_SECS as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let username: String =
        sqlx::query_scalar("SELECT username::text FROM users WHERE id = $1").bind(user.0).fetch_one(&svc.core.db).await?;
    Ok(AuthorizeOutcome::Consent(ConsentView {
        request_id: id,
        client_id: p.client_id.clone(),
        client_host: id_url.host_str().unwrap_or("").to_string(),
        client_name: doc.client_name,
        redirect_host: cimd::redirect_host(&p.redirect_uri),
        loopback: cimd::is_loopback_redirect(&p.redirect_uri),
        requested_scopes: requested,
        username,
    }))
}

#[derive(sqlx::FromRow)]
struct ReqRow {
    id: i64,
    client_id: String,
    client_name: String,
    redirect_uri: String,
    state: Option<String>,
    resource: String,
    code_challenge: String,
    live: bool,
}

/// Locks and consumes a request that belongs to this person and this browser session.
async fn take_request(tx: &mut sqlx::PgConnection, user: UserId, session_cookie: &str, request_id: &str) -> Result<Option<ReqRow>> {
    if request_id.is_empty() || request_id.len() > 64 {
        return Ok(None);
    }
    let row: Option<ReqRow> = sqlx::query_as(
        "SELECT id, client_id, client_name, redirect_uri, state, resource, code_challenge, (used_at IS NULL AND expires_at > now()) AS live
         FROM oauth_requests WHERE request_hash = $1 AND user_id = $2 AND session_hash = $3 FOR UPDATE",
    )
    .bind(dbt::hash_token(request_id))
    .bind(user.0)
    .bind(crate::core::hash_cookie(session_cookie))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if !row.live {
        return Ok(None);
    }
    sqlx::query("UPDATE oauth_requests SET used_at = now() WHERE id = $1").bind(row.id).execute(&mut *tx).await?;
    Ok(Some(row))
}

fn return_url(redirect: &str, origin: &str, state: &Option<String>, pairs: &[(&str, &str)]) -> Result<String> {
    let mut u = url::Url::parse(redirect).map_err(|_| Error::invalid("Authorization", "redirect_uri", "not a valid URL"))?;
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in pairs {
            q.append_pair(k, v);
        }
        if let Some(s) = state {
            q.append_pair("state", s);
        }
        q.append_pair("iss", origin);
    }
    Ok(u.to_string())
}

fn gone() -> Error {
    Error::invalid("Authorization", "request", "This request expired or was already used. Start again from the app.")
}

pub(super) async fn deny(
    svc: &AppsService,
    actor: &Actor,
    session_cookie: &str,
    request_id: &str,
    client: &ClientInfo,
) -> Result<ReturnTarget> {
    let (user, _) = require_session_user(actor)?;
    let mut tx = svc.core.db.begin().await?;
    let Some(req) = take_request(&mut tx, user, session_cookie, request_id).await? else { return Err(gone()) };
    svc.core
        .audit_tx(&mut tx, actor, client, action::APP_DENIED, Some(("user", user.0)), json!({"client_host": host_of(&req.client_id)}))
        .await?;
    tx.commit().await?;
    let url = return_url(&req.redirect_uri, svc.core.external_url(), &req.state, &[("error", "access_denied")])?;
    Ok(ReturnTarget { host: cimd::redirect_host(&req.redirect_uri), url })
}

fn host_of(client_id: &str) -> String {
    url::Url::parse(client_id).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

async fn readable(svc: &AppsService, user: UserId, repos: &[RepoId], needed: Permission) -> Result<bool> {
    for r in repos {
        // Site admin power never counts here: a connected app never has it.
        let p = svc.authz.user_permission_for_app(user, *r).await?;
        if p < needed {
            return Ok(false);
        }
    }
    Ok(true)
}

fn dedup(mut v: Vec<RepoId>) -> Vec<RepoId> {
    v.sort();
    v.dedup();
    v
}

pub(super) async fn approve(
    svc: &AppsService,
    actor: &Actor,
    session_cookie: &str,
    c: ConsentChoices,
    client: &ClientInfo,
) -> Result<ReturnTarget> {
    let (user, _) = require_session_user(actor)?;
    let bad = |m: &str| Error::invalid("Authorization", "choices", m);
    if !matches!(c.expires_days, 90 | 180) {
        return Err(bad("Choose 90 or 180 days."));
    }
    // Validate the choices against what the person can do right now, before consuming anything.
    let read_sel = match &c.read {
        RepoChoice::None => return Err(bad("Choose which repositories the app may read.")),
        RepoChoice::All => None,
        RepoChoice::Selected(v) if v.is_empty() => return Err(bad("Choose at least one repository to read, or all of them.")),
        RepoChoice::Selected(v) => Some(dedup(v.clone())),
    };
    let write_sel = match &c.write {
        RepoChoice::None => None,
        RepoChoice::All => {
            if read_sel.is_some() {
                return Err(bad("Write to all repositories needs read on all of them."));
            }
            Some(None)
        }
        RepoChoice::Selected(v) => Some(Some(dedup(v.clone()))),
    };
    let wf_sel = match &c.workflows {
        RepoChoice::None => None,
        RepoChoice::All => {
            if !matches!(c.write, RepoChoice::All) {
                return Err(bad("Workflows on all repositories needs write on all of them."));
            }
            Some(None)
        }
        RepoChoice::Selected(v) => {
            if write_sel.is_none() {
                return Err(bad("Workflows need write access first."));
            }
            Some(Some(dedup(v.clone())))
        }
    };
    if let (Some(rs), Some(Some(ws))) = (&read_sel, &write_sel)
        && ws.iter().any(|w| !rs.contains(w))
    {
        return Err(bad("A repository the app may write to must also be one it may read."));
    }
    if let (Some(Some(ws)), Some(Some(wf))) = (&write_sel, &wf_sel)
        && wf.iter().any(|f| !ws.contains(f))
    {
        return Err(bad("A repository whose workflows the app may change must also be one it may write to."));
    }
    if let Some(rs) = &read_sel
        && !readable(svc, user, rs, Permission::Read).await?
    {
        return Err(bad("You cannot read one of those repositories."));
    }
    for sel in [&write_sel, &wf_sel].into_iter().flatten().flatten() {
        if !readable(svc, user, sel, Permission::Write).await? {
            return Err(bad("You cannot write to one of those repositories."));
        }
    }

    let mut tx = svc.core.db.begin().await?;
    let Some(req) = take_request(&mut tx, user, session_cookie, &c.request_id).await? else { return Err(gone()) };
    // Rechecked inside the transaction: the person must still be allowed to do this.
    let ok: bool = sqlx::query_scalar(
        "SELECT suspended_at IS NULL AND totp_enabled_at IS NOT NULL AND kind = 'user' FROM users WHERE id = $1 FOR SHARE",
    )
    .bind(user.0)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !ok {
        return Err(Error::forbidden("Turn on two-factor sign-in, then connect again."));
    }
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_grants WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()")
            .bind(user.0)
            .fetch_one(&mut *tx)
            .await?;
    if live >= MAX_LIVE_GRANTS {
        return Err(Error::conflict("You already have 20 connected apps. Remove one in Settings, Connected apps, then connect again."));
    }
    let mut scopes = vec![SCOPE_READ.to_string()];
    if write_sel.is_some() {
        scopes.push(SCOPE_WRITE.to_string());
    }
    if wf_sel.is_some() {
        scopes.push(SCOPE_WORKFLOWS.to_string());
    }
    let read_all = read_sel.is_none();
    let write_all = matches!(write_sel, Some(None));
    let workflows_all = matches!(wf_sel, Some(None));
    let grant_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_grants (user_id, client_id, client_host, client_name, redirect_uri, resource, scopes, read_all, write_all, workflows_all, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now() + make_interval(days => $11)) RETURNING id",
    )
    .bind(user.0)
    .bind(&req.client_id)
    .bind(host_of(&req.client_id))
    .bind(&req.client_name)
    .bind(&req.redirect_uri)
    .bind(&req.resource)
    .bind(&scopes)
    .bind(read_all)
    .bind(write_all)
    .bind(workflows_all)
    .bind(c.expires_days as i32)
    .fetch_one(&mut *tx)
    .await?;
    // One row per repository the app was given something on.
    let mut rows: std::collections::BTreeMap<i64, (bool, bool)> = std::collections::BTreeMap::new();
    for r in read_sel.iter().flatten() {
        rows.entry(r.0).or_default();
    }
    for r in write_sel.iter().flatten().flatten() {
        rows.entry(r.0).or_default().0 = true;
    }
    for r in wf_sel.iter().flatten().flatten() {
        rows.entry(r.0).or_default().1 = true;
    }
    for (repo, (w, f)) in &rows {
        sqlx::query("INSERT INTO oauth_grant_repos (grant_id, repo_id, can_write, can_workflows) VALUES ($1, $2, $3, $4)")
            .bind(grant_id)
            .bind(repo)
            .bind(w)
            .bind(f)
            .execute(&mut *tx)
            .await?;
    }
    let code = dbt::new_token(CODE_PREFIX, 43);
    sqlx::query("INSERT INTO oauth_codes (code_hash, grant_id, redirect_uri, code_challenge, expires_at) VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))")
        .bind(dbt::hash_token(&code))
        .bind(grant_id)
        .bind(&req.redirect_uri)
        .bind(&req.code_challenge)
        .bind(CODE_TTL_SECS as f64)
        .execute(&mut *tx)
        .await?;
    svc.core
        .audit_tx(
            &mut tx,
            actor,
            client,
            action::APP_AUTHORIZED,
            Some(("oauth_grant", grant_id)),
            json!({"client_host": host_of(&req.client_id), "scopes": scopes, "read_all": read_all, "write_all": write_all, "workflows_all": workflows_all, "repos": rows.len(), "expires_days": c.expires_days}),
        )
        .await?;
    tx.commit().await?;
    let url = return_url(&req.redirect_uri, svc.core.external_url(), &req.state, &[("code", &code)])?;
    Ok(ReturnTarget { host: cimd::redirect_host(&req.redirect_uri), url })
}

#[derive(sqlx::FromRow)]
struct GRow {
    id: i64,
    client_id: String,
    client_host: String,
    client_name: String,
    redirect_uri: String,
    created_at: chrono::DateTime<Utc>,
    last_used_at: Option<chrono::DateTime<Utc>>,
    expires_at: chrono::DateTime<Utc>,
    revoked_at: Option<chrono::DateTime<Utc>>,
    scopes: Vec<String>,
    read_all: bool,
    write_all: bool,
    workflows_all: bool,
}

async fn view_of(svc: &AppsService, r: GRow) -> Result<GrantView> {
    let repos = sqlx::query(
        "SELECT g.repo_id, g.can_write, g.can_workflows, o.username::text || '/' || r.name AS full_name
         FROM oauth_grant_repos g JOIN repositories r ON r.id = g.repo_id JOIN users o ON o.id = r.owner_id
         WHERE g.grant_id = $1 ORDER BY full_name",
    )
    .bind(r.id)
    .fetch_all(&svc.core.db)
    .await?;
    let who: (String, String, Option<String>) = sqlx::query_as(
        "SELECT u.username::text, u.display_name, (SELECT e.email::text FROM user_emails e WHERE e.user_id = u.id ORDER BY e.is_primary DESC, e.id LIMIT 1)
         FROM oauth_grants g JOIN users u ON u.id = g.user_id WHERE g.id = $1",
    )
    .bind(r.id)
    .fetch_one(&svc.core.db)
    .await?;
    Ok(GrantView {
        id: GrantId(r.id),
        display_name: if who.1.is_empty() { who.0.clone() } else { who.1 },
        username: who.0,
        email: who.2,
        client_id: r.client_id,
        client_host: r.client_host,
        client_name: r.client_name,
        redirect_host: cimd::redirect_host(&r.redirect_uri),
        created_at: r.created_at,
        last_used_at: r.last_used_at,
        expires_at: r.expires_at,
        revoked_at: r.revoked_at,
        scopes: r.scopes,
        read_all: r.read_all,
        write_all: r.write_all,
        workflows_all: r.workflows_all,
        repos: repos
            .iter()
            .map(|x| GrantRepoView {
                repo_id: RepoId(x.get("repo_id")),
                full_name: x.get("full_name"),
                can_write: x.get("can_write"),
                can_workflows: x.get("can_workflows"),
            })
            .collect(),
    })
}

const GCOLS: &str = "id, client_id, client_host, client_name, redirect_uri, created_at, last_used_at, expires_at, revoked_at, scopes, read_all, write_all, workflows_all";

pub(super) async fn list_grants(svc: &AppsService, actor: &Actor) -> Result<Vec<GrantView>> {
    // `user_id()`, not `author_id()`: an app cannot list or end anyone's connections.
    let Some(user) = actor.user_id() else { return Err(Error::unauthorized()) };
    let rows: Vec<GRow> = sqlx::query_as(&format!(
        "SELECT {GCOLS} FROM oauth_grants WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now() AND activated_at IS NOT NULL ORDER BY created_at DESC"
    ))
    .bind(user.0)
    .fetch_all(&svc.core.db)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        out.push(view_of(svc, r).await?);
    }
    Ok(out)
}

pub(super) async fn revoke_grant(svc: &AppsService, actor: &Actor, id: GrantId, client: &ClientInfo) -> Result<()> {
    actor.deny_app("end a connection")?;
    let Some(user) = actor.user_id() else { return Err(Error::unauthorized()) };
    let mut tx = svc.core.db.begin().await?;
    let n = sqlx::query("UPDATE oauth_grants SET revoked_at = now(), revoked_reason = 'revoked by the person' WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL")
        .bind(id.0)
        .bind(user.0)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(Error::NotFound);
    }
    sqlx::query("UPDATE oauth_tokens SET revoked_at = now() WHERE grant_id = $1 AND revoked_at IS NULL")
        .bind(id.0)
        .execute(&mut *tx)
        .await?;
    svc.core
        .audit_tx(&mut tx, actor, client, action::APP_REVOKED, Some(("oauth_grant", id.0)), json!({"reason": "revoked by the person"}))
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn grant_of(svc: &AppsService, actor: &Actor) -> Result<GrantView> {
    let Actor::App { grant_id, user_id, .. } = actor else { return Err(Error::NotFound) };
    let row: GRow = sqlx::query_as(&format!("SELECT {GCOLS} FROM oauth_grants WHERE id = $1 AND user_id = $2"))
        .bind(grant_id.0)
        .bind(user_id.0)
        .fetch_optional(&svc.core.db)
        .await?
        .ok_or(Error::NotFound)?;
    view_of(svc, row).await
}
