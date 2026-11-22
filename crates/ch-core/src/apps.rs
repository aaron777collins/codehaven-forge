//! Connected apps: the contract between the OAuth service (`ch-auth`), the consent pages
//! (`ch-web`) and the MCP endpoint (`ch-mcp`). A person lets an MCP client such as Claude act
//! for them, within exactly the repositories and powers they choose at the consent page.

use crate::actor::Actor;
use crate::error::Result;
use crate::ids::*;
use crate::model::ClientInfo;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

pub const SCOPE_READ: &str = "read";
pub const SCOPE_WRITE: &str = "write";
pub const SCOPE_WORKFLOWS: &str = "workflows";
/// Accepted and ignored: a refresh token is always issued.
pub const SCOPE_OFFLINE: &str = "offline_access";

/// Token prefixes, so a secret scanner can recognise them. Access tokens are never accepted as
/// a personal access token, a job token or a runner credential, and the other way round.
pub const CODE_PREFIX: &str = "chmc_";
pub const ACCESS_PREFIX: &str = "chma_";
pub const REFRESH_PREFIX: &str = "chmr_";

/// True when `s` has the shape of an app access token. Checked before any lookup.
pub fn looks_like_access_token(s: &str) -> bool {
    s.len() == ACCESS_PREFIX.len() + 43
        && s.starts_with(ACCESS_PREFIX)
        && s[ACCESS_PREFIX.len()..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The parameters of an authorization request, exactly as the browser sent them.
#[derive(Debug, Clone, Default)]
pub struct AuthorizeParams {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

/// What the consent page shows. Every string that came from the client is data to be escaped.
#[derive(Debug, Clone)]
pub struct ConsentView {
    /// Opaque, bound to the person and their session. The form carries only this.
    pub request_id: String,
    pub client_id: String,
    /// The host of the client id document: the name the person is asked to trust.
    pub client_host: String,
    /// The app's own claim about its name. Secondary, labelled as a claim.
    pub client_name: String,
    /// `host` or `host:port` of the redirect.
    pub redirect_host: String,
    /// The redirect goes to a program on this computer.
    pub loopback: bool,
    pub requested_scopes: Vec<String>,
    pub username: String,
}

/// What `begin_authorize` decided. Anything but `Consent` renders a plain page and never
/// redirects to the client, because an unchecked redirect would be an open redirect.
#[derive(Debug, Clone)]
pub enum AuthorizeOutcome {
    /// Two-factor sign-in must be on first. Nothing was fetched and nothing was stored.
    NeedsTwoFactor,
    /// A reason in plain words. Nothing was stored.
    Refused(String),
    Consent(ConsentView),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoChoice {
    None,
    All,
    Selected(Vec<RepoId>),
}

/// The person's answers on the consent page.
#[derive(Debug, Clone)]
pub struct ConsentChoices {
    pub request_id: String,
    pub read: RepoChoice,
    pub write: RepoChoice,
    pub workflows: RepoChoice,
    /// 90 or 180.
    pub expires_days: u32,
}

/// Where to send the browser after the person answered.
#[derive(Debug, Clone)]
pub struct ReturnTarget {
    pub url: String,
    /// Host and port of the redirect, for the "Return to" link.
    pub host: String,
}

#[derive(Debug, Clone, Default)]
pub struct TokenRequest {
    pub grant_type: Option<String>,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_verifier: Option<String>,
    pub client_id: Option<String>,
    pub resource: Option<String>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    /// What was granted, space separated. It can be less or more than was asked.
    pub scope: String,
}

/// An RFC 6749 error. `code` is one of `invalid_request`, `invalid_grant`, `invalid_target`,
/// `unsupported_grant_type`, `invalid_client`.
#[derive(Debug, Clone)]
pub struct OAuthError {
    pub code: &'static str,
    pub description: String,
}

impl OAuthError {
    pub fn new(code: &'static str, description: impl Into<String>) -> Self {
        Self { code, description: description.into() }
    }
}

#[derive(Debug, Clone)]
pub struct GrantRepoView {
    pub repo_id: RepoId,
    /// `owner/name`, or an empty string when the person can no longer see the repository.
    pub full_name: String,
    pub can_write: bool,
    pub can_workflows: bool,
}

/// One connected app, for the person's settings page and for `get_connection`.
#[derive(Debug, Clone)]
pub struct GrantView {
    pub id: GrantId,
    /// The person's username, display name and primary email. The email is used to sign
    /// commits and is never returned to a model.
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub client_name: String,
    pub redirect_host: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    pub read_all: bool,
    pub write_all: bool,
    pub workflows_all: bool,
    pub repos: Vec<GrantRepoView>,
}

#[async_trait]
pub trait Apps: Send + Sync {
    /// Steps 1 to 9 of authorize, for a signed-in person. `session_cookie` binds the stored
    /// request to this browser session.
    async fn begin_authorize(
        &self,
        actor: &Actor,
        session_cookie: &str,
        p: AuthorizeParams,
        client: &ClientInfo,
    ) -> Result<AuthorizeOutcome>;
    /// Consent submitted: creates the grant and a one-time code and says where to send the browser.
    async fn approve(&self, actor: &Actor, session_cookie: &str, choices: ConsentChoices, client: &ClientInfo) -> Result<ReturnTarget>;
    /// The person said no.
    async fn deny(&self, actor: &Actor, session_cookie: &str, request_id: &str, client: &ClientInfo) -> Result<ReturnTarget>;
    /// `POST /oauth/token`.
    async fn token(&self, req: TokenRequest, client: &ClientInfo) -> std::result::Result<TokenResponse, OAuthError>;
    /// `POST /oauth/revoke`. Never says whether the token existed.
    async fn revoke_token(&self, token: &str, client_id: Option<&str>, client: &ClientInfo) -> Result<()>;
    /// The person's live connected apps.
    async fn list_grants(&self, actor: &Actor) -> Result<Vec<GrantView>>;
    /// Ends one of the person's grants.
    async fn revoke_grant(&self, actor: &Actor, id: GrantId, client: &ClientInfo) -> Result<()>;
    /// The grant behind an app actor.
    async fn grant_of(&self, actor: &Actor) -> Result<GrantView>;
}
