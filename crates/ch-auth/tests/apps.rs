//! Connected apps: authorize, consent, the token endpoint, revocation and what an app actor may do.
//! Real services on a real database. Only the fetch of a client id document is replaced.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

mod common;

use async_trait::async_trait;
use base64::Engine;
use ch_auth::apps::cimd::{ClientDoc, ClientDocFetcher, Fetched};
use ch_core::apps::*;
use ch_core::{Actor, GrantId, Permission, RepoId, UserId};
use common::*;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const WEB: &str = "https://claude.ai/oauth/mcp-oauth-client-metadata";
const CODE: &str = "https://claude.ai/oauth/claude-code-client-metadata";
const WEB_CB: &str = "https://claude.ai/api/mcp/auth_callback";
const ORIGIN: &str = "https://code.example.com";
const RESOURCE: &str = "https://code.example.com/mcp";

struct Fake {
    calls: AtomicUsize,
}

#[async_trait]
impl ClientDocFetcher for Fake {
    async fn fetch(&self, client_id: &str) -> Result<Fetched, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let doc = match client_id {
            WEB => ClientDoc { client_id: WEB.into(), client_name: "Claude".into(), redirect_uris: vec![WEB_CB.into()] },
            CODE => ClientDoc {
                client_id: CODE.into(),
                client_name: "Claude Code".into(),
                redirect_uris: vec!["http://localhost/callback".into(), "http://127.0.0.1/callback".into()],
            },
            _ => return Err("The client's host did not resolve.".into()),
        };
        Ok(Fetched { doc, max_age: None })
    }
}

struct F {
    c: Ctx,
    codes: std::cell::RefCell<Vec<String>>,
    fake: Arc<Fake>,
    alice: UserId,
    cookie: String,
    actor: Actor,
    r1: RepoId,
    r2: RepoId,
    bob_private: RepoId,
}

async fn fixture(pool: PgPool) -> F {
    let fake = Arc::new(Fake { calls: AtomicUsize::new(0) });
    let c = ctx_full(
        pool,
        |cfg| {
            cfg.server.external_url = ORIGIN.into();
            cfg.mcp.enabled = true;
        },
        Some(fake.clone()),
    )
    .await;
    let alice = make_user(&c, "alice", false).await;
    let bob = make_user(&c, "bob", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let (_, codes, cookie, actor) = enable_2fa(&c, &cookie, &actor).await;
    let r1 = c.env.repo(alice, "one").await;
    let r2 = c.env.repo(alice, "two").await;
    let bob_private = c.env.repo(bob, "secret").await;
    F { c, codes: std::cell::RefCell::new(codes), fake, alice, cookie, actor, r1, r2, bob_private }
}

/// Another browser session of the person, signed in with a recovery code.
async fn relogin(f: &F, name: &str) -> (String, Actor) {
    let s = sign_in(&f.c, name, PW).await;
    let code = f.codes.borrow_mut().pop().unwrap();
    let done = f.c.state.credentials.complete_two_factor(&s.grant.cookie, &code, &c1()).await.unwrap();
    let actor = actor_for_cookie(&f.c, &done.grant.cookie).await;
    (done.grant.cookie, actor)
}

/// Makes a new person with two-factor on and puts them in the fixture's place.
async fn become_person(f: &mut F, name: &str) {
    f.alice = make_user(&f.c, name, false).await;
    let (cookie, actor) = session(&f.c, name).await;
    let (_, codes, cookie, actor) = enable_2fa(&f.c, &cookie, &actor).await;
    f.cookie = cookie;
    f.actor = actor;
    *f.codes.borrow_mut() = codes;
}

fn verifier() -> String {
    "v".repeat(43)
}

fn challenge(v: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ch_db::tokens::sha256(v.as_bytes()))
}

fn params(client_id: &str, redirect: &str) -> AuthorizeParams {
    AuthorizeParams {
        client_id: client_id.into(),
        redirect_uri: redirect.into(),
        response_type: "code".into(),
        code_challenge: challenge(&verifier()),
        code_challenge_method: "S256".into(),
        state: Some("st-123".into()),
        scope: Some("read write".into()),
        resource: Some(RESOURCE.into()),
    }
}

fn choices(request_id: &str, read: RepoChoice, write: RepoChoice, workflows: RepoChoice) -> ConsentChoices {
    ConsentChoices { request_id: request_id.into(), read, write, workflows, expires_days: 90 }
}

async fn consent(f: &F, p: AuthorizeParams) -> ConsentView {
    match f.c.state.apps.begin_authorize(&f.actor, &f.cookie, p, &c1()).await.unwrap() {
        AuthorizeOutcome::Consent(v) => v,
        other => panic!("expected the consent page, got {other:?}"),
    }
}

async fn refused(f: &F, p: AuthorizeParams) -> String {
    match f.c.state.apps.begin_authorize(&f.actor, &f.cookie, p, &c1()).await.unwrap() {
        AuthorizeOutcome::Refused(m) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn query(url: &str, name: &str) -> Option<String> {
    url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

/// Authorize and approve, returning the one-time code.
async fn get_code(f: &F, client_id: &str, redirect: &str, ch: ConsentChoices) -> String {
    let v = consent(f, params(client_id, redirect)).await;
    let target = f.c.state.apps.approve(&f.actor, &f.cookie, ConsentChoices { request_id: v.request_id, ..ch }, &c1()).await.unwrap();
    assert_eq!(query(&target.url, "state").as_deref(), Some("st-123"));
    assert_eq!(query(&target.url, "iss").as_deref(), Some(ORIGIN));
    query(&target.url, "code").expect("a code")
}

fn exchange_req(code: &str, client_id: &str, redirect: &str) -> TokenRequest {
    TokenRequest {
        grant_type: Some("authorization_code".into()),
        code: Some(code.into()),
        redirect_uri: Some(redirect.into()),
        code_verifier: Some(verifier()),
        client_id: Some(client_id.into()),
        resource: Some(RESOURCE.into()),
        ..Default::default()
    }
}

async fn connect(f: &F, read: RepoChoice, write: RepoChoice, workflows: RepoChoice) -> TokenResponse {
    let code = get_code(f, WEB, WEB_CB, choices("", read, write, workflows)).await;
    f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await.unwrap()
}

async fn rows(f: &F, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(&f.c.pool).await.unwrap()
}

fn refresh_req(rt: &str, client_id: &str) -> TokenRequest {
    TokenRequest {
        grant_type: Some("refresh_token".into()),
        refresh_token: Some(rt.into()),
        client_id: Some(client_id.into()),
        ..Default::default()
    }
}

// ------------------------------------------------------------------ the happy paths

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn both_client_documents_connect_end_to_end(pool: PgPool) {
    let f = fixture(pool).await;
    // The web client, to a fixed https redirect.
    let v = consent(&f, params(WEB, WEB_CB)).await;
    assert_eq!(v.client_host, "claude.ai");
    assert_eq!(v.client_name, "Claude");
    assert!(!v.loopback);
    assert_eq!(v.redirect_host, "claude.ai");
    let t =
        f.c.state
            .apps
            .approve(
                &f.actor,
                &f.cookie,
                choices(&v.request_id, RepoChoice::Selected(vec![f.r1, f.r2]), RepoChoice::Selected(vec![f.r1]), RepoChoice::None),
                &c1(),
            )
            .await
            .unwrap();
    let code = query(&t.url, "code").unwrap();
    assert!(code.starts_with("chmc_") && code.len() == 48);
    let tok = f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await.unwrap();
    assert!(tok.access_token.starts_with("chma_") && tok.access_token.len() == 48);
    assert!(tok.refresh_token.starts_with("chmr_") && tok.refresh_token.len() == 48);
    assert_eq!(tok.scope, "read write", "what was granted, not what was asked");
    assert!(tok.expires_in > 3500 && tok.expires_in <= 3600);
    let who = f.c.state.authn.from_app_bearer(&tok.access_token, &c1()).await.unwrap();
    let Actor::App { user_id, grant, .. } = &who else { panic!("{who:?}") };
    assert_eq!(*user_id, f.alice);
    assert!(grant.read.has(f.r1) && grant.read.has(f.r2) && !grant.read.is_all());
    assert!(grant.write.has(f.r1) && !grant.write.has(f.r2));
    assert!(grant.workflows.is_empty());
    assert_eq!(who.user_id(), None, "an app is never 'the person' to a self-service check");
    assert_eq!(who.author_id(), Some(f.alice));

    // Claude Code, to a loopback redirect with a port the program picked.
    let v = consent(&f, params(CODE, "http://localhost:54321/callback")).await;
    assert!(v.loopback);
    assert_eq!(v.redirect_host, "localhost:54321");
    let t =
        f.c.state
            .apps
            .approve(&f.actor, &f.cookie, choices(&v.request_id, RepoChoice::All, RepoChoice::None, RepoChoice::None), &c1())
            .await
            .unwrap();
    assert!(t.url.starts_with("http://localhost:54321/callback?"), "{}", t.url);
    let code = query(&t.url, "code").unwrap();
    let tok = f.c.state.apps.token(exchange_req(&code, CODE, "http://localhost:54321/callback"), &c1()).await.unwrap();
    let who = f.c.state.authn.from_app_bearer(&tok.access_token, &c1()).await.unwrap();
    let Actor::App { grant, .. } = &who else { panic!() };
    assert!(grant.read.is_all() && grant.write.is_empty());
    assert!(f.c.state.apps.list_grants(&f.actor).await.unwrap().len() == 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deny_returns_access_denied_with_state_and_issuer(pool: PgPool) {
    let f = fixture(pool).await;
    let v = consent(&f, params(WEB, WEB_CB)).await;
    let t = f.c.state.apps.deny(&f.actor, &f.cookie, &v.request_id, &c1()).await.unwrap();
    assert_eq!(query(&t.url, "error").as_deref(), Some("access_denied"));
    assert_eq!(query(&t.url, "state").as_deref(), Some("st-123"));
    assert_eq!(query(&t.url, "iss").as_deref(), Some(ORIGIN));
    assert!(query(&t.url, "code").is_none());
    assert_eq!(rows(&f, "oauth_grants").await, 0);
    assert_eq!(count_audit(&f.c, "app.denied").await, 1);
}

// ------------------------------------------------------------------ authorize refusals store nothing

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn no_two_factor_means_no_fetch_and_no_row(pool: PgPool) {
    let f = fixture(pool).await;
    make_user(&f.c, "carol", false).await;
    let (cookie, actor) = session(&f.c, "carol").await;
    let out = f.c.state.apps.begin_authorize(&actor, &cookie, params(WEB, WEB_CB), &c1()).await.unwrap();
    assert!(matches!(out, AuthorizeOutcome::NeedsTwoFactor));
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0, "nothing was fetched");
    assert_eq!(rows(&f, "oauth_requests").await, 0);
    // A signed-out caller is not accepted at all.
    assert!(f.c.state.apps.begin_authorize(&Actor::Anonymous, "", params(WEB, WEB_CB), &c1()).await.is_err());
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn every_refusal_renders_a_page_and_stores_nothing(pool: PgPool) {
    let f = fixture(pool).await;
    let base = params(WEB, WEB_CB);
    let cases: Vec<(&str, AuthorizeParams)> = vec![
        ("untrusted host", params("https://evil.example/oauth/x", WEB_CB)),
        ("client id with a port", params("https://claude.ai:8443/oauth/x", WEB_CB)),
        ("unknown document", params("https://claude.ai/oauth/unknown", WEB_CB)),
        ("redirect host swap", params(WEB, "https://evil.example/api/mcp/auth_callback")),
        ("redirect path", params(WEB, "https://claude.ai/api/mcp/other")),
        ("redirect with a query", params(WEB, "https://claude.ai/api/mcp/auth_callback?x=1")),
        ("loopback for a web client", params(WEB, "http://localhost:1/api/mcp/auth_callback")),
        ("response type token", AuthorizeParams { response_type: "token".into(), ..base.clone() }),
        ("plain pkce", AuthorizeParams { code_challenge_method: "plain".into(), ..base.clone() }),
        ("short challenge", AuthorizeParams { code_challenge: "abc".into(), ..base.clone() }),
        ("missing resource", AuthorizeParams { resource: None, ..base.clone() }),
        ("wrong resource", AuthorizeParams { resource: Some("https://other.example/mcp".into()), ..base.clone() }),
        ("resource with a slash", AuthorizeParams { resource: Some(format!("{RESOURCE}/")), ..base.clone() }),
        ("unknown scope", AuthorizeParams { scope: Some("read admin".into()), ..base.clone() }),
        ("huge state", AuthorizeParams { state: Some("s".repeat(3000)), ..base.clone() }),
        ("control characters in state", AuthorizeParams { state: Some("a\nb".into()), ..base.clone() }),
    ];
    for (name, p) in cases {
        let m = refused(&f, p).await;
        assert!(!m.is_empty(), "{name}");
        assert_eq!(rows(&f, "oauth_requests").await, 0, "{name} stored a request");
    }
    assert_eq!(rows(&f, "oauth_grants").await, 0);
    // The good one still works, and a case-and-default-port spelling of the resource is accepted.
    consent(&f, AuthorizeParams { resource: Some("HTTPS://CODE.EXAMPLE.COM:443/mcp".into()), ..base }).await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_fetch_is_limited_per_person_and_cached(pool: PgPool) {
    let f = fixture(pool).await;
    for _ in 0..30 {
        consent(&f, params(WEB, WEB_CB)).await;
    }
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 1, "a good document is cached");
    // Errors are never cached, so each unknown id is a fetch, and ten a minute is the ceiling.
    let mut limited = false;
    for i in 0..15 {
        let m = refused(&f, params(&format!("https://claude.ai/oauth/nope-{i}"), WEB_CB)).await;
        if m.contains("Too many") {
            limited = true;
        }
    }
    assert!(limited, "the eleventh fetch in a minute is refused");
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_requests WHERE used_at IS NULL").fetch_one(&f.c.pool).await.unwrap();
    assert!(live <= 20, "at most 20 live requests per person, got {live}");
}

// ------------------------------------------------------------------ consent

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn consent_cannot_grant_what_the_person_cannot(pool: PgPool) {
    let f = fixture(pool).await;
    let v = consent(&f, params(WEB, WEB_CB)).await;
    let id = v.request_id.clone();
    let try_it = |c: ConsentChoices| {
        let f = &f;
        async move { f.c.state.apps.approve(&f.actor, &f.cookie, c, &c1()).await }
    };
    // Someone else's private repository.
    assert!(
        try_it(choices(&id, RepoChoice::Selected(vec![f.bob_private]), RepoChoice::None, RepoChoice::None)).await.is_err(),
        "unreadable"
    );
    // Write outside read.
    assert!(
        try_it(choices(&id, RepoChoice::Selected(vec![f.r1]), RepoChoice::Selected(vec![f.r2]), RepoChoice::None)).await.is_err(),
        "write outside read"
    );
    // Workflows outside write.
    assert!(
        try_it(choices(&id, RepoChoice::Selected(vec![f.r1, f.r2]), RepoChoice::Selected(vec![f.r1]), RepoChoice::Selected(vec![f.r2])))
            .await
            .is_err(),
        "workflows outside write"
    );
    // Write all needs read all. Workflows all needs write all.
    assert!(try_it(choices(&id, RepoChoice::Selected(vec![f.r1]), RepoChoice::All, RepoChoice::None)).await.is_err());
    assert!(try_it(choices(&id, RepoChoice::All, RepoChoice::Selected(vec![f.r1]), RepoChoice::All)).await.is_err());
    // Nothing chosen for read, an empty selection, and an expiry that is not offered.
    assert!(try_it(choices(&id, RepoChoice::None, RepoChoice::None, RepoChoice::None)).await.is_err());
    assert!(try_it(choices(&id, RepoChoice::Selected(vec![]), RepoChoice::None, RepoChoice::None)).await.is_err());
    assert!(
        try_it(ConsentChoices { expires_days: 365, ..choices(&id, RepoChoice::All, RepoChoice::None, RepoChoice::None) }).await.is_err()
    );
    assert_eq!(rows(&f, "oauth_grants").await, 0, "none of that stored anything");
    // The request survived every refusal, and a valid answer still works.
    let ok =
        try_it(choices(&id, RepoChoice::Selected(vec![f.r1, f.r2]), RepoChoice::Selected(vec![f.r1]), RepoChoice::Selected(vec![f.r1])))
            .await
            .unwrap();
    assert!(query(&ok.url, "code").is_some());
    let g = f.c.state.apps.list_grants(&f.actor).await;
    assert!(g.unwrap().is_empty(), "a grant is listed only once the app has its first token");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_request_is_bound_to_the_person_and_the_session_and_works_once(pool: PgPool) {
    let f = fixture(pool).await;
    let all = || choices("", RepoChoice::All, RepoChoice::None, RepoChoice::None);
    // Wrong browser session.
    let v = consent(&f, params(WEB, WEB_CB)).await;
    let (c2, a2) = relogin(&f, "alice").await;
    let r = f.c.state.apps.approve(&a2, &c2, ConsentChoices { request_id: v.request_id.clone(), ..all() }, &c1()).await;
    assert!(r.is_err(), "another session of the same person cannot answer it");
    // Another person.
    make_user(&f.c, "dave", false).await;
    let (cd, ad) = session(&f.c, "dave").await;
    let (_, _, cd, ad) = enable_2fa(&f.c, &cd, &ad).await;
    assert!(f.c.state.apps.approve(&ad, &cd, ConsentChoices { request_id: v.request_id.clone(), ..all() }, &c1()).await.is_err());
    // The right one, once.
    f.c.state.apps.approve(&f.actor, &f.cookie, ConsentChoices { request_id: v.request_id.clone(), ..all() }, &c1()).await.unwrap();
    assert!(
        f.c.state.apps.approve(&f.actor, &f.cookie, ConsentChoices { request_id: v.request_id.clone(), ..all() }, &c1()).await.is_err(),
        "replayed"
    );
    assert!(f.c.state.apps.deny(&f.actor, &f.cookie, &v.request_id, &c1()).await.is_err());
    // Expired.
    let v = consent(&f, params(WEB, WEB_CB)).await;
    sqlx::query("UPDATE oauth_requests SET expires_at = now() - interval '1 second'").execute(&f.c.pool).await.unwrap();
    assert!(f.c.state.apps.approve(&f.actor, &f.cookie, ConsentChoices { request_id: v.request_id, ..all() }, &c1()).await.is_err());
    // Made up.
    assert!(f.c.state.apps.approve(&f.actor, &f.cookie, ConsentChoices { request_id: "x".repeat(43), ..all() }, &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn at_most_twenty_live_grants_per_person(pool: PgPool) {
    let f = fixture(pool).await;
    for _ in 0..20 {
        let v = consent(&f, params(WEB, WEB_CB)).await;
        f.c.state
            .apps
            .approve(&f.actor, &f.cookie, choices(&v.request_id, RepoChoice::All, RepoChoice::None, RepoChoice::None), &c1())
            .await
            .unwrap();
    }
    let v = consent(&f, params(WEB, WEB_CB)).await;
    let r =
        f.c.state
            .apps
            .approve(&f.actor, &f.cookie, choices(&v.request_id, RepoChoice::All, RepoChoice::None, RepoChoice::None), &c1())
            .await;
    assert!(r.is_err(), "the 21st is refused");
}

// ------------------------------------------------------------------ the token endpoint

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn code_exchange_checks_everything(pool: PgPool) {
    let f = fixture(pool).await;
    let all = || choices("", RepoChoice::All, RepoChoice::None, RepoChoice::None);
    let code = get_code(&f, WEB, WEB_CB, all()).await;
    let bad = |name: &str, r: TokenRequest, want: &str| {
        let f = &f;
        let name = name.to_string();
        let want = want.to_string();
        async move {
            let e = f.c.state.apps.token(r, &c1()).await.unwrap_err();
            assert_eq!(e.code, want, "{name}");
        }
    };
    // None of these consume the code.
    bad("wrong verifier", TokenRequest { code_verifier: Some("w".repeat(43)), ..exchange_req(&code, WEB, WEB_CB) }, "invalid_grant").await;
    bad("short verifier", TokenRequest { code_verifier: Some("w".repeat(20)), ..exchange_req(&code, WEB, WEB_CB) }, "invalid_request")
        .await;
    bad(
        "verifier with bad characters",
        TokenRequest { code_verifier: Some(format!("{}!", "w".repeat(43))), ..exchange_req(&code, WEB, WEB_CB) },
        "invalid_request",
    )
    .await;
    bad("missing resource", TokenRequest { resource: None, ..exchange_req(&code, WEB, WEB_CB) }, "invalid_target").await;
    bad(
        "wrong resource",
        TokenRequest { resource: Some("https://code.example.com/other".into()), ..exchange_req(&code, WEB, WEB_CB) },
        "invalid_target",
    )
    .await;
    bad(
        "resource with a slash",
        TokenRequest { resource: Some(format!("{RESOURCE}/")), ..exchange_req(&code, WEB, WEB_CB) },
        "invalid_target",
    )
    .await;
    bad("another client", exchange_req(&code, CODE, WEB_CB), "invalid_grant").await;
    bad("another redirect", exchange_req(&code, WEB, "https://claude.ai/api/mcp/auth_callback2"), "invalid_grant").await;
    bad("no grant type", TokenRequest { grant_type: None, ..exchange_req(&code, WEB, WEB_CB) }, "invalid_request").await;
    bad(
        "jwt bearer is never offered",
        TokenRequest { grant_type: Some("urn:ietf:params:oauth:grant-type:jwt-bearer".into()), ..exchange_req(&code, WEB, WEB_CB) },
        "unsupported_grant_type",
    )
    .await;
    bad("a made up code", exchange_req(&format!("chmc_{}", "z".repeat(43)), WEB, WEB_CB), "invalid_grant").await;
    // A code from one client does not work for another even with the right verifier and redirect.
    let other = get_code(&f, CODE, "http://localhost:9/callback", all()).await;
    bad("cross-client code", exchange_req(&other, WEB, WEB_CB), "invalid_grant").await;
    // The right request works.
    let ok = f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&ok.access_token, &c1()).await.is_ok());
    // An expired code.
    let code2 = get_code(&f, WEB, WEB_CB, all()).await;
    sqlx::query("UPDATE oauth_codes SET expires_at = now() - interval '1 second' WHERE used_at IS NULL").execute(&f.c.pool).await.unwrap();
    bad("expired", exchange_req(&code2, WEB, WEB_CB), "invalid_grant").await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_replayed_code_revokes_the_grant(pool: PgPool) {
    let f = fixture(pool).await;
    let code = get_code(&f, WEB, WEB_CB, choices("", RepoChoice::All, RepoChoice::None, RepoChoice::None)).await;
    let tok = f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&tok.access_token, &c1()).await.is_ok());
    let e = f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await.unwrap_err();
    assert_eq!(e.code, "invalid_grant");
    assert!(f.c.state.authn.from_app_bearer(&tok.access_token, &c1()).await.is_err(), "the first holder's token died with the grant");
    assert!(f.c.state.apps.token(refresh_req(&tok.refresh_token, WEB), &c1()).await.is_err());
    assert_eq!(count_audit(&f.c, "app.code_reuse").await, 1);
    let reason: String = sqlx::query_scalar("SELECT revoked_reason FROM oauth_grants").fetch_one(&f.c.pool).await.unwrap();
    assert!(reason.contains("code"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn concurrent_code_exchange_has_one_winner(pool: PgPool) {
    let f = fixture(pool).await;
    let code = get_code(&f, WEB, WEB_CB, choices("", RepoChoice::All, RepoChoice::None, RepoChoice::None)).await;
    let a = f.c.state.apps.clone();
    let b = f.c.state.apps.clone();
    let (ca, cb) = (c1(), c1());
    let (r1, r2) = tokio::join!(a.token(exchange_req(&code, WEB, WEB_CB), &ca), b.token(exchange_req(&code, WEB, WEB_CB), &cb));
    assert_eq!([r1.is_ok(), r2.is_ok()].iter().filter(|x| **x).count(), 1, "exactly one exchange succeeds");
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE kind = 'access'").fetch_one(&f.c.pool).await.unwrap();
    assert_eq!(tokens, 1, "only one set of tokens was ever made");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn refresh_rotates_and_reuse_is_handled_by_age(pool: PgPool) {
    let f = fixture(pool).await;
    let t1 = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    // A refresh needs the same client, and may not widen.
    assert_eq!(
        f.c.state.apps.token(refresh_req(&t1.refresh_token, CODE), &c1()).await.unwrap_err().code,
        "invalid_grant",
        "another client"
    );
    assert_eq!(
        f.c.state
            .apps
            .token(TokenRequest { scope: Some("read write".into()), ..refresh_req(&t1.refresh_token, WEB) }, &c1())
            .await
            .unwrap_err()
            .code,
        "invalid_request",
        "no widening"
    );
    assert_eq!(
        f.c.state
            .apps
            .token(TokenRequest { resource: Some("https://code.example.com/x".into()), ..refresh_req(&t1.refresh_token, WEB) }, &c1())
            .await
            .unwrap_err()
            .code,
        "invalid_target"
    );
    let t2 = f.c.state.apps.token(refresh_req(&t1.refresh_token, WEB), &c1()).await.unwrap();
    assert_ne!(t2.refresh_token, t1.refresh_token, "a refresh token rotates every use");
    assert!(f.c.state.authn.from_app_bearer(&t2.access_token, &c1()).await.is_ok());
    // The old refresh token again, within 30 seconds: refused, and the grant survives.
    assert_eq!(f.c.state.apps.token(refresh_req(&t1.refresh_token, WEB), &c1()).await.unwrap_err().code, "invalid_grant");
    assert!(f.c.state.authn.from_app_bearer(&t2.access_token, &c1()).await.is_ok(), "a second process racing the first is not a thief");
    assert_eq!(count_audit(&f.c, "app.refresh_reuse").await, 0);
    // The same again after the window: the grant ends and it is audited.
    sqlx::query("UPDATE oauth_tokens SET used_at = now() - interval '2 minutes' WHERE used_at IS NOT NULL AND kind = 'refresh'")
        .execute(&f.c.pool)
        .await
        .unwrap();
    assert_eq!(f.c.state.apps.token(refresh_req(&t1.refresh_token, WEB), &c1()).await.unwrap_err().code, "invalid_grant");
    assert!(f.c.state.authn.from_app_bearer(&t2.access_token, &c1()).await.is_err());
    assert!(f.c.state.apps.token(refresh_req(&t2.refresh_token, WEB), &c1()).await.is_err());
    assert_eq!(count_audit(&f.c, "app.refresh_reuse").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn concurrent_refresh_has_one_winner_and_no_revocation(pool: PgPool) {
    let f = fixture(pool).await;
    let t1 = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    let a = f.c.state.apps.clone();
    let b = f.c.state.apps.clone();
    let (ca, cb) = (c1(), c1());
    let (r1, r2) = tokio::join!(a.token(refresh_req(&t1.refresh_token, WEB), &ca), b.token(refresh_req(&t1.refresh_token, WEB), &cb));
    let winners: Vec<_> = [r1, r2].into_iter().filter_map(|r| r.ok()).collect();
    assert_eq!(winners.len(), 1);
    assert!(f.c.state.authn.from_app_bearer(&winners[0].access_token, &c1()).await.is_ok(), "the loser's refusal did not end the grant");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_grant_expiry_is_absolute(pool: PgPool) {
    let f = fixture(pool).await;
    let t1 = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    sqlx::query("UPDATE oauth_grants SET expires_at = now() + interval '10 minutes'").execute(&f.c.pool).await.unwrap();
    let t2 = f.c.state.apps.token(refresh_req(&t1.refresh_token, WEB), &c1()).await.unwrap();
    assert!(t2.expires_in <= 600, "an hour-long access token is cut to the grant: {}", t2.expires_in);
    for tok in [&t2.access_token, &t2.refresh_token] {
        let within: bool = sqlx::query_scalar(
            "SELECT t.expires_at <= g.expires_at FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id WHERE t.token_hash = $1",
        )
        .bind(ch_db::tokens::hash_token(tok))
        .fetch_one(&f.c.pool)
        .await
        .unwrap();
        assert!(within, "no token outlives its grant");
    }
    sqlx::query("UPDATE oauth_grants SET expires_at = now() - interval '1 second'").execute(&f.c.pool).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&t2.access_token, &c1()).await.is_err());
    assert!(f.c.state.apps.token(refresh_req(&t2.refresh_token, WEB), &c1()).await.is_err());
}

// ------------------------------------------------------------------ revocation

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn every_trigger_ends_every_grant_of_the_person(pool: PgPool) {
    let mut f = fixture(pool).await;
    let live = |f: &F| {
        let pool = f.c.pool.clone();
        let who = f.alice.0;
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_grants WHERE revoked_at IS NULL AND user_id = $1")
                .bind(who)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    // Signing out other sessions does not touch connected apps.
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    f.c.state.credentials.sign_out_others(&f.actor, &f.cookie, &c1()).await.unwrap();
    assert_eq!(live(&f).await, 1);
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_ok());
    // Sign out everywhere.
    f.c.state.accounts.sign_out_everywhere(&Actor::System, f.alice, &c1()).await.unwrap();
    assert_eq!(live(&f).await, 0, "sign out everywhere");
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err());
    // An admin sets a password.
    become_person(&mut f, "p2").await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    f.c.state.accounts.set_password(&Actor::System, f.alice, PW2, &c1()).await.unwrap();
    assert_eq!(live(&f).await, 0, "set_password");
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err());
    // The person changes their password.
    become_person(&mut f, "p3").await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    f.c.state.credentials.change_password(&f.actor, &f.cookie, PW, PW2, &c1()).await.unwrap();
    assert_eq!(live(&f).await, 0, "password change");
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err());
    // Turning two-factor off.
    become_person(&mut f, "p4").await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    let code = f.codes.borrow()[0].clone();
    f.c.state.twofactor.disable(&f.actor, &f.cookie, PW, &code, &c1()).await.unwrap();
    assert_eq!(live(&f).await, 0, "two-factor off");
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err());
    // Suspension.
    become_person(&mut f, "p5").await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    f.c.state.accounts.suspend(&Actor::System, f.alice, &c1()).await.unwrap();
    assert_eq!(live(&f).await, 0, "suspension");
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err());
    assert_eq!(count_audit(&f.c, "app.revoked").await, 0, "bulk revocation is part of the change that caused it");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_person_can_revoke_only_their_own_and_the_app_can_revoke_itself(pool: PgPool) {
    let f = fixture(pool).await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    let grants = f.c.state.apps.list_grants(&f.actor).await.unwrap();
    assert_eq!(grants.len(), 1);
    let id: GrantId = grants[0].id;
    // Someone else, and an app, get nothing.
    make_user(&f.c, "mallory", false).await;
    let (cm, am) = session(&f.c, "mallory").await;
    let _ = cm;
    assert!(f.c.state.apps.revoke_grant(&am, id, &c1()).await.is_err());
    assert!(f.c.state.apps.list_grants(&am).await.unwrap().is_empty());
    let app = f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.unwrap();
    assert!(f.c.state.apps.list_grants(&app).await.is_err(), "an app cannot list connections");
    assert!(f.c.state.apps.revoke_grant(&app, id, &c1()).await.is_err(), "an app cannot end connections");
    // The app revokes with its own client id; another client id does nothing; unknown tokens say nothing.
    f.c.state.apps.revoke_token(&t.refresh_token, Some(CODE), &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_ok());
    f.c.state.apps.revoke_token("chmr_unknown", Some(WEB), &c1()).await.unwrap();
    f.c.state.apps.revoke_token(&t.refresh_token, None, &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_ok(), "no client id, no revocation");
    f.c.state.apps.revoke_token(&t.refresh_token, Some(WEB), &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err(), "the whole grant ended");
    assert_eq!(f.c.state.apps.list_grants(&f.actor).await.unwrap().len(), 0);
    assert_eq!(count_audit(&f.c, "app.revoked").await, 1);
    // The person can end one too.
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    let id = f.c.state.apps.list_grants(&f.actor).await.unwrap()[0].id;
    f.c.state.apps.revoke_grant(&f.actor, id, &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn no_token_text_reaches_the_database_or_the_audit_trail(pool: PgPool) {
    let f = fixture(pool).await;
    let code = get_code(&f, WEB, WEB_CB, choices("", RepoChoice::All, RepoChoice::None, RepoChoice::None)).await;
    let t = f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await.unwrap();
    let _ = f.c.state.apps.token(exchange_req(&code, WEB, WEB_CB), &c1()).await;
    let t2 = f.c.state.apps.token(refresh_req(&t.refresh_token, WEB), &c1()).await;
    let secrets =
        [code.as_str(), &t.access_token, &t.refresh_token, &verifier(), t2.as_ref().map(|x| x.access_token.as_str()).unwrap_or("none")];
    for table in ["oauth_grants", "oauth_requests", "oauth_codes", "oauth_tokens", "audit_events"] {
        let dump: String = sqlx::query_scalar(&format!("SELECT COALESCE(string_agg(to_json(t)::text, ' '), '') FROM {table} t"))
            .fetch_one(&f.c.pool)
            .await
            .unwrap();
        for s in secrets {
            assert!(!dump.contains(s), "{table} holds a secret");
        }
    }
    // Hashes only: the stored value is 32 bytes.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE octet_length(token_hash) = 32").fetch_one(&f.c.pool).await.unwrap();
    assert!(n >= 2);
}

// ------------------------------------------------------------------ what an app actor is

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_app_is_capped_by_its_grant_and_never_admin(pool: PgPool) {
    let f = fixture(pool).await;
    let bob = f.c.env.user("erin").await;
    let public = f.c.env.public_repo(bob, "open").await;
    let t = connect(&f, RepoChoice::Selected(vec![f.r1, f.r2]), RepoChoice::Selected(vec![f.r1]), RepoChoice::None).await;
    let app = f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.unwrap();
    let perm = |r: RepoId| {
        let app = app.clone();
        let f = &f;
        async move { f.c.state.authz.repo_permission(&app, r).await.unwrap() }
    };
    assert_eq!(perm(f.r1).await, Permission::Write, "selected for write, and never Maintain or Admin though the person owns it");
    assert_eq!(perm(f.r2).await, Permission::Read);
    assert_eq!(perm(public).await, Permission::None, "a public repository that was not selected is missing");
    assert_eq!(perm(f.bob_private).await, Permission::None);
    assert!(f.c.state.authz.require(&app, f.r1, Permission::Admin).await.is_err());
    assert!(matches!(f.c.state.authz.require(&app, f.r2, Permission::Write).await, Err(ch_core::Error::Forbidden(_))));
    assert!(matches!(f.c.state.authz.require(&app, f.r2, Permission::Admin).await, Err(ch_core::Error::Forbidden(_))));
    assert!(
        matches!(f.c.state.authz.require(&app, f.bob_private, Permission::Read).await, Err(ch_core::Error::NotFound)),
        "unselected equals missing"
    );
    assert!(f.c.state.authz.org_role(&app, ch_core::OrgId(1)).await.unwrap().is_none());
    assert!(!app.is_site_admin());

    // Read on all includes public repositories of other people. Write stays where it was given.
    let t = connect(&f, RepoChoice::All, RepoChoice::Selected(vec![f.r2]), RepoChoice::None).await;
    let app = f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.unwrap();
    assert_eq!(f.c.state.authz.repo_permission(&app, public).await.unwrap(), Permission::Read);
    assert_eq!(f.c.state.authz.repo_permission(&app, f.r1).await.unwrap(), Permission::Read);
    assert_eq!(f.c.state.authz.repo_permission(&app, f.r2).await.unwrap(), Permission::Write);
    assert_eq!(f.c.state.authz.repo_permission(&app, f.bob_private).await.unwrap(), Permission::None, "the person cannot read it either");

    // The person losing access takes the app's access with it.
    let carol = f.c.env.user("carol2").await;
    let shared = f.c.env.repo(carol, "shared").await;
    f.c.env.collaborator(shared, f.alice, "write").await;
    let t = connect(&f, RepoChoice::Selected(vec![shared]), RepoChoice::Selected(vec![shared]), RepoChoice::None).await;
    let app = f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.unwrap();
    assert_eq!(f.c.state.authz.repo_permission(&app, shared).await.unwrap(), Permission::Write);
    sqlx::query("DELETE FROM repo_collaborators WHERE repo_id = $1").bind(shared.0).execute(&f.c.pool).await.unwrap();
    assert_eq!(f.c.state.authz.repo_permission(&app, shared).await.unwrap(), Permission::None, "a collaborator who is removed");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_site_admin_who_connects_an_app_gives_it_no_admin_power(pool: PgPool) {
    let mut f = fixture(pool).await;
    make_user(&f.c, "root2", true).await;
    let (cookie, actor) = session(&f.c, "root2").await;
    let (_, codes, cookie, actor) = enable_2fa(&f.c, &cookie, &actor).await;
    assert!(actor.is_site_admin());
    f.alice = actor.user_id().unwrap();
    f.cookie = cookie;
    f.actor = actor;
    *f.codes.borrow_mut() = codes;
    // A repository the admin neither owns nor collaborates on.
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    let app = f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.unwrap();
    assert_eq!(f.c.state.authz.repo_permission(&f.actor, f.bob_private).await.unwrap(), Permission::Admin, "the admin themselves can");
    assert_eq!(f.c.state.authz.repo_permission(&app, f.bob_private).await.unwrap(), Permission::None, "the app cannot");
    assert!(!app.is_site_admin());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn each_token_kind_has_its_own_door(pool: PgPool) {
    let f = fixture(pool).await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    // `from_bearer` is for personal access tokens, job tokens and runner credentials. An app
    // token is simply not one, and a refresh token or a code is not an access token.
    assert!(f.c.state.authn.from_bearer(&t.access_token, &c1()).await.is_err());
    assert!(f.c.state.authn.from_bearer(&t.refresh_token, &c1()).await.is_err());
    assert!(f.c.state.authn.from_basic("alice", &t.access_token, &c1()).await.is_err());
    assert!(f.c.state.authn.from_app_bearer(&t.refresh_token, &c1()).await.is_err());
    // And a personal access token is not an app token.
    let (_, pat) = f.c.state.accounts.create_token(&f.actor, f.alice, "ci", vec!["repo".into()], 30, &c1()).await.unwrap();
    assert!(f.c.state.authn.from_app_bearer(&pat, &c1()).await.is_err());
    assert!(f.c.state.authn.from_app_bearer("chma_short", &c1()).await.is_err());
    assert!(f.c.state.authn.from_app_bearer(&format!("chma_{}", "x".repeat(43)), &c1()).await.is_err());
    // A flood of unknown bearers does not touch the failed-auth budget of the address, so a
    // valid personal access token from the same address still works.
    for _ in 0..80 {
        let _ = f.c.state.authn.from_app_bearer(&format!("chma_{}", "y".repeat(43)), &c1()).await;
    }
    assert!(f.c.state.authn.from_bearer(&pat, &c1()).await.is_ok());
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_sweep_removes_old_rows_and_keeps_live_ones(pool: PgPool) {
    let f = fixture(pool).await;
    let t = connect(&f, RepoChoice::All, RepoChoice::None, RepoChoice::None).await;
    // Leftovers: an unanswered request, an unused grant, an old access token, an old ended grant.
    consent(&f, params(WEB, WEB_CB)).await;
    let v = consent(&f, params(WEB, WEB_CB)).await;
    f.c.state
        .apps
        .approve(&f.actor, &f.cookie, choices(&v.request_id, RepoChoice::All, RepoChoice::None, RepoChoice::None), &c1())
        .await
        .unwrap();
    sqlx::query("UPDATE oauth_requests SET expires_at = now() - interval '2 days'").execute(&f.c.pool).await.unwrap();
    sqlx::query("UPDATE oauth_grants SET created_at = now() - interval '2 days' WHERE activated_at IS NULL")
        .execute(&f.c.pool)
        .await
        .unwrap();
    let before = rows(&f, "oauth_grants").await;
    ch_auth::sweep::sweep(&f.c.pool).await.unwrap();
    assert_eq!(rows(&f, "oauth_grants").await, before - 1, "a grant that never got a token goes after a day");
    assert_eq!(rows(&f, "oauth_requests").await, 0);
    assert!(f.c.state.authn.from_app_bearer(&t.access_token, &c1()).await.is_ok(), "live rows stay");
    sqlx::query("UPDATE oauth_grants SET revoked_at = now() - interval '100 days'").execute(&f.c.pool).await.unwrap();
    ch_auth::sweep::sweep(&f.c.pool).await.unwrap();
    assert_eq!(rows(&f, "oauth_grants").await, 0, "ended grants go after 90 days");
    assert_eq!(rows(&f, "oauth_tokens").await, 0, "and their tokens with them");
}
