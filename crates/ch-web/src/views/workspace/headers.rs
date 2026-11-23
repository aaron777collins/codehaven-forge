//! Response headers of the workspace (spec section 7). Each response sets its own
//! `Content-Security-Policy`, so the global layer keeps it and adds the rest.

use axum::http::{HeaderMap, HeaderValue, header};

/// The shell page. `base` is the absolute canonical workspace URL, `origin` the instance origin.
pub fn shell_csp(origin: &str, base: &str, terminal: bool) -> String {
    let frame = if terminal { format!("{base}/terminal") } else { "'none'".to_string() };
    format!(
        "default-src 'none'; script-src {base}/assets/; style-src {base}/assets/ {origin}/static/ 'unsafe-inline'; \
         img-src data: {origin}/favicon.svg; font-src {base}/assets/ data:; connect-src {base}/; worker-src {base}/assets/; \
         frame-src {frame}; object-src 'none'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'; \
         require-trusted-types-for 'script'"
    )
}

/// Assets and Workers.
pub fn asset_csp(base: &str) -> String {
    format!("default-src 'none'; script-src {base}/assets/; connect-src 'none'; frame-ancestors 'none'; base-uri 'none'")
}

fn set(h: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

pub fn shell_headers(h: &mut HeaderMap, origin: &str, base: &str, terminal: bool) {
    set(h, header::CONTENT_SECURITY_POLICY, &shell_csp(origin, base, terminal));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
}

pub fn asset_headers(h: &mut HeaderMap, base: &str, etag: &str) {
    set(h, header::CONTENT_SECURITY_POLICY, &asset_csp(base));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    set(h, header::ETAG, &format!("\"{etag}\""));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding, Cookie"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_policy_names_only_this_workspace() {
        let csp = shell_csp("https://code.example.com", "https://code.example.com/alice/demo/workspace", false);
        assert_eq!(
            csp,
            "default-src 'none'; script-src https://code.example.com/alice/demo/workspace/assets/; \
             style-src https://code.example.com/alice/demo/workspace/assets/ https://code.example.com/static/ 'unsafe-inline'; \
             img-src data: https://code.example.com/favicon.svg; font-src https://code.example.com/alice/demo/workspace/assets/ data:; \
             connect-src https://code.example.com/alice/demo/workspace/; worker-src https://code.example.com/alice/demo/workspace/assets/; \
             frame-src 'none'; object-src 'none'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'; \
             require-trusted-types-for 'script'"
        );
        assert!(!csp.contains("unsafe-eval"));
        let on = shell_csp("https://o", "https://o/a/b/workspace", true);
        assert!(on.contains("frame-src https://o/a/b/workspace/terminal;"));
    }

    #[test]
    fn assets_cannot_connect_anywhere() {
        let csp = asset_csp("https://o/a/b/workspace");
        assert!(csp.contains("connect-src 'none'"));
        assert!(csp.contains("script-src https://o/a/b/workspace/assets/;"));
    }
}
