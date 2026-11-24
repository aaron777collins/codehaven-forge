//! Fail-closed placeholder for every domain service trait.
//!
//! Until a stream ships the real implementation, `NotBuilt` stands in so `AppState`
//! can hold an `Arc<dyn Trait>` from day one. Authentication degrades to anonymous,
//! authorization grants nothing, and everything else answers 503 with the name of
//! the service that is missing. Nothing here can pretend a feature works.

#![allow(unused_variables)]

use crate::identity::*;
use crate::{actor::*, error::*, ids::*, model::*, services::*, web_contract::RepoUpdate};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

/// The one error this module produces: "<service> is not built yet", as 503.
fn nb(service: &str) -> Error {
    Error::unavailable(format!("{service} is not built yet"))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NotBuilt;

#[async_trait]
impl Authn for NotBuilt {
    async fn from_session(&self, cookie_value: &str, client: &ClientInfo) -> Result<Actor> {
        Ok(Actor::Anonymous)
    }
    async fn from_basic(&self, username: &str, secret: &str, client: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_bearer(&self, token: &str, client: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_ssh_key(&self, fingerprint: &str) -> Result<Option<SshIdentity>> {
        Ok(None)
    }
    async fn from_app_bearer(&self, token: &str, client: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
}

#[async_trait]
impl crate::apps::Apps for NotBuilt {
    async fn begin_authorize(
        &self,
        actor: &Actor,
        session_cookie: &str,
        p: crate::apps::AuthorizeParams,
        client: &ClientInfo,
    ) -> Result<crate::apps::AuthorizeOutcome> {
        Err(nb("Apps"))
    }
    async fn approve(
        &self,
        actor: &Actor,
        session_cookie: &str,
        choices: crate::apps::ConsentChoices,
        client: &ClientInfo,
    ) -> Result<crate::apps::ReturnTarget> {
        Err(nb("Apps"))
    }
    async fn deny(&self, actor: &Actor, session_cookie: &str, request_id: &str, client: &ClientInfo) -> Result<crate::apps::ReturnTarget> {
        Err(nb("Apps"))
    }
    async fn token(
        &self,
        req: crate::apps::TokenRequest,
        client: &ClientInfo,
    ) -> std::result::Result<crate::apps::TokenResponse, crate::apps::OAuthError> {
        Err(crate::apps::OAuthError::new("invalid_request", "not available"))
    }
    async fn revoke_token(&self, token: &str, client_id: Option<&str>, client: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn list_grants(&self, actor: &Actor) -> Result<Vec<crate::apps::GrantView>> {
        Ok(Vec::new())
    }
    async fn revoke_grant(&self, actor: &Actor, id: GrantId, client: &ClientInfo) -> Result<()> {
        Err(nb("Apps"))
    }
    async fn grant_of(&self, actor: &Actor) -> Result<crate::apps::GrantView> {
        Err(Error::NotFound)
    }
}

#[async_trait]
impl Invites for NotBuilt {
    async fn create(&self, actor: &Actor, req: NewInvite, client: &ClientInfo) -> Result<CreatedInvite> {
        Err(nb("invites"))
    }
    async fn list(&self, actor: &Actor) -> Result<Vec<Invite>> {
        Err(nb("invites"))
    }
    async fn revoke(&self, actor: &Actor, id: InviteId, client: &ClientInfo) -> Result<()> {
        Err(nb("invites"))
    }
    async fn peek(&self, token: &str) -> Result<InvitePeek> {
        Err(nb("invites"))
    }
    async fn accept(&self, token: &str, account: NewAccount, client: &ClientInfo) -> Result<User> {
        Err(nb("invites"))
    }
}

#[async_trait]
impl Authz for NotBuilt {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        Ok(Permission::None)
    }
    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        Ok(None)
    }
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        Err(Error::NotFound)
    }
}

#[async_trait]
impl Accounts for NotBuilt {
    async fn get_user(&self, id: UserId) -> Result<User> {
        Err(nb("accounts"))
    }
    async fn get_by_username(&self, actor: &Actor, username: &str) -> Result<User> {
        Err(nb("accounts"))
    }
    async fn count_users(&self) -> Result<i64> {
        Ok(0)
    }
    async fn create_user(&self, actor: &Actor, new: NewUser, client: &ClientInfo) -> Result<User> {
        Err(nb("accounts"))
    }
    async fn issue_link(
        &self,
        actor: &Actor,
        user: UserId,
        purpose: LinkPurpose,
        ttl_minutes: u32,
        client: &ClientInfo,
    ) -> Result<OneTimeLink> {
        Err(nb("accounts"))
    }
    async fn set_password(&self, actor: &Actor, user: UserId, password: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
    async fn sign_out_everywhere(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
    async fn unlock(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
    async fn suspend(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
    async fn set_can_run_ci(&self, actor: &Actor, user: UserId, value: bool, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
    async fn list_ssh_keys(&self, actor: &Actor, user: UserId) -> Result<Vec<SshKey>> {
        Err(nb("accounts"))
    }
    async fn add_ssh_key(&self, actor: &Actor, user: UserId, title: &str, public_key: &str, client: &ClientInfo) -> Result<SshKey> {
        Err(nb("accounts"))
    }
    async fn delete_ssh_key(&self, actor: &Actor, id: SshKeyId, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
    async fn list_tokens(&self, actor: &Actor, user: UserId) -> Result<Vec<AccessToken>> {
        Err(nb("accounts"))
    }
    async fn create_token(
        &self,
        actor: &Actor,
        user: UserId,
        name: &str,
        scopes: Vec<String>,
        expires_in_days: u32,
        client: &ClientInfo,
    ) -> Result<(AccessToken, String)> {
        Err(nb("accounts"))
    }
    async fn delete_token(&self, actor: &Actor, id: TokenId, client: &ClientInfo) -> Result<()> {
        Err(nb("accounts"))
    }
}

#[async_trait]
impl Repos for NotBuilt {
    async fn create(&self, actor: &Actor, new: NewRepo, client: &ClientInfo) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn get(&self, actor: &Actor, id: RepoId) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn list_for_owner(&self, actor: &Actor, owner: UserId) -> Result<Vec<Repo>> {
        Err(nb("repos"))
    }
    async fn rename(&self, actor: &Actor, id: RepoId, new_name: &str, client: &ClientInfo) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn transfer(&self, actor: &Actor, id: RepoId, new_owner: UserId, client: &ClientInfo) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn update(&self, actor: &Actor, id: RepoId, update: RepoUpdate, client: &ClientInfo) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn fork(&self, actor: &Actor, id: RepoId, into: UserId, client: &ClientInfo) -> Result<Repo> {
        Err(nb("repos"))
    }
    async fn delete(&self, actor: &Actor, id: RepoId, client: &ClientInfo) -> Result<()> {
        Err(nb("repos"))
    }
    async fn collaborators(&self, actor: &Actor, id: RepoId) -> Result<Vec<Collaborator>> {
        Err(nb("repos"))
    }
    async fn set_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, permission: Permission, client: &ClientInfo) -> Result<()> {
        Err(nb("repos"))
    }
    async fn remove_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("repos"))
    }
    async fn protections(&self, actor: &Actor, id: RepoId) -> Result<Vec<BranchProtection>> {
        Err(nb("repos"))
    }
    async fn set_protection(&self, actor: &Actor, id: RepoId, protection: BranchProtection, client: &ClientInfo) -> Result<()> {
        Err(nb("repos"))
    }
    async fn remove_protection(&self, actor: &Actor, id: RepoId, pattern: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("repos"))
    }
}

#[async_trait]
impl GitService for NotBuilt {
    async fn refs(&self, repo: RepoId, prefix: Option<&str>) -> Result<Vec<GitRef>> {
        Err(nb("git"))
    }
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        Err(nb("git"))
    }
    async fn tree(&self, repo: RepoId, rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
        Err(nb("git"))
    }
    async fn blob(&self, repo: RepoId, rev: &str, path: &str, max_bytes: u64) -> Result<Blob> {
        Err(nb("git"))
    }
    async fn log(&self, repo: RepoId, rev: &str, path: Option<&str>, skip: u32, limit: u32) -> Result<Vec<CommitInfo>> {
        Err(nb("git"))
    }
    async fn commit(&self, repo: RepoId, rev: &str) -> Result<CommitInfo> {
        Err(nb("git"))
    }
    async fn compare(&self, repo: RepoId, base: &str, head: &str) -> Result<Compare> {
        Err(nb("git"))
    }
    async fn diff(&self, repo: RepoId, base: &str, head: &str) -> Result<Diff> {
        Err(nb("git"))
    }
    async fn grep(&self, repo: RepoId, rev: &str, pattern: &str, limit: u32) -> Result<Vec<SearchHit>> {
        Err(nb("git"))
    }
    async fn archive(&self, repo: RepoId, rev: &str, format: ArchiveFormat) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        Err(nb("git"))
    }
    async fn commit_file_change(&self, actor: &Actor, req: CommitFileChange, client: &ClientInfo) -> Result<String> {
        Err(nb("git"))
    }
    async fn create_ref(&self, actor: &Actor, repo: RepoId, name: &str, sha: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("git"))
    }
    async fn delete_ref(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("git"))
    }
    async fn test_merge(&self, repo: RepoId, base_ref: &str, head_repo: RepoId, head_ref: &str) -> Result<MergeableState> {
        Err(nb("git"))
    }
    async fn merge(&self, actor: &Actor, req: MergeRequest, client: &ClientInfo) -> Result<MergeOutcome> {
        Err(nb("git"))
    }
}

#[async_trait]
impl GitTransport for NotBuilt {
    async fn open(
        &self,
        caller: GitCaller,
        owner: &str,
        repo: &str,
        wire: GitWire,
        mode: GitMode,
        protocol: Option<String>,
        client: &ClientInfo,
    ) -> Result<GitSession> {
        Err(nb("git"))
    }
}

#[async_trait]
impl Issues for NotBuilt {
    async fn create(&self, actor: &Actor, new: NewIssue, client: &ClientInfo) -> Result<Issue> {
        Err(nb("issues"))
    }
    async fn get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Issue> {
        Err(nb("issues"))
    }
    async fn list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<Issue>> {
        Err(nb("issues"))
    }
    async fn set_state(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        open: bool,
        reason: Option<&str>,
        client: &ClientInfo,
    ) -> Result<Issue> {
        Err(nb("issues"))
    }
    async fn edit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        title: Option<String>,
        body: Option<String>,
        client: &ClientInfo,
    ) -> Result<Issue> {
        Err(nb("issues"))
    }
    async fn set_labels(&self, actor: &Actor, repo: RepoId, number: i64, labels: Vec<String>, client: &ClientInfo) -> Result<Issue> {
        Err(nb("issues"))
    }
    async fn comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Comment>> {
        Err(nb("issues"))
    }
    async fn add_comment(&self, actor: &Actor, repo: RepoId, number: i64, body: &str, client: &ClientInfo) -> Result<Comment> {
        Err(nb("issues"))
    }
}

#[async_trait]
impl Pulls for NotBuilt {
    async fn create(&self, actor: &Actor, new: NewPull, client: &ClientInfo) -> Result<PullRequest> {
        Err(nb("pulls"))
    }
    async fn get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullRequest> {
        Err(nb("pulls"))
    }
    async fn list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<PullRequest>> {
        Err(nb("pulls"))
    }
    async fn merge_gate(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<GateReport> {
        Err(nb("pulls"))
    }
    async fn merge(&self, actor: &Actor, repo: RepoId, number: i64, method: MergeMethod, client: &ClientInfo) -> Result<PullRequest> {
        Err(nb("pulls"))
    }
    async fn close(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullRequest> {
        Err(nb("pulls"))
    }
}

#[async_trait]
impl Reviews for NotBuilt {
    async fn list(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Review>> {
        Err(nb("reviews"))
    }
    async fn submit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        state: ReviewState,
        body: &str,
        comments: Vec<NewReviewComment>,
        client: &ClientInfo,
    ) -> Result<Review> {
        Err(nb("reviews"))
    }
    async fn request_review(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        client: &ClientInfo,
    ) -> Result<()> {
        Err(nb("reviews"))
    }
    async fn resolve_thread(&self, actor: &Actor, repo: RepoId, thread_id: i64, resolved: bool, client: &ClientInfo) -> Result<()> {
        Err(nb("reviews"))
    }
}

#[async_trait]
impl Releases for NotBuilt {
    async fn list(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Release>> {
        Err(nb("releases"))
    }
    async fn get_by_tag(&self, actor: &Actor, repo: RepoId, tag: &str) -> Result<Release> {
        Err(nb("releases"))
    }
    async fn latest(&self, actor: &Actor, repo: RepoId) -> Result<Release> {
        Err(nb("releases"))
    }
}

/// Mail is off: nothing to send with.
#[async_trait]
impl Mail for NotBuilt {
    fn enabled(&self) -> bool {
        false
    }
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        Err(nb("mail"))
    }
}

#[async_trait]
impl Search for NotBuilt {
    async fn repos(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        Err(nb("search"))
    }
    async fn issues(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        Err(nb("search"))
    }
    async fn users(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        Err(nb("search"))
    }
    async fn code(&self, actor: &Actor, repo: RepoId, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        Err(nb("search"))
    }
}

#[async_trait]
impl Notifications for NotBuilt {
    async fn inbox(&self, actor: &Actor, unread_only: bool, page: u32, per_page: u32) -> Result<Vec<Notification>> {
        Err(nb("notifications"))
    }
    async fn unread_count(&self, actor: &Actor) -> Result<i64> {
        Err(nb("notifications"))
    }
    async fn mark_read(&self, actor: &Actor, id: Option<NotificationId>) -> Result<()> {
        Err(nb("notifications"))
    }
    async fn subscribe(&self, actor: &Actor, repo: RepoId, level: &str) -> Result<()> {
        Err(nb("notifications"))
    }
    async fn watch_level(&self, actor: &Actor, repo: RepoId) -> Result<String> {
        Err(nb("notifications"))
    }
    async fn mark_done(&self, actor: &Actor, id: NotificationId) -> Result<()> {
        Err(nb("notifications"))
    }
    async fn set_thread_subscription(&self, actor: &Actor, subject_type: &str, subject_id: i64, subscribed: bool) -> Result<()> {
        Err(nb("notifications"))
    }
}

#[async_trait]
impl Webhooks for NotBuilt {
    async fn list(&self, actor: &Actor, repo: Option<RepoId>, org: Option<OrgId>) -> Result<Vec<Webhook>> {
        Err(nb("webhooks"))
    }
    async fn create(&self, actor: &Actor, new: NewWebhook, client: &ClientInfo) -> Result<Webhook> {
        Err(nb("webhooks"))
    }
    async fn delete(&self, actor: &Actor, id: WebhookId, client: &ClientInfo) -> Result<()> {
        Err(nb("webhooks"))
    }
    async fn redeliver(&self, actor: &Actor, delivery_id: i64) -> Result<()> {
        Err(nb("webhooks"))
    }
    async fn get(&self, actor: &Actor, id: WebhookId) -> Result<Webhook> {
        Err(nb("webhooks"))
    }
    async fn update(&self, actor: &Actor, id: WebhookId, change: WebhookChange, client: &ClientInfo) -> Result<Webhook> {
        Err(nb("webhooks"))
    }
    async fn deliveries(&self, actor: &Actor, id: WebhookId, limit: u32) -> Result<Vec<WebhookDelivery>> {
        Err(nb("webhooks"))
    }
    async fn ping(&self, actor: &Actor, id: WebhookId, client: &ClientInfo) -> Result<()> {
        Err(nb("webhooks"))
    }
}

#[async_trait]
impl Checks for NotBuilt {
    async fn check_runs_for_sha(&self, repo: RepoId, sha: &str) -> Result<Vec<CheckRun>> {
        Err(nb("checks"))
    }
    async fn statuses_for_sha(&self, repo: RepoId, sha: &str) -> Result<Vec<CommitStatus>> {
        Err(nb("checks"))
    }
    async fn combined(&self, repo: RepoId, sha: &str) -> Result<CombinedState> {
        Err(nb("checks"))
    }
    async fn create_status(
        &self,
        actor: &Actor,
        repo: RepoId,
        sha: &str,
        state: &str,
        context: &str,
        description: Option<&str>,
        target_url: Option<&str>,
    ) -> Result<CommitStatus> {
        Err(nb("checks"))
    }
    async fn required_satisfied(&self, repo: RepoId, sha: &str, contexts: &[String]) -> Result<bool> {
        Err(nb("checks"))
    }
}

#[async_trait]
impl Ci for NotBuilt {
    async fn runs(&self, actor: &Actor, repo: RepoId, page: u32, per_page: u32) -> Result<Vec<WorkflowRun>> {
        Err(nb("ci"))
    }
    async fn get_run(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<WorkflowRun> {
        Err(nb("ci"))
    }
    async fn dispatch(
        &self,
        actor: &Actor,
        repo: RepoId,
        workflow: &str,
        git_ref: &str,
        inputs: serde_json::Value,
        client: &ClientInfo,
    ) -> Result<RunId> {
        Err(nb("ci"))
    }
    async fn cancel(&self, actor: &Actor, repo: RepoId, run: RunId, client: &ClientInfo) -> Result<()> {
        Err(nb("ci"))
    }
    async fn rerun(&self, actor: &Actor, repo: RepoId, run: RunId, failed_only: bool, client: &ClientInfo) -> Result<RunId> {
        Err(nb("ci"))
    }
    async fn log_range(&self, actor: &Actor, repo: RepoId, job: JobId, range: LogRange) -> Result<Bytes> {
        Err(nb("ci"))
    }
    async fn create_runner_token(&self, actor: &Actor, repo: Option<RepoId>, client: &ClientInfo) -> Result<RunnerRegistrationToken> {
        Err(nb("ci"))
    }
}

#[async_trait]
impl Credentials for NotBuilt {
    async fn sign_in(&self, login: &str, password: &str, client: &ClientInfo) -> Result<SignIn> {
        Err(nb("sign-in"))
    }
    async fn complete_two_factor(&self, pending_cookie: &str, code: &str, client: &ClientInfo) -> Result<SignIn> {
        Err(nb("sign-in"))
    }
    async fn sign_out(&self, cookie: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("sign-in"))
    }
    async fn sessions(&self, actor: &Actor, current_cookie: &str) -> Result<Vec<SessionInfo>> {
        Err(nb("sessions"))
    }
    async fn revoke_session(&self, actor: &Actor, id: SessionId, client: &ClientInfo) -> Result<()> {
        Err(nb("sessions"))
    }
    async fn sign_out_others(&self, actor: &Actor, current_cookie: &str, client: &ClientInfo) -> Result<u64> {
        Err(nb("sessions"))
    }
    async fn sign_in_history(&self, actor: &Actor, limit: u32) -> Result<Vec<SignInRecord>> {
        Err(nb("sessions"))
    }
    async fn change_password(
        &self,
        actor: &Actor,
        current_cookie: &str,
        current_password: &str,
        new_password: &str,
        client: &ClientInfo,
    ) -> Result<SessionGrant> {
        Err(nb("sessions"))
    }
    async fn peek_link(&self, token: &str) -> Result<LinkPeek> {
        Err(nb("one-time links"))
    }
    async fn redeem_link(&self, token: &str, new_password: &str, client: &ClientInfo) -> Result<User> {
        Err(nb("one-time links"))
    }
    fn check_password(&self, username: &str, email: &str, password: &str) -> Result<()> {
        Err(nb("sign-in"))
    }
}

#[async_trait]
impl TwoFactor for NotBuilt {
    async fn status(&self, actor: &Actor) -> Result<TwoFactorStatus> {
        Err(nb("two-factor"))
    }
    async fn begin_setup(&self, actor: &Actor) -> Result<TotpSetup> {
        Err(nb("two-factor"))
    }
    async fn confirm_setup(&self, actor: &Actor, current_cookie: &str, code: &str, client: &ClientInfo) -> Result<TwoFactorEnabled> {
        Err(nb("two-factor"))
    }
    async fn disable(&self, actor: &Actor, current_cookie: &str, password: &str, client: &ClientInfo) -> Result<SessionGrant> {
        Err(nb("two-factor"))
    }
    async fn regenerate_recovery_codes(&self, actor: &Actor, password: &str, client: &ClientInfo) -> Result<Vec<String>> {
        Err(nb("two-factor"))
    }
}

#[async_trait]
impl Orgs for NotBuilt {
    async fn create(&self, actor: &Actor, name: &str, description: &str, client: &ClientInfo) -> Result<Org> {
        Err(nb("orgs"))
    }
    async fn get(&self, actor: &Actor, name: &str) -> Result<Org> {
        Err(nb("orgs"))
    }
    async fn list_for_user(&self, actor: &Actor, user: UserId) -> Result<Vec<Org>> {
        Err(nb("orgs"))
    }
    async fn update(
        &self,
        actor: &Actor,
        org: OrgId,
        display_name: Option<String>,
        description: Option<String>,
        base_permission: Option<Permission>,
        client: &ClientInfo,
    ) -> Result<Org> {
        Err(nb("orgs"))
    }
    async fn delete(&self, actor: &Actor, org: OrgId, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn members(&self, actor: &Actor, org: OrgId) -> Result<Vec<OrgMember>> {
        Err(nb("orgs"))
    }
    async fn set_member(&self, actor: &Actor, org: OrgId, user: UserId, role: OrgRole, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn remove_member(&self, actor: &Actor, org: OrgId, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn teams(&self, actor: &Actor, org: OrgId) -> Result<Vec<Team>> {
        Err(nb("orgs"))
    }
    async fn get_team(&self, actor: &Actor, org: OrgId, name: &str) -> Result<Team> {
        Err(nb("orgs"))
    }
    async fn create_team(
        &self,
        actor: &Actor,
        org: OrgId,
        name: &str,
        description: &str,
        permission: Permission,
        client: &ClientInfo,
    ) -> Result<Team> {
        Err(nb("orgs"))
    }
    async fn set_team_permission(&self, actor: &Actor, team: TeamId, permission: Permission, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn delete_team(&self, actor: &Actor, team: TeamId, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn team_members(&self, actor: &Actor, team: TeamId) -> Result<Vec<OrgMember>> {
        Err(nb("orgs"))
    }
    async fn add_team_member(&self, actor: &Actor, team: TeamId, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn remove_team_member(&self, actor: &Actor, team: TeamId, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn team_repos(&self, actor: &Actor, team: TeamId) -> Result<Vec<TeamRepo>> {
        Err(nb("orgs"))
    }
    async fn set_team_repo(&self, actor: &Actor, team: TeamId, repo: RepoId, permission: Permission, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
    async fn remove_team_repo(&self, actor: &Actor, team: TeamId, repo: RepoId, client: &ClientInfo) -> Result<()> {
        Err(nb("orgs"))
    }
}

#[async_trait]
impl Directory for NotBuilt {
    async fn list_users(&self, actor: &Actor, filter: UserListFilter) -> Result<Vec<User>> {
        Err(nb("directory"))
    }
    async fn set_admin(&self, actor: &Actor, user: UserId, value: bool, client: &ClientInfo) -> Result<()> {
        Err(nb("directory"))
    }
    async fn unsuspend(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        Err(nb("directory"))
    }
    async fn emails(&self, actor: &Actor, user: UserId) -> Result<Vec<EmailAddress>> {
        Err(nb("directory"))
    }
    async fn add_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<EmailAddress> {
        Err(nb("directory"))
    }
    async fn remove_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("directory"))
    }
    async fn set_primary_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<()> {
        Err(nb("directory"))
    }
    async fn update_profile(&self, actor: &Actor, user: UserId, profile: Profile, client: &ClientInfo) -> Result<User> {
        Err(nb("directory"))
    }
}

#[async_trait]
impl crate::pages::Pages for NotBuilt {
    fn enabled(&self) -> bool {
        false
    }
    async fn site(&self, actor: &Actor, repo: RepoId) -> Result<Option<crate::pages::PagesSite>> {
        Ok(None)
    }
    async fn configure(
        &self,
        actor: &Actor,
        repo: RepoId,
        input: crate::pages::PagesConfigInput,
        client: &ClientInfo,
    ) -> Result<crate::pages::PagesSite> {
        Err(nb("Pages"))
    }
    async fn deploy(&self, actor: &Actor, repo: RepoId, client: &ClientInfo) -> Result<()> {
        Err(nb("Pages"))
    }
    async fn rollback(&self, actor: &Actor, repo: RepoId, deployment: i64, client: &ClientInfo) -> Result<()> {
        Err(nb("Pages"))
    }
    async fn disable(&self, actor: &Actor, repo: RepoId, client: &ClientInfo) -> Result<()> {
        Err(nb("Pages"))
    }
    async fn open(&self, actor: &Actor, owner: &str, project: &str, path: &str) -> Result<String> {
        Err(Error::NotFound)
    }
    async fn settings(&self) -> Result<crate::pages::PagesSettings> {
        Ok(crate::pages::PagesSettings::default())
    }
    async fn set_settings(&self, actor: &Actor, settings: crate::pages::PagesSettings, client: &ClientInfo) -> Result<()> {
        Err(nb("Pages"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authn_is_anonymous_for_sessions_and_rejects_secrets() {
        let not_built = NotBuilt;
        let actor = not_built.from_session("x", &ClientInfo::local()).await.unwrap();
        assert_eq!(actor, Actor::Anonymous);
        assert!(not_built.from_basic("u", "p", &ClientInfo::local()).await.is_err());
    }

    #[tokio::test]
    async fn authz_grants_anonymous_nothing() {
        let not_built = NotBuilt;
        let permission = not_built.repo_permission(&Actor::Anonymous, RepoId(1)).await.unwrap();
        assert_eq!(permission, Permission::None);
    }
}
