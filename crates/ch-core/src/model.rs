//! Domain models passed between services. They hold ids and plain data, no URLs: the REST
//! layer turns them into `ch-api-types` DTOs and adds `url` and `html_url` at the edge.

use crate::actor::Permission;
use crate::ids::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Where a request came from, for rate limits, lockout and the audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub ip: IpAddr,
    pub user_agent: Option<String>,
}

impl ClientInfo {
    pub fn local() -> Self {
        Self { ip: IpAddr::from([127, 0, 0, 1]), user_agent: None }
    }
}

// ---------------------------------------------------------------- accounts

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserKind {
    User,
    Org,
    Bot,
}

impl UserKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UserKind::User => "user",
            UserKind::Org => "org",
            UserKind::Bot => "bot",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(UserKind::User),
            "org" => Some(UserKind::Org),
            "bot" => Some(UserKind::Bot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub kind: UserKind,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub can_run_ci: bool,
    pub suspended: bool,
    pub has_2fa: bool,
    pub has_password: bool,
    pub invited_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewUser {
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub is_admin: bool,
    pub can_run_ci: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPurpose {
    SetPassword,
    ResetPassword,
}

impl LinkPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkPurpose::SetPassword => "set_password",
            LinkPurpose::ResetPassword => "reset_password",
        }
    }
}

/// A one-time link. `token` is shown once and never stored, only its hash is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneTimeLink {
    pub token: String,
    pub path: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub id: InviteId,
    pub created_by: Option<UserId>,
    pub email: Option<String>,
    pub grants_admin: bool,
    pub grants_ci: bool,
    pub org_id: Option<OrgId>,
    pub team_id: Option<TeamId>,
    pub note: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub used_by: Option<UserId>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Invite {
    pub fn status(&self, now: DateTime<Utc>) -> InviteStatus {
        if self.used_at.is_some() {
            InviteStatus::Used
        } else if self.revoked_at.is_some() {
            InviteStatus::Revoked
        } else if self.expires_at <= now {
            InviteStatus::Expired
        } else {
            InviteStatus::Pending
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteStatus {
    Pending,
    Used,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewInvite {
    pub email: Option<String>,
    pub grants_admin: bool,
    pub grants_ci: bool,
    pub org_id: Option<OrgId>,
    pub team_id: Option<TeamId>,
    pub note: Option<String>,
    /// From `invites.min_expiry_hours` to `invites.max_expiry_hours`. `None` is the default.
    pub expires_in_hours: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedInvite {
    pub invite: Invite,
    /// The full token, `chi_` plus 32 base62 characters. Shown once.
    pub token: String,
    /// `<external_url>/invite/<token>`.
    pub url: String,
}

/// What the accept page needs to show, with no hint about why a bad link is bad.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePeek {
    pub invited_by: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAccount {
    pub username: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshKey {
    pub id: SshKeyId,
    pub user_id: UserId,
    pub title: String,
    pub key_type: String,
    pub fingerprint: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessToken {
    pub id: TokenId,
    pub user_id: UserId,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// What an SSH fingerprint resolves to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SshIdentity {
    User(UserId),
    /// A deploy key, bound to one repository.
    Deploy {
        repo_id: RepoId,
        read_only: bool,
    },
}

// ---------------------------------------------------------------- repos

/// Who can read a repository (epic ch-4pn). Stored in `repositories.access_tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Only people granted access.
    Private,
    /// Also every signed-in, active account on this instance. Never anyone signed out.
    Internal,
    /// Also anyone on the internet, with no account, while the instance allows public
    /// repositories. Signed out is read only.
    Public,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Internal => "internal",
            Visibility::Public => "public",
        }
    }

    /// The stored and wire spelling, exactly. Anything else is `None`, never a default.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "private" => Some(Visibility::Private),
            "internal" => Some(Visibility::Internal),
            "public" => Some(Visibility::Public),
            _ => None,
        }
    }

    /// The legacy REST flag: true unless the repository is public.
    pub fn is_private_flag(self) -> bool {
        self != Visibility::Public
    }
}

/// Who may open issues, comment, react, review and send pull requests without a grant, on an
/// internal or public repository. Stored in `repositories.contribution_policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContributionPolicy {
    /// Every signed-in account that can read the repository.
    #[default]
    Everyone,
    /// Only people with a grant on the repository.
    Grants,
}

impl ContributionPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ContributionPolicy::Everyone => "everyone",
            ContributionPolicy::Grants => "grants",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "everyone" => Some(ContributionPolicy::Everyone),
            "grants" => Some(ContributionPolicy::Grants),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: RepoId,
    pub owner_id: UserId,
    pub owner_name: String,
    pub name: String,
    pub description: String,
    pub visibility: Visibility,
    pub default_branch: String,
    pub archived: bool,
    pub is_empty: bool,
    pub fork_parent_id: Option<RepoId>,
    pub topics: Vec<String>,
    pub pushed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub contribution_policy: ContributionPolicy,
}

impl Repo {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner_name, self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRepo {
    pub owner_id: UserId,
    pub name: String,
    pub description: String,
    pub visibility: Visibility,
    pub default_branch: Option<String>,
    /// Create with a first commit holding a README.
    pub auto_init: bool,
}

/// Fields of a repository an admin can change. `None` leaves a field alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoSettingsUpdate {
    pub description: Option<String>,
    pub website: Option<String>,
    pub visibility: Option<Visibility>,
    pub default_branch: Option<String>,
    pub archived: Option<bool>,
    pub topics: Option<Vec<String>>,
    pub has_issues: Option<bool>,
    pub has_pulls: Option<bool>,
    pub has_actions: Option<bool>,
    pub allow_merge_commit: Option<bool>,
    pub allow_squash_merge: Option<bool>,
    pub allow_rebase_merge: Option<bool>,
    pub delete_branch_on_merge: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collaborator {
    pub user_id: UserId,
    pub username: String,
    pub permission: Permission,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BranchProtection {
    pub pattern: String,
    pub required_approvals: u32,
    pub dismiss_stale_reviews: bool,
    pub require_code_owner_review: bool,
    pub required_checks: Vec<String>,
    pub strict: bool,
    pub require_resolved_threads: bool,
    pub block_force_push: bool,
    pub block_deletion: bool,
    pub allow_direct_push: bool,
}

// ---------------------------------------------------------------- git

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitProtocolService {
    UploadPack,
    ReceivePack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRef {
    pub name: String,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub parents: Vec<String>,
    pub tree: String,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Blob,
    Tree,
    Commit,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub mode: String,
    pub sha: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub sha: String,
    pub size: u64,
    pub is_binary: bool,
    pub truncated: bool,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Removed,
    Renamed,
    Copied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub status: FileStatus,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub binary: bool,
    /// True when the file is over the size limits and the hunks were left out.
    pub collapsed: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diff {
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compare {
    pub base_sha: String,
    pub head_sha: String,
    pub merge_base: String,
    pub ahead_by: u32,
    pub behind_by: u32,
    pub commits: Vec<CommitInfo>,
    pub diff: Diff,
}

/// A run of consecutive lines last changed by one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameRange {
    /// 1-based line in the file at the blamed revision.
    pub start_line: u32,
    pub line_count: u32,
    pub sha: String,
    pub author: Signature,
    pub summary: String,
    /// The path the lines had in that commit, which differs after a rename.
    pub orig_path: String,
    pub orig_start_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blame {
    pub ranges: Vec<BlameRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequest {
    pub repo_id: RepoId,
    pub base_ref: String,
    pub head_repo_id: RepoId,
    pub head_ref: String,
    pub method: MergeMethod,
    pub title: String,
    pub message: String,
    pub author: Signature,
    pub committer: Signature,
    /// The base sha the caller based its decision on. The ref update is compare-and-swap
    /// on it, so a concurrent push fails the merge instead of losing commits.
    pub expected_base_sha: String,
    /// True for "update branch", which writes into a pull request's head branch. That write has
    /// passed no merge gate, so branch protection treats it as a direct push (ch-jnm).
    #[serde(default)]
    pub update_branch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MergeOutcome {
    Merged {
        sha: String,
    },
    Conflict {
        paths: Vec<String>,
    },
    /// The base moved since `expected_base_sha`.
    BaseMoved {
        current_sha: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    /// `None` deletes the file.
    pub content: Option<Vec<u8>>,
    /// Rename: the old path to remove.
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitFileChange {
    pub repo_id: RepoId,
    pub branch: String,
    /// Create the branch from `branch`'s head when `new_branch` is set.
    pub new_branch: Option<String>,
    pub expected_sha: Option<String>,
    pub changes: Vec<FileChange>,
    pub message: String,
    pub author: Signature,
}

// ---------------------------------------------------------------- collaboration

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: IssueId,
    pub repo_id: RepoId,
    pub number: i64,
    pub is_pull: bool,
    pub author_id: UserId,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub state_reason: Option<String>,
    pub locked: bool,
    pub labels: Vec<String>,
    pub assignees: Vec<UserId>,
    pub milestone_id: Option<MilestoneId>,
    pub comments: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IssueFilter {
    pub open: Option<bool>,
    pub labels: Vec<String>,
    pub assignee: Option<UserId>,
    pub author: Option<UserId>,
    pub milestone: Option<MilestoneId>,
    pub query: Option<String>,
    pub sort: Option<String>,
    pub page: u32,
    pub per_page: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewIssue {
    pub repo_id: RepoId,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<UserId>,
    pub milestone_id: Option<MilestoneId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub issue_id: IssueId,
    pub author_id: UserId,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeableState {
    Clean,
    Conflict,
    Behind,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub issue: Issue,
    pub head_repo_id: Option<RepoId>,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    pub merge_base: Option<String>,
    pub mergeable: MergeableState,
    pub merge_commit_sha: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub merged_by: Option<UserId>,
    pub draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPull {
    pub repo_id: RepoId,
    pub title: String,
    pub body: String,
    pub head_repo_id: RepoId,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: ReviewId,
    pub pull_id: PullId,
    pub author_id: UserId,
    pub state: ReviewState,
    pub body: String,
    pub commit_sha: String,
    pub submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewReviewComment {
    pub path: String,
    pub side: String,
    pub line: u32,
    pub start_line: Option<u32>,
    pub body: String,
    pub in_reply_to: Option<CommentId>,
}

/// Every reason a merge is blocked, so the UI can show a checklist instead of a flat no.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GateReport {
    pub mergeable: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub id: ReleaseId,
    pub repo_id: RepoId,
    pub tag_name: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
    pub author_id: UserId,
    pub created_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub kind: String,
    pub id: i64,
    pub title: String,
    pub snippet: String,
}

// ---------------------------------------------------------------- notifications and webhooks

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub user_id: UserId,
    pub repo_id: Option<RepoId>,
    pub reason: String,
    pub subject_type: String,
    pub subject_id: i64,
    pub title: String,
    pub unread: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: WebhookId,
    pub repo_id: Option<RepoId>,
    pub org_id: Option<OrgId>,
    pub url: String,
    pub events: Vec<String>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewWebhook {
    pub repo_id: Option<RepoId>,
    pub org_id: Option<OrgId>,
    pub url: String,
    pub secret: Option<String>,
    pub events: Vec<String>,
    pub active: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebhookChange {
    pub url: Option<String>,
    pub secret: Option<String>,
    pub events: Option<Vec<String>>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: i64,
    pub webhook_id: WebhookId,
    pub guid: String,
    pub event: String,
    pub action: Option<String>,
    pub attempt: i32,
    pub success: Option<bool>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub request_headers: Option<serde_json::Value>,
    pub payload: serde_json::Value,
    pub response_status: Option<i32>,
    pub response_headers: Option<serde_json::Value>,
    pub response_body: Option<String>,
    pub duration_ms: Option<i32>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------- CI

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: i64,
    pub repo_id: RepoId,
    pub head_sha: String,
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    pub conclusion: Option<String>,
    pub details_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitStatus {
    pub id: i64,
    pub repo_id: RepoId,
    pub sha: String,
    pub context: String,
    /// `pending`, `success`, `failure` or `error`.
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CombinedState {
    Pending,
    Success,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: RunId,
    pub repo_id: RepoId,
    pub workflow_id: Option<WorkflowId>,
    pub run_number: i64,
    /// 1 for the first run, then one more for each re-run.
    #[serde(default = "first_attempt")]
    pub attempt: i32,
    /// The run's title: the commit message's first line, the pull request title or `run-name:`.
    #[serde(default)]
    pub title: String,
    /// The workflow's display name at the time of the run.
    #[serde(default)]
    pub workflow_name: String,
    pub event: String,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: DateTime<Utc>,
}

fn first_attempt() -> i32 {
    1
}

/// What a workflow's status badge shows (ch-c1d).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowBadge {
    pub workflow_name: String,
    /// The latest finished run's conclusion, `None` when nothing has finished.
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerRegistrationToken {
    /// Shown once. Single use, expires in 1 hour by default.
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// A line range in a job log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRange {
    pub from: u64,
    pub to: Option<u64>,
}

/// A workflow file in a repository, as the Actions tab lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowFile {
    pub id: Option<WorkflowId>,
    pub path: String,
    pub name: String,
    /// `active` or `disabled`.
    pub state: String,
    /// Events that start it: `push`, `pull_request`, `workflow_dispatch`, `schedule`, `release`.
    pub events: Vec<String>,
    /// Validation problems, one sentence each, with the line number. Empty when the file is fine.
    pub errors: Vec<String>,
    /// Inputs a manual run asks for. Empty unless `workflow_dispatch` declares some.
    pub dispatch_inputs: Vec<DispatchInputInfo>,
    /// The `push` trigger's filters, or `None` when the workflow does not listen to pushes. The
    /// workspace applies them to say which workflows a commit will start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushFilters>,
}

/// A `push` trigger's branch, tag and path filters, as written. `None` means the key was not
/// written, which is not the same as an empty list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushFilters {
    pub branches: Option<Vec<String>>,
    pub branches_ignore: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub tags_ignore: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub paths_ignore: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchInputInfo {
    pub name: String,
    pub description: String,
    pub required: bool,
    /// `string`, `boolean`, `choice` or `number`.
    pub kind: String,
    pub default: Option<serde_json::Value>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepInfo {
    pub number: i32,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: JobId,
    pub run_id: RunId,
    pub repo_id: RepoId,
    pub key: String,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub labels: Vec<String>,
    pub runner_id: Option<RunnerId>,
    pub runner_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub steps: Vec<StepInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub id: i64,
    pub run_id: RunId,
    pub name: String,
    pub size: i64,
    pub expired: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSummary {
    pub id: RunnerId,
    pub name: String,
    pub labels: Vec<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
    pub repo_id: Option<RepoId>,
    /// `online`, `busy` or `offline`.
    pub status: String,
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Whether a repository's workflows may run, and if not, why. The Actions tab shows the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiAvailability {
    Enabled,
    /// The owner has not been marked `can_run_ci` by a site admin.
    OwnerNotAllowed,
    /// Actions is switched off in the repository settings.
    TurnedOff,
}

/// What making a repository public would expose, shown on the confirm page (spec section 6).
/// `snapshot` is a hash of the refs and the totals: the page sends it back, and a change in
/// between shows the page again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublishSummary {
    pub branches: i64,
    pub tags: i64,
    pub pull_refs: i64,
    pub size_kb: i64,
    pub issues: i64,
    pub pulls: i64,
    pub comments: i64,
    pub comments_by_others: i64,
    pub releases: i64,
    pub release_files: i64,
    pub forks: i64,
    pub pulls_from_private_forks: i64,
    pub linked_packages: Vec<String>,
    pub snapshot: String,
}

#[cfg(test)]
mod visibility_tests {
    use super::*;

    #[test]
    fn three_tiers_round_trip_and_nothing_else_parses() {
        for v in [Visibility::Private, Visibility::Internal, Visibility::Public] {
            assert_eq!(Visibility::parse(v.as_str()), Some(v));
            assert_eq!(serde_json::to_string(&v).unwrap_or_default(), format!("\"{}\"", v.as_str()));
        }
        for bad in ["Public", "PRIVATE", "", "secret", " public", "public "] {
            assert_eq!(Visibility::parse(bad), None, "{bad:?}");
        }
        assert!(Visibility::Private < Visibility::Internal && Visibility::Internal < Visibility::Public);
        assert!(Visibility::Internal.is_private_flag(), "the REST flag is true unless public");
        assert!(!Visibility::Public.is_private_flag());
    }

    #[test]
    fn contribution_policy_round_trips() {
        assert_eq!(ContributionPolicy::default(), ContributionPolicy::Everyone);
        for p in [ContributionPolicy::Everyone, ContributionPolicy::Grants] {
            assert_eq!(ContributionPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(ContributionPolicy::parse("Everyone"), None);
        assert_eq!(ContributionPolicy::parse(""), None);
    }
}

// ---------------------------------------------------------------- browser workspace

/// Caps on one workspace tree listing. Past either, the listing stops and is marked cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceTreeLimits {
    pub max_entries: usize,
    /// Bytes of `ls-tree` output.
    pub max_bytes: usize,
}

impl Default for WorkspaceTreeLimits {
    fn default() -> Self {
        Self { max_entries: 100_000, max_bytes: 16 * 1024 * 1024 }
    }
}

/// One entry of a recursive workspace tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    /// The path's raw bytes, standard base64. The only safe way to name a non-UTF-8 path.
    pub path_b64: String,
    /// The path as text, when its bytes are valid UTF-8.
    pub path: Option<String>,
    /// `100644`, `100755`, `120000` (symlink), `160000` (submodule) or `040000` (folder).
    pub mode: String,
    /// `blob`, `tree` or `commit`.
    pub kind: String,
    pub sha: String,
    /// Blob size in bytes. `None` for folders and submodules.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTree {
    pub entries: Vec<WorkspaceEntry>,
    /// The listing hit a cap and is incomplete. A cut tree is read only.
    pub cut: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceSnapshotLimits {
    pub max_files: usize,
    pub max_bytes: u64,
}

impl Default for WorkspaceSnapshotLimits {
    fn default() -> Self {
        Self { max_files: 2000, max_bytes: 25 * 1024 * 1024 }
    }
}

/// One regular file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshotFile {
    /// UTF-8 path from the repository root.
    pub path: String,
    pub executable: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub files: Vec<WorkspaceSnapshotFile>,
    /// Over `max_files` or `max_bytes`: `files` is empty and the two totals say how much there was.
    pub too_large: bool,
    pub total_files: usize,
    pub total_bytes: u64,
    /// Paths left out: symlinks, submodules and names that are not UTF-8. Count only.
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBlob {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub mode: String,
    /// Bigger than the cap: `bytes` is empty.
    pub too_large: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceSearchLimits {
    pub max_hits: usize,
    pub max_bytes: usize,
    pub timeout_secs: u64,
}

impl Default for WorkspaceSearchLimits {
    fn default() -> Self {
        Self { max_hits: 200, max_bytes: 8 * 1024 * 1024, timeout_secs: 5 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceHit {
    /// Raw path bytes, standard base64.
    pub path_b64: String,
    pub path: Option<String>,
    /// 1 based.
    pub line: u64,
    /// The matching line, at most 300 characters.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSearch {
    pub hits: Vec<WorkspaceHit>,
    /// A cap was hit (hits, bytes or time). The page says the results are cut.
    pub cut: bool,
    pub timed_out: bool,
}

/// The site admin's switches for the browser workspace, stored in `instance_settings` under
/// the key `workspace`. The config file is the ceiling (`WorkspaceConfig`). Both start on now
/// that each has been reviewed: the editor at spec step 15, the terminal at step 19.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSettings {
    pub editor: bool,
    pub terminal: bool,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self { editor: true, terminal: true }
    }
}

impl WorkspaceSettings {
    /// The editor is on: allowed by the config file and switched on by an admin.
    pub fn editor_on(&self, cfg: &crate::config::Config) -> bool {
        cfg.workspace.enabled && self.editor
    }
    /// The terminal is on: the editor is on, and the terminal is allowed and switched on.
    pub fn terminal_on(&self, cfg: &crate::config::Config) -> bool {
        self.editor_on(cfg) && cfg.workspace.terminal && self.terminal
    }
}
