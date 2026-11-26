//! Sign-in, lockout, two-factor and sessions (ARCHITECTURE.md sections 7.1 and 7.6).
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_auth::lockout::ACCOUNT_THRESHOLD;
use ch_auth::totp;
use ch_core::identity::SignInStep;
use ch_core::model::*;
use ch_core::{Actor, AuthVia, Error};
use sqlx::PgPool;
use tokio::task::JoinSet;

fn is_generic(e: &Error) -> bool {
    matches!(e, Error::Unauthorized(m) if m == "That username and password don't match.")
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn sign_in_with_username_or_email_and_case_does_not_matter(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "Alice", false).await;
    for login in ["Alice", "alice", "ALICE", "alice@example.org", "Alice@Example.org", "  alice  "] {
        let s = c.state.credentials.sign_in(login, PW, &c1()).await.unwrap_or_else(|e| panic!("{login}: {e:?}"));
        assert_eq!(s.step, SignInStep::Complete);
        assert_eq!(s.grant.cookie.len(), 64);
        assert_eq!(s.grant.max_age_secs, 7 * 24 * 3600);
        let a = actor_for_cookie(&c, &s.grant.cookie).await;
        assert!(matches!(a, Actor::User { via: AuthVia::Session, is_admin: false, has_2fa: false, .. }));
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn failures_are_identical_for_unknown_wrong_suspended_and_unset(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "real", false).await;
    let sus = make_user(&c, "gone", false).await;
    c.state.accounts.suspend(&Actor::System, sus, &ClientInfo::local()).await.unwrap();
    let nopw = c
        .state
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: "nopw".into(), email: "nopw@example.org".into(), display_name: None, is_admin: false, can_run_ci: false },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
    let _ = nopw;
    for (login, pw) in
        [("real", "wrong password entirely"), ("nobody", PW), ("gone", PW), ("nopw", PW), ("real", ""), ("", PW), ("real", "x")]
    {
        let e = c.state.credentials.sign_in(login, pw, &c1()).await.unwrap_err();
        assert!(is_generic(&e), "{login:?}/{pw:?} gave {e:?}");
    }
    // An org is not a person: it never signs in.
    c.env.org("haven").await;
    assert!(is_generic(&c.state.credentials.sign_in("haven", PW, &c1()).await.unwrap_err()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn ten_failures_lock_the_account_but_not_its_tokens_or_keys(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let (_, secret) = c.state.accounts.create_token(&actor, id, "git", vec!["repo".into()], 30, &c1()).await.unwrap();
    let key = c.state.accounts.add_ssh_key(&actor, id, "laptop", keys::ED1, &c1()).await.unwrap();

    // The lock is per account and address (ch-rx2): ten wrong guesses from one address lock
    // that address out of this account.
    for i in 0..10 {
        let e = c.state.credentials.sign_in("alice", "guess number", &client("198.51.100.1")).await.unwrap_err();
        assert!(is_generic(&e), "attempt {i}: {e:?}");
    }
    // Now even the right password is refused from that address, and says so.
    let e = c.state.credentials.sign_in("alice", PW, &client("198.51.100.1")).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { retry_after_secs } if (14 * 60..=15 * 60).contains(&retry_after_secs)), "{e:?}");
    // Basic with the password is refused the same way.
    assert!(c.state.authn.from_basic("alice", PW, &client("198.51.100.1")).await.is_err());

    // The token, the SSH key and the existing session keep working.
    let a = c.state.authn.from_basic("alice", &secret, &client("198.51.100.97")).await.unwrap();
    assert!(matches!(a, Actor::Token { .. }));
    assert!(matches!(c.state.authn.from_bearer(&secret, &c1()).await.unwrap(), Actor::Token { .. }));
    assert!(c.state.authn.from_ssh_key(&key.fingerprint).await.unwrap().is_some());
    assert!(matches!(actor_for_cookie(&c, &cookie).await, Actor::User { .. }));

    // A lockout is audited with the source addresses, and the owner is told at next sign-in.
    assert_eq!(count_audit(&c, "auth.lockout").await, 1);
    let ips: serde_json::Value = sqlx::query_scalar("SELECT details->'source_ips' FROM audit_events WHERE action = 'auth.lockout'")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(ips.as_array().unwrap().len(), 1, "{ips}");

    // admin unlock clears it.
    c.state.accounts.unlock(&Actor::System, id, &ClientInfo::local()).await.unwrap();
    let s = c.state.credentials.sign_in("alice", PW, &client("198.51.100.50")).await.unwrap();
    let notice = s.notice.expect("the owner is told about the lockout");
    assert_eq!(notice.source_ips.len(), 1);
    // Told once.
    let s2 = c.state.credentials.sign_in("alice", PW, &client("198.51.100.50")).await.unwrap();
    assert!(s2.notice.is_none());
    assert_eq!(count_audit(&c, "auth.unlock").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_made_up_username_locks_the_same_way_so_nothing_leaks(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "real", false).await;
    let mut last = None;
    for _ in 0..11 {
        last = Some(c.state.credentials.sign_in("ghost", "whatever password", &client("198.51.100.1")).await.unwrap_err());
    }
    assert!(matches!(last, Some(Error::RateLimited { .. })), "{last:?}");
    // The real account is untouched.
    c.state.credentials.sign_in("real", PW, &client("198.51.100.1")).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_address_cannot_spray_many_accounts(pool: PgPool) {
    let c = ctx(pool).await;
    for i in 0..45 {
        let _ = c.state.credentials.sign_in(&format!("victim{i}"), "spray and pray 1", &client("203.0.113.77")).await;
    }
    make_user(&c, "later", false).await;
    let e = c.state.credentials.sign_in("later", PW, &client("203.0.113.77")).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }), "the address is locked: {e:?}");
    c.state.credentials.sign_in("later", PW, &client("203.0.113.78")).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn concurrent_wrong_guesses_cannot_exceed_the_attempt_budget(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    // Fifty guesses at once, from one address, so only the account-and-address budget
    // decides who reaches the hasher. The attempt is
    // reserved before the argon2 work, so the budget is ten however many run at once.
    let mut set = JoinSet::new();
    for _ in 0..50 {
        let svc = c.state.credentials.clone();
        let from = client("203.0.113.7");
        set.spawn(async move { svc.sign_in("alice", "one wrong guess in a burst", &from).await });
    }
    let (mut generic, mut limited) = (0, 0);
    while let Some(joined) = set.join_next().await {
        let r = joined.unwrap();
        match r {
            Err(Error::RateLimited { .. }) => limited += 1,
            Err(e) => {
                assert!(is_generic(&e), "{e:?}");
                generic += 1;
            }
            Ok(_) => panic!("a wrong password signed in"),
        }
    }
    assert!(generic <= ACCOUNT_THRESHOLD, "{generic} of 50 guesses reached the hasher");
    assert!(limited >= 40, "{limited} of 50 were refused");
    assert!(count_audit(&c, "auth.sign_in_failed").await <= ACCOUNT_THRESHOLD as i64);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_correct_password_inside_a_burst_still_hits_the_budget(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    // Sixty guesses at once and the last one spawned is the right password. Task order is
    // not deterministic, so the invariant is the point: at most ACCOUNT_THRESHOLD of the
    // sixty reach the hasher, whichever one has the right password.
    let mut set = JoinSet::new();
    for i in 0..60 {
        let svc = c.state.credentials.clone();
        let from = client("203.0.113.7");
        let pw = if i == 59 { PW } else { "one wrong guess in a burst" };
        set.spawn(async move { svc.sign_in("alice", pw, &from).await });
    }
    let mut through = 0;
    while let Some(joined) = set.join_next().await {
        let r = joined.unwrap();
        match r {
            Err(Error::RateLimited { .. }) => {}
            Err(e) => {
                assert!(is_generic(&e), "{e:?}");
                through += 1;
            }
            Ok(_) => through += 1,
        }
    }
    assert!(through <= ACCOUNT_THRESHOLD, "{through} of 60 sign-ins reached the hasher");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn basic_auth_guesses_cannot_exceed_the_attempt_budget(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    // Basic credentials (the API and git) go through the same reservation: forty guesses at
    // once from one address, and the budget is still only ten.
    let mut set = JoinSet::new();
    for _ in 0..40 {
        let svc = c.state.authn.clone();
        let from = client("203.0.113.8");
        set.spawn(async move { svc.from_basic("alice", "one wrong guess in a burst", &from).await });
    }
    let mut through = 0;
    while let Some(joined) = set.join_next().await {
        let r = joined.unwrap();
        match r {
            Err(Error::RateLimited { .. }) => {}
            Err(e) => {
                assert!(is_generic(&e), "{e:?}");
                through += 1;
            }
            Ok(_) => panic!("a wrong password authenticated"),
        }
    }
    assert!(through <= ACCOUNT_THRESHOLD, "{through} of 40 guesses reached the hasher");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn two_factor_sign_in_end_to_end(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let (secret, recovery, new_cookie, actor2) = enable_2fa(&c, &cookie, &actor).await;
    assert_eq!(recovery.len(), 10);
    assert!(matches!(actor2, Actor::User { has_2fa: true, .. }));
    // Turning it on ended the old session and issued a new id.
    assert!(matches!(actor_for_cookie(&c, &cookie).await, Actor::Anonymous));
    assert_ne!(cookie, new_cookie);

    let s = sign_in(&c, "alice", PW).await;
    assert_eq!(s.step, SignInStep::TwoFactorRequired);
    assert_eq!(s.grant.max_age_secs, 600);
    // Half signed in is nobody.
    assert!(matches!(actor_for_cookie(&c, &s.grant.cookie).await, Actor::Anonymous));

    // A wrong code fails, and the right one works. The step used at setup is spent, so use the next one.
    assert!(c.state.credentials.complete_two_factor(&s.grant.cookie, "000000", &c1()).await.is_err());
    let next = totp::code_at(&secret, now() + 30);
    let done = c.state.credentials.complete_two_factor(&s.grant.cookie, &next, &c1()).await.unwrap();
    assert_eq!(done.step, SignInStep::Complete);
    assert_ne!(done.grant.cookie, s.grant.cookie, "the id changes when the session becomes real");
    assert!(matches!(actor_for_cookie(&c, &done.grant.cookie).await, Actor::User { has_2fa: true, .. }));
    assert!(matches!(actor_for_cookie(&c, &s.grant.cookie).await, Actor::Anonymous));

    // The same code cannot be used again.
    let s2 = sign_in(&c, "alice", PW).await;
    assert!(c.state.credentials.complete_two_factor(&s2.grant.cookie, &next, &c1()).await.is_err(), "replay");
    // A pending cookie is useless as a session, and a session is useless for step two.
    assert!(c.state.credentials.complete_two_factor(&done.grant.cookie, &next, &c1()).await.is_err());

    // Basic with a password is refused for an account with two-factor: use a token.
    assert!(c.state.authn.from_basic("alice", PW, &c1()).await.is_err());
    assert_eq!(count_audit(&c, "auth.two_factor_on").await, 1);
    assert!(count_audit(&c, "auth.two_factor_failed").await >= 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn recovery_codes_work_once(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let (_, codes, _, _) = enable_2fa(&c, &cookie, &actor).await;
    let s = sign_in(&c, "alice", PW).await;
    let done = c.state.credentials.complete_two_factor(&s.grant.cookie, &codes[0].to_uppercase(), &c1()).await.unwrap();
    assert_eq!(done.step, SignInStep::Complete);
    let s = sign_in(&c, "alice", PW).await;
    assert!(c.state.credentials.complete_two_factor(&s.grant.cookie, &codes[0], &c1()).await.is_err(), "used");
    let ok = c.state.credentials.complete_two_factor(&s.grant.cookie, &codes[1], &c1()).await;
    assert!(ok.is_ok());
    assert_eq!(count_audit(&c, "auth.recovery_code_used").await, 2);
    let a = actor_for_cookie(&c, &ok.unwrap().grant.cookie).await;
    assert_eq!(c.state.twofactor.status(&a).await.unwrap().recovery_codes_left, 8);
    // Regenerating needs the password and replaces the old codes.
    assert!(c.state.twofactor.regenerate_recovery_codes(&a, "wrong password!", &c1()).await.is_err());
    let fresh = c.state.twofactor.regenerate_recovery_codes(&a, PW, &c1()).await.unwrap();
    assert_eq!(fresh.len(), 10);
    let s = sign_in(&c, "alice", PW).await;
    assert!(c.state.credentials.complete_two_factor(&s.grant.cookie, &codes[2], &c1()).await.is_err(), "old code is dead");
    assert!(c.state.credentials.complete_two_factor(&s.grant.cookie, &fresh[0], &c1()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn two_factor_failures_lock_the_account_too(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    enable_2fa(&c, &cookie, &actor).await;
    // A pending session gives five wrong codes, then it is gone (the person starts again).
    let s = sign_in(&c, "alice", PW).await;
    for i in 0..5 {
        let e = c.state.credentials.complete_two_factor(&s.grant.cookie, "111111", &client("198.51.100.1")).await.unwrap_err();
        assert!(matches!(e, Error::Unauthorized(ref m) if m == "That code is not right."), "{i}: {e:?}");
    }
    let e = c.state.credentials.complete_two_factor(&s.grant.cookie, "111111", &client("198.51.100.50")).await.unwrap_err();
    assert!(matches!(&e, Error::Unauthorized(m) if m.contains("timed out")), "the pending sign-in ended: {e:?}");
    // The five failures stay on the account. Five more, on new pending sessions, lock it.
    let s = sign_in(&c, "alice", PW).await;
    for _ in 0..5 {
        let e = c.state.credentials.complete_two_factor(&s.grant.cookie, "111111", &client("198.51.100.1")).await;
        assert!(e.is_err());
    }
    let e = c.state.credentials.complete_two_factor(&s.grant.cookie, "111111", &client("198.51.100.1")).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }) || matches!(&e, Error::Unauthorized(m) if m.contains("timed out")), "{e:?}");
    let e = c.state.credentials.sign_in("alice", PW, &client("198.51.100.1")).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }), "the account is locked: {e:?}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_right_password_does_not_reset_the_count_of_wrong_codes(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    enable_2fa(&c, &cookie, &actor).await;
    // Someone who has the password walks the code space: right password from a fresh address,
    // then a wrong code, again and again. The password stage must not wipe the failure count.
    let mut locked = false;
    for i in 0..12 {
        let from = client(&format!("198.51.100.{}", 10 + i));
        let s = match c.state.credentials.sign_in("alice", PW, &from).await {
            Ok(s) => s,
            Err(Error::RateLimited { .. }) => {
                locked = true;
                break;
            }
            Err(e) => panic!("{i}: {e:?}"),
        };
        assert_eq!(s.step, SignInStep::TwoFactorRequired);
        match c.state.credentials.complete_two_factor(&s.grant.cookie, "111111", &from).await {
            Err(Error::RateLimited { .. }) => {
                locked = true;
                break;
            }
            Err(Error::Unauthorized(_)) => {}
            other => panic!("{i}: {other:?}"),
        }
    }
    assert!(locked, "12 right-password and wrong-code rounds must lock the account");
    // The code stage is locked account-wide, from any address, even with a fresh pending sign-in.
    let from = client("198.51.100.200");
    let s = c.state.credentials.sign_in("alice", PW, &from).await.unwrap();
    let e = c.state.credentials.complete_two_factor(&s.grant.cookie, "111111", &from).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }), "{e:?}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn two_factor_setup_rules(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    assert!(!c.state.twofactor.status(&actor).await.unwrap().enabled);
    // Confirming without starting, or with a wrong code, does nothing.
    assert!(c.state.twofactor.confirm_setup(&actor, &cookie, "123456", &c1()).await.is_err());
    let setup = c.state.twofactor.begin_setup(&actor).await.unwrap();
    assert!(setup.otpauth_uri.starts_with("otpauth://totp/Codehaven:alice?secret="));
    assert!(setup.otpauth_uri.contains(&setup.secret));
    assert!(c.state.twofactor.confirm_setup(&actor, &cookie, "000000", &c1()).await.is_err());
    assert!(!c.state.twofactor.status(&actor).await.unwrap().enabled);
    // The secret is sealed, not stored in the clear.
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT totp_secret_sealed FROM users WHERE username = 'alice'").fetch_one(&c.pool).await.unwrap();
    let raw = totp::base32_decode(&setup.secret).unwrap();
    assert!(!sealed.windows(raw.len()).any(|w| w == raw.as_slice()));
    // Setup is session only.
    assert!(c.state.twofactor.begin_setup(&ch_testkit::fixtures::token_actor(ch_core::UserId(1), &["repo"])).await.is_err());
    assert!(c.state.twofactor.begin_setup(&Actor::Anonymous).await.is_err());

    let (secret2, codes2, cookie2, actor2) = enable_2fa(&c, &cookie, &actor).await;
    assert!(c.state.twofactor.begin_setup(&actor2).await.is_err(), "already on");
    assert_eq!(c.state.twofactor.status(&actor2).await.unwrap().recovery_codes_left, 10);
    // Turning it off needs the password and, as it is the last second factor, a current code (an
    // app code or a recovery code), and ends every session.
    assert!(c.state.twofactor.disable(&actor2, &cookie2, "not the password!", &codes2[0], &c1()).await.is_err());
    assert!(c.state.twofactor.disable(&actor2, &cookie2, PW, "", &c1()).await.is_err(), "no current factor");
    assert!(c.state.twofactor.disable(&actor2, &cookie2, PW, "000000", &c1()).await.is_err(), "a wrong code");
    let _ = &secret2;
    let grant = c.state.twofactor.disable(&actor2, &cookie2, PW, &codes2[0], &c1()).await.unwrap();
    assert!(matches!(actor_for_cookie(&c, &cookie2).await, Actor::Anonymous));
    assert!(matches!(actor_for_cookie(&c, &grant.cookie).await, Actor::User { has_2fa: false, .. }));
    assert_eq!(sign_in(&c, "alice", PW).await.step, SignInStep::Complete);
    assert_eq!(count_audit(&c, "auth.two_factor_off").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn site_admin_power_needs_two_factor(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "boss", true).await;
    let (cookie, actor) = session(&c, "boss").await;
    assert!(!actor.is_site_admin());
    assert!(actor.is_admin_without_2fa());
    let (_, _, _, actor) = enable_2fa(&c, &cookie, &actor).await;
    assert!(actor.is_site_admin());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn sessions_list_revoke_and_sign_out(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let s1 = c.state.credentials.sign_in("alice", PW, &client("203.0.113.1")).await.unwrap();
    let s2 = c.state.credentials.sign_in("alice", PW, &client("203.0.113.2")).await.unwrap();
    let s3 = c.state.credentials.sign_in("alice", PW, &client("203.0.113.3")).await.unwrap();
    let actor = actor_for_cookie(&c, &s1.grant.cookie).await;
    let list = c.state.credentials.sessions(&actor, &s1.grant.cookie).await.unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list.iter().filter(|s| s.current).count(), 1);
    assert!(list.iter().any(|s| s.ip.as_deref() == Some("203.0.113.2") && s.user_agent.as_deref() == Some("test-agent/1.0")));

    let other = list.iter().find(|s| s.ip.as_deref() == Some("203.0.113.2")).unwrap();
    c.state.credentials.revoke_session(&actor, other.id, &c1()).await.unwrap();
    assert!(matches!(actor_for_cookie(&c, &s2.grant.cookie).await, Actor::Anonymous));
    assert!(matches!(c.state.credentials.revoke_session(&actor, other.id, &c1()).await, Err(Error::NotFound)));

    // Someone else's session id is not found.
    let bob = make_user(&c, "bob", false).await;
    let _ = bob;
    let (bob_cookie, bob_actor) = session(&c, "bob").await;
    let bob_sessions = c.state.credentials.sessions(&bob_actor, &bob_cookie).await.unwrap();
    assert!(matches!(c.state.credentials.revoke_session(&actor, bob_sessions[0].id, &c1()).await, Err(Error::NotFound)));

    let n = c.state.credentials.sign_out_others(&actor, &s1.grant.cookie, &c1()).await.unwrap();
    assert_eq!(n, 1);
    assert!(matches!(actor_for_cookie(&c, &s3.grant.cookie).await, Actor::Anonymous));
    assert!(matches!(actor_for_cookie(&c, &s1.grant.cookie).await, Actor::User { .. }));

    c.state.credentials.sign_out(&s1.grant.cookie, &c1()).await.unwrap();
    assert!(matches!(actor_for_cookie(&c, &s1.grant.cookie).await, Actor::Anonymous));
    c.state.credentials.sign_out("garbage", &c1()).await.unwrap();
    assert_eq!(count_audit(&c, "auth.sign_out").await, 1);
    assert!(c.state.credentials.sessions(&Actor::Anonymous, "x").await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn session_expiry_idle_absolute_and_sliding(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let s = sign_in(&c, "alice", PW).await;
    assert!(matches!(actor_for_cookie(&c, &s.grant.cookie).await, Actor::User { .. }));

    // Idle for a week: gone.
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 minute'").execute(&c.pool).await.unwrap();
    assert!(matches!(actor_for_cookie(&c, &s.grant.cookie).await, Actor::Anonymous));

    // Use slides the idle window forward.
    let s = sign_in(&c, "alice", PW).await;
    sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '5 minutes', expires_at = now() + interval '1 hour', created_at = now() - interval '10 days' WHERE token_hash = $1").bind(ch_auth::core::hash_cookie(&s.grant.cookie)).execute(&c.pool).await.unwrap();
    actor_for_cookie(&c, &s.grant.cookie).await;
    let left: f64 = sqlx::query_scalar("SELECT extract(epoch FROM (expires_at - now()))::float8 FROM sessions WHERE token_hash = $1")
        .bind(ch_auth::core::hash_cookie(&s.grant.cookie))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert!(left > 6.9 * 86400.0 && left <= 7.0 * 86400.0 + 5.0, "slid to about 7 days, got {left}");

    // 30 days after sign-in it ends no matter how busy.
    sqlx::query(
        "UPDATE sessions SET created_at = now() - interval '31 days', expires_at = now() + interval '3 days' WHERE token_hash = $1",
    )
    .bind(ch_auth::core::hash_cookie(&s.grant.cookie))
    .execute(&c.pool)
    .await
    .unwrap();
    assert!(matches!(actor_for_cookie(&c, &s.grant.cookie).await, Actor::Anonymous));

    // And near the limit the window is capped at the 30 day mark.
    let s = sign_in(&c, "alice", PW).await;
    sqlx::query(
        "UPDATE sessions SET last_seen_at = now() - interval '5 minutes', created_at = now() - interval '29 days' WHERE token_hash = $1",
    )
    .bind(ch_auth::core::hash_cookie(&s.grant.cookie))
    .execute(&c.pool)
    .await
    .unwrap();
    actor_for_cookie(&c, &s.grant.cookie).await;
    let left: f64 = sqlx::query_scalar("SELECT extract(epoch FROM (expires_at - now()))::float8 FROM sessions WHERE token_hash = $1")
        .bind(ch_auth::core::hash_cookie(&s.grant.cookie))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert!(left < 1.1 * 86400.0, "capped at the absolute limit, got {left}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn malformed_and_unknown_cookies_are_anonymous(pool: PgPool) {
    let c = ctx(pool).await;
    for v in ["", "x", &"a".repeat(64), &"g".repeat(64), &"a".repeat(65), "' OR 1=1 --"] {
        assert!(matches!(c.state.authn.from_session(v, &c1()).await.unwrap(), Actor::Anonymous), "{v:?}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn change_password_rotates_the_session_and_ends_the_others(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let s1 = sign_in(&c, "alice", PW).await;
    let s2 = sign_in(&c, "alice", PW).await;
    let actor = actor_for_cookie(&c, &s1.grant.cookie).await;
    assert!(c.state.credentials.change_password(&actor, &s1.grant.cookie, "not my password!", PW2, &c1()).await.is_err());
    assert!(c.state.credentials.change_password(&actor, &s1.grant.cookie, PW, "short", &c1()).await.is_err());
    assert!(c.state.credentials.change_password(&actor, &s1.grant.cookie, PW, "password1234", &c1()).await.is_err());
    let grant = c.state.credentials.change_password(&actor, &s1.grant.cookie, PW, PW2, &c1()).await.unwrap();
    assert!(matches!(actor_for_cookie(&c, &s1.grant.cookie).await, Actor::Anonymous), "old id is dead");
    assert!(matches!(actor_for_cookie(&c, &s2.grant.cookie).await, Actor::Anonymous), "other sessions end");
    assert!(matches!(actor_for_cookie(&c, &grant.cookie).await, Actor::User { .. }));
    assert!(c.state.credentials.sign_in("alice", PW, &c1()).await.is_err());
    c.state.credentials.sign_in("alice", PW2, &c1()).await.unwrap();
    assert_eq!(count_audit(&c, "auth.password_changed").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn suspending_ends_sessions_and_disables_tokens_at_once(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    let (_, secret) = c.state.accounts.create_token(&actor, id, "ci", vec!["repo".into()], 10, &c1()).await.unwrap();
    let key = c.state.accounts.add_ssh_key(&actor, id, "k", keys::ED1, &c1()).await.unwrap();
    assert!(c.state.authn.from_bearer(&secret, &c1()).await.is_ok());
    c.state.accounts.suspend(&Actor::System, id, &ClientInfo::local()).await.unwrap();
    assert!(matches!(actor_for_cookie(&c, &cookie).await, Actor::Anonymous));
    assert!(c.state.authn.from_bearer(&secret, &c1()).await.is_err());
    assert!(c.state.authn.from_basic("alice", &secret, &c1()).await.is_err());
    assert!(c.state.authn.from_ssh_key(&key.fingerprint).await.unwrap().is_none());
    assert!(c.state.credentials.sign_in("alice", PW, &c1()).await.is_err());
    // Unsuspending restores them.
    let (_, admin) = site_admin(&c, "root1").await;
    c.state.directory.unsuspend(&admin, id, &c1()).await.unwrap();
    assert!(c.state.authn.from_bearer(&secret, &c1()).await.is_ok());
    c.state.credentials.sign_in("alice", PW, &c1()).await.unwrap();
    // Nobody can suspend themselves.
    assert!(c.state.accounts.suspend(&admin, admin.user_id().unwrap(), &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn sign_in_history_shows_successes_and_failures(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let _ = c.state.credentials.sign_in("alice", "wrong wrong wrong", &client("198.51.100.5")).await;
    let (_, actor) = session(&c, "alice").await;
    let h = c.state.credentials.sign_in_history(&actor, 10).await.unwrap();
    assert!(h.iter().any(|r| !r.success && r.ip.as_deref() == Some("198.51.100.5") && r.action == "auth.sign_in_failed"));
    assert!(h.iter().any(|r| r.success && r.action == "auth.sign_in"));
    // Newest first, and only this account's.
    make_user(&c, "bob", false).await;
    let (_, bob) = session(&c, "bob").await;
    let hb = c.state.credentials.sign_in_history(&bob, 10).await.unwrap();
    assert!(hb.iter().all(|r| r.ip.as_deref() != Some("198.51.100.5")));
}
