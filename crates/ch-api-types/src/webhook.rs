//! Webhook event payloads Codehaven POSTs to consumer endpoints, with GitHub's field names so existing readers keep working.

use serde::{Deserialize, Serialize};

use crate::ci::{CheckRun, Workflow, WorkflowJob, WorkflowRun};
use crate::common::SimpleUser;
use crate::hooks::Hook;
use crate::issues::{Issue, IssueComment, Label};
use crate::pulls::{PullRequest, Review, ReviewComment};
use crate::releases::Release;
use crate::repos::Repository;

/// Event names Codehaven sends with webhook deliveries.
pub mod event {
    pub const PING: &str = "ping";
    pub const PUSH: &str = "push";
    pub const CREATE: &str = "create";
    pub const DELETE: &str = "delete";
    pub const ISSUES: &str = "issues";
    pub const ISSUE_COMMENT: &str = "issue_comment";
    pub const PULL_REQUEST: &str = "pull_request";
    pub const PULL_REQUEST_REVIEW: &str = "pull_request_review";
    pub const PULL_REQUEST_REVIEW_COMMENT: &str = "pull_request_review_comment";
    pub const RELEASE: &str = "release";
    pub const WORKFLOW_RUN: &str = "workflow_run";
    pub const WORKFLOW_JOB: &str = "workflow_job";
    pub const CHECK_RUN: &str = "check_run";
    pub const STATUS: &str = "status";
    pub const MEMBER: &str = "member";
    pub const PACKAGE: &str = "package";

    pub const ALL: &[&str] = &[
        PING,
        PUSH,
        CREATE,
        DELETE,
        ISSUES,
        ISSUE_COMMENT,
        PULL_REQUEST,
        PULL_REQUEST_REVIEW,
        PULL_REQUEST_REVIEW_COMMENT,
        RELEASE,
        WORKFLOW_RUN,
        WORKFLOW_JOB,
        CHECK_RUN,
        STATUS,
        MEMBER,
        PACKAGE,
    ];
}

/// Author or committer of a commit in a push payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// A commit as it appears in a push payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushCommit {
    pub id: String,
    pub tree_id: String,
    pub message: String,
    /// ISO 8601 string, not a DateTime: GitHub sends a plain string here.
    pub timestamp: String,
    pub url: String,
    pub author: CommitAuthor,
    pub committer: CommitAuthor,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub modified: Vec<String>,
}

/// Who pushed, from the push event header equivalent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pusher {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// `ping` event body, sent when a hook is created or tested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingPayload {
    pub zen: String,
    pub hook_id: i64,
    pub hook: Hook,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<SimpleUser>,
}

/// `push` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushPayload {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub before: String,
    pub after: String,
    pub created: bool,
    pub deleted: bool,
    pub forced: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    pub compare: String,
    #[serde(default)]
    pub commits: Vec<PushCommit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<PushCommit>,
    pub repository: Repository,
    pub pusher: Pusher,
    pub sender: SimpleUser,
}

/// `create` and `delete` event body: branch or tag created or deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateDeletePayload {
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// `branch` or `tag`.
    pub ref_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `issues` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssuesPayload {
    pub action: String,
    pub issue: Issue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<SimpleUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `issue_comment` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueCommentPayload {
    pub action: String,
    pub issue: Issue,
    pub comment: IssueComment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<serde_json::Value>,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `pull_request` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestPayload {
    pub action: String,
    pub number: i64,
    pub pull_request: PullRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_reviewer: Option<SimpleUser>,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `pull_request_review` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewPayload {
    pub action: String,
    pub review: Review,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `pull_request_review_comment` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewCommentPayload {
    pub action: String,
    pub comment: ReviewComment,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `release` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleasePayload {
    pub action: String,
    pub release: Release,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `workflow_run` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunPayload {
    pub action: String,
    pub workflow_run: WorkflowRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<Workflow>,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `workflow_job` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowJobPayload {
    pub action: String,
    pub workflow_job: WorkflowJob,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `check_run` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckRunPayload {
    pub action: String,
    pub check_run: CheckRun,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `status` event body: a commit status changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusPayload {
    pub id: i64,
    pub sha: String,
    /// Full name of the repository, like GitHub sends.
    pub name: String,
    pub context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_url: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// `member` event body: a collaborator was added or removed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberPayload {
    pub action: String,
    pub member: SimpleUser,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// The package a `package` event is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageInfo {
    pub name: String,
    pub package_type: String,
    pub html_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// `package` event body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackagePayload {
    pub action: String,
    pub package: PackageInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    pub sender: SimpleUser,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repository {
        serde_json::from_value(serde_json::json!({
            "id": 1001,
            "node_id": "R_kgDO",
            "name": "codehaven",
            "full_name": "alice/codehaven",
            "private": true,
            "owner": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "html_url": "https://code.example.com/alice/codehaven",
            "description": "self-hosted forge",
            "fork": false,
            "url": "https://code.example.com/api/v3/repos/alice/codehaven",
            "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
            "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
            "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
            "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
            "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
            "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
            "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
            "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
            "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
            "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
            "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "pushed_at": "2026-01-02T03:04:05Z",
            "git_url": "git://code.example.com/alice/codehaven.git",
            "ssh_url": "git@code.example.com:alice/codehaven.git",
            "clone_url": "https://code.example.com/alice/codehaven.git",
            "svn_url": "https://code.example.com/alice/codehaven",
            "homepage": "",
            "size": 42,
            "stargazers_count": 1,
            "watchers_count": 1,
            "language": "Rust",
            "has_issues": true,
            "has_projects": true,
            "has_downloads": true,
            "has_wiki": true,
            "has_pages": false,
            "has_discussions": false,
            "forks_count": 0,
            "mirror_url": null,
            "archived": false,
            "disabled": false,
            "open_issues_count": 2,
            "license": null,
            "allow_forking": true,
            "is_template": false,
            "web_commit_signoff_required": false,
            "topics": ["forge"],
            "visibility": "private",
            "forks": 0,
            "open_issues": 2,
            "watchers": 1,
            "default_branch": "main",
            "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
            "empty": false,
            "allow_squash_merge": true,
            "allow_merge_commit": true,
            "allow_rebase_merge": false,
            "delete_branch_on_merge": true
        }))
        .unwrap()
    }

    fn issue() -> Issue {
        serde_json::from_value(serde_json::json!({
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/7",
            "repository_url": "https://code.example.com/api/v3/repos/alice/codehaven",
            "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/7/labels{/name}",
            "comments_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/7/comments",
            "events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/7/events",
            "html_url": "https://code.example.com/alice/codehaven/issues/7",
            "id": 2001,
            "node_id": "ISSUE",
            "number": 7,
            "title": "ci red on main",
            "user": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "labels": [
                {
                    "id": 3001,
                    "node_id": "LABEL",
                    "url": "https://code.example.com/api/v3/repos/alice/codehaven/labels/bug",
                    "name": "bug",
                    "color": "d73a4a",
                    "default": true,
                    "description": null
                }
            ],
            "state": "open",
            "locked": false,
            "assignee": null,
            "assignees": [],
            "milestone": null,
            "comments": 1,
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "closed_at": null,
            "author_association": "OWNER",
            "active_lock_reason": null,
            "body": "the build is red",
            "reactions": {
                "url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/7/reactions",
                "total_count": 0,
                "+1": 0,
                "-1": 0,
                "laugh": 0,
                "hooray": 0,
                "confused": 0,
                "heart": 0,
                "rocket": 0,
                "eyes": 0
            }
        }))
        .unwrap()
    }

    fn issue_comment() -> IssueComment {
        serde_json::from_value(serde_json::json!({
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments/7001",
            "html_url": "https://code.example.com/alice/codehaven/issues/7#comment-7001",
            "issue_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/7",
            "id": 7001,
            "node_id": "IC",
            "user": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "author_association": "OWNER",
            "body": "fixed by #9"
        }))
        .unwrap()
    }

    fn label() -> Label {
        serde_json::from_value(serde_json::json!({
            "id": 3001,
            "node_id": "LABEL",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/labels/bug",
            "name": "bug",
            "color": "d73a4a",
            "default": true,
            "description": null
        }))
        .unwrap()
    }

    fn pr() -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9",
            "id": 4001,
            "node_id": "PR",
            "html_url": "https://code.example.com/alice/codehaven/pulls/9",
            "number": 9,
            "state": "open",
            "locked": false,
            "title": "add ci types",
            "user": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "body": "adds the ci DTOs",
            "labels": [],
            "milestone": null,
            "active_lock_reason": null,
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "closed_at": null,
            "merged_at": null,
            "merge_commit_sha": null,
            "assignee": null,
            "assignees": [],
            "requested_reviewers": [],
            "requested_teams": [],
            "head": {
                "label": "alice:ci",
                "ref": "ci",
                "sha": "abc123def4567890abcdef0123456789abcdef01",
                "user": {
                    "login": "alice",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatar/alice.svg",
                    "url": "https://code.example.com/api/v3/users/alice",
                    "html_url": "https://code.example.com/alice",
                    "type": "User",
                    "site_admin": false
                },
                "repo": {
                    "id": 1001,
                    "node_id": "R_kgDO",
                    "name": "codehaven",
                    "full_name": "alice/codehaven",
                    "private": true,
                    "owner": {
                        "login": "alice",
                        "id": 1,
                        "avatar_url": "https://code.example.com/avatar/alice.svg",
                        "url": "https://code.example.com/api/v3/users/alice",
                        "html_url": "https://code.example.com/alice",
                        "type": "User",
                        "site_admin": false
                    },
                    "html_url": "https://code.example.com/alice/codehaven",
                    "description": "self-hosted forge",
                    "fork": false,
                    "url": "https://code.example.com/api/v3/repos/alice/codehaven",
                    "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
                    "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
                    "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
                    "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
                    "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
                    "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
                    "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
                    "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
                    "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
                    "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
                    "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
                    "created_at": "2026-01-02T03:04:05Z",
                    "updated_at": "2026-01-02T03:04:05Z",
                    "pushed_at": "2026-01-02T03:04:05Z",
                    "git_url": "git://code.example.com/alice/codehaven.git",
                    "ssh_url": "git@code.example.com:alice/codehaven.git",
                    "clone_url": "https://code.example.com/alice/codehaven.git",
                    "svn_url": "https://code.example.com/alice/codehaven",
                    "homepage": "",
                    "size": 42,
                    "stargazers_count": 1,
                    "watchers_count": 1,
                    "language": "Rust",
                    "has_issues": true,
                    "has_projects": true,
                    "has_downloads": true,
                    "has_wiki": true,
                    "has_pages": false,
                    "has_discussions": false,
                    "forks_count": 0,
                    "mirror_url": null,
                    "archived": false,
                    "disabled": false,
                    "open_issues_count": 2,
                    "license": null,
                    "allow_forking": true,
                    "is_template": false,
                    "web_commit_signoff_required": false,
                    "topics": ["forge"],
                    "visibility": "private",
                    "forks": 0,
                    "open_issues": 2,
                    "watchers": 1,
                    "default_branch": "main",
                    "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
                    "empty": false,
                    "allow_squash_merge": true,
                    "allow_merge_commit": true,
                    "allow_rebase_merge": false,
                    "delete_branch_on_merge": true
                }
            },
            "base": {
                "label": "alice:main",
                "ref": "main",
                "sha": "40a1234567890abcdef0123456789abcdef012345",
                "user": {
                    "login": "alice",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatar/alice.svg",
                    "url": "https://code.example.com/api/v3/users/alice",
                    "html_url": "https://code.example.com/alice",
                    "type": "User",
                    "site_admin": false
                },
                "repo": {
                    "id": 1001,
                    "node_id": "R_kgDO",
                    "name": "codehaven",
                    "full_name": "alice/codehaven",
                    "private": true,
                    "owner": {
                        "login": "alice",
                        "id": 1,
                        "avatar_url": "https://code.example.com/avatar/alice.svg",
                        "url": "https://code.example.com/api/v3/users/alice",
                        "html_url": "https://code.example.com/alice",
                        "type": "User",
                        "site_admin": false
                    },
                    "html_url": "https://code.example.com/alice/codehaven",
                    "description": "self-hosted forge",
                    "fork": false,
                    "url": "https://code.example.com/api/v3/repos/alice/codehaven",
                    "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
                    "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
                    "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
                    "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
                    "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
                    "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
                    "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
                    "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
                    "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
                    "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
                    "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
                    "created_at": "2026-01-02T03:04:05Z",
                    "updated_at": "2026-01-02T03:04:05Z",
                    "pushed_at": "2026-01-02T03:04:05Z",
                    "git_url": "git://code.example.com/alice/codehaven.git",
                    "ssh_url": "git@code.example.com:alice/codehaven.git",
                    "clone_url": "https://code.example.com/alice/codehaven.git",
                    "svn_url": "https://code.example.com/alice/codehaven",
                    "homepage": "",
                    "size": 42,
                    "stargazers_count": 1,
                    "watchers_count": 1,
                    "language": "Rust",
                    "has_issues": true,
                    "has_projects": true,
                    "has_downloads": true,
                    "has_wiki": true,
                    "has_pages": false,
                    "has_discussions": false,
                    "forks_count": 0,
                    "mirror_url": null,
                    "archived": false,
                    "disabled": false,
                    "open_issues_count": 2,
                    "license": null,
                    "allow_forking": true,
                    "is_template": false,
                    "web_commit_signoff_required": false,
                    "topics": ["forge"],
                    "visibility": "private",
                    "forks": 0,
                    "open_issues": 2,
                    "watchers": 1,
                    "default_branch": "main",
                    "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
                    "empty": false,
                    "allow_squash_merge": true,
                    "allow_merge_commit": true,
                    "allow_rebase_merge": false,
                    "delete_branch_on_merge": true
                }
            },
            "_links": {
                "self": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9" },
                "html": { "href": "https://code.example.com/alice/codehaven/pulls/9" },
                "issue": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/issues/9" },
                "comments": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/issues/9/comments" },
                "review_comments": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9/comments" },
                "review_comment": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/comments{/number}" },
                "commits": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9/commits" },
                "statuses": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/statuses/abc123def4567890abcdef0123456789abcdef01" }
            },
            "author_association": "OWNER",
            "auto_merge": null,
            "draft": false,
            "commits": 1,
            "additions": 10,
            "deletions": 2,
            "changed_files": 1,
            "merged": false,
            "mergeable": true,
            "rebaseable": true,
            "mergeable_state": "clean",
            "merged_by": null,
            "comments": 0,
            "review_comments": 0,
            "maintainer_can_modify": false,
            "comments_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/9/comments",
            "review_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9/comments{/number}",
            "review_comments_url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9/comments",
            "issue_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/9",
            "commits_url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9/commits",
            "statuses_url": "https://code.example.com/api/v3/repos/alice/codehaven/statuses/abc123def4567890abcdef0123456789abcdef01",
            "diff_url": "https://code.example.com/alice/codehaven/pulls/9/files",
            "patch_url": "https://code.example.com/alice/codehaven/pulls/9/files"
        }))
        .unwrap()
    }

    fn review() -> Review {
        serde_json::from_value(serde_json::json!({
            "id": 5001,
            "node_id": "REV",
            "user": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "body": "lgtm",
            "state": "approved",
            "html_url": "https://code.example.com/alice/codehaven/pulls/9#review-5001",
            "pull_request_url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9",
            "submitted_at": "2026-01-02T03:04:05Z",
            "commit_id": "abc123def4567890abcdef0123456789abcdef01",
            "author_association": "OWNER",
            "_links": {
                "html": { "href": "https://code.example.com/alice/codehaven/pulls/9#review-5001" },
                "pull_request": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9" }
            }
        }))
        .unwrap()
    }

    fn review_comment() -> ReviewComment {
        serde_json::from_value(serde_json::json!({
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/comments/6001",
            "pull_request_review_id": 5001,
            "id": 6001,
            "node_id": "RC",
            "diff_hunk": "@@ -1,1 +1,2 @@ fn main() {}",
            "path": "src/ci.rs",
            "position": 1,
            "original_position": 1,
            "commit_id": "abc123def4567890abcdef0123456789abcdef01",
            "original_commit_id": "abc123def4567890abcdef0123456789abcdef01",
            "user": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "body": "typo in doc comment",
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "html_url": "https://code.example.com/alice/codehaven/pulls/9/files",
            "pull_request_url": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9",
            "author_association": "OWNER",
            "_links": {
                "self": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/comments/6001" },
                "html": { "href": "https://code.example.com/alice/codehaven/pulls/9/files" },
                "pull_request": { "href": "https://code.example.com/api/v3/repos/alice/codehaven/pulls/9" }
            },
            "start_line": null,
            "line": 3,
            "original_line": 3,
            "start_side": null,
            "side": "RIGHT",
            "in_reply_to_id": null
        }))
        .unwrap()
    }

    fn release() -> Release {
        serde_json::from_value(serde_json::json!({
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/releases/8001",
            "html_url": "https://code.example.com/alice/codehaven/releases/v1.0.0",
            "id": 8001,
            "node_id": "REL",
            "author": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "tag_name": "v1.0.0",
            "target_commitish": "main",
            "name": "v1.0.0",
            "draft": false,
            "prerelease": false,
            "created_at": "2026-01-02T03:04:05Z",
            "published_at": "2026-01-02T03:04:05Z",
            "assets": [],
            "tarball_url": "https://code.example.com/api/v3/repos/alice/codehaven/tarball/v1.0.0",
            "zipball_url": "https://code.example.com/api/v3/repos/alice/codehaven/zipball/v1.0.0",
            "upload_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases/1/assets{?name,label}",
            "body": "first release"
        }))
        .unwrap()
    }

    fn push_commit() -> PushCommit {
        let author = CommitAuthor { name: "Alice".into(), email: "alice@example.test".into(), username: Some("alice".into()) };
        PushCommit {
            id: "abc123def4567890abcdef0123456789abcdef01".into(),
            tree_id: "0d1a5e5faabbccdd112233445566778899aabbcc".into(),
            message: "add webhook payload types".into(),
            timestamp: "2026-01-02T03:04:05Z".into(),
            url: "https://code.example.com/alice/codehaven/commit/abc123def4567890abcdef0123456789abcdef01".into(),
            author: author.clone(),
            committer: author,
            added: vec!["crates/ch-api-types/src/webhook.rs".into()],
            removed: vec![],
            modified: vec![],
        }
    }

    fn hook() -> Hook {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "web",
            "active": true,
            "events": ["push"],
            "config": {
                "url": "https://consumer.example/codehaven/hooks",
                "content_type": "json",
                "secret": "hook-secret",
                "insecure_ssl": "0"
            },
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks/1",
            "ping_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks/1/pings",
            "deliveries_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks/1/deliveries",
            "last_response": {
                "code": 200,
                "status": "OK",
                "message": "OK"
            }
        }))
        .unwrap()
    }

    fn workflow_run() -> WorkflowRun {
        serde_json::from_value(serde_json::json!({
            "id": 4101,
            "name": "CI",
            "run_number": 12,
            "run_attempt": 1,
            "event": "push",
            "status": "completed",
            "conclusion": "success",
            "workflow_id": 31,
            "head_branch": "main",
            "head_sha": "abc123def4567890abcdef0123456789abcdef01",
            "path": ".github/workflows/ci.yml",
            "display_title": "add ci types",
            "actor": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "triggering_actor": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "run_started_at": "2026-01-02T03:04:05Z",
            "html_url": "https://code.example.com/alice/codehaven/actions/runs/4101",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/4101",
            "jobs_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/4101/jobs",
            "logs_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/4101/logs",
            "cancel_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/4101/cancel",
            "rerun_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/4101/rerun",
            "repository": {
                "id": 1001,
                "node_id": "R_kgDO",
                "name": "codehaven",
                "full_name": "alice/codehaven",
                "private": true,
                "owner": {
                    "login": "alice",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatar/alice.svg",
                    "url": "https://code.example.com/api/v3/users/alice",
                    "html_url": "https://code.example.com/alice",
                    "type": "User",
                    "site_admin": false
                },
                "html_url": "https://code.example.com/alice/codehaven",
                "description": "self-hosted forge",
                "fork": false,
                "url": "https://code.example.com/api/v3/repos/alice/codehaven",
                "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
                "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
                "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
                "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
                "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
                "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
                "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
                "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
                "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
                "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
                "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
                "created_at": "2026-01-02T03:04:05Z",
                "updated_at": "2026-01-02T03:04:05Z",
                "pushed_at": "2026-01-02T03:04:05Z",
                "git_url": "git://code.example.com/alice/codehaven.git",
                "ssh_url": "git@code.example.com:alice/codehaven.git",
                "clone_url": "https://code.example.com/alice/codehaven.git",
                "svn_url": "https://code.example.com/alice/codehaven",
                "homepage": "",
                "size": 42,
                "stargazers_count": 1,
                "watchers_count": 1,
                "language": "Rust",
                "has_issues": true,
                "has_projects": true,
                "has_downloads": true,
                "has_wiki": true,
                "has_pages": false,
                "has_discussions": false,
                "forks_count": 0,
                "mirror_url": null,
                "archived": false,
                "disabled": false,
                "open_issues_count": 2,
                "license": null,
                "allow_forking": true,
                "is_template": false,
                "web_commit_signoff_required": false,
                "topics": ["forge"],
                "visibility": "private",
                "forks": 0,
                "open_issues": 2,
                "watchers": 1,
                "default_branch": "main",
                "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
                "empty": false,
                "allow_squash_merge": true,
                "allow_merge_commit": true,
                "allow_rebase_merge": false,
                "delete_branch_on_merge": true
            }
        }))
        .unwrap()
    }

    fn workflow_job() -> WorkflowJob {
        serde_json::from_value(serde_json::json!({
            "id": 4201,
            "run_id": 4101,
            "run_attempt": 1,
            "name": "build",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-01-02T03:04:05Z",
            "completed_at": "2026-01-02T03:04:05Z",
            "steps": [
                {
                    "name": "check",
                    "status": "completed",
                    "conclusion": "success",
                    "number": 1,
                    "started_at": "2026-01-02T03:04:05Z",
                    "completed_at": "2026-01-02T03:04:05Z"
                }
            ],
            "labels": ["self-hosted"],
            "runner_id": 1,
            "runner_name": "runner-1",
            "html_url": "https://code.example.com/alice/codehaven/actions/runs/4101/jobs/4201",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/jobs/4201",
            "check_run_url": "https://code.example.com/api/v3/repos/alice/codehaven/check-runs/4201"
        }))
        .unwrap()
    }

    fn check_run() -> CheckRun {
        serde_json::from_value(serde_json::json!({
            "id": 4301,
            "name": "build",
            "head_sha": "abc123def4567890abcdef0123456789abcdef01",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-01-02T03:04:05Z",
            "completed_at": "2026-01-02T03:04:05Z",
            "details_url": "https://ci.example/codehaven/builds/4301",
            "external_id": "build-4301",
            "output": {
                "title": "build",
                "summary": "cargo build and test passed",
                "text": "42 tests passed"
            },
            "html_url": "https://code.example.com/alice/codehaven/runs/4301",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/check-runs/4301"
        }))
        .unwrap()
    }

    #[test]
    fn ping_payload_roundtrip() {
        let value = PingPayload {
            zen: "Keep it logically simple.".into(),
            hook_id: 7,
            hook: hook(),
            repository: Some(repo()),
            sender: Some(crate::user("alice", 1)),
        };
        crate::roundtrip("webhook_ping_payload", &value);
    }

    #[test]
    fn push_payload_roundtrip() {
        let value = PushPayload {
            git_ref: "refs/heads/main".into(),
            before: "40a1234567890abcdef0123456789abcdef012345".into(),
            after: "abc123def4567890abcdef0123456789abcdef01".into(),
            created: false,
            deleted: false,
            forced: false,
            base_ref: None,
            compare: "https://code.example.com/alice/codehaven/compare/40a1234...abc1234".into(),
            commits: vec![push_commit()],
            head_commit: Some(push_commit()),
            repository: repo(),
            pusher: Pusher { name: "alice".into(), email: Some("alice@example.test".into()) },
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_push_payload", &value);
    }

    #[test]
    fn create_delete_payload_roundtrip() {
        let value = CreateDeletePayload {
            git_ref: "refs/heads/ci".into(),
            ref_type: "branch".into(),
            master_branch: Some("main".into()),
            description: Some("self-hosted forge".into()),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_create_delete_payload", &value);
    }

    #[test]
    fn issues_payload_roundtrip() {
        let value = IssuesPayload {
            action: "opened".into(),
            issue: issue(),
            changes: None,
            assignee: Some(crate::user("alice", 1)),
            label: Some(label()),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_issues_payload", &value);
    }

    #[test]
    fn issue_comment_payload_roundtrip() {
        let value = IssueCommentPayload {
            action: "created".into(),
            issue: issue(),
            comment: issue_comment(),
            changes: None,
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_issue_comment_payload", &value);
    }

    #[test]
    fn pull_request_payload_roundtrip() {
        let value = PullRequestPayload {
            action: "opened".into(),
            number: 9,
            pull_request: pr(),
            changes: None,
            requested_reviewer: Some(crate::user("alice", 1)),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_pull_request_payload", &value);
    }

    #[test]
    fn pull_request_review_payload_roundtrip() {
        let value = PullRequestReviewPayload {
            action: "submitted".into(),
            review: review(),
            pull_request: pr(),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_pull_request_review_payload", &value);
    }

    #[test]
    fn pull_request_review_comment_payload_roundtrip() {
        let value = PullRequestReviewCommentPayload {
            action: "created".into(),
            comment: review_comment(),
            pull_request: pr(),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_pull_request_review_comment_payload", &value);
    }

    #[test]
    fn release_payload_roundtrip() {
        let value = ReleasePayload { action: "published".into(), release: release(), repository: repo(), sender: crate::user("alice", 1) };
        crate::roundtrip("webhook_release_payload", &value);
    }

    #[test]
    fn workflow_run_payload_roundtrip() {
        let value = WorkflowRunPayload {
            action: "completed".into(),
            workflow_run: workflow_run(),
            workflow: None,
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_workflow_run_payload", &value);
    }

    #[test]
    fn workflow_job_payload_roundtrip() {
        let value = WorkflowJobPayload {
            action: "completed".into(),
            workflow_job: workflow_job(),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_workflow_job_payload", &value);
    }

    #[test]
    fn check_run_payload_roundtrip() {
        let value =
            CheckRunPayload { action: "completed".into(), check_run: check_run(), repository: repo(), sender: crate::user("alice", 1) };
        crate::roundtrip("webhook_check_run_payload", &value);
    }

    #[test]
    fn status_payload_roundtrip() {
        let value = StatusPayload {
            id: 4401,
            sha: "abc123def4567890abcdef0123456789abcdef01".into(),
            name: "alice/codehaven".into(),
            context: "ci/build".into(),
            description: Some("build passed".into()),
            state: "success".into(),
            target_url: Some("https://ci.example/codehaven/builds/4401".into()),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            repository: repo(),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_status_payload", &value);
    }

    #[test]
    fn member_payload_roundtrip() {
        let value =
            MemberPayload { action: "added".into(), member: crate::user("alice", 1), repository: repo(), sender: crate::user("alice", 1) };
        crate::roundtrip("webhook_member_payload", &value);
    }

    #[test]
    fn package_payload_roundtrip() {
        let value = PackagePayload {
            action: "published".into(),
            package: PackageInfo {
                name: "codehaven".into(),
                package_type: "container".into(),
                html_url: "https://code.example.com/alice/codehaven/pkgs/container/codehaven".into(),
                tag: Some("v1.0.0".into()),
            },
            repository: Some(repo()),
            sender: crate::user("alice", 1),
        };
        crate::roundtrip("webhook_package_payload", &value);
    }

    #[test]
    fn webhook_event_names() {
        assert_eq!(
            event::ALL,
            &[
                event::PING,
                event::PUSH,
                event::CREATE,
                event::DELETE,
                event::ISSUES,
                event::ISSUE_COMMENT,
                event::PULL_REQUEST,
                event::PULL_REQUEST_REVIEW,
                event::PULL_REQUEST_REVIEW_COMMENT,
                event::RELEASE,
                event::WORKFLOW_RUN,
                event::WORKFLOW_JOB,
                event::CHECK_RUN,
                event::STATUS,
                event::MEMBER,
                event::PACKAGE,
            ]
        );
    }

    #[test]
    fn parses_github_push_json() {
        let raw = r#"{
            "ref": "refs/heads/main",
            "before": "40a1234567890abcdef0123456789abcdef012345",
            "after": "abc123def4567890abcdef0123456789abcdef01",
            "created": false,
            "deleted": false,
            "forced": false,
            "base_ref": null,
            "compare": "https://code.example.com/alice/codehaven/compare/40a1234...abc1234",
            "commits": [
                {
                    "id": "abc123def4567890abcdef0123456789abcdef01",
                    "tree_id": "0d1a5e5faabbccdd112233445566778899aabbcc",
                    "message": "add webhook payload types",
                    "timestamp": "2026-01-02T03:04:05Z",
                    "url": "https://code.example.com/alice/codehaven/commit/abc123def4567890abcdef0123456789abcdef01",
                    "author": {
                        "name": "Alice",
                        "email": "alice@example.test",
                        "username": "alice"
                    },
                    "committer": {
                        "name": "Alice",
                        "email": "alice@example.test",
                        "username": "alice"
                    },
                    "added": [],
                    "removed": [],
                    "modified": []
                }
            ],
            "head_commit": {
                "id": "abc123def4567890abcdef0123456789abcdef01",
                "tree_id": "0d1a5e5faabbccdd112233445566778899aabbcc",
                "message": "add webhook payload types",
                "timestamp": "2026-01-02T03:04:05Z",
                "url": "https://code.example.com/alice/codehaven/commit/abc123def4567890abcdef0123456789abcdef01",
                "author": {
                    "name": "Alice",
                    "email": "alice@example.test",
                    "username": "alice"
                },
                "committer": {
                    "name": "Alice",
                    "email": "alice@example.test",
                    "username": "alice"
                },
                "added": [],
                "removed": [],
                "modified": []
            },
            "repository": {
                "id": 1001,
                "node_id": "R_kgDO",
                "name": "codehaven",
                "full_name": "alice/codehaven",
                "private": true,
                "owner": {
                    "login": "alice",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatar/alice.svg",
                    "url": "https://code.example.com/api/v3/users/alice",
                    "html_url": "https://code.example.com/alice",
                    "type": "User",
                    "site_admin": false
                },
                "html_url": "https://code.example.com/alice/codehaven",
                "description": "self-hosted forge",
                "fork": false,
                "url": "https://code.example.com/api/v3/repos/alice/codehaven",
                "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
                "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
                "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
                "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
                "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
                "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
                "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
                "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
                "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
                "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
                "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
                "created_at": "2026-01-02T03:04:05Z",
                "updated_at": "2026-01-02T03:04:05Z",
                "pushed_at": "2026-01-02T03:04:05Z",
                "git_url": "git://code.example.com/alice/codehaven.git",
                "ssh_url": "git@code.example.com:alice/codehaven.git",
                "clone_url": "https://code.example.com/alice/codehaven.git",
                "svn_url": "https://code.example.com/alice/codehaven",
                "homepage": "",
                "size": 42,
                "stargazers_count": 1,
                "watchers_count": 1,
                "language": "Rust",
                "has_issues": true,
                "has_projects": true,
                "has_downloads": true,
                "has_wiki": true,
                "has_pages": false,
                "has_discussions": false,
                "forks_count": 0,
                "mirror_url": null,
                "archived": false,
                "disabled": false,
                "open_issues_count": 2,
                "license": null,
                "allow_forking": true,
                "is_template": false,
                "web_commit_signoff_required": false,
                "topics": ["forge"],
                "visibility": "private",
                "forks": 0,
                "open_issues": 2,
                "watchers": 1,
                "default_branch": "main",
                "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
                "empty": false,
                "allow_squash_merge": true,
                "allow_merge_commit": true,
                "allow_rebase_merge": false,
                "delete_branch_on_merge": true
            },
            "pusher": {
                "name": "alice",
                "email": "alice@example.test"
            },
            "sender": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/alice.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "unknown_future_field": {
                "x": 1
            }
        }"#;
        let payload = serde_json::from_str::<PushPayload>(raw).unwrap();
        assert_eq!(payload.after, "abc123def4567890abcdef0123456789abcdef01");
        assert_eq!(payload.git_ref, "refs/heads/main");
        assert_eq!(payload.commits.len(), 1);
    }
}
