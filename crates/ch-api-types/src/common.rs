//! Shapes shared by every group. Names follow GitHub's REST v3 where they mean the same thing.

use serde::{Deserialize, Serialize};

/// The error body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiFieldError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiFieldError {
    pub resource: String,
    pub field: String,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A user, org or bot as embedded in other resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleUser {
    pub login: String,
    pub id: i64,
    pub avatar_url: String,
    pub url: String,
    pub html_url: String,
    /// `User`, `Organization` or `Bot`.
    #[serde(rename = "type")]
    pub kind: String,
    pub site_admin: bool,
}

/// What the caller may do on a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    pub admin: bool,
    pub maintain: bool,
    pub push: bool,
    pub triage: bool,
    pub pull: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitResource {
    pub limit: u32,
    pub remaining: u32,
    pub reset: i64,
    pub used: u32,
}

/// `GET /rate_limit`. Only for signed-in callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub resources: RateLimitResources,
    pub rate: RateLimitResource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitResources {
    pub core: RateLimitResource,
    pub search: RateLimitResource,
}

/// `GET /version` (signed in only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub api_version: String,
}

/// A tiny reference to another resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlRef {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaRef {
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
