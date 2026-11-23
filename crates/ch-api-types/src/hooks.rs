//! Repository webhooks, their deliveries, and notifications.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::repos::Repository;

/// Delivery settings of a webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookConfig {
    pub url: String,
    /// Usually `json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// `0` or `1` as a string, like GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure_ssl: Option<String>,
}

/// Outcome of the most recent delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookLastResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A repository webhook, `GET/POST /repos/{owner}/{repo}/hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hook {
    pub id: i64,
    /// Always `web` today.
    pub name: String,
    pub active: bool,
    #[serde(default)]
    pub events: Vec<String>,
    pub config: HookConfig,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub url: String,
    pub ping_url: String,
    pub deliveries_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_response: Option<HookLastResponse>,
}

/// Request body for `POST /repos/{owner}/{repo}/hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateHook {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub config: HookConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// Request body for `PATCH /repos/{owner}/{repo}/hooks/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateHook {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<HookConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// One delivery attempt of a webhook, `GET /repos/{owner}/{repo}/hooks/{id}/deliveries`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDelivery {
    pub id: i64,
    pub guid: String,
    pub delivered_at: DateTime<Utc>,
    pub redelivery: bool,
    /// Seconds, fractional.
    pub duration: f64,
    pub status: String,
    pub status_code: i32,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// The request or response half of a delivery detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveryPayload {
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub payload: serde_json::Value,
}

/// Full detail of one delivery, `GET /repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery_id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDeliveryDetail {
    #[serde(flatten)]
    pub delivery: HookDelivery,
    pub request: DeliveryPayload,
    pub response: DeliveryPayload,
}

/// What a notification is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSubject {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_comment_url: Option<String>,
    /// `Issue`, `PullRequest`, `Release` and so on.
    #[serde(rename = "type")]
    pub kind: String,
}

/// A notification, `GET /notifications`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub unread: bool,
    pub reason: String,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at: Option<DateTime<Utc>>,
    pub subject: NotificationSubject,
    pub repository: Repository,
    pub url: String,
    pub subscription_url: String,
}

/// Request body for `PUT /notifications`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkNotificationsRead {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
}

/// Subscription state for a repository's notifications, `GET/PUT /repos/{owner}/{repo}/subscription`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSubscription {
    pub subscribed: bool,
    pub ignored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub url: String,
    pub repository_url: String,
}

/// Request body for `PUT /repos/{owner}/{repo}/subscription`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutRepoSubscription {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> crate::repos::Repository {
        let value = serde_json::json!({
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
        });
        serde_json::from_value(value).unwrap()
    }

    fn sample_config() -> HookConfig {
        HookConfig {
            url: "https://example.test/hook".into(),
            content_type: Some("json".into()),
            secret: Some("s3cret".into()),
            insecure_ssl: Some("0".into()),
        }
    }

    fn sample_delivery() -> HookDelivery {
        HookDelivery {
            id: 9001,
            guid: "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d".into(),
            delivered_at: crate::t0(),
            redelivery: false,
            duration: 0.27,
            status: "OK".into(),
            status_code: 200,
            event: "push".into(),
            action: None,
        }
    }

    #[test]
    fn hooks_hook() {
        let hook = Hook {
            id: 77,
            name: "web".into(),
            active: true,
            events: vec!["push".into(), "pull_request".into()],
            config: sample_config(),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            url: "https://code.example.com/api/v3/hooks/77".into(),
            ping_url: "https://code.example.com/api/v3/hooks/77/pings".into(),
            deliveries_url: "https://code.example.com/api/v3/hooks/77/deliveries".into(),
            last_response: Some(HookLastResponse { code: Some(200), status: Some("OK".into()), message: Some("delivered".into()) }),
        };
        crate::roundtrip("hooks_hook", &hook);
    }

    #[test]
    fn hooks_create_hook() {
        let req = CreateHook {
            name: Some("web".into()),
            config: sample_config(),
            events: Some(vec!["push".into(), "pull_request".into()]),
            active: Some(true),
        };
        crate::roundtrip("hooks_create_hook", &req);
    }

    #[test]
    fn hooks_update_hook() {
        let req = UpdateHook { config: Some(sample_config()), events: Some(vec!["push".into()]), active: Some(false) };
        crate::roundtrip("hooks_update_hook", &req);
    }

    #[test]
    fn hooks_hook_delivery() {
        let delivery = sample_delivery();
        crate::roundtrip("hooks_hook_delivery", &delivery);
    }

    #[test]
    fn hooks_hook_delivery_detail() {
        let mut request_headers = serde_json::Map::new();
        request_headers.insert("content-type".into(), "application/json".into());
        let detail = HookDeliveryDetail {
            delivery: sample_delivery(),
            request: DeliveryPayload { headers: request_headers, payload: serde_json::json!({ "ref": "refs/heads/main" }) },
            response: DeliveryPayload { headers: serde_json::Map::new(), payload: serde_json::json!({ "ok": true }) },
        };
        crate::roundtrip("hooks_hook_delivery_detail", &detail);
    }

    #[test]
    fn hooks_notification() {
        let notification = Notification {
            id: "1234567".into(),
            unread: true,
            reason: "review_requested".into(),
            updated_at: crate::t0(),
            last_read_at: None,
            subject: NotificationSubject {
                title: "ci red on main".into(),
                url: Some("https://code.example.com/repos/alice/codehaven/issues/7".into()),
                latest_comment_url: None,
                kind: "Issue".into(),
            },
            repository: repo(),
            url: "https://code.example.com/notifications/threads/1".into(),
            subscription_url: "https://code.example.com/notifications/threads/1/subscription".into(),
        };
        crate::roundtrip("hooks_notification", &notification);
    }

    #[test]
    fn hooks_mark_notifications_read() {
        let req = MarkNotificationsRead { last_read_at: Some(crate::t0()), read: Some(true) };
        crate::roundtrip("hooks_mark_notifications_read", &req);
    }

    #[test]
    fn hooks_repo_subscription() {
        let sub = RepoSubscription {
            subscribed: true,
            ignored: false,
            reason: None,
            created_at: crate::t0(),
            url: "https://code.example.com/repos/alice/codehaven/subscription".into(),
            repository_url: "https://code.example.com/repos/alice/codehaven".into(),
        };
        crate::roundtrip("hooks_repo_subscription", &sub);
    }

    #[test]
    fn hooks_put_repo_subscription() {
        let req = PutRepoSubscription { subscribed: Some(true), ignored: Some(false) };
        crate::roundtrip("hooks_put_repo_subscription", &req);
    }

    #[test]
    fn parses_github_hook_json() {
        let json = r#"{
            "id": 77,
            "name": "web",
            "active": true,
            "events": ["push", "pull_request"],
            "config": {
                "url": "https://example.test/hook",
                "content_type": "json",
                "insecure_ssl": "0",
                "secret": "s3cret"
            },
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks/77",
            "ping_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks/77/pings",
            "deliveries_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks/77/deliveries",
            "last_response": { "code": 200, "status": "OK", "message": "delivered" },
            "type": "Repository",
            "test_url": "https://example.test/ping",
            "unknown_future_field": 1
        }"#;
        let hook = serde_json::from_str::<Hook>(json).unwrap();
        assert_eq!(hook.id, 77);
        assert_eq!(hook.config.url, "https://example.test/hook");
        assert_eq!(hook.events.len(), 2);
    }
}
