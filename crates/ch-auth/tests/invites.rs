//! Invite-only access (ARCHITECTURE.md section 7.4).
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::model::*;
use ch_core::{Actor, Error};
use sqlx::PgPool;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn there_is_no_way_to_create_an_account_without_an_invite(pool: PgPool) {
    let c = ctx(pool).await;
    assert_eq!(c.state.accounts.count_users().await.unwrap(), 0);
    // Not as anonymous, not as a user, not as a site admin: only the CLI makes accounts.
    let new = NewUser { username: "mallory".into(), email: "m@example.org".into(), display_name: None, is_admin: false, can_run_ci: false };
    assert!(c.state.accounts.create_user(&Actor::Anonymous, new.clone(), &c1()).await.is_err());
    let (_, admin) = site_admin(&c, "root1").await;
    assert!(c.state.accounts.create_user(&admin, new, &c1()).await.is_err());
    assert_eq!(c.state.accounts.count_users().await.unwrap(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn invite_flow_end_to_end(pool: PgPool) {
    let c = ctx(pool).await;
    let (admin_id, admin) = site_admin(&c, "alice").await;
    let created = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    assert!(created.url.starts_with("https://test.example/invite/chi_"), "{}", created.url);
    assert_eq!(created.token.len(), 36);
    // Only the hash is stored.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM invites WHERE token_hash = $1")
        .bind(ch_db::tokens::hash_token(&created.token))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
    let raw: i64 = sqlx::query_scalar("SELECT count(*) FROM invites WHERE note = $1 OR email = $1")
        .bind(&created.token)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(raw, 0);

    let peek = c.state.invites.peek(&created.token).await.unwrap();
    assert_eq!(peek.invited_by.as_deref(), Some("alice"));

    let user = c.state.invites.accept(&created.token, new_account("friend"), &c1()).await.unwrap();
    assert_eq!(user.username, "friend");
    assert_eq!(user.invited_by, Some(admin_id));
    assert!(!user.is_admin);
    assert!(user.has_password);
    // The new account can sign in with the password it chose.
    let s = sign_in(&c, "friend", PW).await;
    assert_eq!(s.user.username, "friend");

    // The link is spent.
    assert!(matches!(c.state.invites.peek(&created.token).await, Err(Error::NotFound)));
    assert!(matches!(c.state.invites.accept(&created.token, new_account("second"), &c1()).await, Err(Error::NotFound)));
    let list = c.state.invites.list(&admin).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].used_by, Some(user.id));
    assert_eq!(list[0].status(chrono::Utc::now()), InviteStatus::Used);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_links_all_look_the_same(pool: PgPool) {
    let c = ctx(pool.clone()).await;
    let (_, admin) = site_admin(&c, "alice").await;
    let used = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    c.state.invites.accept(&used.token, new_account("one"), &c1()).await.unwrap();
    let revoked = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    c.state.invites.revoke(&admin, revoked.invite.id, &c1()).await.unwrap();
    let expired = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    sqlx::query("UPDATE invites SET expires_at = now() - interval '1 second', created_at = now() - interval '2 days' WHERE id = $1")
        .bind(expired.invite.id.0)
        .execute(&pool)
        .await
        .unwrap();

    let made_up = format!("chi_{}", "x".repeat(32));
    for (name, token) in [
        ("used", used.token.clone()),
        ("revoked", revoked.token.clone()),
        ("expired", expired.token.clone()),
        ("made up", made_up),
        ("garbage", "nope".to_string()),
        ("empty", String::new()),
    ] {
        let peek = c.state.invites.peek(&token).await;
        assert!(matches!(peek, Err(Error::NotFound)), "{name}: peek gave {peek:?}");
        let accept = c.state.invites.accept(&token, new_account("mallory"), &c1()).await;
        assert!(matches!(accept, Err(Error::NotFound)), "{name}: accept gave {accept:?}");
    }
    // Nothing was created by any of that, and each refusal is in the audit log.
    assert_eq!(c.state.accounts.count_users().await.unwrap(), 2);
    assert!(count_audit(&c, "invite.refused").await >= 6);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn twenty_racing_accepts_make_exactly_one_account(pool: PgPool) {
    let c = std::sync::Arc::new(ctx(pool).await);
    let (_, admin) = site_admin(&c, "alice").await;
    let created = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..20 {
        let c = c.clone();
        let token = created.token.clone();
        tasks.push(tokio::spawn(async move {
            c.state.invites.accept(&token, new_account(&format!("racer{i}")), &client(&format!("198.51.100.{i}"))).await
        }));
    }
    let mut ok = 0;
    let mut not_found = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(Error::NotFound) => not_found += 1,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!(ok, 1, "one winner");
    assert_eq!(not_found, 19);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username LIKE 'racer%'").fetch_one(&c.pool).await.unwrap();
    assert_eq!(users, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_mistake_on_the_form_does_not_burn_the_invite(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "alice").await;
    make_user(&c, "taken", false).await;
    let created = c.state.invites.create(&admin, new_invite(Some("friend@example.org")), &c1()).await.unwrap();

    let mut weak = new_account("friend");
    weak.password = "password1234".into();
    assert!(matches!(c.state.invites.accept(&created.token, weak, &c1()).await, Err(Error::Validation(_))));
    assert!(
        matches!(c.state.invites.accept(&created.token, new_account("taken"), &c1()).await, Err(Error::Validation(_))),
        "username already used"
    );
    let mut wrong_email = new_account("friend");
    wrong_email.email = "someone-else@example.org".into();
    assert!(
        matches!(c.state.invites.accept(&created.token, wrong_email, &c1()).await, Err(Error::Validation(_))),
        "invite names an address"
    );
    assert!(
        matches!(c.state.invites.accept(&created.token, new_account("login"), &c1()).await, Err(Error::Validation(_))),
        "reserved name"
    );

    // Still pending, and the right form works.
    c.state.invites.peek(&created.token).await.unwrap();
    let mut right = new_account("friend");
    right.email = "FRIEND@example.org".into();
    let u = c.state.invites.accept(&created.token, right, &c1()).await.unwrap();
    assert_eq!(u.username, "friend");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn invite_options_and_limits(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "alice").await;
    let org = c.env.org("haven").await;

    let mut req = new_invite(None);
    req.grants_admin = true;
    req.grants_ci = true;
    req.org_id = Some(ch_core::OrgId(org.0));
    req.note = Some("for Sam".into());
    req.expires_in_hours = Some(1);
    let created = c.state.invites.create(&admin, req, &c1()).await.unwrap();
    assert!(created.invite.expires_at < chrono::Utc::now() + chrono::Duration::minutes(61));
    let u = c.state.invites.accept(&created.token, new_account("sam"), &c1()).await.unwrap();
    assert!(u.is_admin && u.can_run_ci);
    let member: i64 = sqlx::query_scalar("SELECT count(*) FROM org_members WHERE org_id = $1 AND user_id = $2 AND role = 'member'")
        .bind(org.0)
        .bind(u.id.0)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(member, 1);

    for hours in [0u32, 721, 10_000] {
        let mut r = new_invite(None);
        r.expires_in_hours = Some(hours);
        assert!(matches!(c.state.invites.create(&admin, r, &c1()).await, Err(Error::Validation(_))), "{hours}h");
    }
    let mut r = new_invite(None);
    r.expires_in_hours = Some(720);
    c.state.invites.create(&admin, r, &c1()).await.unwrap();
    let mut r = new_invite(None);
    r.org_id = Some(ch_core::OrgId(99_999));
    assert!(c.state.invites.create(&admin, r, &c1()).await.is_err(), "no such org");
    assert!(c.state.invites.create(&admin, new_invite(Some("not an email")), &c1()).await.is_err());
    // Default expiry is 7 days.
    let d = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    let days = (d.invite.expires_at - d.invite.created_at).num_hours();
    assert!((167..=168).contains(&days), "7 days, got {days}h");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_site_admins_with_two_factor_can_invite(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "plain", false).await;
    let (_, plain) = session(&c, "plain").await;
    make_user(&c, "boss", true).await;
    let (_, boss_no_2fa) = session(&c, "boss").await;
    assert!(boss_no_2fa.is_admin_without_2fa());

    for who in [&Actor::Anonymous, &plain, &boss_no_2fa] {
        assert!(c.state.invites.create(who, new_invite(None), &c1()).await.is_err());
        assert!(c.state.invites.list(who).await.is_err());
    }
    // The CLI (System) can.
    let cli = c.state.invites.create(&Actor::System, new_invite(None), &ClientInfo::local()).await.unwrap();
    assert_eq!(cli.invite.created_by, None);
    let via: String =
        sqlx::query_scalar("SELECT created_via FROM invites WHERE id = $1").bind(cli.invite.id.0).fetch_one(&c.pool).await.unwrap();
    assert_eq!(via, "cli");
    // A token cannot, even a token of an admin.
    let tok = ch_testkit::fixtures::token_actor(ch_core::UserId(1), &["repo"]);
    assert!(c.state.invites.create(&tok, new_invite(None), &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_owners_cannot_invite_unless_the_site_admin_turns_it_on(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "owner", false).await;
    let (_, owner) = session(&c, "owner").await;
    let org = c.state.orgs.create(&owner, "haven", "", &c1()).await.unwrap();
    let mut req = new_invite(None);
    req.org_id = Some(org.id);
    assert!(c.state.invites.create(&owner, req, &c1()).await.is_err(), "off by default");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_owner_invites_into_their_org_when_enabled(pool: PgPool) {
    let c = ctx_with(pool, |cfg| cfg.invites.org_owners_can_invite = true).await;
    make_user(&c, "owner", false).await;
    make_user(&c, "outsider", false).await;
    let (_, owner) = session(&c, "owner").await;
    let (_, outsider) = session(&c, "outsider").await;
    let org = c.state.orgs.create(&owner, "haven", "", &c1()).await.unwrap();
    let mut req = new_invite(None);
    req.org_id = Some(org.id);
    let created = c.state.invites.create(&owner, req.clone(), &c1()).await.unwrap();
    assert!(c.state.invites.create(&outsider, req.clone(), &c1()).await.is_err(), "not an owner of that org");
    let mut admin_req = req.clone();
    admin_req.grants_admin = true;
    assert!(c.state.invites.create(&owner, admin_req, &c1()).await.is_err(), "owners cannot grant admin");
    assert!(c.state.invites.create(&owner, new_invite(None), &c1()).await.is_err(), "must be into their org");
    let u = c.state.invites.accept(&created.token, new_account("newbie"), &c1()).await.unwrap();
    let m: i64 = sqlx::query_scalar("SELECT count(*) FROM org_members WHERE org_id = $1 AND user_id = $2")
        .bind(org.id.0)
        .bind(u.id.0)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(m, 1);
    assert_eq!(c.state.invites.list(&owner).await.unwrap().len(), 1, "an owner sees only their own invites");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn revoke_and_audit(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "alice").await;
    let i = c.state.invites.create(&admin, new_invite(None), &c1()).await.unwrap();
    c.state.invites.revoke(&admin, i.invite.id, &c1()).await.unwrap();
    assert!(matches!(c.state.invites.revoke(&admin, i.invite.id, &c1()).await, Err(Error::Conflict(_))), "already revoked");
    assert!(matches!(c.state.invites.revoke(&admin, ch_core::InviteId(9999), &c1()).await, Err(Error::NotFound)));
    let actions = audit_actions(&c).await;
    for want in ["invite.created", "invite.revoked"] {
        assert!(actions.iter().any(|a| a == want), "missing {want} in {actions:?}");
    }
    // The token itself never reaches the audit log.
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE details::text LIKE '%' || $1 || '%'")
        .bind(&i.token)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(leaked, 0);
}
