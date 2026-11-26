//! ch-wf0 through the pages: a passkey is added at /settings/security and used as the second step
//! at /login/two-factor, with a software authenticator answering the same JSON a browser would.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use ch_core::model::*;
use ch_core::{Actor, Config};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url};

const PW: &str = "violet harbour lantern 83 teapot";
const ORIGIN: &str = "https://code.example.com";

struct H {
    app: Router,
    s: ch_core::state::AppState,
    guard: ch_auth::Guard,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    let cfg = Arc::new(cfg);
    let s = wiring::build_state(cfg.clone(), pool).await.unwrap();
    let guard = ch_auth::Guard::new(&cfg);
    let app = build_router(s.clone(), Metrics::new(), None);
    H { app, s, guard, _dir: dir }
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

/// One request. `cookies` is the whole Cookie header; `form` is the urlencoded body of a POST.
async fn call(h: &H, method: Method, path: &str, cookies: &str, form: &str) -> (StatusCode, HeaderMap, String) {
    let mut b = Request::builder().method(method.clone()).uri(path).header("origin", ORIGIN);
    if !cookies.is_empty() {
        b = b.header("cookie", cookies);
    }
    let body = if method == Method::POST {
        b = b.header("content-type", "application/x-www-form-urlencoded");
        form.to_string()
    } else {
        String::new()
    };
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find_map(|v| {
        let first = v.split(';').next()?;
        first.strip_prefix(&format!("{name}=")).map(str::to_string)
    })
}

fn enc(s: &str) -> String {
    url_encode(s)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn field_value(html: &str, name: &str) -> String {
    let at =
        html.find(&format!("name=\"{name}\" value=\"")).unwrap_or_else(|| panic!("no field {name} in {}", &html[..html.len().min(1500)]));
    let rest = &html[at + format!("name=\"{name}\" value=\"").len()..];
    rest[..rest.find('"').unwrap()].to_string()
}

fn origin() -> Url {
    Url::parse(ORIGIN).unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_passkey_is_added_on_the_security_page_and_used_at_the_second_step(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = h
        .s
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: "alice".into(), email: "alice@example.org".into(), display_name: None, is_admin: false, can_run_ci: false },
            &c(),
        )
        .await
        .unwrap();
    h.s.accounts.set_password(&Actor::System, alice.id, PW, &c()).await.unwrap();
    let first = h.s.credentials.sign_in("alice", PW, &c()).await.unwrap();
    let session = format!("__Host-ch_session={}", first.grant.cookie);
    let csrf = h.guard.token_for_session(&first.grant.cookie);

    // The page offers a passkey and says there is none yet.
    let (st, _, page) = call(&h, Method::GET, "/settings/security", &session, "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("Passkeys") && page.contains("data-passkey-register"), "{}", &page[..200.min(page.len())]);

    // Begin: JSON options for the browser. Without the CSRF header or field it is refused.
    let (st, _, _) = call(&h, Method::POST, "/settings/security/passkeys/begin", &session, "").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a post without the CSRF token");
    let (st, headers, options) = call(&h, Method::POST, "/settings/security/passkeys/begin", &session, &format!("_csrf={csrf}")).await;
    assert_eq!(st, StatusCode::OK, "{options}");
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    let ccr: CreationChallengeResponse = serde_json::from_str(&options).unwrap();

    // The authenticator answers and the page posts the answer as a form field.
    let mut key = SoftPasskey::new(true);
    let answer = serde_json::to_string(&key.do_registration(origin(), ccr).unwrap()).unwrap();
    let (st, headers, done) = call(
        &h,
        Method::POST,
        "/settings/security/passkeys/finish",
        &session,
        &format!("_csrf={csrf}&name={}&credential={}", enc("Laptop"), enc(&answer)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert!(done.contains("Recovery codes") && done.contains("Passkey added, two-factor is on"));
    let new_session = set_cookie(&headers, "__Host-ch_session").expect("the new session cookie is set in the same response");
    assert_ne!(new_session, first.grant.cookie);
    let (st, _, page) = call(&h, Method::GET, "/settings/security", &format!("__Host-ch_session={new_session}"), "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("Laptop"), "the passkey is listed by its name");

    // Signing in now asks for the passkey.
    let pending = h.s.credentials.sign_in("alice", PW, &c()).await.unwrap();
    let pending_cookie = format!("ch_2fa={}", pending.grant.cookie);
    let (st, headers, page) = call(&h, Method::GET, "/login/two-factor", &pending_cookie, "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("Use my passkey") && page.contains("data-passkey-signin") && page.contains("Recovery code"), "{page}");
    let anon_cookie = set_cookie(&headers, "__Host-ch_csrf").expect("an anonymous CSRF cookie");
    let anon_csrf = field_value(&page, "_csrf");
    let cookies = format!("{pending_cookie}; __Host-ch_csrf={anon_cookie}");

    // Begin and finish, as the page script does.
    let (st, _, options) = call(&h, Method::POST, "/login/two-factor/passkey/begin", &cookies, &format!("_csrf={anon_csrf}")).await;
    assert_eq!(st, StatusCode::OK, "{options}");
    let rcr: RequestChallengeResponse = serde_json::from_str(&options).unwrap();
    let answer = serde_json::to_string(&key.do_authentication(origin(), rcr).unwrap()).unwrap();
    let (st, headers, _) =
        call(&h, Method::POST, "/login/two-factor/passkey", &cookies, &format!("_csrf={anon_csrf}&next=%2F&credential={}", enc(&answer)))
            .await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let signed_in = set_cookie(&headers, "__Host-ch_session").expect("a whole session");
    let (st, _, _) = call(&h, Method::GET, "/settings/security", &format!("__Host-ch_session={signed_in}"), "").await;
    assert_eq!(st, StatusCode::OK, "the new session works");

    // A wrong answer shows the page again with a plain sentence and no session.
    let pending = h.s.credentials.sign_in("alice", PW, &c()).await.unwrap();
    let cookies = format!("ch_2fa={}; __Host-ch_csrf={anon_cookie}", pending.grant.cookie);
    let (_, _, _) = call(&h, Method::POST, "/login/two-factor/passkey/begin", &cookies, &format!("_csrf={anon_csrf}")).await;
    let (st, headers, page) =
        call(&h, Method::POST, "/login/two-factor/passkey", &cookies, &format!("_csrf={anon_csrf}&credential={}", enc("{}"))).await;
    assert_eq!(st, StatusCode::OK, "{:?} {}", headers.get("location"), &page[..300.min(page.len())]);
    assert!(page.contains("That passkey did not work") && set_cookie(&headers, "__Host-ch_session").is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_passkey_steps_of_sign_in_do_nothing_without_the_password_step(pool: PgPool) {
    let h = harness(pool).await;
    // With nobody registered the sign-in page is the first-run page, so make an account first.
    h.s.accounts
        .create_user(
            &Actor::System,
            NewUser { username: "alice".into(), email: "alice@example.org".into(), display_name: None, is_admin: false, can_run_ci: false },
            &c(),
        )
        .await
        .unwrap();
    // No pending cookie: the options are refused, whatever else is sent.
    let (st, anon) = {
        let (code, headers, page) = call(&h, Method::GET, "/login", "", "").await;
        assert_eq!(code, StatusCode::OK, "{:?}", headers.get("location"));
        (set_cookie(&headers, "__Host-ch_csrf"), field_value(&page, "_csrf"))
    };
    let cookies = format!("__Host-ch_csrf={}", st.unwrap());
    let (st, _, body) = call(&h, Method::POST, "/login/two-factor/passkey/begin", &cookies, &format!("_csrf={anon}")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert!(!body.contains("challenge"));
    // The answer step with no pending cookie goes back to sign in.
    let (st, headers, _) =
        call(&h, Method::POST, "/login/two-factor/passkey", &cookies, &format!("_csrf={anon}&credential={}", enc("{}"))).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(headers.get("location").unwrap(), "/login");
    // And no CSRF token, no business.
    let (st, _, _) = call(&h, Method::POST, "/login/two-factor/passkey/begin", "", "").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}
