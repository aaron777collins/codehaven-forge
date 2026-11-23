//! The workspace's own files and the vendored editor, served only to people who can read the
//! repository (spec sections 5 and 7). They never come from `/static`, which is public.
//!
//! `workspace/src/` holds plain JS modules and CSS. `workspace/vendor/` holds prebuilt files
//! stored gzipped (`name.js.gz` is served as `name.js` with `Content-Encoding: gzip`), plus
//! `LOCK` and `THIRD_PARTY_NOTICES`, which are not served. A file is served only when its
//! exact name is in the manifest. The build id changes whenever any file does.

use super::ctx::{Key, Need, WorkspaceCtx, WsResult};
use super::headers::asset_headers;
use crate::ctx::Ctx;
use crate::error::WebError;
use axum::extract::{OriginalUri, Path};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::OnceLock;

#[derive(RustEmbed)]
#[folder = "workspace/src/"]
struct Src;

#[derive(RustEmbed)]
#[folder = "workspace/vendor/"]
struct Vendor;

pub struct Asset {
    pub data: Vec<u8>,
    pub gzip: bool,
    pub sha256: String,
    pub content_type: &'static str,
}

pub struct Manifest {
    pub build: String,
    pub files: BTreeMap<String, Asset>,
}

/// Files in `vendor/` that describe the vendored code and are never served.
const NOT_SERVED: &[&str] = &["LOCK", "THIRD_PARTY_NOTICES", "README.md"];

pub fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e) {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("ttf") => "font/ttf",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

pub fn manifest() -> &'static Manifest {
    static M: OnceLock<Manifest> = OnceLock::new();
    M.get_or_init(|| {
        let mut files = BTreeMap::new();
        for name in Src::iter() {
            if let Some(f) = Src::get(&name) {
                let data = f.data.to_vec();
                let sha256 = hex::encode(Sha256::digest(&data));
                files.insert(name.to_string(), Asset { content_type: content_type(&name), data, gzip: false, sha256 });
            }
        }
        for name in Vendor::iter() {
            if NOT_SERVED.contains(&name.as_ref()) {
                continue;
            }
            let Some(published) = name.strip_suffix(".gz") else { continue };
            if let Some(f) = Vendor::get(&name) {
                let data = f.data.to_vec();
                let sha256 = hex::encode(Sha256::digest(&data));
                files.insert(published.to_string(), Asset { content_type: content_type(published), data, gzip: true, sha256 });
            }
        }
        let mut h = Sha256::new();
        for (name, a) in &files {
            h.update(name.as_bytes());
            h.update([0]);
            h.update(a.sha256.as_bytes());
            h.update([0]);
        }
        let build = hex::encode(h.finalize())[..16].to_string();
        Manifest { build, files }
    })
}

/// The URL prefix of this build's assets under a workspace base.
pub fn assets_url(base: &str) -> String {
    format!("{base}/assets/{}", manifest().build)
}

/// `GET B/assets/{build}/{*file}`. The file name is taken from the raw path, not decoded.
pub async fn get(
    ctx: Ctx,
    Path((owner, name, build, _file)): Path<(String, String, String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::None, Need::Read, &headers).await?;
    let m = manifest();
    let prefix = format!("{}/assets/{}/", ws.base(), m.build);
    let Some(file) = uri.path().strip_prefix(&prefix) else { return Err(WebError::not_found().into()) };
    if build != m.build {
        return Err(WebError::not_found().into());
    }
    let Some(a) = m.files.get(file) else { return Err(WebError::not_found().into()) };
    let base = ws.abs_base();
    // Access was checked above, so a revalidation is answered only for someone who may read.
    let etag = format!("\"{}\"", a.sha256);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag)) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        asset_headers(r.headers_mut(), &base, &a.sha256);
        return Ok(r);
    }
    let mut r = (StatusCode::OK, a.data.clone()).into_response();
    let h = r.headers_mut();
    asset_headers(h, &base, &a.sha256);
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(a.content_type));
    if a.gzip {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_has_the_shell_and_no_lock_or_notices() {
        let m = manifest();
        assert_eq!(m.build.len(), 16);
        assert!(m.files.contains_key("workspace.js"), "{:?}", m.files.keys().collect::<Vec<_>>());
        assert!(m.files.contains_key("workspace.css"));
        assert!(!m.files.keys().any(|k| k.contains("LOCK") || k.contains("NOTICES")));
        assert!(m.files.keys().all(|k| !k.ends_with(".gz")));
    }
}
