//! Organizations, members and teams.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::services::OrgRole;
use ch_core::{Actor, Error, Permission};
use sqlx::PgPool;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn create_members_and_the_last_owner_rule(pool: PgPool) {
    let c = ctx(pool).await;
    let owner = make_user(&c, "owner", false).await;
    let bob = make_user(&c, "bob", false).await;
    let (_, o) = session(&c, "owner").await;
    let (_, b) = session(&c, "bob").await;

    let org = c.state.orgs.create(&o, "haven", "A harbour", &c1()).await.unwrap();
    assert_eq!(org.name, "haven");
    assert_eq!(org.member_count, 1);
    assert_eq!(org.base_permission, Permission::Read);
    // The name space is shared with people.
    assert!(matches!(c.state.orgs.create(&b, "haven", "", &c1()).await, Err(Error::Validation(_))));
    assert!(matches!(c.state.orgs.create(&b, "owner", "", &c1()).await, Err(Error::Validation(_))));
    assert!(c.state.orgs.create(&b, "login", "", &c1()).await.is_err(), "reserved");
    assert!(c.state.orgs.create(&Actor::Anonymous, "x", "", &c1()).await.is_err());

    // Only owners add people, and the person needs an account already.
    assert!(c.state.orgs.set_member(&b, org.id, bob, OrgRole::Member, &c1()).await.is_err());
    assert!(c.state.orgs.set_member(&o, org.id, ch_core::UserId(99_999), OrgRole::Member, &c1()).await.is_err());
    c.state.orgs.set_member(&o, org.id, bob, OrgRole::Member, &c1()).await.unwrap();
    let members = c.state.orgs.members(&b, org.id).await.unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].role, OrgRole::Owner);
    // A stranger cannot list members.
    make_user(&c, "eve", false).await;
    let (_, eve) = session(&c, "eve").await;
    assert!(matches!(c.state.orgs.members(&eve, org.id).await, Err(Error::NotFound)));
    // But can read the profile.
    assert_eq!(c.state.orgs.get(&eve, "HAVEN").await.unwrap().name, "haven");
    assert!(matches!(c.state.orgs.get(&Actor::Anonymous, "haven").await, Err(Error::NotFound)));
    assert!(matches!(c.state.orgs.get(&eve, "owner").await, Err(Error::NotFound)), "a person is not an org");

    // The last owner can neither leave nor be demoted.
    assert!(matches!(c.state.orgs.remove_member(&o, org.id, owner, &c1()).await, Err(Error::Conflict(_))));
    assert!(matches!(c.state.orgs.set_member(&o, org.id, owner, OrgRole::Member, &c1()).await, Err(Error::Conflict(_))));
    c.state.orgs.set_member(&o, org.id, bob, OrgRole::Owner, &c1()).await.unwrap();
    c.state.orgs.remove_member(&o, org.id, owner, &c1()).await.unwrap();
    // A member may leave by themselves, but cannot remove others.
    make_user(&c, "carl", false).await;
    let carl = c.state.accounts.get_by_username(&b, "carl").await.unwrap();
    let (_, carl_a) = session(&c, "carl").await;
    c.state.orgs.set_member(&b, org.id, carl.id, OrgRole::Member, &c1()).await.unwrap();
    assert!(c.state.orgs.remove_member(&carl_a, org.id, bob, &c1()).await.is_err());
    c.state.orgs.remove_member(&carl_a, org.id, carl.id, &c1()).await.unwrap();
    assert!(c.state.orgs.list_for_user(&b, bob).await.unwrap().iter().any(|x| x.name == "haven"));
    assert!(c.state.orgs.list_for_user(&b, owner).await.is_err(), "someone else's memberships are not listed");
    assert!(count_audit(&c, "org.created").await == 1 && count_audit(&c, "org.member_removed").await == 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn suspended_people_cannot_be_added(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "owner", false).await;
    let gone = make_user(&c, "gone", false).await;
    c.state.accounts.suspend(&Actor::System, gone, &ch_core::model::ClientInfo::local()).await.unwrap();
    let (_, o) = session(&c, "owner").await;
    let org = c.state.orgs.create(&o, "haven", "", &c1()).await.unwrap();
    assert!(c.state.orgs.set_member(&o, org.id, gone, OrgRole::Member, &c1()).await.is_err());
    // Another org is not a member candidate either.
    let other_org = c.env.org("other").await;
    assert!(c.state.orgs.set_member(&o, org.id, other_org, OrgRole::Member, &c1()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn teams(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "owner", false).await;
    let bob = make_user(&c, "bob", false).await;
    let outsider = make_user(&c, "outsider", false).await;
    let (_, o) = session(&c, "owner").await;
    let (_, b) = session(&c, "bob").await;
    let org = c.state.orgs.create(&o, "haven", "", &c1()).await.unwrap();
    c.state.orgs.set_member(&o, org.id, bob, OrgRole::Member, &c1()).await.unwrap();

    assert!(c.state.orgs.create_team(&b, org.id, "devs", "", Permission::Read, &c1()).await.is_err(), "owners only");
    for bad in ["", " x", "-x", "a b", "a/b"] {
        assert!(c.state.orgs.create_team(&o, org.id, bad, "", Permission::Read, &c1()).await.is_err(), "{bad:?}");
    }
    // None is not a team permission: access is taken away by removing the repository.
    assert!(matches!(c.state.orgs.create_team(&o, org.id, "quiet", "", Permission::None, &c1()).await, Err(Error::Validation(_))));
    let t = c.state.orgs.create_team(&o, org.id, "Devs", "Builders", Permission::Write, &c1()).await.unwrap();
    assert_eq!(t.permission, Permission::Write, "the permission comes back on the team");
    assert!(
        matches!(c.state.orgs.create_team(&o, org.id, "devs", "", Permission::Read, &c1()).await, Err(Error::Validation(_))),
        "names are case insensitive"
    );
    assert_eq!(c.state.orgs.get_team(&b, org.id, "devs").await.unwrap().id, t.id);
    assert_eq!(c.state.orgs.get_team(&b, org.id, "devs").await.unwrap().permission, Permission::Write);

    // Owners change a team's permission, plain members cannot.
    assert!(c.state.orgs.set_team_permission(&b, t.id, Permission::Read, &c1()).await.is_err(), "members cannot");
    c.state.orgs.set_team_permission(&o, t.id, Permission::Maintain, &c1()).await.unwrap();
    assert_eq!(c.state.orgs.get_team(&b, org.id, "devs").await.unwrap().permission, Permission::Maintain);
    assert!(matches!(c.state.orgs.set_team_permission(&o, t.id, Permission::None, &c1()).await, Err(Error::Validation(_))));
    c.state.orgs.set_team_permission(&o, t.id, Permission::Write, &c1()).await.unwrap();
    // People join the team only through the org.
    assert!(matches!(c.state.orgs.add_team_member(&o, t.id, outsider, &c1()).await, Err(Error::Conflict(_))));
    c.state.orgs.add_team_member(&o, t.id, bob, &c1()).await.unwrap();
    assert_eq!(c.state.orgs.team_members(&b, t.id).await.unwrap().len(), 1);
    assert_eq!(c.state.orgs.teams(&b, org.id).await.unwrap()[0].member_count, 1);

    // Team repos must belong to the org, need admin on the repo, and not be none.
    let repo = c.env.repo(ch_core::UserId(org.id.0), "tools").await;
    let foreign = c.env.repo(outsider, "theirs").await;
    assert!(c.state.orgs.set_team_repo(&o, t.id, foreign, Permission::Read, &c1()).await.is_err());
    assert!(c.state.orgs.set_team_repo(&b, t.id, repo, Permission::Read, &c1()).await.is_err(), "members cannot");
    assert!(c.state.orgs.set_team_repo(&o, t.id, repo, Permission::None, &c1()).await.is_err());
    c.state.orgs.set_team_repo(&o, t.id, repo, Permission::Write, &c1()).await.unwrap();
    assert_eq!(c.state.orgs.team_repos(&b, t.id).await.unwrap()[0].permission, Permission::Write);
    c.state.orgs.remove_team_repo(&o, t.id, repo, &c1()).await.unwrap();
    assert!(matches!(c.state.orgs.remove_team_repo(&o, t.id, repo, &c1()).await, Err(Error::NotFound)));

    c.state.orgs.remove_team_member(&o, t.id, bob, &c1()).await.unwrap();
    c.state.orgs.delete_team(&o, t.id, &c1()).await.unwrap();
    assert!(matches!(c.state.orgs.get_team(&o, org.id, "devs").await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_an_org(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "owner", false).await;
    let bob = make_user(&c, "bob", false).await;
    let (_, o) = session(&c, "owner").await;
    let (_, b) = session(&c, "bob").await;
    let org = c.state.orgs.create(&o, "haven", "", &c1()).await.unwrap();
    c.state.orgs.set_member(&o, org.id, bob, OrgRole::Member, &c1()).await.unwrap();
    assert!(c.state.orgs.delete(&b, org.id, &c1()).await.is_err());
    c.env.repo(ch_core::UserId(org.id.0), "tools").await;
    assert!(matches!(c.state.orgs.delete(&o, org.id, &c1()).await, Err(Error::Conflict(_))), "repos first");
    sqlx::query("DELETE FROM repositories").execute(&c.pool).await.unwrap();
    c.state.orgs.delete(&o, org.id, &c1()).await.unwrap();
    assert!(matches!(c.state.orgs.get(&o, "haven").await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_token_needs_the_org_scope(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;
    let (_, plain) = c.state.accounts.create_token(&me, id, "plain", vec!["repo".into()], 5, &c1()).await.unwrap();
    let (_, orgy) = c.state.accounts.create_token(&me, id, "orgy", vec!["admin:org".into()], 5, &c1()).await.unwrap();
    let plain_a = c.state.authn.from_bearer(&plain, &c1()).await.unwrap();
    let orgy_a = c.state.authn.from_bearer(&orgy, &c1()).await.unwrap();
    assert!(matches!(c.state.orgs.create(&plain_a, "haven", "", &c1()).await, Err(Error::Forbidden(_))));
    assert!(c.state.orgs.create(&orgy_a, "haven", "", &c1()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_org_may_run_ci_when_its_creator_may_and_an_admin_can_change_it(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "root2").await;
    let allowed = make_user(&c, "dev", false).await;
    make_user(&c, "plain", false).await;
    c.state.accounts.set_can_run_ci(&admin, allowed, true, &c1()).await.unwrap();
    let (_, a) = session(&c, "dev").await;
    let (_, p) = session(&c, "plain").await;

    let acme = c.state.orgs.create(&a, "acme", "", &c1()).await.unwrap();
    let quiet = c.state.orgs.create(&p, "quiet", "", &c1()).await.unwrap();
    let ci = |org: i64| {
        let pool = c.pool.clone();
        async move { sqlx::query_scalar::<_, bool>("SELECT can_run_ci FROM users WHERE id = $1").bind(org).fetch_one(&pool).await.unwrap() }
    };
    assert!(ci(acme.id.0).await, "created by someone who may run CI");
    assert!(!ci(quiet.id.0).await, "created by someone who may not");
    // A site admin can turn it on and off for an org, nobody else can.
    assert!(c.state.accounts.set_can_run_ci(&p, ch_core::UserId(quiet.id.0), true, &c1()).await.is_err());
    c.state.accounts.set_can_run_ci(&admin, ch_core::UserId(quiet.id.0), true, &c1()).await.unwrap();
    assert!(ci(quiet.id.0).await);
    c.state.accounts.set_can_run_ci(&admin, ch_core::UserId(acme.id.0), false, &c1()).await.unwrap();
    assert!(!ci(acme.id.0).await);
}
