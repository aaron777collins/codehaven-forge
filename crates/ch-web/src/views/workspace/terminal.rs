//! The terminal frame (spec section 7, Frame) and the Linux source download.
//!
//! `GET B/terminal` is the document inside the workspace's sandboxed frame. It holds one inline
//! script, our lock script, xterm and our boot script, and nothing about the repository or the
//! person: the page hands the shell and the files over a MessagePort afterwards. The script is
//! named in the policy by its hash, so nothing else can run there, and the policy itself carries
//! `sandbox allow-scripts`, so the document has an opaque origin even when opened on its own.
//!
//! The frame is answered only to a browser session that can read the repository, like every
//! workspace route. It never redirects: a query or a `Sec-Fetch-Dest` other than `iframe` is a
//! plain 404. The Linux image of stage 4 is not part of this version, so `source.tar` is 404.

use super::assets::manifest;
use super::ctx::{Key, Need, WorkspaceCtx, WsResult};
use crate::ctx::Ctx;
use crate::error::WebError;
use axum::extract::{OriginalUri, Path};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};

const LOCK_JS: &str = include_str!("../../../workspace/frame/lock.js");
const BOOT_JS: &str = include_str!("../../../workspace/frame/boot.js");
const FRAME_CSS: &str = include_str!("../../../workspace/frame/frame.css");

/// The shell's two pieces, as the page fetches them from this build's assets, in order.
pub const RUNTIME: [&str; 2] = ["terminal/just-bash.js", "shell-worker.js"];

/// 404 unless the terminal is on.
pub fn require_terminal(ws: &WorkspaceCtx) -> WsResult<()> {
    if ws.terminal_on() { Ok(()) } else { Err(WebError::not_found().into()) }
}

/// The frame document for one instance origin, and the policy that names its script's hash.
pub struct FrameDoc {
    pub html: String,
    /// The same document gzipped, for browsers that accept it: xterm makes it about 500 KB.
    pub gzip: Vec<u8>,
    pub csp: String,
}

fn vendored_text(name: &str) -> String {
    let Some(a) = manifest().files.get(name) else { return String::new() };
    if !a.gzip {
        return String::from_utf8_lossy(&a.data).into_owned();
    }
    let mut out = String::new();
    if flate2::read::GzDecoder::new(&a.data[..]).read_to_string(&mut out).is_err() {
        return String::new();
    }
    out
}

/// The inline script: the lock first, then xterm, then our boot script, which learns the
/// instance origin (the only sender it accepts the port from) as a JSON string.
fn frame_script(origin: &str) -> String {
    // `<` is escaped so no origin can close the script element early.
    let origin_js = serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".into()).replace('<', "\\u003c");
    let boot = BOOT_JS.replace("__CH_ORIGIN__", &origin_js);
    format!("{LOCK_JS}\n;{}\n;{boot}", vendored_text("terminal/xterm.js"))
}

fn build(origin: &str) -> FrameDoc {
    let script = frame_script(origin);
    let css = format!("{}\n{FRAME_CSS}", vendored_text("terminal/xterm.css"));
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script.as_bytes()));
    let html = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Terminal</title>\
         <style>{css}</style></head><body><div id=\"t\" role=\"application\" aria-label=\"Terminal\"></div>\
         <script>{script}</script></body></html>\n"
    );
    let csp = format!(
        "sandbox allow-scripts; default-src 'none'; script-src 'sha256-{hash}'; worker-src blob:; style-src 'unsafe-inline'; \
         connect-src 'none'; img-src 'none'; frame-src 'none'; object-src 'none'; form-action 'none'; frame-ancestors 'self'; \
         base-uri 'none'"
    );
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let gzip = match std::io::Write::write_all(&mut enc, html.as_bytes()) {
        Ok(()) => enc.finish().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    FrameDoc { html, gzip, csp }
}

/// Built once per origin: the hash depends on the origin inside the boot script.
pub fn frame_doc(origin: &str) -> Arc<FrameDoc> {
    static DOCS: OnceLock<Mutex<HashMap<String, Arc<FrameDoc>>>> = OnceLock::new();
    let docs = DOCS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = docs.lock().unwrap_or_else(|p| p.into_inner());
    map.entry(origin.to_string()).or_insert_with(|| Arc::new(build(origin))).clone()
}

/// The `integrity` value of each runtime piece, as the page checks it before handing it over.
/// The hash is of the bytes the browser sees, after any gzip is undone.
pub fn runtime_integrity() -> Vec<(String, String)> {
    static R: OnceLock<Vec<(String, String)>> = OnceLock::new();
    R.get_or_init(|| {
        RUNTIME
            .iter()
            .map(|name| {
                let text = vendored_text(name);
                let sum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(text.as_bytes()));
                (name.to_string(), format!("sha256-{sum}"))
            })
            .collect()
    })
    .clone()
}

pub async fn frame(
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::None, Need::Read, &headers).await?;
    require_terminal(&ws)?;
    // No query ever: a URL can name where to look, never what to do.
    if uri.query().is_some() {
        return Err(WebError::not_found().into());
    }
    // Only the workspace's own frame loads this document. A top-level visit, a fetch or an
    // embed of another kind is a plain 404. Browsers that send no header are held by
    // `frame-ancestors` and the sandbox in the policy instead.
    if let Some(d) = headers.get("sec-fetch-dest")
        && !d.as_bytes().eq_ignore_ascii_case(b"iframe")
    {
        return Err(WebError::not_found().into());
    }
    let doc = frame_doc(ws.ctx.state.config.origin());
    let gzip_ok = !doc.gzip.is_empty()
        && headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|e| e.split(';').next().is_some_and(|e| e.trim().eq_ignore_ascii_case("gzip"))));
    let mut r =
        if gzip_ok { (StatusCode::OK, doc.gzip.clone()).into_response() } else { (StatusCode::OK, doc.html.clone()).into_response() };
    let h = r.headers_mut();
    if gzip_ok {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&doc.csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    Ok(r)
}

pub async fn source(ctx: Ctx, Path((owner, name)): Path<(String, String)>, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::None, Need::Read, &headers).await?;
    require_terminal(&ws)?;
    // Stage 4 (an optional Linux image) is not part of this version: there is no source to serve.
    Err(WebError::not_found().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_runs_one_hashed_script_and_nothing_from_outside() {
        let doc = build("https://code.example.com");
        assert_eq!(doc.html.matches("<script").count(), 1, "one script element");
        assert!(!doc.html.contains("<script src"), "no script is loaded from anywhere");
        assert!(!doc.html.contains("<link"), "no stylesheet or prefetch link");
        assert!(doc.html.contains("const ORIGIN = \"https://code.example.com\";"));
        let start = doc.html.find("<script>").unwrap() + "<script>".len();
        let end = doc.html.rfind("</script>").unwrap();
        let script = &doc.html[start..end];
        assert!(!script.to_ascii_lowercase().contains("</script"), "the script cannot end early");
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script.as_bytes()));
        assert!(doc.csp.contains(&format!("script-src 'sha256-{hash}';")), "{}", doc.csp);
        assert!(doc.csp.starts_with("sandbox allow-scripts;"));
        for part in ["connect-src 'none'", "frame-src 'none'", "form-action 'none'", "frame-ancestors 'self'", "worker-src blob:"] {
            assert!(doc.csp.contains(part), "{part}");
        }
        assert!(!doc.csp.contains("allow-same-origin") && !doc.csp.contains("unsafe-eval") && !doc.csp.contains("wasm"));
        assert!(script.contains("new Terminal("), "xterm is inlined");
        assert!(script.find("History.prototype").unwrap() < script.find("new Terminal(").unwrap(), "the lock runs first");
        // Firefox and WebKit ignore the iframe's `allow` list for the clipboard, and WebKit allows a
        // SharedWorker in a sandboxed frame, so the lock script removes both (ch-1ke.10).
        let lock_end = script.find("new Terminal(").unwrap();
        assert!(script[..lock_end].contains("Navigator.prototype, \"clipboard\""), "the clipboard is locked away before xterm");
        assert!(script[..lock_end].contains("\"SharedWorker\""), "SharedWorker is locked away before xterm");
        assert!(!doc.html.contains("sourceMappingURL"), "no source map is asked for");
    }

    #[test]
    fn the_gzipped_frame_is_the_same_document() {
        let doc = build("https://code.example.com");
        let mut out = String::new();
        flate2::read::GzDecoder::new(&doc.gzip[..]).read_to_string(&mut out).unwrap();
        assert_eq!(out, doc.html);
        assert!(doc.gzip.len() < doc.html.len() / 2);
    }

    #[test]
    fn each_origin_gets_its_own_hash() {
        let a = frame_doc("https://a.example");
        let b = frame_doc("https://b.example");
        assert_ne!(a.csp, b.csp);
        assert!(Arc::ptr_eq(&a, &frame_doc("https://a.example")), "built once per origin");
    }

    #[test]
    fn the_runtime_pieces_are_in_the_build() {
        let r = runtime_integrity();
        assert_eq!(r.len(), 2);
        for (name, integrity) in &r {
            assert!(manifest().files.contains_key(name.as_str()), "{name} is served");
            assert!(integrity.starts_with("sha256-") && integrity.len() > 20);
        }
        assert!(vendored_text("terminal/just-bash.js").contains("JustBash"));
        assert!(vendored_text("shell-worker.js").contains("TrackedFs"));
    }
}
