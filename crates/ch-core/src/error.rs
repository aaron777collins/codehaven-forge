//! One error type for every service, and its mapping to HTTP.
//!
//! The JSON body follows GitHub's shape:
//! `{ "message": ..., "errors": [{ "resource", "field", "code" }], "documentation_url": ... }`.
//! A private repository the caller cannot read is `NotFound`, never `Forbidden`.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub resource: String,
    pub field: String,
    /// `missing`, `missing_field`, `invalid`, `already_exists`, `custom`.
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl FieldError {
    pub fn new(resource: &str, field: &str, code: &str) -> Self {
        Self { resource: resource.into(), field: field.into(), code: code.into(), message: None }
    }
    pub fn custom(resource: &str, field: &str, message: impl Into<String>) -> Self {
        Self { resource: resource.into(), field: field.into(), code: "custom".into(), message: Some(message.into()) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Not Found")]
    NotFound,
    /// No or bad credentials. Maps to 401.
    #[error("{0}")]
    Unauthorized(String),
    /// Signed in but not allowed. Maps to 403. Never use it to hide that something exists.
    #[error("{0}")]
    Forbidden(String),
    #[error("Validation Failed")]
    Validation(Vec<FieldError>),
    /// State conflict. Maps to 409, or 405 for a blocked merge (see `Error::MergeBlocked`).
    #[error("{0}")]
    Conflict(String),
    /// A pull request that cannot be merged yet, with the reasons. Maps to 405.
    #[error("Pull Request is not mergeable")]
    MergeBlocked(Vec<String>),
    #[error("Rate limit exceeded")]
    RateLimited { retry_after_secs: u64 },
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl Error {
    pub fn unauthorized() -> Self {
        Error::Unauthorized("Requires authentication".into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Error::Forbidden(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Error::Conflict(msg.into())
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Error::Unavailable(msg.into())
    }
    pub fn invalid(resource: &str, field: &str, message: impl Into<String>) -> Self {
        Error::Validation(vec![FieldError::custom(resource, field, message)])
    }
    pub fn internal(e: impl Into<anyhow::Error>) -> Self {
        Error::Internal(e.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::MergeBlocked(_) => StatusCode::METHOD_NOT_ALLOWED,
            Error::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The text a client sees. Internal errors say nothing about the cause.
    pub fn public_message(&self) -> String {
        match self {
            Error::Internal(_) => "Internal Server Error".into(),
            Error::MergeBlocked(reasons) if !reasons.is_empty() => {
                format!("Pull Request is not mergeable: {}", reasons.join("; "))
            }
            other => other.to_string(),
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Error::NotFound,
            other => Error::Internal(other.into()),
        }
    }
}

#[derive(Serialize)]
struct Body<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<FieldError>,
    documentation_url: &'a str,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if let Error::Internal(e) = &self {
            // The cause goes to the log, never to the client.
            tracing::error!(error = ?e, "internal error");
        }
        let status = self.status();
        let message = self.public_message();
        let errors = match &self {
            Error::Validation(v) => v.clone(),
            _ => Vec::new(),
        };
        let body = Body { message: &message, errors, documentation_url: "/api/v3" };
        let json = serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"message\":\"error\"}".to_vec());
        let mut resp = (status, json).into_response();
        resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
        if let Error::RateLimited { retry_after_secs } = &self
            && let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        if matches!(self, Error::Unauthorized(_)) {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Codehaven\""));
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util_shim::to_bytes;

    // Tiny local body reader so ch-core does not need http-body-util.
    mod http_body_util_shim {
        use axum::body::Body;
        pub async fn to_bytes(b: Body) -> Vec<u8> {
            axum::body::to_bytes(b, 1 << 20).await.unwrap().to_vec()
        }
    }

    #[tokio::test]
    async fn not_found_is_github_shaped() {
        let r = Error::NotFound.into_response();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_slice(&to_bytes(r.into_body()).await).unwrap();
        assert_eq!(v["message"], "Not Found");
        assert!(v.get("errors").is_none());
    }

    #[tokio::test]
    async fn internal_error_hides_cause() {
        let r = Error::internal(anyhow::anyhow!("select * from secrets exploded")).into_response();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let text = String::from_utf8(to_bytes(r.into_body()).await).unwrap();
        assert!(!text.contains("secrets"));
        assert!(text.contains("Internal Server Error"));
    }

    #[tokio::test]
    async fn validation_lists_field_errors() {
        let r = Error::Validation(vec![FieldError::new("Repository", "name", "already_exists")]).into_response();
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v: serde_json::Value = serde_json::from_slice(&to_bytes(r.into_body()).await).unwrap();
        assert_eq!(v["errors"][0]["code"], "already_exists");
    }

    #[test]
    fn merge_blocked_is_405_with_reasons() {
        let e = Error::MergeBlocked(vec!["1 approval required".into()]);
        assert_eq!(e.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(e.public_message().contains("1 approval required"));
    }

    #[test]
    fn rate_limited_sets_retry_after() {
        let r = Error::RateLimited { retry_after_secs: 30 }.into_response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()[header::RETRY_AFTER], "30");
    }
}
