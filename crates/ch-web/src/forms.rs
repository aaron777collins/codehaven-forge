//! Form posts. `FormData` is the only way a handler reads a request body, and it refuses the
//! request unless the Origin is ours and the CSRF token is valid. So a POST route cannot
//! forget the check: there is no other extractor to use.

use crate::ctx::Ctx;
use crate::error::WebError;
use axum::body::to_bytes;
use axum::extract::{FromRequest, Request};
use axum::http::header;
use ch_core::state::AppState;

const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Debug, Default, Clone)]
pub struct FormData {
    pairs: Vec<(String, String)>,
}

impl FormData {
    pub fn parse(bytes: &[u8]) -> Self {
        FormData { pairs: form_urlencoded::parse(bytes).map(|(k, v)| (k.into_owned(), v.into_owned())).collect() }
    }

    /// The first value, or "" when missing.
    pub fn get(&self, name: &str) -> &str {
        self.pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    }

    /// The first value, trimmed, `None` when missing or blank.
    pub fn opt(&self, name: &str) -> Option<&str> {
        let v = self.get(name).trim();
        if v.is_empty() { None } else { Some(v) }
    }

    pub fn all(&self, name: &str) -> Vec<&str> {
        self.pairs.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// A checkbox is present when ticked.
    pub fn flag(&self, name: &str) -> bool {
        self.pairs.iter().any(|(k, v)| k == name && v != "0" && v != "false")
    }

    pub fn int(&self, name: &str) -> Option<i64> {
        self.get(name).trim().parse().ok()
    }
}

impl FromRequest<AppState> for FormData {
    type Rejection = WebError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = req.into_parts();
        let Ok(ctx) = <Ctx as axum::extract::FromRequestParts<AppState>>::from_request_parts(&mut parts, state).await;
        if !ctx.origin_valid() {
            return Err(WebError::Csrf);
        }
        let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let has_body = parts.headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).is_some_and(|l| l != "0");
        if has_body && !ct.starts_with("application/x-www-form-urlencoded") {
            return Err(WebError::bad("Forms must be sent as application/x-www-form-urlencoded."));
        }
        let bytes = to_bytes(body, MAX_BODY).await.map_err(|_| WebError::bad("That form is too large."))?;
        let form = FormData::parse(&bytes);
        let supplied = parts.headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or_else(|| form.get("_csrf"));
        if !ctx.csrf_valid(supplied) {
            return Err(WebError::Csrf);
        }
        Ok(form)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_and_encoded_values() {
        let f = FormData::parse(b"a=1&b=two+words&a=3&c=%26&empty=&on=on");
        assert_eq!(f.get("a"), "1");
        assert_eq!(f.all("a"), vec!["1", "3"]);
        assert_eq!(f.get("b"), "two words");
        assert_eq!(f.get("c"), "&");
        assert_eq!(f.get("missing"), "");
        assert_eq!(f.opt("empty"), None);
        assert_eq!(f.opt("b"), Some("two words"));
        assert!(f.flag("on") && !f.flag("off"));
        assert_eq!(f.int("a"), Some(1));
        assert_eq!(f.int("b"), None);
    }
}
