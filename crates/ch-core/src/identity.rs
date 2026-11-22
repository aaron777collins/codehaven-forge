//! Contracts for signing in, sessions, two-factor, organizations and the user directory.
//!
//! Added by the identity stream. The foundation traits (`Authn`, `Invites`, `Authz`,
//! `Accounts`) cover credentials that arrive with a request and the account lifecycle. The
//! web layer also needs the interactive flows: the sign-in form, the second factor, the
//! one-time links, the sessions list, organizations and the admin user list. They live
//! here so `ch-web` can build pages against them without depending on `ch-auth`.
//!
//! Rules every implementation follows:
//! - A wrong username, a wrong password, a suspended account and a locked account all look
//!   the same to the caller: `Error::Unauthorized("That username and password don't match.")`
//!   or `Error::RateLimited`. Nothing says which usernames exist.
//! - Cookies are opaque. A `SessionGrant` carries the value to set; the web layer builds the
//!   `Set-Cookie` header with `access::session_set_cookie`.

use crate::actor::{Actor, Permission};
use crate::error::Result;
use crate::ids::*;
use crate::model::{ClientInfo, LinkPurpose, User};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The per-request CSRF token. The guard layer in `ch-auth` puts it in the request
/// extensions of every page request. Put it in forms as `<input type="hidden" name="_csrf">`
/// and on `<body hx-headers='{"X-CSRF-Token": "..."}'>`.
#[derive(Debug, Clone)]
pub struct CsrfToken(pub String);

/// A session cookie to set. The value is shown once, only its hash is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGrant {
    pub cookie: String,
    pub expires_at: DateTime<Utc>,
    pub max_age_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInStep {
    /// Fully signed in.
    Complete,
    /// The password was right. The cookie only works on `/login/two-factor`.
    TwoFactorRequired,
}

/// Shown to the owner at the next sign-in after someone locked the account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityNotice {
    pub locked_at: DateTime<Utc>,
    pub source_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignIn {
    pub user: User,
    pub grant: SessionGrant,
    pub step: SignInStep,
    pub notice: Option<SecurityNotice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignInRecord {
    pub at: DateTime<Utc>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// An action name from `audit::action`.
    pub action: String,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPeek {
    pub username: String,
    pub purpose: LinkPurpose,
}

#[async_trait]
pub trait Credentials: Send + Sync {
    /// Username or email plus password. Applies lockout and records the audit event.
    async fn sign_in(&self, login: &str, password: &str, client: &ClientInfo) -> Result<SignIn>;
    /// Finishes a `TwoFactorRequired` sign-in with a TOTP code or a recovery code. Returns a
    /// fresh cookie: the id changes when the session becomes fully signed in.
    async fn complete_two_factor(&self, pending_cookie: &str, code: &str, client: &ClientInfo) -> Result<SignIn>;
    async fn sign_out(&self, cookie: &str, client: &ClientInfo) -> Result<()>;
    /// Settings > Sessions.
    async fn sessions(&self, actor: &Actor, current_cookie: &str) -> Result<Vec<SessionInfo>>;
    async fn revoke_session(&self, actor: &Actor, id: SessionId, client: &ClientInfo) -> Result<()>;
    /// Signs out every session except the current one. Returns how many ended.
    async fn sign_out_others(&self, actor: &Actor, current_cookie: &str, client: &ClientInfo) -> Result<u64>;
    /// The account's own sign-in history, newest first (Settings > Security).
    async fn sign_in_history(&self, actor: &Actor, limit: u32) -> Result<Vec<SignInRecord>>;
    /// Needs the current password. Signs out every other session and returns a new cookie.
    async fn change_password(
        &self,
        actor: &Actor,
        current_cookie: &str,
        current_password: &str,
        new_password: &str,
        client: &ClientInfo,
    ) -> Result<SessionGrant>;
    /// For the `/password/{token}` page. Expired, used and made-up tokens are `NotFound`.
    async fn peek_link(&self, token: &str) -> Result<LinkPeek>;
    /// Sets the password and uses the link up. The caller then signs in with `sign_in`.
    async fn redeem_link(&self, token: &str, new_password: &str, client: &ClientInfo) -> Result<User>;
    /// The password rules, for forms that want to say what is wrong before submitting.
    /// At least 12 characters, not a common or guessable password, not the username or email.
    fn check_password(&self, username: &str, email: &str, password: &str) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TotpSetup {
    /// Base32, for typing in by hand.
    pub secret: String,
    /// `otpauth://totp/...`, for a QR code.
    pub otpauth_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoFactorEnabled {
    /// Shown once. Ten codes, each works once.
    pub recovery_codes: Vec<String>,
    pub grant: SessionGrant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoFactorStatus {
    pub enabled: bool,
    pub enabled_at: Option<DateTime<Utc>>,
    pub recovery_codes_left: u32,
}

#[async_trait]
pub trait TwoFactor: Send + Sync {
    async fn status(&self, actor: &Actor) -> Result<TwoFactorStatus>;
    /// Makes a new secret and keeps it pending. Refused when two-factor is already on.
    async fn begin_setup(&self, actor: &Actor) -> Result<TotpSetup>;
    /// Checks a code against the pending secret, turns two-factor on, makes recovery codes and
    /// ends every other session.
    async fn confirm_setup(&self, actor: &Actor, current_cookie: &str, code: &str, client: &ClientInfo) -> Result<TwoFactorEnabled>;
    /// Needs the password. Returns a new cookie and ends every other session.
    async fn disable(&self, actor: &Actor, current_cookie: &str, password: &str, client: &ClientInfo) -> Result<SessionGrant>;
    /// Needs the password. Replaces the old codes.
    async fn regenerate_recovery_codes(&self, actor: &Actor, password: &str, client: &ClientInfo) -> Result<Vec<String>>;
}

// ---------------------------------------------------------------- orgs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Org {
    pub id: OrgId,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub base_permission: Permission,
    pub member_count: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgMember {
    pub user_id: UserId,
    pub username: String,
    pub display_name: String,
    pub role: crate::services::OrgRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: TeamId,
    pub org_id: OrgId,
    pub name: String,
    pub description: String,
    /// The access members get on the repositories given to the team.
    pub permission: Permission,
    pub member_count: i64,
    pub repo_count: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRepo {
    pub repo_id: RepoId,
    pub permission: Permission,
}

#[async_trait]
pub trait Orgs: Send + Sync {
    /// Any signed-in account may create an org and becomes its first owner.
    async fn create(&self, actor: &Actor, name: &str, description: &str, client: &ClientInfo) -> Result<Org>;
    /// Any signed-in account can read an org's profile. Its member list is for members and site admins.
    async fn get(&self, actor: &Actor, name: &str) -> Result<Org>;
    async fn list_for_user(&self, actor: &Actor, user: UserId) -> Result<Vec<Org>>;
    async fn update(
        &self,
        actor: &Actor,
        org: OrgId,
        display_name: Option<String>,
        description: Option<String>,
        base_permission: Option<Permission>,
        client: &ClientInfo,
    ) -> Result<Org>;
    async fn delete(&self, actor: &Actor, org: OrgId, client: &ClientInfo) -> Result<()>;
    async fn members(&self, actor: &Actor, org: OrgId) -> Result<Vec<OrgMember>>;
    /// Owners only. The person must already have an account: this instance has no sign-up.
    async fn set_member(&self, actor: &Actor, org: OrgId, user: UserId, role: crate::services::OrgRole, client: &ClientInfo) -> Result<()>;
    /// Owners, or the member themselves leaving. The last owner cannot leave.
    async fn remove_member(&self, actor: &Actor, org: OrgId, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn teams(&self, actor: &Actor, org: OrgId) -> Result<Vec<Team>>;
    async fn get_team(&self, actor: &Actor, org: OrgId, name: &str) -> Result<Team>;
    async fn create_team(
        &self,
        actor: &Actor,
        org: OrgId,
        name: &str,
        description: &str,
        permission: Permission,
        client: &ClientInfo,
    ) -> Result<Team>;
    /// Owners only. The permission members get on the team's repositories.
    async fn set_team_permission(&self, actor: &Actor, team: TeamId, permission: Permission, client: &ClientInfo) -> Result<()>;
    async fn delete_team(&self, actor: &Actor, team: TeamId, client: &ClientInfo) -> Result<()>;
    async fn team_members(&self, actor: &Actor, team: TeamId) -> Result<Vec<OrgMember>>;
    async fn add_team_member(&self, actor: &Actor, team: TeamId, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn remove_team_member(&self, actor: &Actor, team: TeamId, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn team_repos(&self, actor: &Actor, team: TeamId) -> Result<Vec<TeamRepo>>;
    /// Needs admin on the repo.
    async fn set_team_repo(&self, actor: &Actor, team: TeamId, repo: RepoId, permission: Permission, client: &ClientInfo) -> Result<()>;
    async fn remove_team_repo(&self, actor: &Actor, team: TeamId, repo: RepoId, client: &ClientInfo) -> Result<()>;
}

// ---------------------------------------------------------------- directory

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAddress {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    pub display_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserListFilter {
    pub query: Option<String>,
    pub page: u32,
    pub per_page: u32,
}

#[async_trait]
pub trait Directory: Send + Sync {
    /// Site admins only (Site admin > Users).
    async fn list_users(&self, actor: &Actor, filter: UserListFilter) -> Result<Vec<User>>;
    /// Grants or removes site admin. Needs a session with two-factor, and refuses to remove
    /// the last admin. Ends the target's sessions, since privilege changed.
    async fn set_admin(&self, actor: &Actor, user: UserId, value: bool, client: &ClientInfo) -> Result<()>;
    /// Site admins only. Suspension is `Accounts::suspend`.
    async fn unsuspend(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn emails(&self, actor: &Actor, user: UserId) -> Result<Vec<EmailAddress>>;
    /// Adds an unverified address. There is no mail verification without SMTP, so it
    /// counts for display only and never for sign-in recovery.
    async fn add_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<EmailAddress>;
    async fn remove_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<()>;
    async fn set_primary_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<()>;
    async fn update_profile(&self, actor: &Actor, user: UserId, profile: Profile, client: &ClientInfo) -> Result<User>;
}
