//! What a connected app may do, enforced in the services themselves so that a bug in a tool can
//! never widen it. Real services, real git repositories, a real database. The app actors are built
//! directly: the OAuth side has its own suites.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use ch_core::collab::*;
use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::web_contract::RepoUpdate;
use ch_core::{Actor, AppGrant, AuthVia, Config, GrantId, Install, RepoId, RepoSet, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use sqlx::PgPool;
use std::sync::Arc;

const PW: &str = "violet harbour lantern 83 teapot";

struct H {
    s: AppState,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    cfg.mcp.enabled = true;
    let cfg = Arc::new(cfg);
    let blobs = FsBlobStore::new(cfg.blobs_dir());
    blobs.init().await.unwrap();
    let mut b =
        AppStateBuilder::new(cfg, pool.clone(), Arc::new(PgAudit::new(pool.clone())), Arc::new(PgEventBus::new(pool)), Arc::new(blobs));
    ch_auth::build_with(&b, ch_auth::AuthOptions { hash_params: ch_auth::password::HashParams::FAST_FOR_TESTS, ..Default::default() })
        .unwrap()
        .install(&mut b);
    ch_notify::build(&b).unwrap().install(&mut b);
    ch_git::build(&b).unwrap().install(&mut b);
    ch_collab::build(&b).unwrap().install(&mut b);
    ch_ci::build(&b).unwrap().install(&mut b);
    H { s: b.build(), _dir: dir }
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

async fn user(h: &H, name: &str) -> (UserId, Actor) {
    let u = h
        .s
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: true },
            &c(),
        )
        .await
        .unwrap();
    h.s.accounts.set_password(&Actor::System, u.id, PW, &c()).await.unwrap();
    (u.id, Actor::User { id: u.id, is_admin: false, has_2fa: true, via: AuthVia::Session })
}

async fn repo(h: &H, who: &(UserId, Actor), name: &str, vis: Visibility) -> RepoId {
    h.s.repos
        .create(
            &who.1,
            NewRepo {
                owner_id: who.0,
                name: name.into(),
                description: String::new(),
                visibility: vis,
                default_branch: None,
                auto_init: true,
            },
            &c(),
        )
        .await
        .unwrap()
        .id
}

fn set(ids: &[RepoId]) -> RepoSet {
    RepoSet::Only(ids.iter().copied().collect())
}

fn app(user: UserId, read: RepoSet, write: RepoSet, workflows: RepoSet) -> Actor {
    Actor::App { user_id: user, grant_id: GrantId(1), grant: AppGrant { read, write, workflows } }
}

fn sig() -> Signature {
    Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() }
}

async fn tip(h: &H, r: RepoId, branch: &str) -> String {
    h.s.git.resolve(r, &format!("refs/heads/{branch}")).await.unwrap()
}

async fn commit(
    h: &H,
    who: &Actor,
    r: RepoId,
    branch: &str,
    new_branch: Option<&str>,
    changes: Vec<FileChange>,
) -> ch_core::Result<String> {
    let expected = tip(h, r, branch).await;
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: r,
                branch: branch.into(),
                new_branch: new_branch.map(str::to_string),
                expected_sha: Some(expected),
                changes,
                message: "change".into(),
                author: sig(),
            },
            &c(),
        )
        .await
}

fn file(path: &str, content: &str) -> FileChange {
    FileChange { path: path.into(), content: Some(content.as_bytes().to_vec()), renamed_from: None }
}

fn forbidden<T: std::fmt::Debug>(r: ch_core::Result<T>) {
    match r {
        Err(ch_core::Error::Forbidden(_)) | Err(ch_core::Error::NotFound) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn write_on_one_repository_never_writes_to_another_it_can_only_read(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let rw = repo(&h, &alice, "rw", Visibility::Private).await;
    let ro = repo(&h, &alice, "ro", Visibility::Private).await;
    let ro_issue =
        h.s.issues
            .create(
                &alice.1,
                NewIssue { repo_id: ro, title: "T".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
                &c(),
            )
            .await
            .unwrap()
            .number;
    let a = app(alice.0, RepoSet::All, set(&[rw]), RepoSet::none());
    // Where it has write.
    let made =
        h.s.issues
            .create(
                &a,
                NewIssue { repo_id: rw, title: "ok".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
                &c(),
            )
            .await
            .unwrap();
    h.s.issues.add_comment(&a, rw, made.number, "fine", &c()).await.unwrap();
    // Where it only reads: every write is refused by the service, not by a tool.
    forbidden(
        h.s.issues
            .create(
                &a,
                NewIssue { repo_id: ro, title: "no".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
                &c(),
            )
            .await,
    );
    forbidden(h.s.issues.add_comment(&a, ro, ro_issue, "no", &c()).await);
    forbidden(h.s.issues.update(&a, ro, ro_issue, IssuePatch { title: Some("x".into()), ..Default::default() }, &c()).await);
    forbidden(h.s.issues.set_labels(&a, ro, ro_issue, vec!["bug".into()], &c()).await);
    forbidden(h.s.reviews.submit(&a, ro, ro_issue, ReviewState::Commented, "x", vec![], &c()).await);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM issues WHERE repo_id = $1").bind(ro.0).fetch_one(&h.s.db).await.unwrap();
    assert_eq!(n, 1, "only the issue alice made herself");
    // Reads still work on both, and a repository outside the grant does not exist for it.
    assert!(h.s.repos.get(&a, ro).await.is_ok());
    let only_rw = app(alice.0, set(&[rw]), set(&[rw]), RepoSet::none());
    assert!(matches!(h.s.repos.get(&only_rw, ro).await, Err(ch_core::Error::NotFound)));
    // `@me` and cross-repository lists are narrowed by the grant, not by the person's reach.
    let listed = h.s.repos.list_accessible(&only_rw, 1, 50).await.unwrap();
    assert_eq!(listed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![rw]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_app_is_refused_every_forbidden_action_however_much_it_was_given(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", Visibility::Private).await;
    let all = app(alice.0, RepoSet::All, RepoSet::All, RepoSet::All);
    let main = tip(&h, r, "main").await;
    // A pull request to act on.
    commit(&h, &alice.1, r, "main", Some("feature"), vec![file("a.txt", "a")]).await.unwrap();
    let pr =
        h.s.pulls
            .create(
                &alice.1,
                NewPull {
                    repo_id: r,
                    title: "P".into(),
                    body: String::new(),
                    head_repo_id: r,
                    head_ref: "feature".into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &c(),
            )
            .await
            .unwrap()
            .issue
            .number;
    // Approving, dismissing, resolving, auto-merge, base change.
    forbidden(h.s.reviews.submit(&all, r, pr, ReviewState::Approved, "lgtm", vec![], &c()).await);
    forbidden(h.s.reviews.dismiss(&all, r, pr, ch_core::ReviewId(1), "x", &c()).await);
    forbidden(h.s.reviews.resolve_thread(&all, r, 1, true, &c()).await);
    forbidden(h.s.pulls.set_auto_merge(&all, r, pr, Some(MergeMethod::Squash), &c()).await);
    forbidden(h.s.pulls.update(&all, r, pr, PullPatch { base: Some("other".into()), ..Default::default() }, &c()).await);
    // Statuses, releases, runner tokens, run approval.
    forbidden(h.s.checks.create_status(&all, r, &main, "success", "ci", None, None).await);
    forbidden(
        h.s.releases
            .create(
                &all,
                r,
                ReleaseDraft {
                    tag_name: "v1".into(),
                    target_commitish: None,
                    name: None,
                    body: None,
                    draft: false,
                    prerelease: false,
                    generate_notes: false,
                },
                &c(),
            )
            .await,
    );
    forbidden(h.s.ci.create_runner_token(&all, Some(r), &c()).await);
    forbidden(h.s.ci.approve_run(&all, r, ch_core::RunId(1), &c()).await);
    // Tags and branch deletion are refused at the ref change itself.
    forbidden(h.s.git.create_ref(&all, r, "refs/tags/v1", &main, &c()).await);
    forbidden(h.s.git.delete_ref(&all, r, "feature", &c()).await);
    // Outside refs/heads and refs/tags is refused for everyone, with a validation error.
    assert!(matches!(h.s.git.create_ref(&all, r, "refs/pull/99/head", &main, &c()).await, Err(ch_core::Error::Validation(_))));
    // Repository administration needs Admin, which an app can never reach.
    forbidden(h.s.repos.delete(&all, r, &c()).await);
    forbidden(h.s.repos.transfer(&all, r, alice.0, &c()).await);
    forbidden(h.s.repos.set_collaborator(&all, r, alice.0, ch_core::Permission::Admin, &c()).await);
    forbidden(h.s.repos.update(&all, r, RepoUpdate { visibility: Some(Visibility::Public), ..Default::default() }, &c()).await);
    // Self-service and account actions fail closed because an app has no `user_id`.
    forbidden(h.s.accounts.create_token(&all, alice.0, "t", vec!["repo".into()], 30, &c()).await);
    forbidden(h.s.accounts.list_ssh_keys(&all, alice.0).await);
    forbidden(h.s.apps.revoke_grant(&all, GrantId(1), &c()).await);
    // Allowed: a branch, a commit, a pull request comment.
    h.s.git.create_ref(&all, r, "topic", &main, &c()).await.unwrap();
    h.s.issues.add_comment(&all, r, pr, "a comment", &c()).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn repository_creation_by_an_app_is_private_and_only_in_the_persons_namespace(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let new = |owner: UserId, vis| NewRepo {
        owner_id: owner,
        name: "made".into(),
        description: String::new(),
        visibility: vis,
        default_branch: None,
        auto_init: true,
    };
    let narrow = app(alice.0, RepoSet::All, set(&[]), RepoSet::none());
    forbidden(h.s.repos.create(&narrow, new(alice.0, Visibility::Private), &c()).await);
    let some = app(alice.0, set(&[]), RepoSet::All, RepoSet::none());
    forbidden(h.s.repos.create(&some, new(alice.0, Visibility::Private), &c()).await);
    let wide = app(alice.0, RepoSet::All, RepoSet::All, RepoSet::none());
    forbidden(h.s.repos.create(&wide, new(bob.0, Visibility::Private), &c()).await);
    let r = h.s.repos.create(&wide, new(alice.0, Visibility::Public), &c()).await.unwrap();
    assert_eq!(r.visibility, Visibility::Private, "forced private whatever it asked for");
    assert_eq!(r.owner_id, alice.0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workflow_files_cannot_change_without_the_workflows_permission(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "ci", Visibility::Private).await;
    let wf = ".codehaven/workflows/ci.yml";
    commit(
        &h,
        &alice.1,
        r,
        "main",
        None,
        vec![file(wf, "name: ci\non: push\njobs: {}\n"), file(".github/workflows/old.yml", "name: old\non: push\njobs: {}\n")],
    )
    .await
    .unwrap();
    let no_wf = app(alice.0, RepoSet::All, set(&[r]), RepoSet::none());
    let with_wf = app(alice.0, RepoSet::All, set(&[r]), set(&[r]));
    // An ordinary file is fine on the default branch (no schedules yet).
    commit(&h, &no_wf, r, "main", None, vec![file("docs/readme.md", "hi")]).await.unwrap();
    // Edit, add, delete, rename out of the directory, and the .github directory, all refused.
    forbidden(
        commit(
            &h,
            &no_wf,
            r,
            "main",
            None,
            vec![file(wf, "name: ci\non: push\njobs:\n  x:\n    runs-on: a\n    steps:\n      - run: echo pwned\n")],
        )
        .await,
    );
    forbidden(commit(&h, &no_wf, r, "main", None, vec![file(".codehaven/workflows/new.yml", "x")]).await);
    forbidden(commit(&h, &no_wf, r, "main", None, vec![FileChange { path: wf.into(), content: None, renamed_from: None }]).await);
    forbidden(
        commit(
            &h,
            &no_wf,
            r,
            "main",
            None,
            vec![FileChange {
                path: "docs/ci.yml".into(),
                content: Some(b"name: ci\non: push\njobs: {}\n".to_vec()),
                renamed_from: Some(wf.into()),
            }],
        )
        .await,
    );
    forbidden(
        commit(&h, &no_wf, r, "main", None, vec![file(".github/workflows/old.yml", "name: old\non: pull_request\njobs: {}\n")]).await,
    );
    forbidden(commit(&h, &no_wf, r, "main", None, vec![file(".github/workflows/added.yml", "x")]).await);
    // A new branch does not launder it: the same change on a branch made by the commit is refused too.
    forbidden(commit(&h, &no_wf, r, "main", Some("sneaky"), vec![file(".codehaven/workflows/evil.yml", "x")]).await);
    assert!(h.s.git.refs(r, Some("refs/heads/sneaky")).await.unwrap().is_empty(), "no branch was made");
    // A new branch from an existing commit with no workflow change is fine.
    commit(&h, &no_wf, r, "main", Some("topic"), vec![file("docs/other.md", "x")]).await.unwrap();
    // With the permission, the same change goes through.
    commit(&h, &with_wf, r, "main", None, vec![file(wf, "name: ci\non: push\njobs: {}\n# edited\n")]).await.unwrap();
    // A person is never subject to the guard.
    commit(&h, &alice.1, r, "main", None, vec![file(wf, "name: ci\non: push\njobs: {}\n# by a person\n")]).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_default_branch_of_a_repository_with_schedules_is_off_limits_without_workflows(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "cron", Visibility::Private).await;
    let no_wf = app(alice.0, RepoSet::All, set(&[r]), RepoSet::none());
    commit(&h, &no_wf, r, "main", None, vec![file("a.txt", "a")]).await.unwrap();
    // A scheduled workflow is known to the server.
    let wid: i64 = sqlx::query_scalar("INSERT INTO workflows (repo_id, path, name, state) VALUES ($1, '.codehaven/workflows/nightly.yml', 'nightly', 'active') RETURNING id").bind(r.0).fetch_one(&h.s.db).await.unwrap();
    sqlx::query("INSERT INTO schedules (workflow_id, repo_id, cron, next_run_at) VALUES ($1, $2, '0 3 * * *', now() + interval '1 day')")
        .bind(wid)
        .bind(r.0)
        .execute(&h.s.db)
        .await
        .unwrap();
    let e = commit(&h, &no_wf, r, "main", None, vec![file("b.txt", "b")]).await.unwrap_err();
    assert!(e.to_string().to_lowercase().contains("schedule"), "says why: {e}");
    // Another branch is fine, and so is the same change with the permission.
    commit(&h, &no_wf, r, "main", Some("other"), vec![file("b.txt", "b")]).await.unwrap();
    let with_wf = app(alice.0, RepoSet::All, set(&[r]), set(&[r]));
    commit(&h, &with_wf, r, "main", None, vec![file("c.txt", "c")]).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_merge_by_an_app_closes_no_issue_elsewhere_and_deletes_no_branch(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let a_repo = repo(&h, &alice, "alpha", Visibility::Private).await;
    let b_repo = repo(&h, &alice, "beta", Visibility::Private).await;
    let issue = |r| {
        let h = &h;
        let a = alice.1.clone();
        async move {
            h.s.issues
                .create(
                    &a,
                    NewIssue { repo_id: r, title: "T".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
                    &c(),
                )
                .await
                .unwrap()
                .number
        }
    };
    let (in_alpha, in_beta) = (issue(a_repo).await, issue(b_repo).await);
    // The repository deletes head branches after merging, which for a person is the default.
    h.s.repos
        .update_settings(&alice.1, a_repo, RepoSettingsUpdate { delete_branch_on_merge: Some(true), ..Default::default() }, &c())
        .await
        .unwrap();
    let pull = |branch: &str, body: String| {
        let h = &h;
        let a = alice.1.clone();
        let branch = branch.to_string();
        async move {
            commit(h, &a, a_repo, "main", Some(&branch), vec![file(&format!("{branch}.txt"), "x")]).await.unwrap();
            h.s.pulls
                .create(
                    &a,
                    NewPull {
                        repo_id: a_repo,
                        title: "P".into(),
                        body,
                        head_repo_id: a_repo,
                        head_ref: branch,
                        base_ref: "main".into(),
                        draft: false,
                    },
                    &c(),
                )
                .await
                .unwrap()
                .issue
                .number
        }
    };
    let me = app(alice.0, RepoSet::All, set(&[a_repo]), RepoSet::none());
    let n = pull("by-app", format!("Fixes #{in_alpha} and Fixes alice/beta#{in_beta}")).await;
    let head = h.s.pulls.view(&me, a_repo, n).await.unwrap().pull.head_sha;
    h.s.pulls.merge_with(&me, a_repo, n, MergeOptions { expected_head_sha: Some(head), ..Default::default() }, &c()).await.unwrap();
    let open = |r: RepoId, num: i64| {
        let h = &h;
        async move { h.s.issues.get(&Actor::System, r, num).await.unwrap().open }
    };
    assert!(!open(a_repo, in_alpha).await, "the issue in the same repository closes");
    assert!(open(b_repo, in_beta).await, "the one in another repository, which the person could close, stays open");
    assert!(
        !h.s.git.refs(a_repo, Some("refs/heads/by-app")).await.unwrap().is_empty(),
        "the head branch is not deleted when an app merges"
    );
    // The same merge by the person: the branch goes (the repository setting), and the other issue closes.
    let in_beta2 = issue(b_repo).await;
    let n2 = pull("by-person", format!("Fixes alice/beta#{in_beta2}")).await;
    h.s.pulls.merge_with(&alice.1, a_repo, n2, MergeOptions::default(), &c()).await.unwrap();
    assert!(!open(b_repo, in_beta2).await, "a person's merge still closes across repositories");
    assert!(h.s.git.refs(a_repo, Some("refs/heads/by-person")).await.unwrap().is_empty());
    // A reference from an app to a repository it cannot write to leaves no mark there.
    let xrefs = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM timeline_events WHERE event = 'cross-referenced'")
            .fetch_one(&h.s.db)
            .await
            .unwrap()
    };
    let before = xrefs().await;
    h.s.issues.add_comment(&me, a_repo, in_alpha, &format!("see alice/beta#{in_beta}"), &c()).await.unwrap();
    assert_eq!(xrefs().await, before, "no cross-reference was written in a repository the app has no write access to");
    // The same comment by the person does leave one.
    h.s.issues.add_comment(&alice.1, a_repo, in_alpha, &format!("see alice/beta#{in_beta}"), &c()).await.unwrap();
    assert_eq!(xrefs().await, before + 1, "a person's own reference is recorded");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_commit_id_is_not_copied_from_a_fork_the_caller_cannot_read(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let upstream = repo(&h, &alice, "up", Visibility::Public).await;
    let bobs = h.s.repos.fork(&bob.1, upstream, bob.0, &c()).await.unwrap().id;
    h.s.repos.update(&bob.1, bobs, RepoUpdate { visibility: Some(Visibility::Private), ..Default::default() }, &c()).await.unwrap();
    let secret = commit(&h, &bob.1, bobs, "main", None, vec![file("secret.txt", "bob's private work")]).await.unwrap();
    let carols = h.s.repos.fork(&carol.1, upstream, carol.0, &c()).await.unwrap().id;
    // Carol names a commit that exists only in Bob's private fork. It must not be copied to her.
    let e = h.s.git.create_ref(&carol.1, carols, "leak", &secret, &c()).await;
    assert!(e.is_err(), "the object was copied from a fork the caller cannot read");
    assert!(h.s.git.commit(carols, &secret).await.is_err(), "and it is not in her repository");
    // The owner of the fork can use his own commit.
    h.s.git.create_ref(&bob.1, bobs, "mine", &secret, &c()).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_inbox_of_an_app_shows_only_what_its_grant_can_read(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let seen = repo(&h, &alice, "seen", Visibility::Private).await;
    let read_only = repo(&h, &alice, "readonly", Visibility::Private).await;
    let hidden = repo(&h, &alice, "hidden", Visibility::Private).await;
    let mut ids = Vec::new();
    for (n, (r, title)) in [
        (Some(seen), "on a repo it may write"),
        (Some(read_only), "on a repo it may read"),
        (Some(hidden), "SECRET TITLE"),
        (None, "no repo"),
    ]
    .into_iter()
    .enumerate()
    {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO notifications (user_id, repo_id, reason, subject_type, subject_id, title, unread) VALUES ($1, $2, 'mention', 'issue', $4, $3, true) RETURNING id",
        )
        .bind(alice.0 .0)
        .bind(r.map(|r| r.0))
        .bind(title)
        .bind(n as i64 + 1)
        .fetch_one(&h.s.db)
        .await
        .unwrap();
        ids.push(id);
    }
    let a = app(alice.0, set(&[seen, read_only]), set(&[seen]), RepoSet::none());
    let inbox = h.s.notifications.inbox(&a, false, 1, 50).await.unwrap();
    let titles: Vec<&str> = inbox.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(titles.len(), 2, "{titles:?}");
    assert!(!titles.iter().any(|t| t.contains("SECRET")) && !titles.contains(&"no repo"));
    assert_eq!(h.s.notifications.unread_count(&a).await.unwrap(), 2, "the count follows the grant too");
    assert_eq!(h.s.notifications.unread_count(&alice.1).await.unwrap(), 4, "the person sees all of them");
    // Mark read: one at a time, only where it may write.
    forbidden(h.s.notifications.mark_read(&a, None).await);
    forbidden(h.s.notifications.mark_read(&a, Some(ch_core::NotificationId(ids[1]))).await);
    forbidden(h.s.notifications.mark_read(&a, Some(ch_core::NotificationId(ids[2]))).await);
    h.s.notifications.mark_read(&a, Some(ch_core::NotificationId(ids[0]))).await.unwrap();
    assert_eq!(h.s.notifications.unread_count(&a).await.unwrap(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_reachable_commit_check_rejects_objects_that_are_not_commits_of_this_repository(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", Visibility::Private).await;
    let other = repo(&h, &alice, "other", Visibility::Private).await;
    let main = tip(&h, r, "main").await;
    let other_tip = commit(&h, &alice.1, other, "main", None, vec![file("x.txt", "x")]).await.unwrap();
    assert!(h.s.git.reachable(r, &main).await.unwrap());
    assert!(!h.s.git.reachable(r, &other_tip).await.unwrap(), "a commit of another repository");
    let tree = h.s.git.commit(r, &main).await.unwrap().tree;
    assert!(!h.s.git.reachable(r, &tree).await.unwrap(), "a tree id");
    assert!(!h.s.git.reachable(r, &main[..12]).await.unwrap(), "an abbreviation");
    assert!(!h.s.git.reachable(r, "HEAD").await.unwrap());
    assert!(!h.s.git.reachable(r, &"0".repeat(40)).await.unwrap());
    assert!(
        h.s.git.commit_diff(r, &main).await.unwrap().files.iter().any(|f| f.new_path.as_deref() == Some("README.md")),
        "a root commit has a diff against the empty tree"
    );
}
