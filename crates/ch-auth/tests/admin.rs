//! The admin side: one-time links, `create-user`, directory, password policy, sweep, and the
//! rule that secrets never reach the audit log.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::identity::Profile;
use ch_core::model::*;
use ch_core::{Actor, Error};
use sqlx::PgPool;

fn sys() -> (Actor, ClientInfo) {
    (Actor::System, ClientInfo::local())
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_first_admin_is_made_on_the_server_with_a_one_time_link(pool: PgPool) {
    let c = ctx(pool).await;
    let (sys, local) = sys();
    assert_eq!(c.state.accounts.count_users().await.unwrap(), 0);
    let u = c
        .state
        .accounts
        .create_user(
            &sys,
            NewUser { username: "alice".into(), email: "alice@example.org".into(), display_name: None, is_admin: true, can_run_ci: true },
            &local,
        )
        .await
        .unwrap();
    assert!(u.is_admin && u.can_run_ci && !u.has_password && !u.has_2fa);
    assert_eq!(u.invited_by, None);
    // No password yet, so nothing signs in.
    assert!(c.state.credentials.sign_in("alice", PW, &c1()).await.is_err());

    let link = c.state.accounts.issue_link(&sys, u.id, LinkPurpose::SetPassword, 60, &local).await.unwrap();
    assert!(link.path.starts_with("/password/cho_"), "{}", link.path);
    assert!((59..=60).contains(&(link.expires_at - chrono::Utc::now()).num_minutes()));
    let peek = c.state.credentials.peek_link(&link.token).await.unwrap();
    assert_eq!((peek.username.as_str(), peek.purpose), ("alice", LinkPurpose::SetPassword));

    // The password must pass the policy. A bad one does not use the link up.
    assert!(matches!(c.state.credentials.redeem_link(&link.token, "short", &c1()).await, Err(Error::Validation(_))));
    assert!(c.state.credentials.redeem_link(&link.token, "alice-alice-alice-1", &c1()).await.is_err());
    c.state.credentials.peek_link(&link.token).await.unwrap();
    let u = c.state.credentials.redeem_link(&link.token, PW, &c1()).await.unwrap();
    assert!(u.has_password);
    // Single use.
    assert!(matches!(c.state.credentials.peek_link(&link.token).await, Err(Error::NotFound)));
    assert!(matches!(c.state.credentials.redeem_link(&link.token, PW2, &c1()).await, Err(Error::NotFound)));
    let s = sign_in(&c, "alice", PW).await;
    assert_eq!(s.user.id, u.id);
    // An admin has no site admin power until two-factor is on.
    let a = actor_for_cookie(&c, &s.grant.cookie).await;
    assert!(!a.is_site_admin() && a.is_admin_without_2fa());

    for want in ["user.created", "user.admin_granted", "auth.password_set"] {
        assert!(count_audit(&c, want).await >= 1, "{want}");
    }
    let who: String =
        sqlx::query_scalar("SELECT actor_label FROM audit_events WHERE action = 'user.created'").fetch_one(&c.pool).await.unwrap();
    assert_eq!(who, "System (cli)");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn links_expire_are_replaced_and_are_not_guessable(pool: PgPool) {
    let c = ctx(pool).await;
    let (sys, local) = sys();
    let id = make_user(&c, "alice", false).await;
    let l1 = c.state.accounts.issue_link(&sys, id, LinkPurpose::ResetPassword, 60, &local).await.unwrap();
    let l2 = c.state.accounts.issue_link(&sys, id, LinkPurpose::ResetPassword, 60, &local).await.unwrap();
    assert!(matches!(c.state.credentials.peek_link(&l1.token).await, Err(Error::NotFound)), "a new link replaces the old one");
    c.state.credentials.peek_link(&l2.token).await.unwrap();
    sqlx::query("UPDATE one_time_links SET expires_at = now() - interval '1 second', created_at = now() - interval '2 hours'")
        .execute(&c.pool)
        .await
        .unwrap();
    assert!(matches!(c.state.credentials.peek_link(&l2.token).await, Err(Error::NotFound)), "expired");
    for bad in ["", "cho_", "cho_short", &format!("cho_{}", "z".repeat(32)), &format!("chi_{}", "z".repeat(32))] {
        assert!(matches!(c.state.credentials.peek_link(bad).await, Err(Error::NotFound)), "{bad}");
        assert!(matches!(c.state.credentials.redeem_link(bad, PW2, &c1()).await, Err(Error::NotFound)), "{bad}");
    }
    // Only the CLI and site admins make links.
    let (_, plain) = session(&c, "alice").await;
    assert!(c.state.accounts.issue_link(&plain, id, LinkPurpose::ResetPassword, 5, &local).await.is_err());
    assert!(c.state.accounts.issue_link(&Actor::Anonymous, id, LinkPurpose::ResetPassword, 5, &local).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reset_signs_out_everywhere_clears_lockout_and_keeps_two_factor(pool: PgPool) {
    let c = ctx(pool).await;
    let (sys, local) = sys();
    let id = make_user(&c, "alice", false).await;
    let (cookie, actor) = session(&c, "alice").await;
    enable_2fa(&c, &cookie, &actor).await;
    for _ in 0..10 {
        let _ = c.state.credentials.sign_in("alice", "wrong password x", &client("198.51.100.1")).await;
    }
    assert!(matches!(c.state.credentials.sign_in("alice", PW, &client("198.51.100.1")).await, Err(Error::RateLimited { .. })));

    c.state.accounts.sign_out_everywhere(&sys, id, &local).await.unwrap();
    let link = c.state.accounts.issue_link(&sys, id, LinkPurpose::ResetPassword, 60, &local).await.unwrap();
    c.state.credentials.redeem_link(&link.token, PW2, &c1()).await.unwrap();
    // The lock is gone, the new password works, and two-factor is still required: a reset link
    // is not a way around it.
    let s = c.state.credentials.sign_in("alice", PW2, &client("198.51.100.201")).await.unwrap();
    assert_eq!(s.step, ch_core::identity::SignInStep::TwoFactorRequired);
    assert!(count_audit(&c, "auth.password_reset").await >= 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn set_password_is_the_cli_only_and_follows_the_policy(pool: PgPool) {
    let c = ctx(pool).await;
    let (sys, local) = sys();
    let id = make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;
    assert!(c.state.accounts.set_password(&me, id, PW2, &local).await.is_err());
    assert!(c.state.accounts.set_password(&Actor::Anonymous, id, PW2, &local).await.is_err());
    assert!(c.state.accounts.set_password(&sys, id, "short", &local).await.is_err());
    c.state.accounts.set_password(&sys, id, PW2, &local).await.unwrap();
    assert!(c.state.credentials.sign_in("alice", PW, &c1()).await.is_err());
    c.state.credentials.sign_in("alice", PW2, &c1()).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn create_user_validation(pool: PgPool) {
    let c = ctx(pool).await;
    let (sys, local) = sys();
    let mk = |u: &str, e: &str| NewUser { username: u.into(), email: e.into(), display_name: None, is_admin: false, can_run_ci: false };
    c.state.accounts.create_user(&sys, mk("alice", "a@example.org"), &local).await.unwrap();
    for (u, e) in [
        ("alice", "b@example.org"),
        ("ALICE", "c@example.org"),
        ("bob", "a@example.org"),
        ("bob", "A@EXAMPLE.ORG"),
        ("bad name", "d@example.org"),
        ("ok", "not-an-email"),
        ("login", "e@example.org"),
    ] {
        assert!(matches!(c.state.accounts.create_user(&sys, mk(u, e), &local).await, Err(Error::Validation(_))), "{u}/{e}");
    }
    assert_eq!(c.state.accounts.count_users().await.unwrap(), 1);
    // Orgs do not count as people.
    c.env.org("haven").await;
    assert_eq!(c.state.accounts.count_users().await.unwrap(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn directory_admin_flag_and_the_last_admin(pool: PgPool) {
    let c = ctx(pool).await;
    let (admin_id, admin) = site_admin(&c, "boss").await;
    let bob = make_user(&c, "bob", false).await;
    let (_, bob_a) = session(&c, "bob").await;
    assert!(c.state.directory.set_admin(&bob_a, bob, true, &c1()).await.is_err(), "not for plain users");
    assert!(matches!(c.state.directory.set_admin(&admin, admin_id, false, &c1()).await, Err(Error::Conflict(_))), "the last admin stays");
    let (bob_cookie, _) = session(&c, "bob").await;
    c.state.directory.set_admin(&admin, bob, true, &c1()).await.unwrap();
    // Privilege changed, so Bob's sessions ended.
    assert!(matches!(actor_for_cookie(&c, &bob_cookie).await, Actor::Anonymous));
    c.state.directory.set_admin(&admin, admin_id, false, &c1()).await.unwrap();
    assert!(!c.state.accounts.get_user(admin_id).await.unwrap().is_admin);
    assert_eq!(count_audit(&c, "user.admin_granted").await, 3, "boss at creation, then bob on, boss off");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn directory_listing_is_for_site_admins(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "boss").await;
    for n in ["alice", "alina", "bob"] {
        make_user(&c, n, false).await;
    }
    let all = c.state.directory.list_users(&admin, Default::default()).await.unwrap();
    assert_eq!(all.len(), 4);
    let q = c
        .state
        .directory
        .list_users(&admin, ch_core::identity::UserListFilter { query: Some("ali".into()), page: 1, per_page: 10 })
        .await
        .unwrap();
    assert_eq!(q.len(), 2);
    let paged =
        c.state.directory.list_users(&admin, ch_core::identity::UserListFilter { query: None, page: 2, per_page: 3 }).await.unwrap();
    assert_eq!(paged.len(), 1);
    let wild = c
        .state
        .directory
        .list_users(&admin, ch_core::identity::UserListFilter { query: Some("%".into()), page: 1, per_page: 10 })
        .await
        .unwrap();
    assert!(wild.is_empty(), "wildcards are escaped");
    let (_, plain) = session(&c, "bob").await;
    assert!(c.state.directory.list_users(&plain, Default::default()).await.is_err());
    assert!(c.state.directory.list_users(&Actor::Anonymous, Default::default()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn emails_and_profile(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let bob = make_user(&c, "bob", false).await;
    let (_, me) = session(&c, "alice").await;
    let e = c.state.directory.add_email(&me, id, "second@example.org", &c1()).await.unwrap();
    assert!(!e.verified && !e.primary);
    assert!(c.state.directory.add_email(&me, id, "second@example.org", &c1()).await.is_err(), "duplicate");
    assert!(c.state.directory.add_email(&me, id, "alice@example.org", &c1()).await.is_err(), "already his");
    assert!(c.state.directory.add_email(&me, id, "bob@example.org", &c1()).await.is_err(), "someone else's");
    assert!(c.state.directory.add_email(&me, bob, "x@example.org", &c1()).await.is_err(), "not for others");
    assert!(c.state.directory.set_primary_email(&me, id, "second@example.org", &c1()).await.is_err(), "unverified cannot be primary");
    assert!(matches!(c.state.directory.remove_email(&me, id, "alice@example.org", &c1()).await, Err(Error::Conflict(_))), "primary stays");
    c.state.directory.remove_email(&me, id, "SECOND@example.org", &c1()).await.unwrap();
    assert_eq!(c.state.directory.emails(&me, id).await.unwrap().len(), 1);
    assert!(c.state.directory.emails(&me, bob).await.is_err());

    let u = c
        .state
        .directory
        .update_profile(&me, id, Profile { display_name: Some("  Alice C  ".into()), description: Some("hello".into()) }, &c1())
        .await
        .unwrap();
    assert_eq!(u.display_name, "Alice C");
    assert!(
        c.state.directory.update_profile(&me, id, Profile { display_name: Some("x".repeat(101)), description: None }, &c1()).await.is_err()
    );
    assert!(c.state.directory.update_profile(&me, bob, Profile::default(), &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn runner_and_ci_flags_are_admin_only(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "boss").await;
    let bob = make_user(&c, "bob", false).await;
    let (_, bob_a) = session(&c, "bob").await;
    assert!(c.state.accounts.set_can_run_ci(&bob_a, bob, true, &c1()).await.is_err(), "no self service");
    c.state.accounts.set_can_run_ci(&admin, bob, true, &c1()).await.unwrap();
    assert!(c.state.accounts.get_user(bob).await.unwrap().can_run_ci);
    assert!(matches!(c.state.accounts.set_can_run_ci(&admin, ch_core::UserId(9999), true, &c1()).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn passwords_secrets_and_tokens_never_reach_the_audit_log(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (cookie, me) = session(&c, "alice").await;
    let (_, secret) = c.state.accounts.create_token(&me, id, "t", vec!["repo".into()], 5, &c1()).await.unwrap();
    let _ = c.state.credentials.sign_in("alice", "my wrong password 12", &c1()).await;
    let (tfa_secret, codes, _, _) = enable_2fa(&c, &cookie, &me).await;
    let inv = c.state.invites.create(&Actor::System, new_invite(None), &ClientInfo::local()).await.unwrap();
    let link = c.state.accounts.issue_link(&Actor::System, id, LinkPurpose::ResetPassword, 10, &ClientInfo::local()).await.unwrap();

    let everything: String = sqlx::query_scalar("SELECT coalesce(string_agg(row_to_json(a)::text, E'\\n'), '') FROM audit_events a")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    let needles = [
        PW.to_string(),
        "my wrong password 12".to_string(),
        secret.clone(),
        secret[4..].to_string(),
        cookie.clone(),
        inv.token.clone(),
        inv.token[4..].to_string(),
        link.token.clone(),
        link.token[4..].to_string(),
        ch_auth::totp::base32_encode(&tfa_secret),
        codes[0].clone(),
        ch_auth::totp::normalize_recovery_code(&codes[0]),
    ];
    for n in needles {
        assert!(!everything.contains(&n), "the audit log contains {n:?}");
    }
    // And the lines that do exist say who and from where.
    let row: (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT actor_label, host(client_ip), user_agent FROM audit_events WHERE action = 'token.created'")
            .fetch_one(&c.pool)
            .await
            .unwrap();
    assert_eq!(row.1.as_deref(), Some("203.0.113.10"));
    assert_eq!(row.2.as_deref(), Some("test-agent/1.0"));
    assert!(row.0.starts_with("user:"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn sweep_removes_only_what_is_stale(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "boss").await;
    make_user(&c, "bob", false).await;
    let keep = sign_in(&c, "bob", PW).await;
    let old = sign_in(&c, "bob", PW).await;
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 day' WHERE token_hash = $1")
        .bind(ch_auth::core::hash_cookie(&old.grant.cookie))
        .execute(&c.pool)
        .await
        .unwrap();
    // One expired invite (kept 90 days for the admin list), one ancient one, a used one.
    let fresh_expired = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    let ancient = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    let used = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    c.state.invites.accept(&used.token, new_account("friend"), &c1()).await.unwrap();
    sqlx::query("UPDATE invites SET created_at = now() - interval '200 days', expires_at = now() - interval '100 days' WHERE id = $1")
        .bind(ancient.invite.id.0)
        .execute(&c.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE invites SET created_at = now() - interval '10 days', expires_at = now() - interval '1 day' WHERE id = $1")
        .bind(fresh_expired.invite.id.0)
        .execute(&c.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO login_failures (key, fail_count, last_failure_at) VALUES ('name:old', 3, now() - interval '3 days'), ('name:new', 3, now())").execute(&c.pool).await.unwrap();
    sqlx::query("INSERT INTO audit_events (at, actor_kind, actor_label, action, outcome) VALUES (now() - interval '2 years', 'system', 'x', 'old.event', 'success')").execute(&c.pool).await.unwrap();

    let s = ch_auth::sweep::sweep(&c.pool).await.unwrap();
    assert_eq!(s.sessions, 1);
    assert_eq!(s.lockouts, 1);
    assert_eq!(s.invites, 1);
    assert_eq!(s.audit, 1);
    assert!(matches!(actor_for_cookie(&c, &keep.grant.cookie).await, Actor::User { .. }));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM invites").fetch_one(&c.pool).await.unwrap();
    assert_eq!(left, 2);
    assert_eq!(ch_auth::sweep::sweep(&c.pool).await.unwrap(), ch_auth::sweep::Swept::default(), "idempotent");
}
