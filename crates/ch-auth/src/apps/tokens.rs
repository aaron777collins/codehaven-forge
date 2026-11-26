//! The token endpoint, revocation and the lookup of a live access token.
//!
//! Only SHA-256 hashes are stored and no token ever reaches a log or the audit trail. Both
//! exchanges run under a row lock, so two requests with the same code or refresh token have one
//! winner. A one-time code used twice, or a rotated refresh token used again after the grace
//! window, ends the whole grant: the second user may be a thief.

use super::*;
use ch_core::audit::{Outcome, action};
use ch_core::{Actor, AppGrant};
use ch_db::tokens as dbt;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use subtle::ConstantTimeEq;

type OResult<T> = std::result::Result<T, OAuthError>;

fn err(code: &'static str, d: &str) -> OAuthError {
    OAuthError::new(code, d)
}
fn grant_err() -> OAuthError {
    err("invalid_grant", "The code or token is not valid.")
}
fn server_err(e: impl std::fmt::Display) -> OAuthError {
    tracing::error!(error = %e, "oauth token endpoint failed");
    OAuthError::new("server_error", "Something went wrong. Try again.")
}

#[derive(sqlx::FromRow)]
struct GrantRow {
    id: i64,
    user_id: i64,
    client_id: String,
    resource: String,
    scopes: Vec<String>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

fn is_verifier(s: &str) -> bool {
    (43..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

pub(super) fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(dbt::sha256(verifier.as_bytes()))
}

/// A label-only actor for the audit trail of the token endpoint.
fn label_actor(user: i64, grant: i64) -> Actor {
    Actor::App {
        user_id: UserId(user),
        grant_id: GrantId(grant),
        grant: AppGrant { read: RepoSet::none(), write: RepoSet::none(), workflows: RepoSet::none() },
    }
}

async fn person_ok(conn: &mut sqlx::PgConnection, user: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT suspended_at IS NULL AND ch_has_second_factor(id) AND kind = 'user' FROM users WHERE id = $1")
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or(false))
}

async fn end_grant(conn: &mut sqlx::PgConnection, grant: i64, reason: &str) -> Result<()> {
    sqlx::query("UPDATE oauth_grants SET revoked_at = now(), revoked_reason = $2 WHERE id = $1 AND revoked_at IS NULL")
        .bind(grant)
        .bind(reason)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE oauth_tokens SET revoked_at = now() WHERE grant_id = $1 AND revoked_at IS NULL")
        .bind(grant)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Inserts a fresh access and refresh token for a grant. Neither outlives the grant.
async fn issue(conn: &mut sqlx::PgConnection, g: &GrantRow) -> Result<TokenResponse> {
    let now = Utc::now();
    let access = dbt::new_token(ACCESS_PREFIX, 43);
    let refresh = dbt::new_token(REFRESH_PREFIX, 43);
    let a_exp = (now + Duration::seconds(ACCESS_TTL_SECS)).min(g.expires_at);
    let r_exp = (now + Duration::days(REFRESH_TTL_DAYS)).min(g.expires_at);
    for (kind, tok, exp) in [("access", &access, a_exp), ("refresh", &refresh, r_exp)] {
        sqlx::query("INSERT INTO oauth_tokens (grant_id, kind, token_hash, expires_at) VALUES ($1, $2, $3, $4)")
            .bind(g.id)
            .bind(kind)
            .bind(dbt::hash_token(tok))
            .bind(exp)
            .execute(&mut *conn)
            .await?;
    }
    Ok(TokenResponse {
        access_token: access,
        refresh_token: refresh,
        expires_in: (a_exp - now).num_seconds().max(1),
        scope: g.scopes.join(" "),
    })
}

pub(super) async fn token(svc: &AppsService, req: TokenRequest, client: &ClientInfo) -> OResult<TokenResponse> {
    match req.grant_type.as_deref() {
        Some("authorization_code") => exchange_code(svc, req, client).await,
        Some("refresh_token") => refresh(svc, req, client).await,
        Some(_) => Err(err("unsupported_grant_type", "Only authorization_code and refresh_token are supported.")),
        None => Err(err("invalid_request", "grant_type is required.")),
    }
}

async fn exchange_code(svc: &AppsService, req: TokenRequest, client: &ClientInfo) -> OResult<TokenResponse> {
    let (Some(code), Some(redirect), Some(verifier), Some(client_id)) =
        (req.code.as_deref(), req.redirect_uri.as_deref(), req.code_verifier.as_deref(), req.client_id.as_deref())
    else {
        return Err(err("invalid_request", "code, redirect_uri, code_verifier and client_id are required."));
    };
    let Some(resource) = req.resource.as_deref() else { return Err(err("invalid_target", "resource is required.")) };
    if !resource_matches(&svc.core.config.mcp_resource(), resource) {
        return Err(err("invalid_target", "That is not this server."));
    }
    if !is_verifier(verifier) {
        return Err(err("invalid_request", "code_verifier is not valid."));
    }
    if !code.starts_with(CODE_PREFIX) || code.len() > 64 {
        return Err(grant_err());
    }
    let mut tx = svc.core.db.begin().await.map_err(server_err)?;
    #[derive(sqlx::FromRow)]
    struct CodeRow {
        id: i64,
        redirect_uri: String,
        code_challenge: String,
        live: bool,
        used: bool,
        grant_id: i64,
    }
    let row: Option<(CodeRow, GrantRow)> = {
        let c: Option<CodeRow> = sqlx::query_as("SELECT id, redirect_uri, code_challenge, (expires_at > now()) AS live, (used_at IS NOT NULL) AS used, grant_id FROM oauth_codes WHERE code_hash = $1 FOR UPDATE")
            .bind(dbt::hash_token(code))
            .fetch_optional(&mut *tx)
            .await
            .map_err(server_err)?;
        match c {
            None => None,
            Some(c) => {
                let g: GrantRow = sqlx::query_as(
                    "SELECT id, user_id, client_id, resource, scopes, expires_at, revoked_at FROM oauth_grants WHERE id = $1 FOR UPDATE",
                )
                .bind(c.grant_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(server_err)?;
                Some((c, g))
            }
        }
    };
    let Some((c, g)) = row else { return Err(grant_err()) };
    let same_client = g.client_id == client_id && c.redirect_uri == redirect;
    if c.used {
        // The same code twice. When it is the same client and redirect, the grant it made may be
        // in a thief's hands, so it ends. The change is committed before the refusal goes out.
        if same_client && g.revoked_at.is_none() {
            end_grant(&mut tx, g.id, "one-time code used twice").await.map_err(server_err)?;
            tx.commit().await.map_err(server_err)?;
            svc.core
                .audit_standalone(
                    &label_actor(g.user_id, g.id),
                    client,
                    action::APP_CODE_REUSE,
                    Outcome::Failure,
                    Some(("oauth_grant", g.id)),
                    json!({"reason": "one-time code used twice"}),
                )
                .await;
        }
        return Err(grant_err());
    }
    if !c.live || g.revoked_at.is_some() || g.expires_at <= Utc::now() || !same_client {
        return Err(grant_err());
    }
    // The resource must be the one the grant was made for.
    if !resource_matches(&g.resource, resource) {
        return Err(err("invalid_target", "That is not the server this grant was made for."));
    }
    let expected = pkce_challenge(verifier);
    if !bool::from(expected.as_bytes().ct_eq(c.code_challenge.as_bytes())) {
        return Err(grant_err());
    }
    if !person_ok(&mut tx, g.user_id).await.map_err(server_err)? {
        return Err(grant_err());
    }
    sqlx::query("UPDATE oauth_codes SET used_at = now() WHERE id = $1").bind(c.id).execute(&mut *tx).await.map_err(server_err)?;
    sqlx::query("UPDATE oauth_grants SET activated_at = COALESCE(activated_at, now()) WHERE id = $1")
        .bind(g.id)
        .execute(&mut *tx)
        .await
        .map_err(server_err)?;
    let out = issue(&mut tx, &g).await.map_err(server_err)?;
    tx.commit().await.map_err(server_err)?;
    Ok(out)
}

async fn refresh(svc: &AppsService, req: TokenRequest, client: &ClientInfo) -> OResult<TokenResponse> {
    let (Some(rt), Some(client_id)) = (req.refresh_token.as_deref(), req.client_id.as_deref()) else {
        return Err(err("invalid_request", "refresh_token and client_id are required."));
    };
    if !rt.starts_with(REFRESH_PREFIX) || rt.len() > 64 {
        return Err(grant_err());
    }
    let mut tx = svc.core.db.begin().await.map_err(server_err)?;
    #[derive(sqlx::FromRow)]
    struct TokRow {
        id: i64,
        grant_id: i64,
        live: bool,
        used_at: Option<DateTime<Utc>>,
    }
    let t: Option<TokRow> = sqlx::query_as(
        "SELECT id, grant_id, (expires_at > now() AND revoked_at IS NULL) AS live, used_at FROM oauth_tokens WHERE token_hash = $1 AND kind = 'refresh' FOR UPDATE",
    )
    .bind(dbt::hash_token(rt))
    .fetch_optional(&mut *tx)
    .await
    .map_err(server_err)?;
    let Some(t) = t else { return Err(grant_err()) };
    let g: GrantRow = sqlx::query_as(
        "SELECT id, user_id, client_id, resource, scopes, expires_at, revoked_at FROM oauth_grants WHERE id = $1 FOR UPDATE",
    )
    .bind(t.grant_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(server_err)?;
    if g.client_id != client_id {
        return Err(grant_err());
    }
    if g.revoked_at.is_some() || g.expires_at <= Utc::now() {
        return Err(grant_err());
    }
    if let Some(used) = t.used_at {
        // A rotated token again. Inside the grace window it is a second process of the same
        // app racing the first: refused, grant kept. Later it may be a thief.
        if (Utc::now() - used).num_seconds() > REFRESH_GRACE_SECS {
            end_grant(&mut tx, g.id, "refresh token used twice").await.map_err(server_err)?;
            tx.commit().await.map_err(server_err)?;
            svc.core
                .audit_standalone(
                    &label_actor(g.user_id, g.id),
                    client,
                    action::APP_REFRESH_REUSE,
                    Outcome::Failure,
                    Some(("oauth_grant", g.id)),
                    json!({"reason": "refresh token used twice"}),
                )
                .await;
        }
        return Err(grant_err());
    }
    if !t.live {
        return Err(grant_err());
    }
    if let Some(r) = req.resource.as_deref()
        && !resource_matches(&g.resource, r)
    {
        return Err(err("invalid_target", "That is not the server this grant was made for."));
    }
    // A scope in the request can only narrow. The answer always says what the grant holds.
    if let Some(s) = req.scope.as_deref()
        && s.split(' ').filter(|x| !x.is_empty()).any(|x| x != SCOPE_OFFLINE && !g.scopes.iter().any(|have| have == x))
    {
        return Err(err("invalid_request", "A refresh cannot add permissions. Connect again to change them."));
    }
    if !person_ok(&mut tx, g.user_id).await.map_err(server_err)? {
        return Err(grant_err());
    }
    sqlx::query("UPDATE oauth_tokens SET used_at = now() WHERE id = $1").bind(t.id).execute(&mut *tx).await.map_err(server_err)?;
    let out = issue(&mut tx, &g).await.map_err(server_err)?;
    tx.commit().await.map_err(server_err)?;
    Ok(out)
}

pub(super) async fn revoke_token(svc: &AppsService, token: &str, client_id: Option<&str>, client: &ClientInfo) -> Result<()> {
    if token.len() > 64 || !(token.starts_with(ACCESS_PREFIX) || token.starts_with(REFRESH_PREFIX)) {
        return Ok(());
    }
    let Some(client_id) = client_id else { return Ok(()) };
    let row: Option<(i64, i64, String)> = sqlx::query_as(
        "SELECT g.id, g.user_id, g.client_id FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id WHERE t.token_hash = $1",
    )
    .bind(dbt::hash_token(token))
    .fetch_optional(&svc.core.db)
    .await?;
    let Some((gid, uid, gclient)) = row else { return Ok(()) };
    if gclient != client_id {
        return Ok(());
    }
    let mut tx = svc.core.db.begin().await?;
    let live = sqlx::query_scalar::<_, i64>("SELECT id FROM oauth_grants WHERE id = $1 AND revoked_at IS NULL FOR UPDATE")
        .bind(gid)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if live {
        end_grant(&mut tx, gid, "revoked by the app").await?;
        svc.core
            .audit_tx(
                &mut tx,
                &label_actor(uid, gid),
                client,
                action::APP_REVOKED,
                Some(("oauth_grant", gid)),
                json!({"reason": "revoked by the app"}),
            )
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Finds the live grant behind an access token, or `None`.
pub(super) async fn app_actor(core: &Core, token: &str) -> Result<Option<Actor>> {
    if !looks_like_access_token(token) {
        return Ok(None);
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        token_id: i64,
        grant_id: i64,
        user_id: i64,
        read_all: bool,
        write_all: bool,
        workflows_all: bool,
        last_used_old: bool,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT t.id AS token_id, g.id AS grant_id, g.user_id, g.read_all, g.write_all, g.workflows_all,
                (g.last_used_at IS NULL OR g.last_used_at < now() - interval '1 minute') AS last_used_old
         FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id JOIN users u ON u.id = g.user_id
         WHERE t.token_hash = $1 AND t.kind = 'access' AND t.revoked_at IS NULL AND t.expires_at > now()
           AND g.revoked_at IS NULL AND g.expires_at > now() AND g.activated_at IS NOT NULL AND g.resource = $2
           AND u.suspended_at IS NULL AND u.kind = 'user' AND ch_has_second_factor(u.id)",
    )
    .bind(dbt::hash_token(token))
    .bind(core.config.mcp_resource())
    .fetch_optional(&core.db)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let _ = r.token_id;
    let repos: Vec<(i64, bool, bool)> =
        sqlx::query_as("SELECT repo_id, can_write, can_workflows FROM oauth_grant_repos WHERE grant_id = $1")
            .bind(r.grant_id)
            .fetch_all(&core.db)
            .await?;
    if r.last_used_old {
        sqlx::query("UPDATE oauth_grants SET last_used_at = now() WHERE id = $1").bind(r.grant_id).execute(&core.db).await?;
    }
    Ok(Some(Actor::App {
        user_id: UserId(r.user_id),
        grant_id: GrantId(r.grant_id),
        grant: super::build_grant(r.read_all, r.write_all, r.workflows_all, &repos),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_vector() {
        // RFC 7636 appendix B.
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_shape() {
        assert!(is_verifier(&"a".repeat(43)));
        assert!(is_verifier(&"a".repeat(128)));
        assert!(!is_verifier(&"a".repeat(42)));
        assert!(!is_verifier(&"a".repeat(129)));
        assert!(!is_verifier(&format!("{}!", "a".repeat(43))));
        assert!(!is_verifier(&format!("{} ", "a".repeat(43))));
    }
}
