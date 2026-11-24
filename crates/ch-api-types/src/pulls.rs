//! Pull request types: branches, reviews, review comments and their request bodies.

use crate::common::SimpleUser;
use crate::issues::{Label, Milestone, ReactionRollup};
use crate::repos::Repository;
use crate::users::Team;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The head or base branch embedded in a `PullRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullBranch {
    pub label: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub user: SimpleUser,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<Repository>,
}

/// The `auto_merge` block of a pull request, from `GET /repos/{owner}/{repo}/pulls/{number}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoMerge {
    pub merge_method: String,
    pub enabled_by: SimpleUser,
}

/// One link in a pull request's `_links`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub href: String,
}

/// A pull request's `_links`, as GitHub sends them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullLinks {
    #[serde(rename = "self")]
    pub self_link: Link,
    pub html: Link,
    pub issue: Link,
    pub comments: Link,
    pub review_comments: Link,
    pub review_comment: Link,
    pub commits: Link,
    pub statuses: Link,
}

/// `GET /repos/{owner}/{repo}/pulls/{number}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub url: String,
    pub id: i64,
    pub number: i64,
    pub html_url: String,
    pub diff_url: String,
    pub patch_url: String,
    pub state: String,
    pub locked: bool,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub user: SimpleUser,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub milestone: Option<Milestone>,
    #[serde(default)]
    pub assignees: Vec<SimpleUser>,
    #[serde(default)]
    pub requested_reviewers: Vec<SimpleUser>,
    pub head: PullBranch,
    pub base: PullBranch,
    pub draft: bool,
    pub merged: bool,
    #[serde(default)]
    pub mergeable: Option<bool>,
    /// Whether the branch can be rebased onto the base cleanly. Codehaven answers as for
    /// `mergeable`, since a clean merge here means no conflicting change.
    #[serde(default)]
    pub rebaseable: Option<bool>,
    pub mergeable_state: String,
    #[serde(default)]
    pub merged_by: Option<SimpleUser>,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    pub comments: u32,
    pub review_comments: u32,
    pub commits: u32,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub merged_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub auto_merge: Option<AutoMerge>,
    #[serde(default, rename = "_links", skip_serializing_if = "Option::is_none")]
    pub links: Option<PullLinks>,
}

/// Response of `PUT /repos/{owner}/{repo}/pulls/{number}/merge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResult {
    pub sha: String,
    pub merged: bool,
    pub message: String,
}

/// One entry of `GET /repos/{owner}/{repo}/pulls/{number}/files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullFile {
    pub sha: String,
    pub filename: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub changes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_filename: Option<String>,
    pub blob_url: String,
    pub raw_url: String,
}

/// One review of a pull request, from `GET /repos/{owner}/{repo}/pulls/{number}/reviews`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: i64,
    pub user: SimpleUser,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<DateTime<Utc>>,
    pub html_url: String,
    pub pull_request_url: String,
    pub author_association: String,
}

/// `GET /repos/{owner}/{repo}/pulls/comments/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_review_id: Option<i64>,
    pub diff_hunk: String,
    pub path: String,
    pub commit_id: String,
    pub original_commit_id: String,
    pub user: SimpleUser,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub html_url: String,
    pub pull_request_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<ReactionRollup>,
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestedReviewers {
    #[serde(default)]
    pub users: Vec<SimpleUser>,
    #[serde(default)]
    pub teams: Vec<Team>,
}

/// Body of `POST /repos/{owner}/{repo}/pulls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePull {
    pub title: String,
    pub head: String,
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintainer_can_modify: Option<bool>,
}

/// Body of `PATCH /repos/{owner}/{repo}/pulls/{number}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePull {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// Body of `PUT /repos/{owner}/{repo}/pulls/{number}/merge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePull {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

/// One pending comment inside a `CreateReview` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftReviewComment {
    pub path: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
}

/// Body of `POST /repos/{owner}/{repo}/pulls/{number}/reviews`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default)]
    pub comments: Vec<DraftReviewComment>,
}

/// Body of `POST /repos/{owner}/{repo}/pulls/{number}/comments`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewComment {
    pub body: String,
    pub commit_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<i64>,
}

/// Body of `POST /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestReviewers {
    #[serde(default)]
    pub reviewers: Vec<String>,
    #[serde(default)]
    pub team_reviewers: Vec<String>,
}

/// Body of `POST /repos/{owner}/{repo}/pulls/{number}/reviews/{id}/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub event: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(label: &str, refname: &str, sha: &str) -> PullBranch {
        PullBranch { label: label.into(), git_ref: refname.into(), sha: sha.into(), user: crate::user("octocat", 1), repo: None }
    }

    #[test]
    fn pulls_pull_branch() {
        let b = branch("octocat:topic", "topic", "aaa111");
        crate::roundtrip("pulls_pull_branch", &b);
    }

    #[test]
    fn pulls_auto_merge() {
        let a = AutoMerge { merge_method: "squash".into(), enabled_by: crate::user("octocat", 1) };
        crate::roundtrip("pulls_auto_merge", &a);
    }

    #[test]
    fn pulls_pull_request() {
        let label = Label {
            id: 7,
            name: "bug".into(),
            color: "d73a4a".into(),
            description: Some("Something is not working".into()),
            default: false,
            url: "https://code.example.com/api/v3/repos/octo/haven/labels/bug".into(),
        };
        let pr = PullRequest {
            url: "https://code.example.com/api/v3/repos/octo/haven/pulls/1".into(),
            id: 1001,
            number: 1,
            html_url: "https://code.example.com/octo/haven/pulls/1".into(),
            diff_url: "https://code.example.com/octo/haven/pulls/1/files".into(),
            patch_url: "https://code.example.com/octo/haven/pulls/1/files".into(),
            state: "open".into(),
            locked: false,
            title: "Fix the thing".into(),
            body: Some("Please review".into()),
            user: crate::user("octocat", 1),
            labels: vec![label],
            milestone: None,
            assignees: vec![],
            requested_reviewers: vec![crate::user("hubot", 2)],
            head: branch("octocat:topic", "topic", "aaa111"),
            base: branch("octo:main", "main", "bbb222"),
            draft: false,
            merged: false,
            mergeable: Some(true),
            rebaseable: None,
            mergeable_state: "clean".into(),
            merged_by: None,
            merge_commit_sha: None,
            comments: 0,
            review_comments: 0,
            commits: 1,
            additions: 3,
            deletions: 1,
            changed_files: 1,
            created_at: crate::t0(),
            updated_at: crate::t0(),
            closed_at: None,
            merged_at: None,
            auto_merge: None,
            links: None,
        };
        crate::roundtrip("pulls_pull_request", &pr);
    }

    #[test]
    fn pulls_merge_result() {
        let m = MergeResult { sha: "abc123".into(), merged: true, message: "Pull Request successfully merged".into() };
        crate::roundtrip("pulls_merge_result", &m);
    }

    #[test]
    fn pulls_pull_file() {
        let f = PullFile {
            sha: "def456".into(),
            filename: "src/main.rs".into(),
            status: "modified".into(),
            additions: 3,
            deletions: 1,
            changes: 4,
            patch: Some("@@ -1,3 +1,5 @@".into()),
            previous_filename: None,
            blob_url: "https://code.example.com/octo/haven/blob/aaa111/src/main.rs".into(),
            raw_url: "https://code.example.com/octo/haven/raw/aaa111/src/main.rs".into(),
        };
        crate::roundtrip("pulls_pull_file", &f);
    }

    #[test]
    fn pulls_review() {
        let r = Review {
            id: 55,
            user: crate::user("hubot", 2),
            body: Some("Looks great".into()),
            state: "APPROVED".into(),
            commit_id: Some("abc123".into()),
            submitted_at: Some(crate::t0()),
            html_url: "https://code.example.com/octo/haven/pulls/1#review-55".into(),
            pull_request_url: "https://code.example.com/api/v3/repos/octo/haven/pulls/1".into(),
            author_association: "CONTRIBUTOR".into(),
        };
        crate::roundtrip("pulls_review", &r);
    }

    #[test]
    fn pulls_review_comment() {
        let c = ReviewComment {
            id: 88,
            pull_request_review_id: Some(55),
            diff_hunk: "@@ -1,3 +1,5 @@".into(),
            path: "src/main.rs".into(),
            commit_id: "abc123".into(),
            original_commit_id: "abc123".into(),
            user: crate::user("hubot", 2),
            body: "Use the helper here".into(),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            html_url: "https://code.example.com/octo/haven/pulls/1/files".into(),
            pull_request_url: "https://code.example.com/api/v3/repos/octo/haven/pulls/1".into(),
            line: Some(5),
            original_line: Some(3),
            start_line: None,
            side: Some("RIGHT".into()),
            in_reply_to_id: None,
            position: Some(4),
            reactions: Some(ReactionRollup {
                url: "https://code.example.com/api/v3/repos/octo/haven/pulls/comments/88/reactions".into(),
                total_count: 1,
                plus_one: 1,
                minus_one: 0,
                laugh: 0,
                confused: 0,
                heart: 0,
                hooray: 0,
                rocket: 0,
                eyes: 0,
            }),
        };
        crate::roundtrip("pulls_review_comment", &c);
    }

    #[test]
    fn pulls_requested_reviewers() {
        let r = RequestedReviewers { users: vec![crate::user("hubot", 2)], teams: vec![] };
        crate::roundtrip("pulls_requested_reviewers", &r);
    }

    #[test]
    fn pulls_create_pull() {
        let c = CreatePull {
            title: "Fix the thing".into(),
            head: "octocat:topic".into(),
            base: "main".into(),
            body: Some("Please review".into()),
            draft: Some(false),
            head_repo: None,
            maintainer_can_modify: Some(true),
        };
        crate::roundtrip("pulls_create_pull", &c);
    }

    #[test]
    fn pulls_update_pull() {
        let u = UpdatePull { title: Some("New title".into()), body: None, state: Some("closed".into()), base: None };
        crate::roundtrip("pulls_update_pull", &u);
    }

    #[test]
    fn pulls_merge_pull() {
        let m = MergePull {
            merge_method: Some("squash".into()),
            commit_title: Some("Squash the thing".into()),
            commit_message: None,
            sha: None,
        };
        crate::roundtrip("pulls_merge_pull", &m);
    }

    #[test]
    fn pulls_draft_review_comment() {
        let c = DraftReviewComment {
            path: "src/main.rs".into(),
            body: "Use the helper here".into(),
            line: Some(5),
            side: Some("RIGHT".into()),
            start_line: None,
            start_side: None,
        };
        crate::roundtrip("pulls_draft_review_comment", &c);
    }

    #[test]
    fn pulls_create_review() {
        let r = CreateReview {
            commit_id: Some("abc123".into()),
            body: Some("Looks great".into()),
            event: Some("APPROVE".into()),
            comments: vec![DraftReviewComment {
                path: "src/main.rs".into(),
                body: "Nit".into(),
                line: Some(5),
                side: None,
                start_line: None,
                start_side: None,
            }],
        };
        crate::roundtrip("pulls_create_review", &r);
    }

    #[test]
    fn pulls_create_review_comment() {
        let c = CreateReviewComment {
            body: "Use the helper here".into(),
            commit_id: "abc123".into(),
            path: "src/main.rs".into(),
            line: Some(5),
            side: Some("RIGHT".into()),
            start_line: None,
            start_side: None,
            in_reply_to: None,
        };
        crate::roundtrip("pulls_create_review_comment", &c);
    }

    #[test]
    fn pulls_request_reviewers() {
        let r = RequestReviewers { reviewers: vec!["hubot".into()], team_reviewers: vec![] };
        crate::roundtrip("pulls_request_reviewers", &r);
    }

    #[test]
    fn pulls_submit_review() {
        let s = SubmitReview { body: Some("Closing this out".into()), event: "COMMENT".into() };
        crate::roundtrip("pulls_submit_review", &s);
    }

    #[test]
    fn pull_request_from_github_json() {
        let json = r#"{
            "url": "https://code.example.com/api/v3/repos/octo/haven/pulls/1",
            "id": 1001,
            "node_id": "MDExOlB1bGxSZXF1ZXN0MTAwMQ==",
            "number": 1,
            "html_url": "https://code.example.com/octo/haven/pull/1",
            "diff_url": "https://code.example.com/octo/haven/pull/1.diff",
            "patch_url": "https://code.example.com/octo/haven/pull/1.patch",
            "state": "open",
            "locked": false,
            "title": "Fix the thing",
            "body": null,
            "user": {
                "login": "octocat",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/octocat.svg",
                "url": "https://code.example.com/api/v3/users/octocat",
                "html_url": "https://code.example.com/octocat",
                "type": "User",
                "site_admin": false
            },
            "labels": [],
            "milestone": null,
            "assignees": [],
            "requested_reviewers": [],
            "head": {
                "label": "octocat:topic",
                "ref": "topic",
                "sha": "aaa111",
                "user": {
                    "login": "octocat",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatar/octocat.svg",
                    "url": "https://code.example.com/api/v3/users/octocat",
                    "html_url": "https://code.example.com/octocat",
                    "type": "User",
                    "site_admin": false
                },
                "repo": null
            },
            "base": {
                "label": "octo:main",
                "ref": "main",
                "sha": "bbb222",
                "user": {
                    "login": "octocat",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatar/octocat.svg",
                    "url": "https://code.example.com/api/v3/users/octocat",
                    "html_url": "https://code.example.com/octocat",
                    "type": "User",
                    "site_admin": false
                },
                "repo": null
            },
            "author_association": "OWNER",
            "draft": false,
            "merged": false,
            "mergeable": null,
            "mergeable_state": "unknown",
            "merged_by": null,
            "merge_commit_sha": null,
            "comments": 0,
            "review_comments": 0,
            "commits": 1,
            "additions": 3,
            "deletions": 1,
            "changed_files": 1,
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "closed_at": null,
            "merged_at": null,
            "auto_merge": null,
            "head_repo": null,
            "references": {}
        }"#;
        let pr: PullRequest = serde_json::from_str(json).expect("valid PR json");
        assert_eq!(pr.number, 1);
        assert!(pr.body.is_none());
        assert_eq!(pr.head.sha, "aaa111");
    }
}
