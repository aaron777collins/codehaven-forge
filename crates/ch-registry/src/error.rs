//! Registry errors in the shape Docker and OCI clients read:
//! `{"errors":[{"code":"BLOB_UNKNOWN","message":"...","detail":...}]}`.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Debug)]
pub struct RegError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub detail: Option<Value>,
}

pub type RegResult<T> = Result<T, RegError>;

impl RegError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), detail: None }
    }
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }
    pub fn name_unknown() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository name not known to registry")
    }
    pub fn name_invalid(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "NAME_INVALID", msg)
    }
    pub fn blob_unknown() -> Self {
        Self::new(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown to registry")
    }
    pub fn upload_unknown() -> Self {
        Self::new(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "blob upload unknown to registry")
    }
    pub fn upload_invalid(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID", msg)
    }
    pub fn digest_invalid(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "DIGEST_INVALID", msg)
    }
    pub fn manifest_unknown() -> Self {
        Self::new(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest unknown")
    }
    pub fn manifest_invalid(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", msg)
    }
    pub fn manifest_blob_unknown(digest: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "MANIFEST_BLOB_UNKNOWN", "manifest references a manifest or blob unknown to registry")
            .with_detail(json!({ "digest": digest }))
    }
    pub fn size_invalid(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "SIZE_INVALID", msg)
    }
    pub fn denied(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "DENIED", msg)
    }
    pub fn unsupported(status: StatusCode, msg: impl Into<String>) -> Self {
        Self::new(status, "UNSUPPORTED", msg)
    }
    pub fn range_invalid() -> Self {
        Self::new(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID", "the chunk does not start where the upload left off")
    }
    pub fn unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "the registry is not available")
    }
    pub fn internal(e: impl std::fmt::Display) -> Self {
        // The cause goes to the log. The client learns nothing about it.
        tracing::error!(error = %e, "registry error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "internal error")
    }
}

impl From<sqlx::Error> for RegError {
    fn from(e: sqlx::Error) -> Self {
        RegError::internal(e)
    }
}

impl From<std::io::Error> for RegError {
    fn from(e: std::io::Error) -> Self {
        RegError::internal(e)
    }
}

impl From<ch_core::Error> for RegError {
    fn from(e: ch_core::Error) -> Self {
        match e {
            ch_core::Error::NotFound => RegError::name_unknown(),
            ch_core::Error::Forbidden(m) => RegError::denied(m),
            ch_core::Error::Unavailable(_) => RegError::unavailable(),
            other => RegError::internal(other),
        }
    }
}

impl IntoResponse for RegError {
    fn into_response(self) -> Response {
        let mut err = json!({ "code": self.code, "message": self.message });
        if let Some(d) = self.detail {
            err["detail"] = d;
        }
        let body = json!({ "errors": [err] }).to_string();
        let mut r = (self.status, body).into_response();
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
        h.insert("docker-distribution-api-version", HeaderValue::from_static("registry/2.0"));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shape_and_headers() {
        let r = RegError::manifest_blob_unknown("sha256:abc").into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers()["docker-distribution-api-version"], "registry/2.0");
        let bytes = axum::body::to_bytes(r.into_body(), 10_000).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["errors"][0]["code"], "MANIFEST_BLOB_UNKNOWN");
        assert_eq!(v["errors"][0]["detail"]["digest"], "sha256:abc");
    }

    #[tokio::test]
    async fn internal_errors_say_nothing() {
        let r = RegError::internal("select * from secrets").into_response();
        let bytes = axum::body::to_bytes(r.into_body(), 10_000).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("secrets"));
    }

    #[test]
    fn core_errors_map() {
        assert_eq!(RegError::from(ch_core::Error::NotFound).code, "NAME_UNKNOWN");
        assert_eq!(RegError::from(ch_core::Error::forbidden("no")).status, StatusCode::FORBIDDEN);
        assert_eq!(RegError::from(ch_core::Error::unavailable("x")).status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
