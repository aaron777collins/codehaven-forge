//! Releases, release assets and generated release notes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::SimpleUser;

/// `GET /repos/{owner}/{repo}/releases/assets/{id}`: one downloadable asset of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub id: i64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub content_type: String,
    pub size: i64,
    pub download_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub browser_download_url: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploader: Option<SimpleUser>,
    /// `uploaded`, `open`, ...
    pub state: String,
}

/// `GET /repos/{owner}/{repo}/releases/{id}`: a tagged release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: i64,
    pub tag_name: String,
    pub target_commitish: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<Utc>>,
    pub author: SimpleUser,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
    pub html_url: String,
    pub url: String,
    pub tarball_url: String,
    pub zipball_url: String,
    /// A template url, keeps the `{?name,label}` suffix.
    pub upload_url: String,
}

/// Response of `POST /repos/{owner}/{repo}/releases/generate-notes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedNotes {
    pub name: String,
    pub body: String,
}

/// Body of `POST /repos/{owner}/{repo}/releases`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRelease {
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerelease: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_release_notes: Option<bool>,
}

/// Body of `PATCH /repos/{owner}/{repo}/releases/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRelease {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerelease: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_release_notes: Option<bool>,
}

/// Body of `POST /repos/{owner}/{repo}/releases/generate-notes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerateNotesRequest {
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_tag_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_asset() -> ReleaseAsset {
        ReleaseAsset {
            id: 99,
            name: "haven-0.1.0.tar.gz".into(),
            label: None,
            content_type: "application/gzip".into(),
            size: 1024,
            download_count: 7,
            created_at: crate::t0(),
            updated_at: crate::t0(),
            browser_download_url: "https://code.example.com/octo/haven/releases/download/v0.1.0/haven-0.1.0.tar.gz".into(),
            url: "https://code.example.com/api/v3/repos/octo/haven/releases/assets/99".into(),
            uploader: Some(crate::user("octocat", 1)),
            state: "uploaded".into(),
        }
    }

    #[test]
    fn releases_release_asset() {
        let asset = sample_asset();
        crate::roundtrip("releases_release_asset", &asset);
    }

    #[test]
    fn releases_release() {
        let release = Release {
            id: 12,
            tag_name: "v0.1.0".into(),
            target_commitish: "main".into(),
            name: Some("First public release".into()),
            body: Some("What changed, in brief.".into()),
            draft: false,
            prerelease: false,
            created_at: crate::t0(),
            published_at: Some(crate::t0()),
            author: crate::user("octocat", 1),
            assets: vec![sample_asset()],
            html_url: "https://code.example.com/octo/haven/releases/tag/v0.1.0".into(),
            url: "https://code.example.com/api/v3/repos/octo/haven/releases/12".into(),
            tarball_url: "https://code.example.com/api/v3/repos/octo/haven/tarball/v0.1.0".into(),
            zipball_url: "https://code.example.com/api/v3/repos/octo/haven/zipball/v0.1.0".into(),
            upload_url: "https://code.example.com/api/v3/repos/octo/haven/releases/12/assets{?name,label}".into(),
        };
        crate::roundtrip("releases_release", &release);
    }

    #[test]
    fn releases_generated_notes() {
        let notes = GeneratedNotes { name: "v0.1.0".into(), body: "* Fix the flaky test".into() };
        crate::roundtrip("releases_generated_notes", &notes);
    }

    #[test]
    fn releases_create_release() {
        let req = CreateRelease {
            tag_name: "v0.1.0".into(),
            target_commitish: Some("main".into()),
            name: Some("First public release".into()),
            body: Some("What changed, in brief.".into()),
            draft: Some(false),
            prerelease: Some(false),
            generate_release_notes: Some(true),
        };
        crate::roundtrip("releases_create_release", &req);
    }

    #[test]
    fn releases_update_release() {
        let req = UpdateRelease {
            tag_name: Some("v0.1.0".into()),
            name: Some("First public release".into()),
            body: None,
            target_commitish: None,
            draft: None,
            prerelease: None,
            generate_release_notes: None,
        };
        crate::roundtrip("releases_update_release", &req);
    }

    #[test]
    fn releases_generate_notes_request() {
        let req = GenerateNotesRequest { tag_name: "v0.1.0".into(), previous_tag_name: None, target_commitish: Some("main".into()) };
        crate::roundtrip("releases_generate_notes_request", &req);
    }

    #[test]
    fn release_from_github_json() {
        let json = r#"{
            "url": "https://code.example.com/api/v3/repos/octo/haven/releases/12",
            "html_url": "https://code.example.com/octo/haven/releases/tag/v0.1.0",
            "assets_url": "https://code.example.com/api/v3/repos/octo/haven/releases/12/assets",
            "upload_url": "https://code.example.com/api/v3/repos/octo/haven/releases/12/assets{?name,label}",
            "tarball_url": "https://code.example.com/api/v3/repos/octo/haven/tarball/v0.1.0",
            "zipball_url": "https://code.example.com/api/v3/repos/octo/haven/zipball/v0.1.0",
            "id": 12,
            "node_id": "MDc6UmVsZWFzZTEy",
            "tag_name": "v0.1.0",
            "target_commitish": "main",
            "name": null,
            "body": "What changed, in brief.",
            "draft": false,
            "prerelease": false,
            "created_at": "2026-01-02T03:04:05Z",
            "published_at": null,
            "author": {
                "login": "octocat",
                "id": 1,
                "avatar_url": "https://code.example.com/avatars/1.svg",
                "url": "https://code.example.com/api/v3/users/octocat",
                "html_url": "https://code.example.com/octocat",
                "type": "User",
                "site_admin": false
            },
            "assets": [
                {
                    "url": "https://code.example.com/api/v3/repos/octo/haven/releases/assets/99",
                    "id": 99,
                    "node_id": "RA_kwDOAAA",
                    "name": "haven-0.1.0.tar.gz",
                    "label": null,
                    "uploader": {
                        "login": "octocat",
                        "id": 1,
                        "avatar_url": "https://code.example.com/avatars/1.svg",
                        "url": "https://code.example.com/api/v3/users/octocat",
                        "html_url": "https://code.example.com/octocat",
                        "type": "User",
                        "site_admin": false
                    },
                    "content_type": "application/gzip",
                    "state": "uploaded",
                    "size": 1024,
                    "download_count": 7,
                    "created_at": "2026-01-02T03:04:05Z",
                    "updated_at": "2026-01-02T03:04:05Z",
                    "browser_download_url": "https://code.example.com/octo/haven/releases/download/v0.1.0/haven-0.1.0.tar.gz"
                }
            ]
        }"#;
        let release: Release = serde_json::from_str(json).expect("deserialize release");
        assert_eq!(release.tag_name, "v0.1.0");
        assert!(release.name.is_none());
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].download_count, 7);
    }
}
