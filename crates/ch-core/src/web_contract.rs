//! Contracts the web UI needs on top of the first set of service traits.
//!
//! Proposed by the web stream (ARCHITECTURE.md section 12). Every method here backs a page
//! that would otherwise have nothing to call: sign-in and two-factor, sessions, organisations
//! and teams, labels and milestones, inline review comments, workflow jobs and runners.
//! The stream that owns the data implements the trait (`Sessions` and `People` in `ch-auth`,
//! `Tracker` in `ch-collab`, `Pipelines` in `ch-ci`). Until then `NotBuilt` answers, and it fails
//! closed like every other placeholder.
//!
//! These are plain additions. Nothing in the first contract changed meaning.

#![allow(unused_variables)]

use crate::actor::{Actor, Permission};
use crate::audit::StoredAudit;
use crate::error::{Error, Result};
use crate::ids::*;
use crate::model::*;
use crate::not_built::NotBuilt;
use crate::services::OrgRole;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

fn nb(service: &str) -> Error {
    Error::unavailable(format!("{service} is not built yet"))
}

// ---------------------------------------------------------------- sessions (ch-auth)

/// A browser session. The cookie value is the secret, shown once and stored hashed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSession {
    pub cookie_value: String,
    /// Absolute lifetime. The cookie is set with this `Max-Age`.
    pub max_age_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignInOutcome {
    Session(NewSession),
    /// The password was right and a code is still needed. `pending` is an opaque, short lived
    /// value the two-factor form sends back.
    TwoFactorRequired {
        pending: String,
    },
    /// Wrong username or password, an account that is locked, an account that does not exist.
    /// The page says the same thing for all of them.
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub ip: String,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoFactorSetup {
    /// Base32, for typing into an authenticator by hand.
    pub secret: String,
    pub otpauth_url: String,
}

/// Turning two-factor on ends every session and issues a new one. The page must set the new
/// cookie in the same response, or the person is signed out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoFactorOn {
    pub recovery_codes: Vec<String>,
    pub session: NewSession,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPeek {
    pub username: String,
    pub purpose: LinkPurpose,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProfile {
    pub display_name: Option<String>,
    pub email: Option<String>,
}

#[async_trait]
pub trait Sessions: Send + Sync {
    /// Password sign-in, step one. Rate limits and lockout apply (ARCHITECTURE.md 7.6) and a
    /// limited caller gets `Error::RateLimited`.
    async fn sign_in(&self, username: &str, password: &str, client: &ClientInfo) -> Result<SignInOutcome>;
    /// Step two. `code` is a TOTP code or a recovery code.
    async fn complete_two_factor(&self, pending: &str, code: &str, client: &ClientInfo) -> Result<Option<NewSession>>;
    async fn sign_out(&self, cookie_value: &str, client: &ClientInfo) -> Result<()>;
    /// The caller's own sessions. `current_cookie` marks which one is this browser.
    async fn sessions(&self, actor: &Actor, current_cookie: &str) -> Result<Vec<SessionInfo>>;
    async fn revoke_session(&self, actor: &Actor, id: SessionId, client: &ClientInfo) -> Result<()>;
    /// The caller's own sign-in and security history.
    async fn security_log(&self, actor: &Actor, limit: i64) -> Result<Vec<StoredAudit>>;
    async fn begin_two_factor_setup(&self, actor: &Actor, client: &ClientInfo) -> Result<TwoFactorSetup>;
    /// Checks the first code and turns two-factor on. Returns the recovery codes, shown once,
    /// and the new session cookie to set. `current_cookie` is this browser's session cookie.
    async fn confirm_two_factor_setup(&self, actor: &Actor, current_cookie: &str, code: &str, client: &ClientInfo) -> Result<TwoFactorOn>;
    /// Needs the password. Returns the new session cookie to set.
    async fn disable_two_factor(&self, actor: &Actor, current_cookie: &str, password: &str, client: &ClientInfo) -> Result<NewSession>;
    /// `Error::NotFound` for a link that is expired, used or made up.
    async fn peek_link(&self, token: &str) -> Result<LinkPeek>;
    /// Sets the password and uses the link up. Weak passwords are `Error::Validation`.
    async fn redeem_link(&self, token: &str, new_password: &str, client: &ClientInfo) -> Result<()>;
    /// Ends every session and returns the new cookie to set.
    async fn change_password(
        &self,
        actor: &Actor,
        current_cookie: &str,
        current: &str,
        new: &str,
        client: &ClientInfo,
    ) -> Result<NewSession>;
    async fn update_profile(&self, actor: &Actor, update: UpdateProfile, client: &ClientInfo) -> Result<User>;
}

// ---------------------------------------------------------------- people (ch-auth)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub user: User,
    pub role: OrgRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: TeamId,
    pub org_id: OrgId,
    pub name: String,
    pub description: String,
    pub permission: Permission,
    pub members: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SiteStats {
    pub users: i64,
    pub orgs: i64,
    pub repos: i64,
    pub runners_online: i64,
    pub queued_jobs: i64,
    pub data_dir_total_bytes: u64,
    pub data_dir_free_bytes: u64,
    pub database_ok: bool,
    pub version: String,
}

#[async_trait]
pub trait People: Send + Sync {
    /// Accounts people can see: everyone signed in may list users and orgs (names only).
    async fn list_users(&self, actor: &Actor, query: Option<&str>, limit: u32) -> Result<Vec<User>>;
    async fn orgs_of(&self, actor: &Actor, user: UserId) -> Result<Vec<User>>;
    async fn org_members(&self, actor: &Actor, org: OrgId) -> Result<Vec<Member>>;
    async fn teams(&self, actor: &Actor, org: OrgId) -> Result<Vec<Team>>;
    async fn team_members(&self, actor: &Actor, team: TeamId) -> Result<Vec<User>>;
    async fn create_org(&self, actor: &Actor, name: &str, display_name: &str, client: &ClientInfo) -> Result<User>;
    /// Site admins only.
    async fn site_stats(&self, actor: &Actor) -> Result<SiteStats>;
}

// ---------------------------------------------------------------- tracker (ch-collab)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub id: LabelId,
    pub repo_id: RepoId,
    pub name: String,
    /// Six hex digits, no `#`.
    pub color: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Milestone {
    pub id: MilestoneId,
    pub repo_id: RepoId,
    pub title: String,
    pub description: String,
    pub due_on: Option<DateTime<Utc>>,
    pub open: bool,
    pub open_issues: i64,
    pub closed_issues: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: CommentId,
    pub review_id: Option<ReviewId>,
    pub author_id: UserId,
    pub path: String,
    /// `left` (the old file) or `right` (the new one).
    pub side: String,
    pub line: u32,
    pub start_line: Option<u32>,
    pub body: String,
    pub in_reply_to: Option<CommentId>,
    pub resolved: bool,
    pub commit_sha: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRelease {
    pub repo_id: RepoId,
    pub tag_name: String,
    /// Branch or sha the tag is created on when it does not exist yet.
    pub target: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
}

#[async_trait]
pub trait Tracker: Send + Sync {
    async fn labels(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Label>>;
    async fn create_label(
        &self,
        actor: &Actor,
        repo: RepoId,
        name: &str,
        color: &str,
        description: &str,
        client: &ClientInfo,
    ) -> Result<Label>;
    async fn delete_label(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()>;
    async fn milestones(&self, actor: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>>;
    async fn create_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        title: &str,
        description: &str,
        due_on: Option<DateTime<Utc>>,
        client: &ClientInfo,
    ) -> Result<Milestone>;
    async fn set_milestone_state(&self, actor: &Actor, repo: RepoId, id: MilestoneId, open: bool, client: &ClientInfo) -> Result<()>;
    /// Moves an issue or pull request into a milestone, or out of every milestone with `None`.
    async fn set_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        milestone: Option<MilestoneId>,
        client: &ClientInfo,
    ) -> Result<()>;
    async fn set_assignees(&self, actor: &Actor, repo: RepoId, number: i64, users: Vec<UserId>, client: &ClientInfo) -> Result<()>;
    /// Inline comments on a pull request, oldest first.
    async fn review_comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ReviewComment>>;
    async fn create_release(&self, actor: &Actor, new: NewRelease, client: &ClientInfo) -> Result<Release>;
    async fn delete_release(&self, actor: &Actor, repo: RepoId, id: ReleaseId, client: &ClientInfo) -> Result<()>;
}

// ---------------------------------------------------------------- pipelines (ch-ci)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInput {
    pub name: String,
    pub description: String,
    pub default: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub path: String,
    pub name: String,
    pub triggers: Vec<String>,
    /// Why the file does not validate. The page shows it next to the workflow.
    pub error: Option<String>,
    pub can_dispatch: bool,
    pub inputs: Vec<WorkflowInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub number: u32,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Byte range of this step inside the job log, for collapsible steps.
    pub log_from: u64,
    pub log_to: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowJob {
    pub id: JobId,
    pub run_id: RunId,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    /// Names of the jobs this one waits for.
    pub needs: Vec<String>,
    pub runner_name: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Runner {
    pub id: RunnerId,
    pub name: String,
    pub labels: Vec<String>,
    pub repo_id: Option<RepoId>,
    /// `online`, `busy` or `offline`.
    pub status: String,
    pub version: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMeta {
    pub name: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: i64,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub created_at: DateTime<Utc>,
    pub expired: bool,
}

#[async_trait]
pub trait Pipelines: Send + Sync {
    async fn workflows(&self, actor: &Actor, repo: RepoId, git_ref: &str) -> Result<Vec<WorkflowDef>>;
    async fn jobs(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<WorkflowJob>>;
    /// Registered runners: those that serve `repo` when given, else every runner (site admins only).
    async fn runners(&self, actor: &Actor, repo: Option<RepoId>) -> Result<Vec<Runner>>;
    async fn revoke_runner(&self, actor: &Actor, id: RunnerId, client: &ClientInfo) -> Result<()>;
    async fn secrets(&self, actor: &Actor, repo: RepoId) -> Result<Vec<SecretMeta>>;
    async fn set_secret(&self, actor: &Actor, repo: RepoId, name: &str, value: &str, client: &ClientInfo) -> Result<()>;
    async fn delete_secret(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()>;
    async fn variables(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Variable>>;
    async fn set_variable(&self, actor: &Actor, repo: RepoId, name: &str, value: &str, client: &ClientInfo) -> Result<()>;
    async fn delete_variable(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()>;
    async fn artifacts(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<Artifact>>;
}

// ---------------------------------------------------------------- repo settings (ch-git)

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoUpdate {
    pub description: Option<String>,
    pub visibility: Option<Visibility>,
    pub default_branch: Option<String>,
    pub archived: Option<bool>,
    pub topics: Option<Vec<String>>,
}

/// Who last changed a run of lines. `GitService::blame` returns these in file order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameHunk {
    pub sha: String,
    pub author: String,
    pub at: DateTime<Utc>,
    pub summary: String,
    /// 1 based.
    pub start_line: u32,
    pub line_count: u32,
}

// ---------------------------------------------------------------- NotBuilt

#[async_trait]
impl Sessions for NotBuilt {
    async fn sign_in(&self, username: &str, password: &str, client: &ClientInfo) -> Result<SignInOutcome> {
        Ok(SignInOutcome::Rejected)
    }
    async fn complete_two_factor(&self, pending: &str, code: &str, client: &ClientInfo) -> Result<Option<NewSession>> {
        Ok(None)
    }
    async fn sign_out(&self, cookie_value: &str, client: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn sessions(&self, actor: &Actor, current_cookie: &str) -> Result<Vec<SessionInfo>> {
        Err(nb("sessions"))
    }
    async fn revoke_session(&self, actor: &Actor, id: SessionId, client: &ClientInfo) -> Result<()> {
        Err(nb("sessions"))
    }
    async fn security_log(&self, actor: &Actor, limit: i64) -> Result<Vec<StoredAudit>> {
        Err(nb("sessions"))
    }
    async fn begin_two_factor_setup(&self, actor: &Actor, client: &ClientInfo) -> Result<TwoFactorSetup> {
        Err(nb("sessions"))
    }
    async fn confirm_two_factor_setup(&self, actor: &Actor, current_cookie: &str, code: &str, client: &ClientInfo) -> Result<TwoFactorOn> {
        Err(nb("sessions"))
    }
    async fn disable_two_factor(&self, actor: &Actor, current_cookie: &str, password: &str, client: &ClientInfo) -> Result<NewSession> {
        Err(nb("sessions"))
    }
    async fn peek_link(&self, token: &str) -> Result<LinkPeek> {
        Err(Error::NotFound)
    }
    async fn redeem_link(&self, token: &str, new_password: &str, client: &ClientInfo) -> Result<()> {
        Err(Error::NotFound)
    }
    async fn change_password(
        &self,
        actor: &Actor,
        current_cookie: &str,
        current: &str,
        new: &str,
        client: &ClientInfo,
    ) -> Result<NewSession> {
        Err(nb("sessions"))
    }
    async fn update_profile(&self, actor: &Actor, update: UpdateProfile, client: &ClientInfo) -> Result<User> {
        Err(nb("sessions"))
    }
}

#[async_trait]
impl People for NotBuilt {
    async fn list_users(&self, actor: &Actor, query: Option<&str>, limit: u32) -> Result<Vec<User>> {
        Err(nb("directory"))
    }
    async fn orgs_of(&self, actor: &Actor, user: UserId) -> Result<Vec<User>> {
        Err(nb("directory"))
    }
    async fn org_members(&self, actor: &Actor, org: OrgId) -> Result<Vec<Member>> {
        Err(nb("directory"))
    }
    async fn teams(&self, actor: &Actor, org: OrgId) -> Result<Vec<Team>> {
        Err(nb("directory"))
    }
    async fn team_members(&self, actor: &Actor, team: TeamId) -> Result<Vec<User>> {
        Err(nb("directory"))
    }
    async fn create_org(&self, actor: &Actor, name: &str, display_name: &str, client: &ClientInfo) -> Result<User> {
        Err(nb("directory"))
    }
    async fn site_stats(&self, actor: &Actor) -> Result<SiteStats> {
        Err(nb("directory"))
    }
}

#[async_trait]
impl Tracker for NotBuilt {
    async fn labels(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Label>> {
        Err(nb("tracker"))
    }
    async fn create_label(
        &self,
        actor: &Actor,
        repo: RepoId,
        name: &str,
        color: &str,
        description: &str,
        client: &ClientInfo,
    ) -> Result<Label> {
        Err(nb("tracker"))
    }
    async fn delete_label(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("tracker"))
    }
    async fn milestones(&self, actor: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>> {
        Err(nb("tracker"))
    }
    async fn create_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        title: &str,
        description: &str,
        due_on: Option<DateTime<Utc>>,
        client: &ClientInfo,
    ) -> Result<Milestone> {
        Err(nb("tracker"))
    }
    async fn set_milestone_state(&self, actor: &Actor, repo: RepoId, id: MilestoneId, open: bool, client: &ClientInfo) -> Result<()> {
        Err(nb("tracker"))
    }
    async fn set_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        milestone: Option<MilestoneId>,
        client: &ClientInfo,
    ) -> Result<()> {
        Err(nb("tracker"))
    }
    async fn set_assignees(&self, actor: &Actor, repo: RepoId, number: i64, users: Vec<UserId>, client: &ClientInfo) -> Result<()> {
        Err(nb("tracker"))
    }
    async fn review_comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ReviewComment>> {
        Err(nb("tracker"))
    }
    async fn create_release(&self, actor: &Actor, new: NewRelease, client: &ClientInfo) -> Result<Release> {
        Err(nb("tracker"))
    }
    async fn delete_release(&self, actor: &Actor, repo: RepoId, id: ReleaseId, client: &ClientInfo) -> Result<()> {
        Err(nb("tracker"))
    }
}

#[async_trait]
impl Pipelines for NotBuilt {
    async fn workflows(&self, actor: &Actor, repo: RepoId, git_ref: &str) -> Result<Vec<WorkflowDef>> {
        Err(nb("pipelines"))
    }
    async fn jobs(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<WorkflowJob>> {
        Err(nb("pipelines"))
    }
    async fn runners(&self, actor: &Actor, repo: Option<RepoId>) -> Result<Vec<Runner>> {
        Err(nb("pipelines"))
    }
    async fn revoke_runner(&self, actor: &Actor, id: RunnerId, client: &ClientInfo) -> Result<()> {
        Err(nb("pipelines"))
    }
    async fn secrets(&self, actor: &Actor, repo: RepoId) -> Result<Vec<SecretMeta>> {
        Err(nb("pipelines"))
    }
    async fn set_secret(&self, actor: &Actor, repo: RepoId, name: &str, value: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("pipelines"))
    }
    async fn delete_secret(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("pipelines"))
    }
    async fn variables(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Variable>> {
        Err(nb("pipelines"))
    }
    async fn set_variable(&self, actor: &Actor, repo: RepoId, name: &str, value: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("pipelines"))
    }
    async fn delete_variable(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("pipelines"))
    }
    async fn artifacts(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<Vec<Artifact>> {
        Err(nb("pipelines"))
    }
}
