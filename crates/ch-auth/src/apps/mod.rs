//! Connected apps: OAuth 2.1 sign-in for MCP clients such as Claude.
//!
//! - Clients name themselves with a Client ID Metadata Document (`cimd`). There is no dynamic
//!   registration and nothing signed out can create a row.
//! - A signed-in person with two-factor on sees a consent page and chooses exactly which
//!   repositories and powers to give (`consent`).
//! - The token endpoint trades the one-time code, then rotating refresh tokens, for short-lived
//!   access tokens (`tokens`). Only SHA-256 hashes are stored. A token never appears in a log or
//!   in the audit trail.
//! - Every request to `/mcp` loads its grant fresh (`app_actor`), so revoking, suspending the
//!   person, turning their two-factor off or changing their password ends access at once.

pub mod cimd;
mod consent;
pub mod http;
mod tokens;

use crate::core::Core;
use crate::ratelimit::Limiter;
use async_trait::async_trait;
use ch_core::apps::*;
use ch_core::error::{Error, Result};
use ch_core::ids::*;
use ch_core::model::ClientInfo;
use ch_core::{Actor, AppGrant, RepoSet};
use cimd::{ClientDocFetcher, DocCache};
use std::collections::BTreeSet;
use std::sync::Arc;

pub use http::{OAuthLimits, routes};

/// Access tokens live an hour, refresh tokens thirty days of idleness, one-time codes two minutes.
pub const ACCESS_TTL_SECS: i64 = 3600;
pub const REFRESH_TTL_DAYS: i64 = 30;
pub const CODE_TTL_SECS: i64 = 120;
pub const REQUEST_TTL_SECS: i64 = 600;
/// A rotated refresh token used again inside this window is a second process that raced the
/// first, not a thief: refused, but the grant stays.
pub const REFRESH_GRACE_SECS: i64 = 30;
pub const MAX_LIVE_REQUESTS: i64 = 20;
pub const MAX_LIVE_GRANTS: i64 = 20;

pub struct AppsService {
    pub(crate) core: Arc<Core>,
    pub(crate) authz: Arc<crate::authz::AuthzService>,
    pub(crate) fetcher: Arc<dyn ClientDocFetcher>,
    pub(crate) cache: DocCache,
    /// Ten client id fetches a minute per person.
    pub(crate) fetch_limit: Limiter,
}

impl AppsService {
    pub fn new(core: Arc<Core>, authz: Arc<crate::authz::AuthzService>, fetcher: Arc<dyn ClientDocFetcher>) -> Self {
        Self { core, authz, fetcher, cache: DocCache::new(), fetch_limit: Limiter::new(10, 10) }
    }
}

/// An app actor for a live access token, or `None`. Never counts a miss against anyone.
pub async fn app_actor(core: &Core, token: &str) -> Result<Option<Actor>> {
    tokens::app_actor(core, token).await
}

/// Ends every connected app of one person, inside the caller's transaction. Called where a
/// password changes, a reset link is used, two-factor is turned off, the account is suspended,
/// or the person signs out everywhere.
pub async fn revoke_all_for_user(conn: &mut sqlx::PgConnection, user: UserId, reason: &str) -> Result<u64> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "UPDATE oauth_grants SET revoked_at = now(), revoked_reason = $2 WHERE user_id = $1 AND revoked_at IS NULL RETURNING id",
    )
    .bind(user.0)
    .bind(reason)
    .fetch_all(&mut *conn)
    .await?;
    if !ids.is_empty() {
        sqlx::query("UPDATE oauth_tokens SET revoked_at = now() WHERE grant_id = ANY($1) AND revoked_at IS NULL")
            .bind(&ids)
            .execute(&mut *conn)
            .await?;
    }
    Ok(ids.len() as u64)
}

/// Builds the actor's grant from the rows.
pub(crate) fn build_grant(read_all: bool, write_all: bool, workflows_all: bool, repos: &[(i64, bool, bool)]) -> AppGrant {
    let set = |all: bool, f: &dyn Fn(&(i64, bool, bool)) -> bool| {
        if all { RepoSet::All } else { RepoSet::Only(repos.iter().filter(|r| f(r)).map(|r| RepoId(r.0)).collect::<BTreeSet<_>>()) }
    };
    AppGrant { read: set(read_all, &|_| true), write: set(write_all, &|r| r.1), workflows: set(workflows_all, &|r| r.2) }
}

#[async_trait]
impl Apps for AppsService {
    async fn begin_authorize(
        &self,
        actor: &Actor,
        session_cookie: &str,
        p: AuthorizeParams,
        client: &ClientInfo,
    ) -> Result<AuthorizeOutcome> {
        consent::begin_authorize(self, actor, session_cookie, p, client).await
    }
    async fn approve(&self, actor: &Actor, session_cookie: &str, choices: ConsentChoices, client: &ClientInfo) -> Result<ReturnTarget> {
        consent::approve(self, actor, session_cookie, choices, client).await
    }
    async fn deny(&self, actor: &Actor, session_cookie: &str, request_id: &str, client: &ClientInfo) -> Result<ReturnTarget> {
        consent::deny(self, actor, session_cookie, request_id, client).await
    }
    async fn token(&self, req: TokenRequest, client: &ClientInfo) -> std::result::Result<TokenResponse, OAuthError> {
        tokens::token(self, req, client).await
    }
    async fn revoke_token(&self, token: &str, client_id: Option<&str>, client: &ClientInfo) -> Result<()> {
        tokens::revoke_token(self, token, client_id, client).await
    }
    async fn list_grants(&self, actor: &Actor) -> Result<Vec<GrantView>> {
        consent::list_grants(self, actor).await
    }
    async fn revoke_grant(&self, actor: &Actor, id: GrantId, client: &ClientInfo) -> Result<()> {
        consent::revoke_grant(self, actor, id, client).await
    }
    async fn grant_of(&self, actor: &Actor) -> Result<GrantView> {
        consent::grant_of(self, actor).await
    }
}

/// Normalises a resource URL for comparison: scheme and host case and the default port only.
/// Anything else (a trailing slash, a query, a different path) stays different.
pub fn resource_matches(expected: &str, given: &str) -> bool {
    fn norm(s: &str) -> Option<String> {
        if s.is_empty() || s.len() > 512 || s.contains('#') || s.contains('?') || s.contains('%') || !s.is_ascii() {
            return None;
        }
        let (scheme, rest) = s.split_once("://")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if hostport.contains('@') {
            return None;
        }
        let scheme = scheme.to_ascii_lowercase();
        let mut host = hostport.to_ascii_lowercase();
        let default = match scheme.as_str() {
            "https" => ":443",
            "http" => ":80",
            _ => return None,
        };
        if let Some(h) = host.strip_suffix(default) {
            host = h.to_string();
        }
        Some(format!("{scheme}://{host}{path}"))
    }
    match (norm(expected), norm(given)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

pub(crate) fn require_session_user(actor: &Actor) -> Result<(UserId, bool)> {
    match actor {
        Actor::User { id, has_2fa, via: ch_core::AuthVia::Session, .. } => Ok((*id, *has_2fa)),
        Actor::Anonymous => Err(Error::unauthorized()),
        _ => Err(Error::forbidden("Sign in with your browser to do that.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_comparison() {
        let want = "https://code.example.com/mcp";
        assert!(resource_matches(want, "https://code.example.com/mcp"));
        assert!(resource_matches(want, "HTTPS://CODE.EXAMPLE.COM/mcp"));
        assert!(resource_matches(want, "https://code.example.com:443/mcp"));
        for bad in [
            "https://code.example.com/mcp/",
            "https://code.example.com/MCP",
            "https://code.example.com/mcp?x=1",
            "https://code.example.com/mcp#f",
            "https://code.example.com",
            "https://code.example.com:8443/mcp",
            "http://code.example.com/mcp",
            "https://code.example.com.evil.example/mcp",
            "https://user@code.example.com/mcp",
            "https://code.example.com/%6dcp",
            "",
        ] {
            assert!(!resource_matches(want, bad), "{bad}");
        }
        assert!(resource_matches("http://127.0.0.1:3200/mcp", "http://127.0.0.1:3200/mcp"));
        assert!(resource_matches("http://localhost/mcp", "http://localhost:80/mcp"));
    }

    #[test]
    fn grant_building() {
        let rows = [(1, false, false), (2, true, false), (3, true, true)];
        let g = build_grant(false, false, false, &rows);
        assert!(g.read.has(RepoId(1)) && g.read.has(RepoId(3)) && !g.read.has(RepoId(4)));
        assert!(!g.write.has(RepoId(1)) && g.write.has(RepoId(2)));
        assert!(!g.workflows.has(RepoId(2)) && g.workflows.has(RepoId(3)));
        let all = build_grant(true, true, false, &[]);
        assert!(all.read.is_all() && all.write.is_all() && all.workflows.is_empty());
    }
}
