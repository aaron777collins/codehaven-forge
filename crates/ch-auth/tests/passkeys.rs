//! WebAuthn passkeys as a second factor beside TOTP (ch-wf0), driven by a software authenticator:
//! real ceremonies, no device. The JSON that crosses the service boundary is the JSON a browser
//! would send, so the page script has the same contract to meet.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::identity::SignInStep;
use ch_core::model::ClientInfo;
use ch_core::{Actor, Error};
use sqlx::PgPool;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Url};

const SITE: &str = "https://code.example.com";

async fn site(pool: PgPool) -> Ctx {
    ctx_with(pool, |c| c.server.external_url = SITE.into()).await
}

fn origin() -> Url {
    Url::parse(SITE).unwrap()
}

fn authenticator() -> SoftPasskey {
    SoftPasskey::new(true)
}

/// Adds a passkey to the account through the two requests of the ceremony.
async fn add_passkey(
    c: &Ctx,
    actor: &Actor,
    cookie: &str,
    key: &mut SoftPasskey,
    name: &str,
) -> ch_core::Result<ch_core::identity::PasskeyAdded> {
    let options = c.state.twofactor.begin_passkey(actor, &c1()).await?;
    let ccr: CreationChallengeResponse = serde_json::from_str(&options).unwrap();
    let response: RegisterPublicKeyCredential = key.do_registration(origin(), ccr).unwrap();
    c.state.twofactor.finish_passkey(actor, cookie, name, &serde_json::to_string(&response).unwrap(), &c1()).await
}

/// Answers the passkey half of a pending sign-in with `key`.
async fn answer(c: &Ctx, pending: &str, key: &mut SoftPasskey) -> ch_core::Result<ch_core::identity::SignIn> {
    let options = c.state.credentials.begin_passkey_sign_in(pending, &c1()).await?;
    let rcr: RequestChallengeResponse = serde_json::from_str(&options).unwrap();
    let response: PublicKeyCredential = key.do_authentication(origin(), rcr).unwrap();
    c.state.credentials.complete_passkey_sign_in(pending, &serde_json::to_string(&response).unwrap(), &c1()).await
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_first_passkey_turns_two_factor_on_with_recovery_codes_and_the_next_changes_nothing(pool: PgPool) {
    let c = site(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    assert!(c.state.twofactor.passkeys_available().await);
    assert!(matches!(actor, Actor::User { has_2fa: false, .. }));

    let mut laptop = authenticator();
    let added = add_passkey(&c, &actor, &cookie, &mut laptop, "  My   laptop ").await.unwrap();
    assert_eq!(added.passkey.name, "My laptop");
    assert_eq!(added.recovery_codes.len(), 10, "two-factor was off, so recovery codes are made");
    let grant = added.grant.expect("every session ended and a new one is issued");
    assert!(matches!(actor_for_cookie(&c, &cookie).await, Actor::Anonymous), "the old session ended");
    let now = actor_for_cookie(&c, &grant.cookie).await;
    assert!(matches!(now, Actor::User { has_2fa: true, .. }), "a passkey is a second factor");
    let status = c.state.twofactor.status(&now).await.unwrap();
    assert_eq!((status.enabled, status.totp, status.passkeys, status.recovery_codes_left), (true, false, 1, 10));
    assert_eq!(c.state.twofactor.passkeys(&now).await.unwrap().len(), 1);

    // A second passkey on an account that has two-factor on adds a factor and changes nothing else.
    let mut phone = authenticator();
    let second = add_passkey(&c, &now, &grant.cookie, &mut phone, "").await.unwrap();
    assert_eq!(second.passkey.name, "Passkey", "a blank name gets a plain one");
    assert!(second.recovery_codes.is_empty() && second.grant.is_none());
    assert!(matches!(actor_for_cookie(&c, &grant.cookie).await, Actor::User { .. }), "the session carries on");
    assert_eq!(c.state.twofactor.passkeys(&now).await.unwrap().len(), 2);
    // The same credential is not stored twice: its id is unique across the site.
    let stored: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT credential_id) FROM webauthn_credentials").fetch_one(&c.pool).await.unwrap();
    assert_eq!(stored, 2);
    assert_eq!(count_audit(&c, "auth.passkey_added").await, 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_passkey_signs_in_as_the_second_step_and_so_still_does_a_recovery_code(pool: PgPool) {
    let c = site(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let mut key = authenticator();
    let added = add_passkey(&c, &actor, &cookie, &mut key, "laptop").await.unwrap();

    // Password first: the session is only half signed in.
    let first = sign_in(&c, "alice", PW).await;
    assert_eq!(first.step, SignInStep::TwoFactorRequired);
    let factors = c.state.credentials.pending_factors(&first.grant.cookie).await.unwrap();
    assert!((factors.totp, factors.passkeys) == (false, true), "{factors:?}");
    // The half session opens nothing by itself.
    assert!(matches!(actor_for_cookie(&c, &first.grant.cookie).await, Actor::Anonymous));
    let done = answer(&c, &first.grant.cookie, &mut key).await.unwrap();
    assert_eq!(done.step, SignInStep::Complete);
    assert!(matches!(actor_for_cookie(&c, &done.grant.cookie).await, Actor::User { has_2fa: true, .. }));
    assert_eq!(count_audit(&c, "auth.passkey_used").await, 1);
    let used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM webauthn_credentials WHERE user_id = (SELECT id FROM users WHERE username = 'alice')")
            .fetch_one(&c.pool)
            .await
            .unwrap();
    assert!(used.is_some(), "use is recorded");

    // The pending session is gone after the swap: it cannot be answered again.
    assert!(c.state.credentials.begin_passkey_sign_in(&first.grant.cookie, &c1()).await.is_err());

    // A recovery code answers the same step.
    let second = sign_in(&c, "alice", PW).await;
    let code = added.recovery_codes[0].clone();
    let by_code = c.state.credentials.complete_two_factor(&second.grant.cookie, &code, &c1()).await.unwrap();
    assert_eq!(by_code.step, SignInStep::Complete);
    // And it works once.
    let third = sign_in(&c, "alice", PW).await;
    assert!(c.state.credentials.complete_two_factor(&third.grant.cookie, &code, &c1()).await.is_err());
    // A six digit code is simply wrong for an account with no authenticator app.
    assert!(c.state.credentials.complete_two_factor(&third.grant.cookie, "123456", &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_answer_cannot_be_replayed_and_another_accounts_passkey_does_not_do(pool: PgPool) {
    let c = site(pool).await;
    for n in ["alice", "bob"] {
        make_user(&c, n, false).await;
    }
    let (ac, aa) = session(&c, "alice").await;
    let (bc, ba) = session(&c, "bob").await;
    let (mut alice_key, mut bob_key) = (authenticator(), authenticator());
    add_passkey(&c, &aa, &ac, &mut alice_key, "a").await.unwrap();
    add_passkey(&c, &ba, &bc, &mut bob_key, "b").await.unwrap();

    // Bob's answer to Bob's challenge, sent for Alice's pending sign-in: refused, and a wrong try.
    let bob_pending = sign_in(&c, "bob", PW).await;
    let rcr: RequestChallengeResponse =
        serde_json::from_str(&c.state.credentials.begin_passkey_sign_in(&bob_pending.grant.cookie, &c1()).await.unwrap()).unwrap();
    let bobs_answer = serde_json::to_string(&bob_key.do_authentication(origin(), rcr).unwrap()).unwrap();
    let pending = sign_in(&c, "alice", PW).await;
    c.state.credentials.begin_passkey_sign_in(&pending.grant.cookie, &c1()).await.unwrap();
    let e = c.state.credentials.complete_passkey_sign_in(&pending.grant.cookie, &bobs_answer, &c1()).await;
    assert!(matches!(e, Err(Error::Unauthorized(_))), "{e:?}");
    assert!(count_audit(&c, "auth.two_factor_failed").await >= 1);

    // A right answer, sent twice: the second finds no challenge.
    let pending = sign_in(&c, "alice", PW).await;
    let options = c.state.credentials.begin_passkey_sign_in(&pending.grant.cookie, &c1()).await.unwrap();
    let rcr: RequestChallengeResponse = serde_json::from_str(&options).unwrap();
    let response = serde_json::to_string(&alice_key.do_authentication(origin(), rcr).unwrap()).unwrap();
    c.state.credentials.complete_passkey_sign_in(&pending.grant.cookie, &response, &c1()).await.unwrap();
    // The session was swapped, so the replay has no pending session either way.
    assert!(c.state.credentials.complete_passkey_sign_in(&pending.grant.cookie, &response, &c1()).await.is_err());
    // Garbage and a missing cookie do nothing.
    let again = sign_in(&c, "alice", PW).await;
    assert!(c.state.credentials.complete_passkey_sign_in(&again.grant.cookie, "{not json", &c1()).await.is_err());
    assert!(c.state.credentials.complete_passkey_sign_in("nope", "{}", &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn taking_the_last_factor_away_needs_the_password_and_a_current_factor(pool: PgPool) {
    let c = site(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let mut key = authenticator();
    let added = add_passkey(&c, &actor, &cookie, &mut key, "laptop").await.unwrap();
    let grant = added.grant.unwrap();
    let now = actor_for_cookie(&c, &grant.cookie).await;
    let id = added.passkey.id;
    let code = added.recovery_codes[0].clone();
    let remove = |password: &'static str, code: String| {
        let (c, now, cookie) = (&c, now.clone(), grant.cookie.clone());
        async move { c.state.twofactor.remove_passkey(&now, &cookie, id, password, &code, &c1()).await }
    };
    assert!(remove("not the password!", code.clone()).await.is_err(), "a wrong password");
    assert!(remove(PW, String::new()).await.is_err(), "no current factor");
    assert!(remove(PW, "ABCDE-FGHJK".into()).await.is_err(), "a code that is not one of theirs");
    assert_eq!(c.state.twofactor.passkeys(&now).await.unwrap().len(), 1, "nothing was removed");
    // Someone else's row id is not theirs to remove.
    assert!(matches!(c.state.twofactor.remove_passkey(&now, &grant.cookie, id + 100, PW, &code, &c1()).await, Err(Error::NotFound)));

    let off = remove(PW, code).await.unwrap().expect("the last factor went, so two-factor is off and a new session is issued");
    assert!(matches!(actor_for_cookie(&c, &grant.cookie).await, Actor::Anonymous));
    assert!(matches!(actor_for_cookie(&c, &off.cookie).await, Actor::User { has_2fa: false, .. }));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM totp_recovery_codes").fetch_one(&c.pool).await.unwrap();
    assert_eq!(left, 0, "the recovery codes went with the last factor");
    assert_eq!(sign_in(&c, "alice", PW).await.step, SignInStep::Complete, "password alone again");
    assert_eq!(count_audit(&c, "auth.passkey_removed").await, 1);
    assert_eq!(count_audit(&c, "auth.two_factor_off").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn with_an_authenticator_app_and_a_passkey_either_can_go_and_two_factor_stays(pool: PgPool) {
    let c = site(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let (secret, codes, cookie, actor) = enable_2fa(&c, &cookie, &actor).await;
    let mut key = authenticator();
    let added = add_passkey(&c, &actor, &cookie, &mut key, "laptop").await.unwrap();
    assert!(added.recovery_codes.is_empty() && added.grant.is_none(), "an app was already on");
    let status = c.state.twofactor.status(&actor).await.unwrap();
    assert_eq!((status.totp, status.passkeys, status.recovery_codes_left), (true, 1, 10));

    // The passkey goes: the app is still a factor, so the password is enough and nothing ends.
    let kept = c.state.twofactor.remove_passkey(&actor, &cookie, added.passkey.id, PW, "", &c1()).await.unwrap();
    assert!(kept.is_none());
    assert!(matches!(actor_for_cookie(&c, &cookie).await, Actor::User { has_2fa: true, .. }));
    // Add it back, then turn the app off: the passkey keeps two-factor on, recovery codes stay.
    let again = add_passkey(&c, &actor, &cookie, &mut authenticator(), "phone").await.unwrap();
    let grant = c.state.twofactor.disable(&actor, &cookie, PW, "", &c1()).await.unwrap();
    let now = actor_for_cookie(&c, &grant.cookie).await;
    assert!(matches!(now, Actor::User { has_2fa: true, .. }), "a passkey is left");
    let status = c.state.twofactor.status(&now).await.unwrap();
    assert_eq!((status.totp, status.passkeys, status.recovery_codes_left), (false, 1, 10));
    let _ = (secret, codes, again);
    // With the app gone, turning it off again is refused: it is not on.
    assert!(c.state.twofactor.disable(&now, &grant.cookie, PW, "", &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn passkeys_are_off_when_the_site_address_is_an_ip_address(pool: PgPool) {
    let c = ctx_with(pool, |c| c.server.external_url = "http://192.0.2.10:3200".into()).await;
    make_user(&c, "alice", false).await;
    let (_, actor) = session(&c, "alice").await;
    assert!(!c.state.twofactor.passkeys_available().await);
    assert!(matches!(c.state.twofactor.begin_passkey(&actor, &c1()).await, Err(Error::Unavailable(_))));
    let _ = ClientInfo::local();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_account_with_a_passkey_is_not_asked_for_a_password_alone_over_basic_or_by_apps(pool: PgPool) {
    let c = site(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    add_passkey(&c, &actor, &cookie, &mut authenticator(), "laptop").await.unwrap();
    // Git over HTTPS with the account password is closed once any second factor is on.
    let basic = c.state.authn.from_basic("alice", PW, &c1()).await;
    assert!(basic.is_err() || matches!(basic, Ok(Actor::Anonymous)), "{basic:?}");
    let _ = actor;
}
