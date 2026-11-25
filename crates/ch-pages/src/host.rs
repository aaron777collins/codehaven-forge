//! Every request for the pages host (`docs/design/pages.md`, sections 2 to 5 and 8).
//!
//! The binary sends a request here by its `Host` before any app layer runs, so nothing of the
//! app is reachable on this host: no route, no session lookup, no CSRF cookie. Cookies and
//! credentials are dropped on the way in, `Set-Cookie` on the way out.

use crate::core::AddressSlot;
use crate::core::{Core, STORAGE_QUOTA};
use crate::path::{self, Parsed, Target};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use ch_core::model::ClientInfo;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;

/// The sandbox of every piece of content on the path form: scripts, forms, popups that stay
/// sandboxed, and a top-level navigation only from a click. Never `allow-same-origin`.
pub const SANDBOX: &str =
    "allow-scripts allow-forms allow-popups allow-modals allow-downloads allow-pointer-lock allow-top-navigation-by-user-activation";

const BRIDGE: &str = include_str!("../assets/bridge.js");
const SHELL_JS: &str = include_str!("../assets/shell.js");
const SHELL_CSS: &str = include_str!("../assets/shell.css");
const PAGE: &str = include_str!("../assets/page.html");

pub fn router(core: Arc<Core>) -> Router {
    Router::new().fallback(handle).with_state(core).layer(from_fn(no_cookies))
}

/// No cookie or credential reaches a handler, and none leaves.
async fn no_cookies(mut req: Request, next: Next) -> Response {
    let h = req.headers_mut();
    h.remove(header::COOKIE);
    h.remove(header::AUTHORIZATION);
    h.remove(header::PROXY_AUTHORIZATION);
    let mut resp = next.run(req).await;
    resp.headers_mut().remove(header::SET_COOKIE);
    resp
}

/// The host a request names, lowercase and without a port: `Host`, or the authority of an
/// HTTP/2 request.
pub fn request_host(req: &Request) -> Option<String> {
    let raw = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().host().map(str::to_string))?;
    let host = match raw.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') && p.bytes().all(|b| b.is_ascii_digit()) => h.to_string(),
        _ => raw,
    };
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

/// True when `host` belongs to Pages: the pages host itself, or one label under it.
pub fn is_pages_host(pages: &str, host: &str) -> bool {
    host == pages
        || host.strip_suffix(pages).and_then(|h| h.strip_suffix('.')).is_some_and(|label| !label.is_empty() && !label.contains('.'))
}

async fn handle(State(core): State<Arc<Core>>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    if req.method() != Method::GET && !head {
        let mut r = plain(StatusCode::METHOD_NOT_ALLOWED, "Only GET and HEAD.\n");
        r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    let Some(pages) = core.host.clone() else { return not_found_plain() };
    let host = request_host(&req).unwrap_or_default();
    let raw = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let document = req.headers().get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("document");

    let inm = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(str::to_string);
    // The binary puts the trusted client address here before the request reaches this router.
    let ip = req.extensions().get::<ClientInfo>().map(|c| c.ip).unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
    let from = ch_core::access::wide_address_key(ip);
    let ask = Ask { document, inm: inm.as_deref(), from: &from };
    let resp = if host == pages {
        if let Some(r) = asset_for(&raw) {
            return finish(r, head);
        }
        match path::parse_path_form(&raw) {
            Parsed::Robots => robots(),
            Parsed::NotFound => content_page(&core, None, StatusCode::NOT_FOUND, Failure::NotFound),
            Parsed::Site(t) => path_form(&core, t, query.as_deref(), &ask).await,
        }
    } else {
        let owner = host.strip_suffix(&pages).and_then(|h| h.strip_suffix('.')).unwrap_or("").to_string();
        if !core.full_mode().await {
            not_found_plain()
        } else {
            match path::parse_sub_form(&owner, &raw) {
                Parsed::Robots => robots(),
                Parsed::NotFound => not_found_plain(),
                Parsed::Site(t) => sub_form(&core, t, query.as_deref(), &ask).await,
            }
        }
    };
    finish(resp, head)
}

/// What every answer on the pages host carries.
fn finish(mut resp: Response, head: bool) -> Response {
    let h = resp.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    h.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-cache"));
    if head {
        *resp.body_mut() = Body::empty();
    }
    resp
}

/// What a content request brings besides its target.
struct Ask<'a> {
    document: bool,
    inm: Option<&'a str>,
    /// The client's address key, for the per-address cap on open replies.
    from: &'a str,
}

async fn path_form(core: &Core, t: Target, query: Option<&str>, ask: &Ask<'_>) -> Response {
    if t.needs_slash {
        return redirect(StatusCode::MOVED_PERMANENTLY, &add_slash(&t, query, true));
    }
    if ask.document {
        if core.full_mode().await && ch_core::pages::valid_owner_label(&t.owner) {
            let tok = t.token.as_deref().map(|k| format!("/-/t/{k}")).unwrap_or_default();
            let q = query.map(|q| format!("?{q}")).unwrap_or_default();
            let to = format!("{}{tok}/{}/{}{q}", core.owner_origin(&t.owner), t.project, encode_rest(&t.rest));
            return redirect(StatusCode::MOVED_PERMANENTLY, &to);
        }
        return shell(core, &t);
    }
    content(core, t, true, ask).await
}

async fn sub_form(core: &Core, t: Target, query: Option<&str>, ask: &Ask<'_>) -> Response {
    if t.needs_slash {
        return redirect(StatusCode::MOVED_PERMANENTLY, &add_slash(&t, query, false));
    }
    content(core, t, false, ask).await
}

/// `/<owner>/<project>` to `/<owner>/<project>/`, keeping a token and a query.
fn add_slash(t: &Target, query: Option<&str>, path_form: bool) -> String {
    let tok = t.token.as_deref().map(|k| format!("/-/t/{k}")).unwrap_or_default();
    let q = query.map(|q| format!("?{q}")).unwrap_or_default();
    if path_form { format!("{tok}/{}/{}/{q}", t.owner, t.project) } else { format!("{tok}/{}/{q}", t.project) }
}

/// The decoded rest of a path, encoded again for a `Location`.
fn encode_rest(rest: &str) -> String {
    rest.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
}

fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The shell: a Codehaven page with no lookup behind it, the same for every address, so it
/// says nothing about which sites exist.
fn shell(core: &Core, t: &Target) -> Response {
    let cfg = serde_json::json!({
        "owner": t.owner,
        "project": t.project,
        "prefix": format!("/{}/{}/", t.owner, t.project),
        "token": t.token,
        "open": core.open_url(&t.owner, &t.project),
        "quota": STORAGE_QUOTA,
        "sandbox": SANDBOX,
    });
    let title = html_escape(&format!("{}/{}", t.owner, t.project));
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\"><title>{title}</title><link rel=\"stylesheet\" href=\"/-/shell.css\">\
         <script type=\"application/json\" id=\"chp\">{}</script><script src=\"/-/shell.js\" defer></script></head>\
         <body><noscript><p>This site needs JavaScript.</p></noscript></body></html>\n",
        crate::inject::script_json(&cfg)
    );
    let origin = core.origin();
    let csp = format!(
        "default-src 'none'; script-src {origin}/-/shell.js; style-src {origin}/-/shell.css; frame-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let mut r = (StatusCode::OK, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    set(h, header::CONTENT_SECURITY_POLICY, &csp);
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::VARY, HeaderValue::from_static("Sec-Fetch-Dest"));
    r
}

/// The shell's own two files. Served only on the path form, at these exact paths, which no
/// site can have (`-` is never an account name).
fn shell_asset(rest: &str) -> Option<Response> {
    let (body, ty) = match rest {
        "shell.js" => (SHELL_JS, "text/javascript; charset=utf-8"),
        "shell.css" => (SHELL_CSS, "text/css; charset=utf-8"),
        _ => return None,
    };
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ty));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Some(r)
}

#[derive(Clone, Copy)]
enum Failure {
    /// No site, not visible, signed out or a token that ran out: one page for all of them.
    SignIn,
    NotFound,
}

/// The content for a target: a file of the live copy, or the one failure page.
async fn content(core: &Core, t: Target, sandboxed: bool, ask: &Ask<'_>) -> Response {
    let (document, inm) = (ask.document, ask.inm);
    // The place lasts as long as the reply's body (or until it stalls or runs too long);
    // `busy` only while the reply is looked up.
    // The address's place comes first and is waited for briefly; nothing else is held meanwhile.
    let address = core.take_address_soon(ask.from).await;
    let (Some(address), Ok(open), Ok(_busy)) = (address, core.open.clone().try_acquire_owned(), core.busy.clone().try_acquire_owned())
    else {
        let mut r = plain(StatusCode::SERVICE_UNAVAILABLE, "Busy. Try again in a few seconds.\n");
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        return r;
    };
    let viewer = t.token.as_deref().and_then(|k| crate::token::open(&core.key, k, chrono::Utc::now().timestamp()));
    let site = match core.find_site(&t.owner, &t.project).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "pages lookup failed");
            return plain(StatusCode::SERVICE_UNAVAILABLE, "Not available right now.\n");
        }
    };
    // A token is for one site: anything else counts as no token.
    let viewer = match (&site, viewer) {
        (Some(s), Some(g)) if g.repo == s.repo => Some(g.user),
        _ => None,
    };
    let visible = match &site {
        Some(s) => core.may_view(s, viewer).await.unwrap_or(false),
        None => false,
    };
    let (Some(site), true) = (site, visible) else { return refused(core, &t, sandboxed, document) };
    let Some((copy, _)) = site.live else { return refused(core, &t, sandboxed, document) };
    serve_file(core, &t, copy, sandboxed, inm, Place { open, address }).await
}

/// What an open reply holds: one of the open places, and one of its address's slots.
struct Place {
    open: OwnedSemaphorePermit,
    address: AddressSlot,
}

/// No site, not visible, or a token that ran out: the same answer for all of them. The path
/// form shows the sign-in page in the frame; full mode, which has no shell, sends a person to
/// sign in and answers anything else with a plain 404.
fn refused(core: &Core, t: &Target, sandboxed: bool, document: bool) -> Response {
    if sandboxed {
        return content_page(core, Some(t), StatusCode::NOT_FOUND, Failure::SignIn);
    }
    if document {
        return redirect(StatusCode::SEE_OTHER, &sign_in_url(core, t));
    }
    not_found_plain()
}

/// The app route that makes a token for this site and this path.
fn sign_in_url(core: &Core, t: &Target) -> String {
    format!("{}{}", core.open_url(&t.owner, &t.project), query_value(&format!("/{}", t.rest)))
}

/// Percent-encodes everything but unreserved characters and `/`, for a query value.
fn query_value(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn serve_file(core: &Core, t: &Target, copy: i64, sandboxed: bool, inm: Option<&str>, open: Place) -> Response {
    let (want, folder) = path::candidates(&t.rest);
    let mut names = want.clone();
    names.extend(folder.clone());
    names.push("404.html".into());
    let rows: Vec<(String, String, bool)> =
        match sqlx::query_as("SELECT path, blob_sha256, utf8 FROM pages_files WHERE deployment_id = $1 AND path = ANY($2)")
            .bind(copy)
            .bind(&names)
            .fetch_all(&core.db)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "pages file lookup failed");
                return plain(StatusCode::SERVICE_UNAVAILABLE, "Not available right now.\n");
            }
        };
    let find = |p: &str| rows.iter().find(|r| r.0 == p);
    if let Some(hit) = want.iter().find_map(|p| find(p)) {
        return send(core, t, copy, hit, sandboxed, StatusCode::OK, inm, open).await;
    }
    if let Some(f) = &folder
        && find(f).is_some()
    {
        let tok = t.token.as_deref().map(|k| format!("/-/t/{k}")).unwrap_or_default();
        let base = if sandboxed { format!("{tok}/{}/{}", t.owner, t.project) } else { format!("{tok}/{}", t.project) };
        return redirect(StatusCode::MOVED_PERMANENTLY, &format!("{base}/{}/", encode_rest(&t.rest)));
    }
    if let Some(hit) = find("404.html") {
        return send(core, t, copy, hit, sandboxed, StatusCode::NOT_FOUND, None, open).await;
    }
    content_page(core, Some(t), StatusCode::NOT_FOUND, Failure::NotFound)
}

/// One file of a copy, with the headers for its form. HTML on the path form gets the bridge.
///
/// Every file is streamed from the blob store, so a reply holds at most one piece of its file
/// (for HTML on the path form, the first `inject::SCAN` bytes, to place the bridge). The reply
/// keeps its place until its body is sent or dropped, or until `watched` cuts it.
#[allow(clippy::too_many_arguments)]
async fn send(
    core: &Core,
    t: &Target,
    copy: i64,
    row: &(String, String, bool),
    sandboxed: bool,
    status: StatusCode,
    inm: Option<&str>,
    open: Place,
) -> Response {
    let (path, sha, utf8) = row;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let essence = mime.essence_str().to_string();
    let html = essence == "text/html";
    let etag = format!("\"p{copy}-{}\"", &sha[..16.min(sha.len())]);
    if !html && status == StatusCode::OK && inm.is_some_and(|v| v.split(',').any(|e| e.trim() == etag)) {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        set(r.headers_mut(), header::ETAG, &etag);
        if sandboxed {
            content_headers(r.headers_mut());
        }
        return r;
    }
    let stream = match core.blobs.get_stream(sha).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, copy, "a pages file is missing from the blob store");
            return plain(StatusCode::SERVICE_UNAVAILABLE, "Not available right now.\n");
        }
    };
    let stream = if html && sandboxed {
        match with_bridge(stream, &bridge(core, t, false)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, copy, "reading a pages file failed");
                return plain(StatusCode::SERVICE_UNAVAILABLE, "Not available right now.\n");
            }
        }
    } else {
        stream
    };
    let pages = &core.config.pages;
    let body = watched(stream, open, Duration::from_secs(pages.reply_stall_secs), Duration::from_secs(pages.reply_deadline_secs));
    let ty = if *utf8 && (html || is_text(&essence)) { format!("{essence}; charset=utf-8") } else { essence.clone() };
    let mut r = Response::new(body);
    *r.status_mut() = status;
    let h = r.headers_mut();
    set(h, header::CONTENT_TYPE, &ty);
    if !html {
        set(h, header::ETAG, &etag);
    }
    if sandboxed {
        content_headers(h);
    }
    r
}

/// An HTML file with the bridge in it: reads only as far as `inject::offset` looks, puts the
/// bridge there, and streams the rest as it comes.
async fn with_bridge(
    mut s: BoxStream<'static, std::io::Result<Bytes>>,
    script: &str,
) -> std::io::Result<BoxStream<'static, std::io::Result<Bytes>>> {
    let mut head = Vec::new();
    while head.len() < crate::inject::SCAN {
        match s.try_next().await? {
            Some(piece) => head.extend_from_slice(&piece),
            None => break,
        }
    }
    let head = Bytes::from(crate::inject::inject(&head, script));
    Ok(futures::stream::once(async move { Ok(head) }).chain(s).boxed())
}

/// A reply's body that gives its place back when it ends, is dropped, goes `stall` without
/// sending a piece, or runs past `deadline`, whichever comes first.
///
/// A client that does not read leaves the body unpolled, so the clock runs in its own task,
/// not in the stream. A cut body ends in an error when it is next polled, which aborts the
/// connection: the client never gets a file that only looks complete. The address slot stays
/// until the body is dropped, so one address cannot pile up connections that never read.
fn watched(stream: BoxStream<'static, std::io::Result<Bytes>>, place: Place, stall: Duration, deadline: Duration) -> Body {
    struct Watch {
        start: Instant,
        /// Milliseconds from `start` to the last piece sent.
        last: AtomicU64,
        cut: AtomicBool,
        open: Mutex<Option<OwnedSemaphorePermit>>,
        /// Dropped with the body, which ends the clock's task at once.
        _gone: tokio::sync::oneshot::Sender<()>,
    }
    let start = Instant::now();
    let (gone, mut body_gone) = tokio::sync::oneshot::channel::<()>();
    let w =
        Arc::new(Watch { start, last: AtomicU64::new(0), cut: AtomicBool::new(false), open: Mutex::new(Some(place.open)), _gone: gone });
    let weak = Arc::downgrade(&w);
    let end = start + deadline;
    tokio::spawn(async move {
        loop {
            let due = match weak.upgrade() {
                Some(w) => (w.start + Duration::from_millis(w.last.load(Ordering::Relaxed)) + stall).min(end),
                None => return,
            };
            tokio::select! {
                _ = tokio::time::sleep_until(due) => {}
                _ = &mut body_gone => return,
            }
            let Some(w) = weak.upgrade() else { return };
            let now = Instant::now();
            let quiet = now >= w.start + Duration::from_millis(w.last.load(Ordering::Relaxed)) + stall;
            if now >= end || quiet {
                w.cut.store(true, Ordering::Relaxed);
                w.open.lock().unwrap_or_else(|e| e.into_inner()).take();
                return;
            }
        }
    });
    let address = place.address;
    let pieces = futures::stream::unfold((stream, w, address, false), |(mut s, w, a, done)| async move {
        if done {
            return None;
        }
        if w.cut.load(Ordering::Relaxed) {
            let err = std::io::Error::other("the reply stalled or ran past its time limit");
            return Some((Err(err), (s, w, a, true)));
        }
        let piece = s.next().await?;
        let failed = piece.is_err();
        let ms = u64::try_from(w.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        w.last.store(ms, Ordering::Relaxed);
        Some((piece, (s, w, a, failed)))
    });
    Body::from_stream(pieces)
}

fn is_text(essence: &str) -> bool {
    essence.starts_with("text/") || matches!(essence, "application/javascript" | "application/json" | "image/svg+xml" | "application/xml")
}

/// The sandbox, on everything served as content on the path form.
fn content_headers(h: &mut HeaderMap) {
    set(h, header::CONTENT_SECURITY_POLICY, &format!("sandbox {SANDBOX}"));
    h.insert(header::VARY, HeaderValue::from_static("Sec-Fetch-Dest"));
}

/// The bridge for one page: the script, and its settings as JSON.
fn bridge(core: &Core, t: &Target, sign_in: bool) -> String {
    let cfg = serde_json::json!({
        "o": core.origin(),
        "p": format!("/{}/{}/", t.owner, t.project),
        "t": t.token.as_deref().map(|k| format!("/-/t/{k}")).unwrap_or_default(),
        "q": STORAGE_QUOTA,
        "a": u8::from(sign_in),
    });
    format!("<script>{}({});</script>", BRIDGE.trim_end(), crate::inject::script_json(&cfg))
}

/// Codehaven's own page as content: the sign-in page or "not found". On the path form it is
/// sandboxed and carries the bridge, so it clears a frame's name like any page does.
fn content_page(core: &Core, t: Option<&Target>, status: StatusCode, failure: Failure) -> Response {
    let (title, text, link) = match (failure, t) {
        (Failure::SignIn, Some(t)) => (
            "Sign in to see this site",
            "This site is not public, or there is no site here.",
            format!("<p><a href=\"{}\" target=\"_top\">Sign in</a></p>", html_escape(&sign_in_url(core, t))),
        ),
        _ => ("Page not found", "There is no page at this address.", String::new()),
    };
    let html = PAGE.replace("{title}", title).replace("{text}", text).replace("{link}", &link);
    let html = match t {
        Some(t) => crate::inject::inject(html.as_bytes(), &bridge(core, t, matches!(failure, Failure::SignIn))),
        None => html.into_bytes(),
    };
    let mut r = (status, html).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    content_headers(h);
    r
}

fn robots() -> Response {
    let mut r = (StatusCode::OK, "User-agent: *\nDisallow: /\n").into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn plain(status: StatusCode, text: &'static str) -> Response {
    let mut r = (status, text).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn not_found_plain() -> Response {
    plain(StatusCode::NOT_FOUND, "Not found.\n")
}

fn redirect(status: StatusCode, to: &str) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    set(r.headers_mut(), header::LOCATION, to);
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn set(h: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The shell's files, by path, before any site lookup. Used by the router for `/-/<file>`.
pub(crate) fn asset_for(raw: &str) -> Option<Response> {
    raw.strip_prefix("/-/").and_then(shell_asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert!(is_pages_host("pages.example.com", "pages.example.com"));
        assert!(is_pages_host("pages.example.com", "alice.pages.example.com"));
        assert!(!is_pages_host("pages.example.com", "a.b.pages.example.com"));
        assert!(!is_pages_host("pages.example.com", "xpages.example.com"));
        assert!(!is_pages_host("pages.example.com", "code.example.com"));
        assert!(!is_pages_host("pages.example.com", ".pages.example.com"));
    }

    #[test]
    fn the_sandbox_never_lets_a_page_out() {
        for word in SANDBOX.split(' ') {
            assert!(word.starts_with("allow-"));
        }
        assert!(!SANDBOX.contains("allow-same-origin"));
        assert!(!SANDBOX.contains("allow-popups-to-escape-sandbox"));
        assert!(!SANDBOX.split(' ').any(|w| w == "allow-top-navigation"));
        assert!(!SANDBOX.contains("allow-storage-access"));
    }

    #[test]
    fn a_rest_is_encoded_again_for_a_redirect() {
        assert_eq!(encode_rest("a b/ü?x"), "a%20b/%C3%BC%3Fx");
    }
}
