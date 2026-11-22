//! Security headers on every response (ARCHITECTURE.md section 7.6). A handler that sets one
//! itself, such as the stricter `Content-Security-Policy: sandbox` on raw files, keeps its
//! value. Everything else gets ours, including the gate's own rejections.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use ch_core::state::AppState;

pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Name and value of every header, in the order they are documented.
pub fn security_headers(https: bool) -> Vec<(&'static str, &'static str)> {
    let mut v = Vec::new();
    if https {
        v.push(("strict-transport-security", "max-age=31536000"));
    }
    v.extend([
        ("content-security-policy", CSP),
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "same-origin"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("permissions-policy", "camera=(), microphone=(), geolocation=()"),
        ("x-robots-tag", "noindex, nofollow"),
    ]);
    v
}

pub fn apply(headers: &mut HeaderMap, https: bool) {
    for (name, value) in security_headers(https) {
        let name = HeaderName::from_static(name);
        if !headers.contains_key(&name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
}

pub async fn security_headers_layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let https = state.config.is_https();
    let mut resp = next.run(req).await;
    apply(resp.headers_mut(), https);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_adds_hsts_http_does_not() {
        let mut h = HeaderMap::new();
        apply(&mut h, true);
        assert_eq!(h["strict-transport-security"], "max-age=31536000");
        let mut h = HeaderMap::new();
        apply(&mut h, false);
        assert!(!h.contains_key("strict-transport-security"));
        assert_eq!(h["x-frame-options"], "DENY");
    }

    #[test]
    fn handler_values_are_kept() {
        let mut h = HeaderMap::new();
        h.insert("content-security-policy", HeaderValue::from_static("sandbox"));
        apply(&mut h, true);
        assert_eq!(h["content-security-policy"], "sandbox");
    }

    #[test]
    fn csp_has_no_unsafe_directives() {
        assert!(!CSP.contains("unsafe"));
        assert!(CSP.contains("frame-ancestors 'none'"));
        assert!(CSP.contains("script-src 'self'"));
    }
}
