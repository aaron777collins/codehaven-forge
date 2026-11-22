//! Static files, embedded in the binary and served with content hashes in their names
//! (`/static/app.1a2b3c4d.css`) and a one-year cache. A name without a hash still works and is
//! revalidated every time, so an old page never breaks.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(RustEmbed)]
#[folder = "static/"]
struct Static;

struct Entry {
    hash: String,
    data: Vec<u8>,
}

fn table() -> &'static HashMap<String, Entry> {
    static TABLE: OnceLock<HashMap<String, Entry>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        for name in Static::iter() {
            if let Some(f) = Static::get(&name) {
                let hash = hex::encode(Sha256::digest(&f.data))[..8].to_string();
                m.insert(name.to_string(), Entry { hash, data: f.data.to_vec() });
            }
        }
        m
    })
}

/// The hashed URL for a static file, or the plain one when the file is unknown.
pub fn url(name: &str) -> String {
    match table().get(name) {
        Some(e) => match name.rsplit_once('.') {
            Some((stem, ext)) => format!("/static/{stem}.{}.{ext}", e.hash),
            None => format!("/static/{name}.{}", e.hash),
        },
        None => format!("/static/{name}"),
    }
}

/// `app.1a2b3c4d.css` -> (`app.css`, Some(`1a2b3c4d`)). `app.css` -> (`app.css`, None).
fn unhash(file: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = file.split('.').collect();
    if parts.len() >= 3 {
        let h = parts[parts.len() - 2];
        if h.len() == 8 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut rest = parts.clone();
            rest.remove(parts.len() - 2);
            return (rest.join("."), Some(h.to_string()));
        }
    }
    (file.to_string(), None)
}

pub async fn serve(Path(path): Path<String>) -> Response {
    if path.contains("..") || path.starts_with('/') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (name, hash) = unhash(&path);
    let Some(entry) = table().get(&name) else { return StatusCode::NOT_FOUND.into_response() };
    let cache = match &hash {
        Some(h) if *h == entry.hash => "public, max-age=31536000, immutable",
        Some(_) => return StatusCode::NOT_FOUND.into_response(),
        None => "no-cache",
    };
    let mime = match name.rsplit('.').next() {
        Some("js") => "text/javascript; charset=utf-8".to_string(),
        Some("css") => "text/css; charset=utf-8".to_string(),
        Some("svg") => "image/svg+xml".to_string(),
        _ => mime_guess::from_path(&name).first_or_octet_stream().to_string(),
    };
    let mut r = (StatusCode::OK, entry.data.clone()).into_response();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_names_round_trip() {
        let u = url("app.css");
        assert!(u.starts_with("/static/app.") && u.ends_with(".css"), "{u}");
        let file = u.trim_start_matches("/static/");
        let (name, hash) = unhash(file);
        assert_eq!(name, "app.css");
        assert_eq!(hash.as_deref(), table().get("app.css").map(|e| e.hash.as_str()));
    }

    #[test]
    fn unhash_leaves_other_names_alone() {
        assert_eq!(unhash("app.css"), ("app.css".into(), None));
        assert_eq!(unhash("htmx.min.js"), ("htmx.min.js".into(), None));
        assert_eq!(unhash("htmx.min.abcdef12.js"), ("htmx.min.js".into(), Some("abcdef12".into())));
    }

    #[test]
    fn unknown_file_gets_a_plain_url() {
        assert_eq!(url("nope.css"), "/static/nope.css");
    }
}
