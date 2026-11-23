//! Issues, labels, milestones, comments, reactions and timelines.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::SimpleUser;

/// A label of a repository, issue or pull request. `GET /repos/{owner}/{repo}/labels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id: i64,
    pub name: String,
    /// Hex color without the leading `#`, like `ff0000`.
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub default: bool,
    pub url: String,
}

/// `GET /repos/{owner}/{repo}/milestones`. A milestone on a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub id: i64,
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `open` or `closed`.
    pub state: String,
    pub open_issues: u32,
    pub closed_issues: u32,
    #[serde(default)]
    pub due_on: Option<DateTime<Utc>>,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub url: String,
    pub html_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<SimpleUser>,
}

/// The minimal pull request object embedded in an `Issue` when the issue is a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub url: String,
    pub html_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<DateTime<Utc>>,
}

/// `GET /repos/{owner}/{repo}/issues/{number}`. An issue, or a pull request's issue view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: i64,
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    pub state: String,
    /// `completed`, `not_planned` or `reopened`.
    #[serde(default)]
    pub state_reason: Option<String>,
    pub locked: bool,
    pub user: SimpleUser,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub assignee: Option<SimpleUser>,
    #[serde(default)]
    pub assignees: Vec<SimpleUser>,
    #[serde(default)]
    pub milestone: Option<Milestone>,
    pub comments: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub closed_by: Option<SimpleUser>,
    pub html_url: String,
    pub url: String,
    pub repository_url: String,
    pub comments_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<ReactionRollup>,
}

/// Reaction counts embedded in an `Issue` or an `IssueComment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionRollup {
    pub url: String,
    pub total_count: i64,
    #[serde(rename = "+1")]
    pub plus_one: i64,
    #[serde(rename = "-1")]
    pub minus_one: i64,
    pub laugh: i64,
    pub confused: i64,
    pub heart: i64,
    pub hooray: i64,
    pub rocket: i64,
    pub eyes: i64,
}

/// `GET /repos/{owner}/{repo}/issues/{number}/reactions`. One reaction on an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub id: i64,
    pub user: SimpleUser,
    /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket` or `eyes`.
    pub content: String,
    pub created_at: DateTime<Utc>,
}

/// `GET /repos/{owner}/{repo}/issues/comments/{id}`. A comment on an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: i64,
    pub body: String,
    pub user: SimpleUser,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub html_url: String,
    pub url: String,
    pub issue_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<ReactionRollup>,
}

/// `GET /repos/{owner}/{repo}/issues/{number}/timeline`. One entry of an issue timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub id: i64,
    /// `cross-referenced`, `assigned`, `labeled` and friends.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<SimpleUser>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<SimpleUser>,
    /// The referencing issue or commit, kept untyped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// `GET /repos/{owner}/{repo}/issues/{number}/events`. One state change of an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueEvent {
    pub id: i64,
    /// `labeled`, `assigned`, `closed` and friends.
    pub event: String,
    pub actor: SimpleUser,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
}

/// `POST /repos/{owner}/{repo}/labels`. Creates a label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateLabel {
    pub name: String,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `PATCH /repos/{owner}/{repo}/labels/{name}`. Edits a label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateLabel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `POST /repos/{owner}/{repo}/milestones`. Creates a milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateMilestone {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_on: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// `PATCH /repos/{owner}/{repo}/milestones/{number}`. Edits a milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateMilestone {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_on: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// `POST /repos/{owner}/{repo}/issues`. Opens an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateIssue {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<i64>,
}

/// `PATCH /repos/{owner}/{repo}/issues/{number}`. Edits an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateIssue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<i64>,
}

/// `POST /repos/{owner}/{repo}/issues/{number}/comments`. Adds a comment to an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateComment {
    pub body: String,
}

/// `POST /repos/{owner}/{repo}/issues/{number}/reactions`. Adds a reaction to an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReaction {
    /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket` or `eyes`.
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_label() {
        let l = Label {
            id: 1,
            name: "bug".into(),
            color: "ff0000".into(),
            description: Some("Something is broken".into()),
            default: false,
            url: "https://code.example.com/api/v3/repos/octo/haven/labels/bug".into(),
        };
        crate::roundtrip("issues_label", &l);
    }

    #[test]
    fn issues_milestone() {
        let m = Milestone {
            id: 7,
            number: 3,
            title: "v1.0".into(),
            description: Some("First release".into()),
            state: "open".into(),
            open_issues: 2,
            closed_issues: 1,
            due_on: Some(crate::t0()),
            closed_at: None,
            created_at: crate::t0(),
            updated_at: crate::t0(),
            url: "https://code.example.com/api/v3/repos/octo/haven/milestones/3".into(),
            html_url: "https://code.example.com/octo/haven/milestone/3".into(),
            creator: Some(crate::user("octocat", 1)),
        };
        crate::roundtrip("issues_milestone", &m);
    }

    #[test]
    fn issues_pull_request_ref() {
        let r = PullRequestRef {
            url: "https://code.example.com/api/v3/repos/octo/haven/pulls/9".into(),
            html_url: "https://code.example.com/octo/haven/pull/9".into(),
            merged_at: Some(crate::t0()),
        };
        crate::roundtrip("issues_pull_request_ref", &r);
    }

    #[test]
    fn issues_issue() {
        let i = Issue {
            id: 100,
            number: 42,
            title: "Found a bug".into(),
            body: Some("Something broke".into()),
            state: "open".into(),
            state_reason: None,
            locked: false,
            user: crate::user("octocat", 1),
            labels: vec![Label {
                id: 2,
                name: "bug".into(),
                color: "ff0000".into(),
                description: None,
                default: false,
                url: "https://code.example.com/api/v3/repos/octo/haven/labels/bug".into(),
            }],
            assignee: None,
            assignees: vec![],
            milestone: None,
            comments: 3,
            created_at: crate::t0(),
            updated_at: crate::t0(),
            closed_at: None,
            closed_by: None,
            html_url: "https://code.example.com/octo/haven/issues/42".into(),
            url: "https://code.example.com/api/v3/repos/octo/haven/issues/42".into(),
            repository_url: "https://code.example.com/api/v3/repos/octo/haven".into(),
            comments_url: "https://code.example.com/api/v3/repos/octo/haven/issues/42/comments".into(),
            pull_request: None,
            reactions: Some(ReactionRollup {
                url: "https://code.example.com/api/v3/repos/octo/haven/issues/42/reactions".into(),
                total_count: 5,
                plus_one: 3,
                minus_one: 0,
                laugh: 1,
                confused: 0,
                heart: 1,
                hooray: 0,
                rocket: 0,
                eyes: 0,
            }),
        };
        crate::roundtrip("issues_issue", &i);
    }

    #[test]
    fn issues_reaction_rollup() {
        let r = ReactionRollup {
            url: "https://code.example.com/api/v3/repos/octo/haven/issues/42/reactions".into(),
            total_count: 2,
            plus_one: 1,
            minus_one: 0,
            laugh: 1,
            confused: 0,
            heart: 0,
            hooray: 0,
            rocket: 0,
            eyes: 0,
        };
        crate::roundtrip("issues_reaction_rollup", &r);
    }

    #[test]
    fn issues_reaction() {
        let r = Reaction { id: 8, user: crate::user("octocat", 1), content: "+1".into(), created_at: crate::t0() };
        crate::roundtrip("issues_reaction", &r);
    }

    #[test]
    fn issues_issue_comment() {
        let c = IssueComment {
            id: 11,
            body: "Me too".into(),
            user: crate::user("octocat", 1),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            html_url: "https://code.example.com/octo/haven/issues/42#issuecomment-11".into(),
            url: "https://code.example.com/api/v3/repos/octo/haven/issues/comments/11".into(),
            issue_url: "https://code.example.com/api/v3/repos/octo/haven/issues/42".into(),
            reactions: None,
        };
        crate::roundtrip("issues_issue_comment", &c);
    }

    #[test]
    fn issues_timeline_event() {
        let e = TimelineEvent {
            id: 21,
            event: "labeled".into(),
            actor: Some(crate::user("octocat", 1)),
            created_at: crate::t0(),
            label: Some(Label {
                id: 2,
                name: "bug".into(),
                color: "ff0000".into(),
                description: None,
                default: false,
                url: "https://code.example.com/api/v3/repos/octo/haven/labels/bug".into(),
            }),
            assignee: None,
            source: Some(serde_json::json!({"issue": {"number": 7}})),
            commit_id: None,
            body: None,
        };
        crate::roundtrip("issues_timeline_event", &e);
    }

    #[test]
    fn issues_issue_event() {
        let e = IssueEvent {
            id: 30,
            event: "closed".into(),
            actor: crate::user("octocat", 1),
            created_at: crate::t0(),
            commit_id: Some("abcdef1234567890".into()),
        };
        crate::roundtrip("issues_issue_event", &e);
    }

    #[test]
    fn issues_create_label() {
        let r = CreateLabel { name: "bug".into(), color: "ff0000".into(), description: Some("Something is broken".into()) };
        crate::roundtrip("issues_create_label", &r);
    }

    #[test]
    fn issues_update_label() {
        let r = UpdateLabel { new_name: Some("defect".into()), color: Some("00ff00".into()), description: None };
        crate::roundtrip("issues_update_label", &r);
    }

    #[test]
    fn issues_create_milestone() {
        let r = CreateMilestone {
            title: "v1.0".into(),
            description: Some("First release".into()),
            due_on: Some(crate::t0()),
            state: Some("open".into()),
        };
        crate::roundtrip("issues_create_milestone", &r);
    }

    #[test]
    fn issues_update_milestone() {
        let r = UpdateMilestone { title: Some("v1.1".into()), description: None, due_on: None, state: Some("closed".into()) };
        crate::roundtrip("issues_update_milestone", &r);
    }

    #[test]
    fn issues_create_issue() {
        let r = CreateIssue {
            title: "Found a bug".into(),
            body: Some("Something broke".into()),
            assignees: vec!["octocat".into()],
            labels: vec!["bug".into()],
            milestone: Some(3),
        };
        crate::roundtrip("issues_create_issue", &r);
    }

    #[test]
    fn issues_update_issue() {
        let r = UpdateIssue {
            title: Some("Found a bug, fixed".into()),
            body: None,
            state: Some("closed".into()),
            state_reason: Some("completed".into()),
            labels: vec!["bug".into()],
            assignees: vec![],
            milestone: None,
        };
        crate::roundtrip("issues_update_issue", &r);
    }

    #[test]
    fn issues_create_comment() {
        let r = CreateComment { body: "Me too".into() };
        crate::roundtrip("issues_create_comment", &r);
    }

    #[test]
    fn issues_create_reaction() {
        let r = CreateReaction { content: "+1".into() };
        crate::roundtrip("issues_create_reaction", &r);
    }

    #[test]
    fn issue_from_github_json() {
        let json = r#"{
            "id": 100,
            "number": 42,
            "title": "Found a bug",
            "body": null,
            "state": "open",
            "state_reason": null,
            "locked": false,
            "user": {
                "login": "octocat",
                "id": 1,
                "avatar_url": "https://code.example.com/avatar/octocat.svg",
                "url": "https://code.example.com/api/v3/users/octocat",
                "html_url": "https://code.example.com/octocat",
                "type": "User",
                "site_admin": false
            },
            "labels": [
                {
                    "id": 2,
                    "name": "bug",
                    "color": "ff0000",
                    "default": false,
                    "url": "https://code.example.com/api/v3/repos/octo/haven/labels/bug",
                    "unknown_label_field": true
                }
            ],
            "assignee": null,
            "assignees": [],
            "milestone": null,
            "comments": 3,
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "closed_at": null,
            "closed_by": null,
            "html_url": "https://code.example.com/octo/haven/issues/42",
            "url": "https://code.example.com/api/v3/repos/octo/haven/issues/42",
            "repository_url": "https://code.example.com/api/v3/repos/octo/haven",
            "comments_url": "https://code.example.com/api/v3/repos/octo/haven/issues/42/comments",
            "pull_request": null,
            "reactions": {
                "url": "https://code.example.com/api/v3/repos/octo/haven/issues/42/reactions",
                "total_count": 3,
                "+1": 2,
                "-1": 1,
                "laugh": 0,
                "confused": 0,
                "heart": 0,
                "hooray": 0,
                "rocket": 0,
                "eyes": 0
            },
            "author_association": "OWNER",
            "node_id": "MDU6SXNzdWUxMDA="
        }"#;
        let issue: Issue = serde_json::from_str(json).expect("deserialize github issue json");
        assert_eq!(issue.number, 42);
        assert!(issue.body.is_none());
        let reactions = issue.reactions.expect("reactions rollup present");
        assert_eq!(reactions.plus_one, 2);
    }
}
