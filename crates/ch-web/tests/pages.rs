//! Every page, served from the demo data through a stand-in for the real gate.
//!
//! These tests drive the router the way a browser does: GET a page, read the CSRF token out of
//! it, POST a form with the right Origin. They check the security rules (CSRF, origin, escaping,
//! signed out behaviour) and that no page fails to render.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use ch_web::demo::{self, ADA_COOKIE, ADMIN_NO_2FA_COOKIE, BOB_COOKIE, DEMO_INVITE, DEMO_LINK, DEMO_PASSWORD, ORIGIN};
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Resp {
    fn location(&self) -> String {
        self.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }
    fn set_cookies(&self) -> Vec<String> {
        self.headers.get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect()
    }
}

fn app() -> Router {
    demo::router()
}

async fn send(app: &Router, method: Method, path: &str, cookie: Option<&str>, origin: Option<&str>, form: Option<&str>) -> Resp {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    if let Some(o) = origin {
        b = b.header(header::ORIGIN, o);
    }
    let body = match form {
        Some(f) => {
            b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").header(header::CONTENT_LENGTH, f.len());
            Body::from(f.to_string())
        }
        None => Body::empty(),
    };
    let r = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let (parts, body) = r.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Resp { status: parts.status, headers: parts.headers, body: String::from_utf8_lossy(&bytes).to_string() }
}

fn ada() -> String {
    format!("ch_session={ADA_COOKIE}")
}

async fn get(path: &str) -> Resp {
    send(&app(), Method::GET, path, Some(&ada()), None, None).await
}

async fn get_as(cookie: &str, path: &str) -> Resp {
    send(&app(), Method::GET, path, Some(cookie), None, None).await
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
    let i = s.find(start).unwrap_or_else(|| panic!("`{start}` not found in page")) + start.len();
    let j = s[i..].find(end).unwrap() + i;
    &s[i..j]
}

fn csrf_of(html: &str) -> String {
    between(html, "\"X-CSRF-Token\": \"", "\"").to_string()
}

fn form_csrf(html: &str) -> String {
    between(html, "name=\"_csrf\" value=\"", "\"").to_string()
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn post(path: &str, fields: &[(&str, &str)]) -> Resp {
    post_as(&ada(), path, fields).await
}

async fn post_as(cookie: &str, path: &str, fields: &[(&str, &str)]) -> Resp {
    let app = app();
    let page = send(&app, Method::GET, "/", Some(cookie), None, None).await;
    let token = csrf_of(&page.body);
    let mut body = format!("_csrf={token}");
    for (k, v) in fields {
        body.push_str(&format!("&{}={}", enc(k), enc(v)));
    }
    send(&app, Method::POST, path, Some(cookie), Some(ORIGIN), Some(&body)).await
}

// ------------------------------------------------------------------ every page renders

const PAGES: &[(&str, &str)] = &[
    ("/", "Your repositories"),
    ("/ada/lighthouse", "Lighthouse"),
    ("/ada/lighthouse/tree/main/src", "beam.rs"),
    ("/ada/lighthouse/tree/feature/signal-colours", "README.md"),
    ("/ada/lighthouse/find/main", "src/beam.rs"),
    ("/ada/lighthouse/blob/main/src/beam.rs", "hl-keyword"),
    ("/ada/lighthouse/blob/main/README.md", "Install"),
    ("/ada/lighthouse/blob/main/README.md?display=source", "cargo install"),
    ("/ada/lighthouse/blob/main/assets/logo.png", "binary"),
    ("/ada/lighthouse/blame/main/src/beam.rs", "bl-sha"),
    ("/ada/lighthouse/commits", "Add colour channels"),
    ("/ada/lighthouse/commit/main", "Add colour channels to Beam"),
    ("/ada/lighthouse/commit/06a1b2c", "diff-file"),
    ("/ada/lighthouse/commits/main/src/beam.rs", "Commits"),
    ("/ada/lighthouse/branches", "feature/signal-colours"),
    ("/ada/lighthouse/tags", "v0.2.0"),
    ("/ada/lighthouse/compare", "Compare changes"),
    ("/ada/lighthouse/compare/main...feature/signal-colours", "Create pull request"),
    ("/ada/lighthouse/compare/main...feature/signal-colours?diff=split", "diff-split"),
    ("/ada/lighthouse/edit/main/src/main.rs", "Commit changes"),
    ("/ada/lighthouse/new/main/src", "File name"),
    ("/ada/lighthouse/delete/main/src/main.rs", "Delete file"),
    ("/ada/lighthouse/issues", "Support colour channels"),
    ("/ada/lighthouse/issues?state=closed", "Add a Signal type"),
    ("/ada/lighthouse/issues?labels=bug", "Crash when the message is empty"),
    ("/ada/lighthouse/issues/new", "Create issue"),
    ("/ada/lighthouse/issues/12", "colour channels"),
    ("/ada/lighthouse/labels", "good first issue"),
    ("/ada/lighthouse/milestones", "0.3.0"),
    ("/ada/lighthouse/pulls", "Colour the signal on the wire"),
    ("/ada/lighthouse/pulls?state=merged", "Print a warning"),
    ("/ada/lighthouse/pulls/new?head=feature/signal-colours", "Create pull request"),
    ("/ada/lighthouse/pulls/14", "wants to merge"),
    ("/ada/lighthouse/pulls/14/commits", "Colour the signal"),
    ("/ada/lighthouse/pulls/14/checks", "test"),
    ("/ada/lighthouse/pulls/14/files", "diff-file"),
    ("/ada/lighthouse/pulls/7", "Merged"),
    ("/ada/lighthouse/pulls/13", "draft"),
    ("/ada/lighthouse/actions", "CI"),
    ("/ada/lighthouse/actions/run", "Run workflow"),
    ("/ada/lighthouse/actions/runs/52", "test"),
    ("/ada/lighthouse/actions/runs/52/jobs/521", "cargo test"),
    ("/ada/lighthouse/actions/runs/53/jobs/531", "data-job-stream"),
    ("/ada/lighthouse/releases", "Lighthouse 0.2.0"),
    ("/ada/lighthouse/releases/tag/v0.2.0", "What is new"),
    ("/ada/lighthouse/releases/new", "Tag"),
    ("/ada/lighthouse/settings", "Danger zone"),
    ("/ada/lighthouse/settings/collaborators", "bob"),
    ("/ada/lighthouse/settings/branches", "main"),
    ("/ada/lighthouse/settings/hooks", "hooks.example.test"),
    ("/ada/lighthouse/settings/secrets", "DEPLOY_KEY"),
    ("/ada/lighthouse/settings/runners", "harbour-runner-1"),
    ("/bob/sandbox", "empty"),
    ("/harbor/dock", "dock"),
    ("/ada", "Ada Marlow"),
    ("/harbor", "Harbor Works"),
    ("/orgs/harbor/people", "bob"),
    ("/orgs/harbor/teams", "maintainers"),
    ("/orgs/harbor/teams/1", "bob"),
    ("/orgs/harbor/settings", "Default repository permission"),
    ("/explore", "lighthouse"),
    ("/search?q=colour&type=issues", "colour"),
    ("/notifications", "Colour the signal"),
    ("/new", "Visibility"),
    ("/new/import", "Import a repository"),
    ("/new/org", "Organization"),
    ("/settings/profile", "Display name"),
    ("/settings/password", "New password"),
    ("/settings/security", "Two-factor"),
    ("/settings/sessions", "This device"),
    ("/settings/keys", "SHA256"),
    ("/settings/tokens", "laptop git"),
    ("/settings/appearance", "Theme"),
    ("/admin", "Overview"),
    ("/admin/invites", "dana@example.test"),
    ("/admin/users", "bob"),
    ("/admin/runners", "harbour-runner-1"),
    ("/admin/audit", "auth.sign_in"),
];

#[tokio::test]
async fn every_page_renders_for_a_signed_in_user() {
    let app = app();
    let mut failures = Vec::new();
    for (path, needle) in PAGES {
        let r = send(&app, Method::GET, path, Some(&ada()), None, None).await;
        if r.status != StatusCode::OK {
            failures.push(format!("{path}: status {}", r.status));
        } else if !r.body.contains(needle) {
            failures.push(format!("{path}: missing `{needle}`"));
        }
        if r.status == StatusCode::OK {
            let cc = r.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).unwrap_or("");
            if !cc.contains("no-store") {
                failures.push(format!("{path}: cache-control is `{cc}`, want no-store"));
            }
            // Accessibility basics every page must have: one main landmark, a level one heading,
            // a text alternative on every image, and a title.
            if r.body.matches("id=\"main\"").count() != 1 {
                failures.push(format!("{path}: want exactly one id=\"main\", found {}", r.body.matches("id=\"main\"").count()));
            }
            if !r.body.contains("<h1") {
                failures.push(format!("{path}: no <h1>"));
            }
            for img in r.body.split("<img ").skip(1) {
                let tag = img.split('>').next().unwrap_or("");
                if !tag.contains(" alt=") {
                    failures.push(format!("{path}: <img> without alt: {tag}"));
                    break;
                }
            }
            if !r.body.contains("<title>") || r.body.contains("<title></title>") {
                failures.push(format!("{path}: missing title"));
            }
            for bad in [" style=\"", "<script>", "onclick=", "javascript:"] {
                if r.body.contains(bad) {
                    failures.push(format!("{path}: contains forbidden `{bad}` (the CSP would block it)"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn the_dispatch_form_offers_a_menu_and_a_checkbox_for_typed_inputs() {
    // The demo CI workflow has a boolean `verbose` and a choice `target` (ch-3a2).
    let r = get("/ada/lighthouse/actions/run").await;
    assert_eq!(r.status, StatusCode::OK);
    let select = between(&r.body, "<select id=\"wf-input-", "</select>");
    assert!(select.contains("name=\"input_target\""), "{select}");
    assert!(select.contains("<option value=\"staging\" selected>staging</option>"), "{select}");
    assert!(select.contains("<option value=\"production\">production</option>"), "{select}");
    let check = between(&r.body, "name=\"input_verbose\" type=\"checkbox\"", ">");
    assert!(check.contains("value=\"true\""), "{check}");
    assert!(!r.body.contains("name=\"input_verbose\" type=\"text\""));
}

#[tokio::test]
async fn a_workflow_badge_is_an_svg_for_a_signed_in_viewer() {
    let r = get("/ada/lighthouse/actions/workflows/ci.yml/badge.svg").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_TYPE], "image/svg+xml");
    assert!(r.headers[header::CACHE_CONTROL].to_str().unwrap().contains("no-store"));
    assert!(r.body.starts_with("<svg") && r.body.contains(">passing</text>"), "{}", r.body);
    assert_eq!(get("/ada/lighthouse/actions/workflows/nothing.yml/badge.svg").await.status, StatusCode::NOT_FOUND);
    // Signed out, the gate answers before the page: no badge for strangers (ch-c1d).
    let anon = send(&app(), Method::GET, "/ada/lighthouse/actions/workflows/ci.yml/badge.svg", None, None, None).await;
    assert_ne!(anon.status, StatusCode::OK);
    assert!(!anon.body.contains("<svg"));
}

#[tokio::test]
async fn pages_without_a_repo_or_an_issue_are_404() {
    for p in [
        "/ada/nope",
        "/nobody/lighthouse",
        "/ada/lighthouse/issues/999",
        "/ada/lighthouse/pulls/999",
        "/ada/lighthouse/commit/ffffffffff",
        "/ada/lighthouse/tree/main/missing",
    ] {
        let r = get(p).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{p}");
        assert!(r.body.contains("Not found"), "{p}");
    }
}

#[tokio::test]
async fn a_private_repo_is_a_404_for_someone_who_cannot_read_it() {
    let r = get_as(&format!("ch_session={BOB_COOKIE}"), "/ada/notes").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = get("/ada/notes").await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn repository_pages_link_to_the_workspace() {
    // The demo has the editor on, so every repository page offers the workspace button.
    let r = get("/ada/lighthouse").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("data-workspace-link"), "the header button is missing");
    assert!(r.body.contains("href=\"/ada/lighthouse/workspace\""));

    // The file page knows the branch and the file, so its button opens that file. The `&` of
    // the query is escaped in the attribute, as on every other link.
    let r = get("/ada/lighthouse/blob/main/src/beam.rs").await;
    assert_eq!(r.status, StatusCode::OK);
    let at = r.body.find("data-workspace-link").expect("the button");
    let tag = &r.body[r.body[..at].rfind("<a ").unwrap()..at];
    assert!(tag.contains("/ada/lighthouse/workspace?branch=main"), "{tag}");
    assert!(tag.contains("beam.rs"), "{tag}");

    // No signed-out check: the demo gate sends every anonymous page request to the sign-in
    // page, so no repository is ever read signed out here.
}

/// The opening `<a ...` tag of the workspace button, up to the `data-workspace-link` attribute.
fn workspace_tag(body: &str) -> String {
    let at = body.find("data-workspace-link").expect("the button");
    body[body[..at].rfind("<a ").unwrap()..at].to_string()
}

#[tokio::test]
async fn a_file_page_at_a_tag_opens_the_workspace_on_the_default_branch() {
    // v0.2.0 is a tag, not a branch, so the button cannot ask the workspace for it. It still
    // opens the same file, on the default branch.
    let r = get("/ada/lighthouse/blob/v0.2.0/src/beam.rs").await;
    assert_eq!(r.status, StatusCode::OK);
    let tag = workspace_tag(&r.body);
    assert!(!tag.contains("branch="), "{tag}");
    assert!(tag.contains("/ada/lighthouse/workspace?path="), "{tag}");
    assert!(tag.contains("beam.rs"), "{tag}");
}

#[tokio::test]
async fn an_open_pull_request_opens_the_workspace_on_its_branch() {
    // Pull 14 is open and comes from a branch of this repository, so every tab of it opens
    // the workspace on that branch. The slash of `feature/signal-colours` is percent-encoded.
    for p in [
        "/ada/lighthouse/pulls/14",
        "/ada/lighthouse/pulls/14/commits",
        "/ada/lighthouse/pulls/14/checks",
        "/ada/lighthouse/pulls/14/files",
    ] {
        let r = get(p).await;
        assert_eq!(r.status, StatusCode::OK, "{p}");
        let tag = workspace_tag(&r.body);
        assert!(tag.contains("/ada/lighthouse/workspace?branch=feature%2Fsignal-colours"), "{p}: {tag}");
    }

    // Pull 7 is merged, so its head branch may be gone; it keeps the plain button.
    let r = get("/ada/lighthouse/pulls/7").await;
    assert_eq!(r.status, StatusCode::OK);
    let tag = workspace_tag(&r.body);
    assert!(!tag.contains("branch="), "{tag}");
}

// ------------------------------------------------------------------ signed out

#[tokio::test]
async fn signed_out_visitors_only_get_the_sign_in_pages() {
    let app = app();
    for (path, _) in PAGES {
        let r = send(&app, Method::GET, path, None, None, None).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path}");
        assert!(r.location().starts_with("/login?next="), "{path}: {}", r.location());
        assert!(!r.body.contains("lighthouse"), "{path} leaked");
    }
    for p in ["/login", "/invite/chi_nope", "/password/chl_nope", "/favicon.svg"] {
        let r = send(&app, Method::GET, p, None, None, None).await;
        assert_ne!(r.status, StatusCode::SEE_OTHER, "{p} should be public");
    }
}

#[tokio::test]
async fn the_sign_in_page_does_not_say_who_is_here() {
    let r = send(&app(), Method::GET, "/login", None, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Sign in to Codehaven"));
    for leak in ["ada", "lighthouse", "Sign up", "Create an account", "Forgot"] {
        assert!(!r.body.contains(&format!(">{leak}<")), "leaked {leak}");
    }
    assert!(!r.body.to_lowercase().contains("sign up"));
    assert!(r.set_cookies().iter().any(|c| c.starts_with("ch_csrf=") && c.contains("HttpOnly") && c.contains("SameSite=Lax")));
}

// ------------------------------------------------------------------ sign in

async fn login_form(app: &Router) -> (String, String) {
    let r = send(app, Method::GET, "/login", None, None, None).await;
    let cookie = r.set_cookies().into_iter().find(|c| c.starts_with("ch_csrf=")).unwrap();
    let pair = cookie.split(';').next().unwrap().to_string();
    let token = form_csrf(&r.body);
    assert_eq!(pair, format!("ch_csrf={token}"), "the form token must equal the cookie");
    (pair, token)
}

async fn login(app: &Router, user: &str, pw: &str, next: &str) -> Resp {
    let (cookie, token) = login_form(app).await;
    let body = format!("_csrf={token}&username={}&password={}&next={}", enc(user), enc(pw), enc(next));
    send(app, Method::POST, "/login", Some(&cookie), Some(ORIGIN), Some(&body)).await
}

#[tokio::test]
async fn signing_in_sets_a_hardened_session_cookie_and_goes_to_next() {
    let r = login(&app(), "ada", DEMO_PASSWORD, "/ada/lighthouse").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/ada/lighthouse");
    let c = r.set_cookies().into_iter().find(|c| c.starts_with("ch_session=")).unwrap();
    assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax") && c.contains("Path=/") && c.contains("Max-Age="), "{c}");
}

#[tokio::test]
async fn next_cannot_send_you_to_another_site() {
    for evil in ["https://evil.example/", "//evil.example", "javascript:alert(1)", "/\\evil.example"] {
        let r = login(&app(), "ada", DEMO_PASSWORD, evil).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        assert_eq!(r.location(), "/", "{evil}");
    }
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_user_look_the_same() {
    let a = login(&app(), "ada", "wrong password here", "/").await;
    let b = login(&app(), "nobody-here", DEMO_PASSWORD, "/").await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.status, b.status);
    for r in [&a, &b] {
        assert!(
            r.body.contains("That username and password don&#39;t match.") || r.body.contains("That username and password don't match.")
        );
        assert!(r.set_cookies().iter().all(|c| !c.starts_with("ch_session=")));
    }
}

#[tokio::test]
async fn the_sign_in_form_refuses_a_missing_token_a_wrong_token_and_a_foreign_origin() {
    let app = app();
    let (cookie, token) = login_form(&app).await;
    let ok = format!("_csrf={token}&username=ada&password={}", enc(DEMO_PASSWORD));
    assert_eq!(send(&app, Method::POST, "/login", Some(&cookie), Some(ORIGIN), Some(&ok)).await.status, StatusCode::SEE_OTHER);
    let none = format!("username=ada&password={}", enc(DEMO_PASSWORD));
    assert_eq!(send(&app, Method::POST, "/login", Some(&cookie), Some(ORIGIN), Some(&none)).await.status, StatusCode::FORBIDDEN);
    let wrong = format!("_csrf=deadbeef&username=ada&password={}", enc(DEMO_PASSWORD));
    assert_eq!(send(&app, Method::POST, "/login", Some(&cookie), Some(ORIGIN), Some(&wrong)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        send(&app, Method::POST, "/login", Some(&cookie), Some("https://evil.example"), Some(&ok)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(send(&app, Method::POST, "/login", Some(&cookie), None, Some(&ok)).await.status, StatusCode::FORBIDDEN);
    // A token from another browser (no cookie) does not work either.
    assert_eq!(send(&app, Method::POST, "/login", None, Some(ORIGIN), Some(&ok)).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn two_factor_takes_a_code_and_then_signs_in() {
    let app = app();
    let r = login(&app, "bob", DEMO_PASSWORD, "/").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/login/two-factor"));
    let pending = r.set_cookies().into_iter().find(|c| c.starts_with("ch_2fa=")).unwrap();
    assert!(pending.contains("HttpOnly") && pending.contains("Path=/login"));
    let pending_pair = pending.split(';').next().unwrap().to_string();

    let (csrf_cookie, token) = login_form(&app).await;
    let cookies = format!("{csrf_cookie}; {pending_pair}");
    let bad = format!("_csrf={token}&code=000000");
    let r = send(&app, Method::POST, "/login/two-factor", Some(&cookies), Some(ORIGIN), Some(&bad)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("did not work"));
    let good = format!("_csrf={token}&code=123456&next=%2F");
    let r = send(&app, Method::POST, "/login/two-factor", Some(&cookies), Some(ORIGIN), Some(&good)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.set_cookies().iter().any(|c| c.starts_with("ch_session=")));
    assert!(r.set_cookies().iter().any(|c| c.starts_with("ch_2fa=") && c.contains("Max-Age=0")));
}

#[tokio::test]
async fn two_factor_without_the_first_step_goes_back_to_sign_in() {
    let r = send(&app(), Method::GET, "/login/two-factor", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/login");
}

#[tokio::test]
async fn signing_out_clears_the_cookie() {
    let r = post("/logout", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/login"));
    assert!(r.set_cookies().iter().any(|c| c.starts_with("ch_session=") && c.contains("Max-Age=0")));
}

// ------------------------------------------------------------------ invites and links

#[tokio::test]
async fn every_bad_invite_looks_the_same() {
    let app = app();
    let mut bodies = Vec::new();
    for t in ["chi_madeUpToken", "chi_", "x", &"a".repeat(300), "chi_has space"] {
        let r = send(&app, Method::GET, &format!("/invite/{}", t.replace(' ', "%20")), None, None, None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{t}");
        assert!(r.body.contains("doesn&#39;t work any more") || r.body.contains("doesn't work any more"), "{t}");
        bodies.push(r.body.replace(|c: char| c.is_ascii_digit(), "#"));
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]), "bad invites must not differ");
}

#[tokio::test]
async fn an_invite_can_be_accepted_once_it_validates() {
    let app = app();
    let path = format!("/invite/{DEMO_INVITE}");
    let r = send(&app, Method::GET, &path, None, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Ada Marlow invited you"));
    let cookie = r.set_cookies().into_iter().find(|c| c.starts_with("ch_csrf=")).unwrap().split(';').next().unwrap().to_string();
    let token = form_csrf(&r.body);
    let submit = |fields: String| {
        let (app, path, cookie) = (app.clone(), path.clone(), cookie.clone());
        async move { send(&app, Method::POST, &path, Some(&cookie), Some(ORIGIN), Some(&fields)).await }
    };
    let weak = submit(format!("_csrf={token}&username=dana&email=d%40example.test&password=short&password_confirm=short")).await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(weak.body.contains("at least 12 characters"));
    assert!(weak.body.contains("value=\"dana\""), "keeps what was typed");
    assert!(!weak.body.contains("short\""), "never echoes a password");
    let taken =
        submit(format!("_csrf={token}&username=ada&email=d%40example.test&password={0}&password_confirm={0}", enc(DEMO_PASSWORD))).await;
    assert_eq!(taken.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(taken.body.contains("already taken"));
    let mismatch = submit(format!(
        "_csrf={token}&username=dana&email=d%40example.test&password={}&password_confirm=different-password-here",
        enc(DEMO_PASSWORD)
    ))
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(mismatch.body.contains("not the same"));
    let ok =
        submit(format!("_csrf={token}&username=dana&email=d%40example.test&password={0}&password_confirm={0}", enc(DEMO_PASSWORD))).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(ok.location(), "/login?notice=invite-accepted");
    let login_page = send(&app, Method::GET, &ok.location(), None, None, None).await;
    assert!(login_page.body.contains("Your account is ready"));
}

#[tokio::test]
async fn a_password_link_works_the_same_way() {
    let app = app();
    let r = send(&app, Method::GET, &format!("/password/{DEMO_LINK}"), None, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Choose a password") && r.body.contains("ada"));
    let bad = send(&app, Method::GET, "/password/chl_nope", None, None, None).await;
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn notices_only_show_known_text() {
    let r = send(&app(), Method::GET, "/login?notice=%3Cscript%3Ealert(1)%3C/script%3E", None, None, None).await;
    assert!(!r.body.contains("alert(1)"));
}

// ------------------------------------------------------------------ CSRF on every POST

fn fill(path: &str) -> String {
    let mut out = String::new();
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        out.push_str(&match seg {
            "{owner}" => "ada".to_string(),
            "{repo}" => "lighthouse".to_string(),
            "{token}" => "chi_x".to_string(),
            s if s.starts_with("{*") => "main/src/main.rs".to_string(),
            s if s.starts_with('{') => "1".to_string(),
            s => s.to_string(),
        });
    }
    if out.is_empty() { "/".into() } else { out }
}

#[tokio::test]
async fn every_post_route_refuses_a_missing_token_and_a_foreign_origin() {
    let app = app();
    let page = send(&app, Method::GET, "/", Some(&ada()), None, None).await;
    let token = csrf_of(&page.body);
    let posts: Vec<_> = ch_web::routes::route_table().into_iter().filter(|(m, _)| *m == "POST").collect();
    assert!(posts.len() > 20, "found only {} POST routes", posts.len());
    let mut failures = Vec::new();
    for (_, pattern) in posts {
        let path = fill(pattern);
        let cases: [(&str, Option<&str>, String); 3] = [
            ("no token", Some(ORIGIN), "x=1".to_string()),
            ("foreign origin", Some("https://evil.example"), format!("_csrf={token}")),
            ("no origin", None, format!("_csrf={token}")),
        ];
        for (what, origin, body) in cases {
            let r = send(&app, Method::POST, &path, Some(&ada()), origin, Some(&body)).await;
            // /login and friends use the signed out token, so a signed in caller is bounced home first.
            let signed_out_form = ["/login", "/invite/", "/password/"].iter().any(|p| path.starts_with(p));
            if signed_out_form {
                if r.status != StatusCode::SEE_OTHER && r.status != StatusCode::FORBIDDEN {
                    failures.push(format!("POST {pattern} {what}: {}", r.status));
                }
            } else if r.status != StatusCode::FORBIDDEN {
                failures.push(format!("POST {pattern} {what}: {} (want 403)", r.status));
            }
        }
        // Signed out, a POST never reaches a handler: it goes to the sign-in page.
        let r = send(&app, Method::POST, &path, None, Some(ORIGIN), Some("x=1")).await;
        let public = ch_core::access::is_public(&Method::POST, &path);
        if !public && r.status != StatusCode::SEE_OTHER {
            failures.push(format!("POST {pattern} signed out: {}", r.status));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn a_token_from_another_session_does_not_work() {
    let app = app();
    let page = send(&app, Method::GET, "/", Some(&format!("ch_session={BOB_COOKIE}")), None, None).await;
    let bobs = csrf_of(&page.body);
    let r = send(&app, Method::POST, "/logout", Some(&ada()), Some(ORIGIN), Some(&format!("_csrf={bobs}"))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_csrf_header_works_for_htmx() {
    let app = app();
    let page = send(&app, Method::GET, "/", Some(&ada()), None, None).await;
    let token = csrf_of(&page.body);
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/preview")
        .header(header::COOKIE, ada())
        .header(header::ORIGIN, ORIGIN)
        .header("x-csrf-token", token);
    b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").header(header::CONTENT_LENGTH, 20);
    let r = app.clone().oneshot(b.body(Body::from("text=%2Aone%2A+and+%23")).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("<em>one</em>"), "{body}");
}

// ------------------------------------------------------------------ escaping and raw files

#[tokio::test]
async fn hostile_text_is_escaped_everywhere_it_appears() {
    for p in ["/ada/lighthouse/issues/15", "/ada/lighthouse/issues"] {
        let r = get(p).await;
        assert_eq!(r.status, StatusCode::OK, "{p}");
        assert!(!r.body.contains("<img src=x"), "{p}: raw tag in page");
        assert!(!r.body.contains("<script>alert"), "{p}: raw script in page");
        assert!(r.body.contains("&#60;img"), "{p}");
    }
    let r = get("/ada/lighthouse/issues/15").await;
    assert!(!r.body.contains("href=\"javascript:"), "unsafe link kept");
    assert!(!r.body.contains("<img src=\"https://tracker.example"), "an off-site image must not be loaded");
}

#[tokio::test]
async fn raw_files_cannot_run_script() {
    let r = get("/ada/lighthouse/raw/main/src/main.rs").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert!(r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().starts_with("sandbox"));
    assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    let r = get("/ada/lighthouse/raw/main/.codehaven/workflows/ci.yml").await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn archives_download_as_attachments() {
    let r = get("/ada/lighthouse/archive/main.zip").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment"));
    assert_eq!(get("/ada/lighthouse/archive/main.rar").await.status, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ static files and misc

#[tokio::test]
async fn static_files_are_cached_by_content_hash() {
    let page = get("/").await;
    let css = between(&page.body, "<link rel=\"stylesheet\" href=\"", "\"").to_string();
    assert!(css.starts_with("/static/app.") && css.ends_with(".css"), "{css}");
    let r = send(&app(), Method::GET, &css, None, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));
    assert_eq!(r.headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    let wrong = send(&app(), Method::GET, "/static/app.00000000.css", None, None, None).await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);
    let plain = send(&app(), Method::GET, "/static/app.css", None, None, None).await;
    assert_eq!(plain.headers[header::CACHE_CONTROL], "no-cache");
    assert_ne!(send(&app(), Method::GET, "/static/%2e%2e/Cargo.toml", None, None, None).await.status, StatusCode::OK);
}

/// The header search icon must centre on the field whatever its height (44px on phones, taller with
/// large text), so it is placed by percentage and never by a fixed pixel offset (ch-71r).
#[tokio::test]
async fn header_search_icon_is_centred_by_percentage() {
    let plain = send(&app(), Method::GET, "/static/app.css", None, None, None).await;
    let rule = plain.body.lines().find(|l| l.starts_with(".topsearch .icon")).expect("topsearch icon rule");
    assert!(rule.contains("top: 50%") && rule.contains("translateY(-50%)"), "{rule}");
}

#[tokio::test]
async fn avatars_are_svg() {
    let r = get("/avatar/ada").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_TYPE], "image/svg+xml");
    assert!(r.body.starts_with("<svg") && !r.body.contains("style="));
}

#[tokio::test]
async fn the_theme_cookie_is_honoured_and_junk_is_not() {
    let app = app();
    let r = send(&app, Method::GET, "/login", Some("ch_theme=dark"), None, None).await;
    assert!(r.body.contains("data-theme=\"dark\""));
    let r = send(&app, Method::GET, "/login", Some("ch_theme=\"><script>"), None, None).await;
    assert!(r.body.contains("data-theme=\"system\""));
}

#[tokio::test]
async fn admin_pages_are_hidden_from_non_admins() {
    let r = get_as(&format!("ch_session={BOB_COOKIE}"), "/admin/invites").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = get_as(&format!("ch_session={BOB_COOKIE}"), "/admin").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn forms_show_their_secrets_once_in_the_response_that_made_them() {
    let r = post("/admin/invites", &[("expires_in_hours", "168")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains(DEMO_INVITE));
    assert!(r.headers[header::CACHE_CONTROL].to_str().unwrap().contains("no-store"));
    assert!(r.set_cookies().is_empty(), "a secret must never travel in a cookie");
    let r = post("/settings/tokens", &[("name", "ci"), ("scopes", "repo"), ("days", "90")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("chp_zZ9yDemoTokenSecretValue"));
    assert!(!get("/settings/tokens").await.body.contains("chp_zZ9yDemoTokenSecretValue"));
}

#[tokio::test]
async fn editing_a_file_commits_and_reports_protection() {
    let r =
        post("/ada/lighthouse/edit/main/src/main.rs", &[("content", "fn main() {}\n"), ("message", "Tidy"), ("target", "direct")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(r.location().contains("/blob/main/src/main.rs"));
    let r = post("/ada/lighthouse/edit/main/src/main.rs", &[("content", "x"), ("message", "protected change"), ("target", "direct")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("main is protected"));
    assert!(r.body.contains("name=\"content\""));
    let r = post("/ada/lighthouse/edit/main/src/main.rs", &[("content", "x"), ("message", "m"), ("target", "branch"), ("new_branch", "")])
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = post("/ada/lighthouse/new/main", &[("path", "../escape.txt"), ("content", "x")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn an_edit_without_a_content_field_commits_nothing() {
    let r = post("/ada/lighthouse/edit/main/src/main.rs", &[("message", "Tidy"), ("target", "direct")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert!(r.body.contains("The file contents were missing from the form. Nothing was committed."));
    let r = post("/ada/lighthouse/new/main", &[("path", "docs/empty.md"), ("message", "Add"), ("target", "direct")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    // An empty field is still allowed: it empties the file.
    let r = post("/ada/lighthouse/edit/main/src/main.rs", &[("content", ""), ("message", "Empty it"), ("target", "direct")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
}

#[tokio::test]
async fn ssh_behind_a_tunnel_says_so() {
    let r = get("/ada/lighthouse").await;
    assert!(r.body.contains("SSH, through a tunnel"), "the clone panel names the tunnel");
    let r = get("/settings/keys").await;
    assert!(r.body.contains("reached through a tunnel"));
}

#[tokio::test]
async fn pull_request_actions() {
    let r = post("/ada/lighthouse/pulls/14/comment", &[("body", "Looks good.")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/ada/lighthouse/pulls/14#comment-"));
    let r = post("/ada/lighthouse/pulls/14/merge", &[("method", "squash")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post("/ada/lighthouse/pulls/14/review", &[("state", "bogus"), ("body", "x")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post(
        "/ada/lighthouse/pulls/14/inline",
        &[("path", "src/beam.rs"), ("side", "right"), ("line", "6"), ("body", "Hmm"), ("back", "files")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("/files#diff-"));
}

#[tokio::test]
async fn the_pull_request_page_shows_the_merge_gate_and_inline_threads() {
    let blocked = get("/ada/lighthouse/pulls/13").await;
    assert!(blocked.body.contains("This pull request is a draft"));
    let ready = get("/ada/lighthouse/pulls/14").await;
    assert!(ready.body.contains("This branch can be merged"));
    assert!(ready.body.contains("Merge pull request"));
    let files = get("/ada/lighthouse/pulls/14/files").await;
    assert!(files.body.contains("inline-comment"));
    assert!(files.body.contains("line-comment-btn"));
    assert!(files.body.contains("inline-comment-template"));
    let merged = get("/ada/lighthouse/pulls/7").await;
    assert!(!merged.body.contains("Merge pull request"));
}

#[tokio::test]
async fn review_threads_can_be_replied_to_and_resolved_from_both_tabs() {
    // The Files tab draws the thread under its line, with a reply form that keeps the thread
    // and a resolve button for the pull request's author or a writer.
    let files = get("/ada/lighthouse/pulls/14/files").await;
    assert_eq!(files.status, StatusCode::OK);
    for needle in [
        "name=\"in_reply_to\" value=\"31\"",
        "name=\"path\" value=\"src/beam.rs\"",
        "name=\"thread\" value=\"41\"",
        "action=\"/ada/lighthouse/pulls/14/resolve\"",
        ">Resolve</button>",
        "placeholder=\"Reply\"",
    ] {
        assert!(files.body.contains(needle), "files tab missing {needle}");
    }
    // The Conversation tab shows the same thread once, not once per comment.
    let talk = get("/ada/lighthouse/pulls/14").await;
    assert_eq!(talk.status, StatusCode::OK);
    for needle in ["name=\"in_reply_to\" value=\"31\"", "name=\"thread\" value=\"41\"", "src/beam.rs:6"] {
        assert!(talk.body.contains(needle), "conversation tab missing {needle}");
    }
    assert_eq!(talk.body.matches("id=\"rc31\"").count(), 1, "a thread with several comments shows once");
    assert_eq!(talk.body.matches("id=\"rc32\"").count(), 1);
    // Resolving lands back where the form came from, with a flash.
    let r = post("/ada/lighthouse/pulls/14/resolve", &[("thread", "41"), ("resolved", "1"), ("back", "files")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/ada/lighthouse/pulls/14/files");
    assert!(r.set_cookies().iter().any(|c| c.starts_with("ch_flash")), "a resolve flashes a message");
    let r = post("/ada/lighthouse/pulls/14/resolve", &[("thread", "41"), ("resolved", "0")]).await;
    assert_eq!(r.location(), "/ada/lighthouse/pulls/14");
    // A thread that is not there flashes an error and stays on the page.
    let r = post("/ada/lighthouse/pulls/14/resolve", &[("thread", "999"), ("resolved", "1")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/ada/lighthouse/pulls/14");
    // A reply keeps the thread: it carries in_reply_to and the same anchor.
    let r = post(
        "/ada/lighthouse/pulls/14/inline",
        &[("path", "src/beam.rs"), ("side", "right"), ("line", "6"), ("body", "Done"), ("in_reply_to", "31"), ("back", "files")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("/files#diff-"));
}

#[tokio::test]
async fn sidebar_marks_the_current_section_and_hides_settings_from_readers() {
    let r = get("/ada/lighthouse/issues").await;
    assert!(r.body.contains("side-link current\" href=\"/ada/lighthouse/issues\""));
    let bob = get_as(&format!("ch_session={BOB_COOKIE}"), "/harbor/dock").await;
    assert_eq!(bob.status, StatusCode::OK);
    assert!(!bob.body.contains("/harbor/dock/settings\""));
}

#[tokio::test]
async fn an_admin_without_two_factor_is_told_to_turn_it_on() {
    let cookie = format!("ch_session={ADMIN_NO_2FA_COOKIE}");
    for p in ["/admin", "/admin/invites", "/admin/users", "/admin/runners", "/admin/audit", "/admin/workspace"] {
        let r = get_as(&cookie, p).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{p}");
        assert!(r.body.contains("two-factor"), "{p}");
    }
    let home = get_as(&cookie, "/").await;
    assert!(home.body.contains("Turn on two-factor"), "the banner points at the security page");
    assert!(home.body.contains("/settings/security"));
    assert!(!home.body.contains("href=\"/admin\""), "no admin link until two-factor is on");
    // Admin actions are refused too, not just the pages.
    let r = post_as(&cookie, "/admin/invites", &[("expires_in_hours", "168")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = post_as(&cookie, "/admin/runners/token", &[]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // And with two-factor on the admin link shows.
    assert!(get("/").await.body.contains("href=\"/admin\""));
}

async fn first_chunk(path: &str, to_end: bool) -> (StatusCode, HeaderMap, String) {
    let req = Request::builder().uri(path).header(header::COOKIE, ada()).body(Body::empty()).unwrap();
    let r = app().oneshot(req).await.unwrap();
    let (parts, mut body) = r.into_parts();
    let mut text = String::new();
    let wait = std::time::Duration::from_secs(4);
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(wait, body.frame()).await {
        if let Some(d) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(d));
        }
        if !to_end && text.contains("\n\n") {
            break;
        }
    }
    (parts.status, parts.headers, text)
}

#[tokio::test]
async fn a_running_job_page_has_one_stream_and_a_panel_per_step() {
    let r = get("/ada/lighthouse/actions/runs/53/jobs/531").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body.matches("data-job-stream").count(), 1, "the stream hook appears exactly once");
    assert!(r.body.contains("data-active=\"1\""));
    assert!(r.body.contains("data-step=\"3\""));
    assert!(r.body.contains("data-step-log=\"3\""));
    assert!(r.body.contains("data-state=\"running\""), "the step still writing is marked running");
    assert!(r.body.contains("data-state=\"done\""));
    assert!(r.body.contains("data-job-started="), "the job header ticks its elapsed time");
    assert!(!r.body.contains("data-live-log"), "no step is a special live container any more");
    let finished = get("/ada/lighthouse/actions/runs/52/jobs/521").await;
    assert!(!finished.body.contains("data-job-stream"), "a finished job does not start a stream");
}

#[tokio::test]
async fn a_live_log_streams_html_lines_as_server_sent_events() {
    let (status, headers, text) = first_chunk("/ada/lighthouse/actions/runs/53/jobs/531/log/stream?offset=0&line=1", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert!(headers[header::CACHE_CONTROL].to_str().unwrap().contains("no-"), "an event stream must not be cached");
    assert!(text.contains("event: log") || text.contains("event:log"), "{text}");
    assert!(text.contains("log-line"), "{text}");
    assert!(text.contains("\"step\":"), "each log event says which step owns it: {text}");
    assert!(text.contains("id: "), "each log event carries its byte offset as the id: {text}");
    assert!(!text.contains("<script"), "log text is escaped");
}

#[tokio::test]
async fn a_finished_job_streams_done_and_a_signed_out_visitor_cannot_stream() {
    let (status, _, text) = first_chunk("/ada/lighthouse/actions/runs/52/jobs/521/log/stream?offset=0", true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("event: log") || text.contains("event:log"), "the settled log is routed first: {text}");
    assert!(text.contains("event: steps") || text.contains("event:steps"), "the final step summary is sent: {text}");
    assert!(text.contains("done"), "{text}");
    let r = send(&app(), Method::GET, "/ada/lighthouse/actions/runs/53/jobs/531/log/stream", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn logs_and_artifacts_download_as_attachments() {
    let r = get("/ada/lighthouse/actions/runs/52/jobs/521/log.txt").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment"));
    assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    let r = get("/ada/lighthouse/actions/runs/52/artifacts/1").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_DISPOSITION].to_str().unwrap().contains("lighthouse-linux-x64"));
    assert_eq!(get("/ada/lighthouse/actions/runs/52/artifacts/99").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn danger_zone_actions_need_the_name_typed_back() {
    let r = post("/ada/lighthouse/settings/delete", &[("confirm", "wrong")]).await;
    assert_ne!(r.status, StatusCode::SEE_OTHER, "a wrong confirmation must not delete");
    let r = post("/ada/lighthouse/settings/delete", &[("confirm", "ada/lighthouse")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post("/ada/lighthouse/settings/transfer", &[("new_owner", "bob"), ("confirm", "nope")]).await;
    assert_ne!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn release_assets_are_listed_and_download() {
    // The release page and the list show an uploaded file with its size and download count.
    let page = get("/ada/lighthouse/releases/tag/v0.2.0").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("lighthouse-0.2.0-linux.tar.gz"), "the asset is listed: {}", page.body);
    assert!(page.body.contains("/ada/lighthouse/releases/download/v0.2.0/lighthouse-0.2.0-linux.tar.gz"));
    let list = get("/ada/lighthouse/releases").await;
    assert!(list.body.contains("lighthouse-0.2.0-linux.tar.gz"));
    // The link the API hands out works for a signed-in person, and is a plain download.
    let file = get("/ada/lighthouse/releases/download/v0.2.0/lighthouse-0.2.0-linux.tar.gz").await;
    assert_eq!(file.status, StatusCode::OK);
    assert_eq!(file.headers.get(header::CONTENT_TYPE).unwrap(), "application/octet-stream");
    assert!(file.headers.get(header::CONTENT_DISPOSITION).unwrap().to_str().unwrap().contains("lighthouse-0.2.0-linux.tar.gz"));
    assert_eq!(file.headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    // A file that is not there is a 404, and signed out is sent to sign in.
    assert_eq!(get("/ada/lighthouse/releases/download/v0.2.0/nothing.bin").await.status, StatusCode::NOT_FOUND);
    let anon = send(&app(), Method::GET, "/ada/lighthouse/releases/download/v0.2.0/lighthouse-0.2.0-linux.tar.gz", None, None, None).await;
    assert!(anon.status == StatusCode::SEE_OTHER || anon.status == StatusCode::UNAUTHORIZED, "{}", anon.status);
}

// ------------------------------------------------------------------ visibility tiers

#[tokio::test]
async fn settings_show_three_visibility_choices() {
    let r = get("/ada/lighthouse/settings").await;
    assert_eq!(r.status, StatusCode::OK);
    for needle in [
        "value=\"private\"",
        "value=\"internal\"",
        "value=\"public\"",
        "Anyone signed in to this Codehaven can read it.",
        "Anyone on the internet can read it without an account.",
        "Who can open issues, comment and send pull requests",
        "Everyone signed in who can read it",
        "Only people with access",
        "This also covers reactions and reviews.",
    ] {
        assert!(r.body.contains(needle), "missing {needle}");
    }
    // The demo lighthouse is internal, so exactly that radio is checked.
    assert!(r.body.contains("name=\"value\" value=\"internal\" checked"), "the current tier is checked");
    assert!(!r.body.contains("name=\"value\" value=\"private\" checked"));
    assert!(!r.body.contains("name=\"value\" value=\"public\" checked"));
}

#[tokio::test]
async fn publish_page_lists_what_becomes_public() {
    let r = get("/ada/lighthouse/settings/visibility").await;
    assert_eq!(r.status, StatusCode::OK);
    for needle in [
        "Make ada/lighthouse public",
        "Anyone on the internet will be able to read and copy this repository without an account.",
        "I have checked the code and its history for secrets and personal information.",
        "name=\"snapshot\" value=\"demo-snapshot\"",
        "from other people",
    ] {
        assert!(r.body.contains(needle), "missing {needle}");
    }
    assert!(r.body.contains("<span class=\"chip\">lighthouse</span>"), "the linked package is a chip: {}", r.body);
}

#[tokio::test]
async fn publish_refuses_a_wrong_name_or_no_box() {
    let r =
        post("/ada/lighthouse/settings/visibility", &[("value", "public"), ("confirm", "ada/wrong"), ("snapshot", "demo-snapshot")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Type ada/lighthouse to confirm."));
    assert!(r.body.contains("Tick the box once you have checked."));
    let r = post(
        "/ada/lighthouse/settings/visibility",
        &[("value", "public"), ("confirm", "ada/lighthouse"), ("checked_history", "1"), ("snapshot", "stale")],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("The repository changed since this page was loaded."));
}

#[tokio::test]
async fn admin_settings_page() {
    let r = get("/admin/settings").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Allow public repositories"));
    assert!(r.body.contains("Turning this on does not make any repository public."));
    let bob = get_as(&format!("ch_session={BOB_COOKIE}"), "/admin/settings").await;
    assert_eq!(bob.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_workspace_page() {
    let r = get("/admin/workspace").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Code editor in the browser"));
    assert!(r.body.contains("Turn the editor off"), "the demo has the editor on");
    assert!(r.body.contains("href=\"/admin/workspace\""), "the admin menu links it");
    let bob = get_as(&format!("ch_session={BOB_COOKIE}"), "/admin/workspace").await;
    assert_eq!(bob.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn new_repo_offers_three_choices() {
    let r = get("/new").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("value=\"internal\""));
    assert!(r.body.contains("A public repository is created private first"));
}

#[tokio::test]
async fn the_import_page_asks_for_an_address_and_offers_no_public() {
    let r = get("/new/import").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Import a repository"));
    assert!(r.body.contains("name=\"url\""));
    assert!(r.body.contains("value=\"private\""));
    assert!(r.body.contains("value=\"internal\""));
    assert!(!r.body.contains("value=\"public\""), "an import is never created public");
    // The new repository page points at it.
    let r = get("/new").await;
    assert!(r.body.contains("href=\"/new/import\""));
    // An empty address is refused with the form still on the page.
    let r = post("/new/import", &[("owner_id", "1"), ("name", ""), ("description", ""), ("visibility", "private")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Give the address of the repository to import."));
    // Signed out, the gate sends the page to sign in, as for every other page.
    let r = send(&app(), Method::GET, "/new/import", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/login?next="));
}

#[tokio::test]
async fn chips_name_the_tier() {
    let r = get("/ada/lighthouse").await;
    assert_eq!(r.status, StatusCode::OK);
    // The demo lighthouse is internal, so the chip explains that tier.
    assert!(r.body.contains("title=\"Anyone signed in to this Codehaven can read it."));
    assert!(!r.body.contains("Every account on this server can read it. Nobody else can."));
}

#[tokio::test]
async fn two_factor_setup_shows_a_qr_code() {
    let r = post("/settings/security/2fa/start", &[]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("<div class=\"qr\"><svg role=\"img\" aria-label=\"QR code for your authenticator app\""), "{}", r.body);
    assert!(!r.body.contains("There is no QR code here"));
}

#[tokio::test]
async fn required_checks_take_one_name_per_line_and_suggest_recent_names() {
    let r = get("/ada/lighthouse/settings/branches").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("<textarea id=\"required_checks\" name=\"required_checks\""), "{}", r.body);
    assert!(!r.body.contains("placeholder=\"build, test\""));
    assert!(r.body.contains("One check name per line"));
    assert!(r.body.contains("data-copy=\"test\">test</button>"), "recent check names are offered: {}", r.body);
}

#[tokio::test]
async fn notifications_link_by_number_and_say_what_state_the_thread_is_in() {
    let r = get("/notifications?all=1").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("href=\"/ada/lighthouse/pulls/14\""), "{}", r.body);
    assert!(r.body.contains("<span class=\"state-word state-merged\">Merged</span>"), "{}", r.body);
    assert!(r.body.contains("You were assigned"), "the fan-out's spelling reads as words: {}", r.body);
}

#[tokio::test]
async fn the_merge_box_takes_a_title_and_message_and_names_the_head_it_saw() {
    let r = get("/ada/lighthouse/pulls/14").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("wants to merge"));
    assert!(r.body.contains("<input type=\"hidden\" name=\"head_sha\" value=\""), "{}", r.body);
    assert!(r.body.contains("id=\"merge-title\" name=\"title\""));
    assert!(r.body.contains("id=\"merge-body\" name=\"message\""));
    assert!(r.body.contains("name=\"delete_branch\""), "a branch of this repository can go after the merge");
    // A push after the page loaded stops the merge.
    let r = post("/ada/lighthouse/pulls/14/merge", &[("method", "squash"), ("head_sha", "0000000"), ("title", "Colour it")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let page = follow(&r).await;
    assert!(page.contains("The head branch was modified"), "{page}");
}

/// The page a redirect lands on, with the flash it set.
async fn follow(r: &Resp) -> String {
    let flash = r.set_cookies().into_iter().find(|c| c.starts_with("ch_flash=")).map(|c| c.split(';').next().unwrap().to_string());
    let cookie = match flash {
        Some(f) => format!("{}; {f}", ada()),
        None => ada(),
    };
    get_as(&cookie, &r.location()).await.body
}

#[tokio::test]
async fn a_merged_pull_request_says_merged_and_offers_to_delete_its_branch() {
    let r = get("/ada/lighthouse/pulls/7").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("wants to merge"), "{}", r.body);
    assert!(r.body.contains("</strong> merged "), "{}", r.body);
    assert!(r.body.contains("action=\"/ada/lighthouse/pulls/7/delete-branch\""), "{}", r.body);
    let r = post("/ada/lighthouse/pulls/7/delete-branch", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/ada/lighthouse/pulls/7");
    assert!(follow(&r).await.contains("Deleted the branch fix/long-messages."));
    // An open pull request keeps its branch.
    let r = post("/ada/lighthouse/pulls/14/delete-branch", &[]).await;
    assert!(follow(&r).await.contains("Close or merge the pull request before deleting its branch."));
}

#[tokio::test]
async fn a_pull_request_has_review_request_assignee_and_milestone_controls() {
    let r = get("/ada/lighthouse/pulls/14").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Bob <span class=\"chip\">Awaiting review</span>"), "{}", r.body);
    assert!(r.body.contains("action=\"/ada/lighthouse/pulls/14/reviewers\""));
    assert!(r.body.contains("action=\"/ada/lighthouse/pulls/14/assignees\""));
    assert!(r.body.contains("action=\"/ada/lighthouse/pulls/14/milestone\""));
    assert!(r.body.contains("<h2>Assignees</h2>") && r.body.contains("<h2>Milestone</h2>"));
    let r = post("/ada/lighthouse/pulls/14/reviewers", &[("offered", "u:2"), ("username", "cleo")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(follow(&r).await.contains("Reviewers updated."));
    let r = post("/ada/lighthouse/pulls/14/reviewers", &[("username", "nobody-here")]).await;
    assert!(follow(&r).await.contains("There is no account named nobody-here."));
    // A merged pull request takes no requests.
    let r = get("/ada/lighthouse/pulls/7").await;
    assert!(!r.body.contains("/pulls/7/reviewers"));
}

#[tokio::test]
async fn a_review_suggestion_is_labelled_and_can_be_applied() {
    let r = get("/ada/lighthouse/pulls/14/files").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("<p class=\"suggestion-label\">Suggested change</p>"), "{}", r.body);
    assert!(r.body.contains(">Apply suggestion</button>"));
    let r = post("/ada/lighthouse/pulls/14/suggestion", &[("comment", "33"), ("back", "files")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(follow(&r).await.contains("Applied the suggestion as commit"));
    let r = post("/ada/lighthouse/pulls/14/suggestion", &[("comment", "31")]).await;
    assert!(follow(&r).await.contains("This comment has no suggestion to apply"));
}

#[tokio::test]
async fn a_repository_header_has_watch_star_and_fork_for_a_signed_in_reader() {
    let r = get("/ada/lighthouse").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("action=\"/ada/lighthouse/watch\""), "{}", r.body);
    assert!(r.body.contains("action=\"/ada/lighthouse/star\""));
    assert!(r.body.contains("<span class=\"count\">12</span>"));
    assert!(r.body.contains("href=\"/ada/lighthouse/fork\""));
    let r = post("/ada/lighthouse/star", &[("star", "1")]).await;
    assert!(follow(&r).await.contains("Starred."));
    let r = post("/ada/lighthouse/watch", &[("level", "all")]).await;
    assert!(follow(&r).await.contains("You will hear about all activity in this repository."));
    let r = post("/ada/lighthouse/watch", &[("level", "loud")]).await;
    assert!(follow(&r).await.contains("Choose how much you want to hear."));
}

#[tokio::test]
async fn forking_offers_the_owner_accounts_and_caps_the_tier_at_the_parent() {
    let r = get("/ada/lighthouse/fork").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Fork ada/lighthouse"));
    assert!(r.body.contains("name=\"owner_id\""));
    // The demo lighthouse is internal: internal is allowed, public is not.
    assert!(r.body.contains("value=\"internal\">"), "{}", r.body);
    assert!(r.body.contains("value=\"public\" disabled>"), "{}", r.body);
    assert!(r.body.contains("A fork cannot be more visible than the repository it came from."));
    // ch-ev5: the hint says what fork_post does, which is create it internal first.
    assert!(r.body.contains("A public fork starts as internal, then you confirm it on the next page."));
    assert!(!r.body.contains("made private first"));
    let r = post("/ada/lighthouse/fork", &[("owner_id", "999999"), ("visibility", "private")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = get("/ada/lighthouse/forks").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("bob/lighthouse"), "{}", r.body);
    // Signed out, neither page exists.
    let r = send(&app(), Method::GET, "/ada/lighthouse/fork", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = send(&app(), Method::GET, "/ada/lighthouse/forks", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_pull_request_can_come_from_your_own_fork() {
    let bob = format!("ch_session={BOB_COOKIE}");
    let r = get_as(&bob, "/ada/lighthouse/pulls/new").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("<select name=\"head_owner\">"), "{}", r.body);
    assert!(r.body.contains("<option value=\"bob\">bob/lighthouse</option>"));
    // Ada owns no fork, so she gets no repository choice.
    let r = get("/ada/lighthouse/pulls/new").await;
    assert!(!r.body.contains("name=\"head_owner\""));
    // A fork that is not yours is refused before anything is created.
    let r =
        post("/ada/lighthouse/pulls/new", &[("base", "main"), ("head", "feature/signal-colours"), ("head_owner", "bob"), ("title", "T")])
            .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Pick one of your own forks of this repository."));
    // ch-ev5: a branch that exists nowhere in the base repository gets the same message, not a 404.
    let r = post("/ada/lighthouse/pulls/new", &[("base", "main"), ("head", "only-in-a-fork"), ("head_owner", "bob"), ("title", "T")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert!(r.body.contains("Pick one of your own forks of this repository."));
}

#[tokio::test]
async fn code_search_has_a_tab_across_repositories_and_a_page_per_repository() {
    let r = get("/search?q=Beam&type=code").await;
    assert_eq!(r.status, StatusCode::OK);
    let tab = between(&r.body, "class=\"tab current\"", "</a>");
    assert!(tab.ends_with(">Code"), "{tab}");
    assert!(r.body.contains("href=\"/ada/lighthouse/blob/main/src/beam.rs#LC4\""), "{}", r.body);
    assert!(r.body.contains("<code>src/beam.rs:4</code>"));
    let r = get("/ada/lighthouse/search?q=Beam").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("href=\"/ada/lighthouse/blob/main/src/beam.rs#LC4\""), "{}", r.body);
    assert!(r.body.contains("1 matching line."));
    let r = get("/ada/lighthouse/search?q=nothing-like-this").await;
    assert!(r.body.contains("Nothing matches"));
    // Code search needs an account.
    let r = send(&app(), Method::GET, "/ada/lighthouse/search?q=Beam", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_issue_list_filters_by_milestone_and_people_and_heads_a_milestone_page() {
    let r = get("/ada/lighthouse/issues").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("href=\"/ada/lighthouse/issues?milestone=1\""), "{}", r.body);
    assert!(r.body.contains("id=\"issue-author\" name=\"author\""));
    assert!(r.body.contains("id=\"issue-assignee\" name=\"assignee\""));
    assert!(!r.body.contains("class=\"card pad milestone-head\""));
    let r = get("/ada/lighthouse/issues?milestone=1").await;
    assert!(r.body.contains("class=\"card pad milestone-head\""), "{}", r.body);
    assert!(r.body.contains("3 open, 5 closed"));
    assert!(r.body.contains("62% complete"));
    assert!(r.body.contains("<input type=\"hidden\" name=\"milestone\" value=\"1\">"));
    let r = get("/ada/lighthouse/issues?author=cleo").await;
    assert!(r.body.contains("value=\"cleo\""));
}

#[tokio::test]
async fn an_admin_invite_can_add_the_person_to_an_organization() {
    let r = get("/admin/invites").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("id=\"invite-org\" name=\"org\""), "{}", r.body);
    assert!(r.body.contains("id=\"invite-team\" name=\"team\""));
    let r = post("/admin/invites", &[("org", "nobody-at-all"), ("expires_in_hours", "72")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("There is no organization with that name."));
    let r = post("/admin/invites", &[("team", "core"), ("expires_in_hours", "72")]).await;
    assert!(r.body.contains("Name the organization the team is in."));
    // The org page no longer says an org owner can invite.
    let r = get("/orgs/harbor/people").await;
    assert!(!r.body.contains("An owner can invite one from Site admin"));
}

#[tokio::test]
async fn forgot_password_is_open_signed_out_and_says_so_when_mail_is_off() {
    let app = app();
    let r = send(&app, Method::GET, "/login", None, None, None).await;
    assert!(r.body.contains("href=\"/password/forgot\""));
    let r = send(&app, Method::GET, "/password/forgot", None, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("This server does not send email."), "{}", r.body);
    assert!(!r.body.contains("name=\"login\""), "no form when nothing could be sent");
    // Signed in, it goes to the password settings instead.
    let r = get("/password/forgot").await;
    assert_eq!((r.status, r.location()), (StatusCode::SEE_OTHER, "/settings/password".to_string()));
}

#[tokio::test]
async fn small_page_defects_stay_fixed() {
    // The page that confirms two-factor does not ask for it again.
    let cookie = format!("ch_session={ADMIN_NO_2FA_COOKIE}");
    let r = post_as(&cookie, "/settings/security/2fa/confirm", &[("secret", "JBSWY3DPEHPK3PXP"), ("code", "123456")]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("Turn on two-factor sign-in"), "{}", r.body);
    // Blame starts its path from the branch, like the file page.
    let r = get("/ada/lighthouse/blame/main/src/main.rs").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains(">main</a><span class=\"sep\""), "{}", r.body);
    assert!(!r.body.contains(">root</a>"));
}

#[tokio::test]
async fn the_sign_in_page_links_to_public_repositories_only_when_they_are_on() {
    // The demo answers public_repos_allowed with true (ch-web/src/demo/mod.rs) and has no way to
    // switch it off, so the line is there. A server that answers false leaves it out.
    let app = app();
    let r = send(&app, Method::GET, "/login", None, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("<a href=\"/explore\">Browse public repositories</a> without signing in."), "{}", r.body);
}

#[tokio::test]
async fn the_notification_settings_page_says_whether_email_is_possible() {
    // The demo has no mail transport (NotBuilt answers enabled() with false), so the page says so
    // instead of offering the form.
    let r = get("/settings/notifications").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Notifications"), "{}", r.body);
    assert!(r.body.contains("This server does not send email"), "{}", r.body);
    // The settings navigation links the page from the other settings pages.
    let r = get("/settings/profile").await;
    assert!(r.body.contains("href=\"/settings/notifications\""), "{}", r.body);
    // Signed out it bounces to the sign-in page like the other settings pages.
    let app = app();
    let r = send(&app, Method::GET, "/settings/notifications", None, None, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/login?next="), "{}", r.location());
}
