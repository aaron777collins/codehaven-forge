//! The permission model against a real database (ARCHITECTURE.md section 7.2).
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::{Actor, AuthVia, Error, JobPerms, Permission, RunnerId, Scopes, UserId};
use ch_testkit::fixtures::{token_actor, user_actor};
use sqlx::PgPool;

fn user(id: UserId) -> Actor {
    user_actor(id)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn anonymous_gets_nothing_anywhere_even_on_public_repos(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let private = c.env.repo(alice, "private").await;
    let public = c.env.public_repo(alice, "public").await;
    for repo in [private, public] {
        assert_eq!(c.state.authz.repo_permission(&Actor::Anonymous, repo).await.unwrap(), Permission::None);
        assert!(matches!(c.state.authz.require(&Actor::Anonymous, repo, Permission::Read).await, Err(Error::NotFound)));
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn personal_repo_rows(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let bob = c.env.user("bob").await;
    let carol = c.env.user("carol").await;
    let private = c.env.repo(alice, "private").await;
    let public = c.env.public_repo(alice, "public").await;
    c.env.collaborator(private, bob, "write").await;
    c.env.collaborator(public, carol, "triage").await;
    let p = |u: UserId, r| {
        let c = &c;
        async move { c.state.authz.repo_permission(&user(u), r).await.unwrap() }
    };
    assert_eq!(p(alice, private).await, Permission::Admin, "owner");
    assert_eq!(p(bob, private).await, Permission::Write, "collaborator");
    assert_eq!(p(carol, private).await, Permission::None, "outsider, private");
    assert_eq!(p(carol, public).await, Permission::Triage, "collaborator beats public");
    assert_eq!(p(bob, public).await, Permission::Read, "signed in sees public");
    assert_eq!(p(bob, ch_core::RepoId(999_999)).await, Permission::None, "no such repo");

    // require: NotFound when unreadable, Forbidden when readable but short.
    assert!(matches!(c.state.authz.require(&user(carol), private, Permission::Read).await, Err(Error::NotFound)));
    assert!(matches!(c.state.authz.require(&user(bob), public, Permission::Write).await, Err(Error::Forbidden(_))));
    assert!(c.state.authz.require(&user(bob), private, Permission::Write).await.is_ok());
    assert!(matches!(c.state.authz.require(&user(bob), private, Permission::Admin).await, Err(Error::Forbidden(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn site_admin_needs_two_factor_to_reach_other_peoples_repos(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let private = c.env.repo(alice, "private").await;
    make_user(&c, "boss", true).await;
    let (cookie, no_2fa) = session(&c, "boss").await;
    assert_eq!(c.state.authz.repo_permission(&no_2fa, private).await.unwrap(), Permission::None);
    let (_, _, _, with_2fa) = enable_2fa(&c, &cookie, &no_2fa).await;
    assert_eq!(c.state.authz.repo_permission(&with_2fa, private).await.unwrap(), Permission::Admin);
    // The CLI too, and a Basic login of the same admin does not count.
    assert_eq!(c.state.authz.repo_permission(&Actor::System, private).await.unwrap(), Permission::Admin);
    let basic = Actor::User { id: with_2fa.user_id().unwrap(), is_admin: true, has_2fa: true, via: AuthVia::Basic };
    assert_eq!(c.state.authz.repo_permission(&basic, private).await.unwrap(), Permission::None);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_rows_owner_base_teams(pool: PgPool) {
    let c = ctx(pool).await;
    let owner = make_user(&c, "owner", false).await;
    let member = make_user(&c, "member", false).await;
    let teamie = make_user(&c, "teamie", false).await;
    let stranger = make_user(&c, "stranger", false).await;
    let (_, owner_a) = session(&c, "owner").await;
    let org = c.state.orgs.create(&owner_a, "haven", "", &c1()).await.unwrap();
    c.state.orgs.set_member(&owner_a, org.id, member, ch_core::services::OrgRole::Member, &c1()).await.unwrap();
    c.state.orgs.set_member(&owner_a, org.id, teamie, ch_core::services::OrgRole::Member, &c1()).await.unwrap();
    let repo = c.env.repo(ch_core::UserId(org.id.0), "tools").await;

    let perm = |u: UserId| {
        let c = &c;
        async move { c.state.authz.repo_permission(&user(u), repo).await.unwrap() }
    };
    assert_eq!(perm(owner).await, Permission::Admin, "org owner");
    assert_eq!(perm(member).await, Permission::Read, "default base permission is read");
    assert_eq!(perm(stranger).await, Permission::None);

    // Base permission changes follow.
    c.state.orgs.update(&owner_a, org.id, None, None, Some(Permission::Write), &c1()).await.unwrap();
    assert_eq!(perm(member).await, Permission::Write);
    c.state.orgs.update(&owner_a, org.id, None, None, Some(Permission::None), &c1()).await.unwrap();
    assert_eq!(perm(member).await, Permission::None, "a base of none gives members nothing");
    assert_eq!(perm(stranger).await, Permission::None, "and never reaches non-members");

    // A team grants more than the base, to its members only.
    let team = c.state.orgs.create_team(&owner_a, org.id, "maintainers", "", Permission::Maintain, &c1()).await.unwrap();
    c.state.orgs.add_team_member(&owner_a, team.id, teamie, &c1()).await.unwrap();
    c.state.orgs.set_team_repo(&owner_a, team.id, repo, Permission::Maintain, &c1()).await.unwrap();
    assert_eq!(perm(teamie).await, Permission::Maintain);
    assert_eq!(perm(member).await, Permission::None);
    // Two teams take the maximum.
    let t2 = c.state.orgs.create_team(&owner_a, org.id, "readers", "", Permission::Read, &c1()).await.unwrap();
    c.state.orgs.add_team_member(&owner_a, t2.id, teamie, &c1()).await.unwrap();
    c.state.orgs.set_team_repo(&owner_a, t2.id, repo, Permission::Read, &c1()).await.unwrap();
    assert_eq!(perm(teamie).await, Permission::Maintain);
    // Leaving the org removes the team access.
    c.state.orgs.remove_member(&owner_a, org.id, teamie, &c1()).await.unwrap();
    assert_eq!(perm(teamie).await, Permission::None);
    // The org's repo is not "personal": the org's own id is not a user who owns it.
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn tokens_are_capped_by_scope(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let repo = c.env.repo(alice, "mine").await;
    let public = c.env.public_repo(c.env.user("zed").await, "pub").await;
    let perm = |scopes: &[&str], r| {
        let c = &c;
        let a = token_actor(alice, scopes);
        async move { c.state.authz.repo_permission(&a, r).await.unwrap() }
    };
    assert_eq!(perm(&["repo"], repo).await, Permission::Admin);
    assert_eq!(perm(&["repo:read"], repo).await, Permission::Read, "a read token never writes");
    assert_eq!(perm(&["user"], repo).await, Permission::None, "no repo scope, no repo access");
    assert_eq!(perm(&["repo:read"], public).await, Permission::Read);
    assert_eq!(perm(&["user", "read:org"], public).await, Permission::None);
    let _ = Scopes::default();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn archived_repos_refuse_writes_with_a_clear_message(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let repo = c.env.repo(alice, "old").await;
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1").bind(repo.0).execute(&c.pool).await.unwrap();
    let a = user(alice);
    assert!(c.state.authz.require(&a, repo, Permission::Read).await.is_ok());
    for need in [Permission::Triage, Permission::Write, Permission::Maintain] {
        match c.state.authz.require(&a, repo, need).await {
            Err(Error::Forbidden(m)) => assert!(m.contains("archived"), "{m}"),
            other => panic!("{need:?}: {other:?}"),
        }
    }
    // Admin level actions such as unarchiving stay possible.
    assert!(c.state.authz.require(&a, repo, Permission::Admin).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn jobs_runners_and_the_system(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let mine = c.env.repo(alice, "mine").await;
    let other = c.env.public_repo(alice, "other").await;
    let job = Actor::Job { job_id: ch_core::JobId(1), repo_id: mine, perms: JobPerms { contents_read: true, write_checks: true } };
    assert_eq!(c.state.authz.repo_permission(&job, mine).await.unwrap(), Permission::Read);
    assert_eq!(
        c.state.authz.repo_permission(&job, other).await.unwrap(),
        Permission::None,
        "a job reads its own repo only, even when another is public"
    );
    assert!(c.state.authz.require(&job, mine, Permission::Write).await.is_err());
    assert_eq!(c.state.authz.repo_permission(&Actor::Runner { runner_id: RunnerId(1) }, mine).await.unwrap(), Permission::None);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_role_lookup(pool: PgPool) {
    let c = ctx(pool).await;
    let owner = make_user(&c, "owner", false).await;
    let member = make_user(&c, "member", false).await;
    make_user(&c, "out", false).await;
    let (_, o) = session(&c, "owner").await;
    let (_, m) = session(&c, "member").await;
    let (_, out) = session(&c, "out").await;
    let org = c.state.orgs.create(&o, "haven", "", &c1()).await.unwrap();
    c.state.orgs.set_member(&o, org.id, member, ch_core::services::OrgRole::Member, &c1()).await.unwrap();
    use ch_core::services::OrgRole;
    assert_eq!(c.state.authz.org_role(&o, org.id).await.unwrap(), Some(OrgRole::Owner));
    assert_eq!(c.state.authz.org_role(&m, org.id).await.unwrap(), Some(OrgRole::Member));
    assert_eq!(c.state.authz.org_role(&out, org.id).await.unwrap(), None);
    assert_eq!(c.state.authz.org_role(&Actor::Anonymous, org.id).await.unwrap(), None);
    let _ = owner;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_suspended_account_has_no_grant_anywhere(pool: PgPool) {
    // ch-4he: whatever is still open for the account (an SSH connection, an armed auto-merge, a
    // token) asks Authz, and Authz itself says no.
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let bob = c.env.user("bob").await;
    let mine = c.env.repo(alice, "mine").await;
    let shared = c.env.repo(alice, "shared").await;
    let public = c.env.public_repo(alice, "pub").await;
    c.env.collaborator(shared, bob, "write").await;
    assert_eq!(c.state.authz.repo_permission(&user(bob), shared).await.unwrap(), Permission::Write);
    sqlx::query("UPDATE users SET suspended_at = now() WHERE id = $1 OR id = $2")
        .bind(alice.0)
        .bind(bob.0)
        .execute(&c.state.db)
        .await
        .unwrap();
    for (who, repo) in [(alice, mine), (bob, shared), (alice, public)] {
        assert_eq!(c.state.authz.repo_permission(&user(who), repo).await.unwrap(), Permission::None, "{who} on {repo}");
        assert_eq!(c.state.authz.repo_permission(&token_actor(who, &["repo"]), repo).await.unwrap(), Permission::None);
        assert!(!c.state.authz.has_grant(&user(who), repo).await.unwrap());
    }
    assert!(matches!(c.state.authz.require(&user(bob), shared, Permission::Read).await, Err(Error::NotFound)));
}
