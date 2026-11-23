//! Every REST error answers in GitHub's JSON shape (ch-yci). A handler's own errors already do
//! (`ch_core::Error`). This covers what answers before a handler: a path no route has (`404`), a
//! method a route lacks (`405`), and axum's own body and query rejections, whose plain text names
//! Rust types. Successful answers and JSON errors pass through untouched.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ch_core::Error;

/// The most of a plain error body read to reword it. axum's rejections are a line or two.
const MAX_PLAIN_ERROR: usize = 16 * 1024;

/// For `Router::fallback` under `/api/v3`.
pub async fn not_found() -> Response {
    Error::NotFound.into_response()
}

/// For `axum::middleware::map_response` under `/api/v3`.
pub async fn json_errors(resp: Response) -> Response {
    let status = resp.status();
    if !status.is_client_error() {
        return resp;
    }
    let is_json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json") || ct.starts_with("application/vnd.github"));
    if is_json {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let text = axum::body::to_bytes(body, MAX_PLAIN_ERROR).await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let message = match status {
        StatusCode::NOT_FOUND => "Not Found".to_string(),
        StatusCode::METHOD_NOT_ALLOWED => "Method Not Allowed".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST if !text.trim().is_empty() => ch_core::error::json_problem(&text),
        _ if !text.trim().is_empty() && text.len() < 300 && !text.contains('<') => text.trim().to_string(),
        _ => status.canonical_reason().unwrap_or("Error").to_string(),
    };
    let json = serde_json::json!({ "message": message, "documentation_url": "/api/v3" }).to_string();
    let mut out = Response::from_parts(parts, Body::from(json));
    out.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    out.headers_mut().remove(header::CONTENT_LENGTH);
    out
}
