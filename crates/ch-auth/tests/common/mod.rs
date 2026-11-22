//! Shared setup for the identity integration tests. Each test gets its own database from
//! `#[sqlx::test]`, a state with the real identity services, and cheap password hashing.
#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

pub mod keys;

use ch_auth::password::HashParams;
use ch_auth::{AuthOptions, totp};
use ch_core::identity::SignIn;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, UserId};
use ch_db::sealing::MasterKey;
use ch_testkit::TestEnv;
use sqlx::PgPool;

pub const PW: &str = "violet harbour lantern 83 teapot";
pub const PW2: &str = "granite meadow orbit 27 saffron";

pub struct Ctx {
    pub env: TestEnv,
    pub state: AppState,
    pub pool: PgPool,
}

pub async fn ctx(pool: PgPool) -> Ctx {
    ctx_with(pool, |_| {}).await
}

pub async fn ctx_with(pool: PgPool, tweak: impl FnOnce(&mut ch_core::Config)) -> Ctx {
    ctx_full(pool, tweak, None).await
}

/// The same, with an injected OAuth client id fetcher.
pub async fn ctx_full(
    pool: PgPool,
    tweak: impl FnOnce(&mut ch_core::Config),
    fetcher: Option<std::sync::Arc<dyn ch_auth::apps::cimd::ClientDocFetcher>>,
) -> Ctx {
    let mut env = TestEnv::new(pool.clone()).await;
    {
        let mut cfg = (*env.config).clone();
        tweak(&mut cfg);
        env.config = std::sync::Arc::new(cfg);
    }
    let mut b = env.builder().await;
    let services = ch_auth::build_with(
        &b,
        AuthOptions { hash_params: HashParams::FAST_FOR_TESTS, master_key: Some(MasterKey::random()), client_fetcher: fetcher },
    )
    .expect("build identity");
    ch_core::Install::install(services, &mut b);
    let state = b.build();
    Ctx { env, state, pool }
}

pub fn client(ip: &str) -> ClientInfo {
    ClientInfo { ip: ip.parse().unwrap(), user_agent: Some("test-agent/1.0".into()) }
}

pub fn c1() -> ClientInfo {
    client("203.0.113.10")
}

/// A user made the way the CLI makes one, with a password set.
pub async fn make_user(c: &Ctx, name: &str, admin: bool) -> UserId {
    let u = c
        .state
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: admin, can_run_ci: false },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
    c.state.accounts.set_password(&Actor::System, u.id, PW, &ClientInfo::local()).await.unwrap();
    u.id
}

pub async fn sign_in(c: &Ctx, login: &str, pw: &str) -> SignIn {
    c.state.credentials.sign_in(login, pw, &c1()).await.expect("sign in")
}

pub async fn actor_for_cookie(c: &Ctx, cookie: &str) -> Actor {
    c.state.authn.from_session(cookie, &c1()).await.unwrap()
}

/// Signs in and returns the session cookie and the resulting actor.
pub async fn session(c: &Ctx, login: &str) -> (String, Actor) {
    let s = sign_in(c, login, PW).await;
    let a = actor_for_cookie(c, &s.grant.cookie).await;
    (s.grant.cookie, a)
}

/// Turns two-factor on through the real flow. Returns the secret, the recovery codes, the new
/// cookie and the new actor.
pub async fn enable_2fa(c: &Ctx, cookie: &str, actor: &Actor) -> (Vec<u8>, Vec<String>, String, Actor) {
    let setup = c.state.twofactor.begin_setup(actor).await.unwrap();
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let code = totp::code_at(&secret, now());
    let enabled = c.state.twofactor.confirm_setup(actor, cookie, &code, &c1()).await.unwrap();
    let a = actor_for_cookie(c, &enabled.grant.cookie).await;
    (secret, enabled.recovery_codes, enabled.grant.cookie, a)
}

pub fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

/// A site admin session: admin flag plus two-factor.
pub async fn site_admin(c: &Ctx, name: &str) -> (UserId, Actor) {
    let id = make_user(c, name, true).await;
    let (cookie, actor) = session(c, name).await;
    let (_, _, _, actor) = enable_2fa(c, &cookie, &actor).await;
    assert!(actor.is_site_admin());
    (id, actor)
}

pub async fn audit_actions(c: &Ctx) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_events ORDER BY id").fetch_all(&c.pool).await.unwrap()
}

pub async fn count_audit(c: &Ctx, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = $1").bind(action).fetch_one(&c.pool).await.unwrap()
}

pub fn new_invite(email: Option<&str>) -> NewInvite {
    NewInvite {
        email: email.map(String::from),
        grants_admin: false,
        grants_ci: false,
        org_id: None,
        team_id: None,
        note: None,
        expires_in_hours: None,
    }
}

pub fn new_account(name: &str) -> NewAccount {
    NewAccount { username: name.into(), email: format!("{name}@example.org"), password: PW.into(), display_name: None }
}

/// The token from an invite URL.
pub fn token_of(url: &str) -> String {
    url.rsplit('/').next().unwrap().to_string()
}
