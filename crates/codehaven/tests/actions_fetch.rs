//! Actions a step `uses:` (ch-ctk.5): the site's policy, refs resolved to commits that a branch
//! or tag reaches, archives kept and pinned to the run, and which repositories here a job may
//! use. "GitHub" is a local mirror, so nothing leaves the machine.

#![allow(clippy::unwrap_used)]

use ch_ci::action_fetch::{archive_for_job, resolve_for_job};
use ch_ci::ctx::Ctx;
use ch_ci::runs::{JOB_COLS, JobRow};
use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Actor, AuthVia, Config, Install, RepoId, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use ch_testkit::git::TempRepo;
use sqlx::PgPool;
use std::path::Path;
use std::sync::Arc;

struct H {
    s: AppState,
    pool: PgPool,
    dir: tempfile::TempDir,
}

async fn harness(pool: PgPool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    let cfg = Arc::new(cfg);
    let blobs = FsBlobStore::new(cfg.blobs_dir());
    blobs.init().await.unwrap();
    let mut b = AppStateBuilder::new(
        cfg,
        pool.clone(),
        Arc::new(PgAudit::new(pool.clone())),
        Arc::new(PgEventBus::new(pool.clone())),
        Arc::new(blobs),
    );
    ch_auth::build_with(&b, ch_auth::AuthOptions { hash_params: ch_auth::password::HashParams::FAST_FOR_TESTS, ..Default::default() })
        .unwrap()
        .install(&mut b);
    ch_notify::build(&b).unwrap().install(&mut b);
    ch_git::build(&b).unwrap().install(&mut b);
    ch_collab::build(&b).unwrap().install(&mut b);
    ch_ci::build(&b).unwrap().install(&mut b);
    H { s: b.build(), pool, dir }
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

async fn commit(h: &H, who: &Actor, r: RepoId, path: &str, content: &str) -> String {
    let expected = h.s.git.resolve(r, "refs/heads/main").await.unwrap();
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: r,
                branch: "main".into(),
                new_branch: None,
                expected_sha: Some(expected),
                changes: vec![FileChange { path: path.into(), content: Some(content.as_bytes().to_vec()), renamed_from: None }],
                message: "change".into(),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap()
}

/// A manual run of a one-job workflow in `r`, and its job.
async fn job(h: &H, owner: &(UserId, Actor), r: RepoId, n: u32) -> JobRow {
    let file = format!(".github/workflows/w{n}.yml");
    commit(h, &owner.1, r, &file, "on: workflow_dispatch\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n").await;
    let run = h.s.ci.dispatch(&owner.1, r, &format!("w{n}.yml"), "main", serde_json::json!({}), &c()).await.unwrap();
    sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE run_id = $1"))
        .bind(run.0)
        .fetch_one(&h.pool)
        .await
        .unwrap()
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// "GitHub": `acme/tool` with an annotated tag v1, a branch main, and a commit only a pull
/// request reaches (as GitHub serves a fork's commits under the parent's name).
fn fake_github(base: &Path) -> (TempRepo, String, String) {
    let src = TempRepo::new();
    src.commit_file("action.yml", "name: tool\nruns:\n  using: node20\n  main: index.js\n", "first");
    src.commit_file("index.js", "console.log('hi')\n", "second");
    src.git(&["tag", "-a", "v1", "-m", "v1"]);
    let v1 = src.git(&["rev-parse", "v1^{commit}"]);
    src.git(&["checkout", "-q", "-b", "side"]);
    let fork_only = src.commit_file("evil.js", "steal()\n", "fork");
    src.git(&["update-ref", "refs/pull/1/head", &fork_only]);
    src.git(&["checkout", "-q", "main"]);
    src.git(&["branch", "-D", "side"]);
    std::fs::create_dir_all(base.join("acme")).unwrap();
    sh(base, &["clone", "--quiet", "--mirror", src.work.to_str().unwrap(), "acme/tool"]);
    (src, v1, fork_only)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn github_actions_resolve_to_a_branch_or_tag_commit_and_stay_pinned(pool: PgPool) {
    let h = harness(pool).await;
    let ctx = Ctx::from_state(&h.s);
    let gh = h.dir.path().join("github");
    let (src, v1, fork_only) = fake_github(&gh);
    ctx.shared.set_github_base(Some(gh.to_string_lossy().to_string()));
    sqlx::query("UPDATE actions_policy SET allow = ARRAY['acme/*']").execute(&h.pool).await.unwrap();

    let a = user(&h, "alice").await;
    let r = repo(&h, &a, "app", Visibility::Private).await;
    let j = job(&h, &a, r, 1).await;
    let got = resolve_for_job(&ctx, &j, &["acme/tool@v1".into(), "acme/tool/sub@main".into()]).await.unwrap();
    assert_eq!(got[0].sha.as_deref(), Some(v1.as_str()), "the tag is peeled to its commit: {got:?}");
    assert!(got[0].error.is_none());
    assert_eq!(got[1].path, "sub");
    let archive = got[0].archive.clone().unwrap();
    archive_for_job(&ctx, &j, &archive).await.unwrap();

    // The tag moves on GitHub: this run keeps the commit it started with.
    src.commit_file("index.js", "console.log('moved')\n", "third");
    src.git(&["tag", "-f", "-a", "v1", "-m", "v1 again"]);
    sh(&gh.join("acme/tool"), &["fetch", "--quiet", "--force", src.work.to_str().unwrap(), "+refs/tags/*:refs/tags/*"]);
    let again = resolve_for_job(&ctx, &j, &["acme/tool@v1".into()]).await.unwrap();
    assert_eq!(again[0].sha.as_deref(), Some(v1.as_str()));

    // A full SHA on a branch works; one only a pull request reaches is refused; short ones too.
    let j2 = job(&h, &a, r, 2).await;
    let shas =
        resolve_for_job(&ctx, &j2, &[format!("acme/tool@{v1}"), format!("acme/tool@{fork_only}"), format!("acme/tool@{}", &v1[..7])])
            .await
            .unwrap();
    assert_eq!(shas[0].sha.as_deref(), Some(v1.as_str()), "{shas:?}");
    assert!(shas[1].error.as_deref().unwrap_or("").contains("not on a branch or tag"), "{shas:?}");
    assert!(shas[2].error.is_some() && shas[2].sha.is_none());
    // Another run's archive is not this job's to download.
    assert!(archive_for_job(&ctx, &j2, &archive).await.is_err() || shas[0].archive.as_deref() == Some(archive.as_str()));

    // The policy is checked before anything is fetched.
    let denied = resolve_for_job(&ctx, &j2, &["other/thing@v1".into()]).await.unwrap();
    assert!(denied[0].error.as_deref().unwrap_or("").contains("not allowed on this instance"), "{denied:?}");
    sqlx::query("UPDATE actions_policy SET block = ARRAY['acme/tool']").execute(&h.pool).await.unwrap();
    let blocked = resolve_for_job(&ctx, &job(&h, &a, r, 3).await, &["acme/tool@v1".into()]).await.unwrap();
    assert!(blocked[0].error.as_deref().unwrap_or("").contains("blocked"), "{blocked:?}");
    let runs: i64 = sqlx::query_scalar("SELECT count(DISTINCT run_id) FROM action_resolutions").fetch_one(&h.pool).await.unwrap();
    assert_eq!(runs, 2, "refused actions are not recorded");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn repositories_here_are_used_only_when_the_job_may_see_them(pool: PgPool) {
    let h = harness(pool).await;
    let ctx = Ctx::from_state(&h.s);
    let a = user(&h, "alice").await;
    let b = user(&h, "bob").await;
    let app = repo(&h, &a, "app", Visibility::Internal).await;
    let mine_private = repo(&h, &a, "tools", Visibility::Private).await;
    let mine_internal = repo(&h, &a, "shared", Visibility::Internal).await;
    let bobs_private = repo(&h, &b, "secret", Visibility::Private).await;
    for r in [mine_private, mine_internal, bobs_private] {
        commit(&h, if r == bobs_private { &b.1 } else { &a.1 }, r, "action.yml", "runs:\n  using: node20\n  main: i.js\n").await;
    }
    let j = job(&h, &a, app, 1).await;
    let got = resolve_for_job(
        &ctx,
        &j,
        &[
            "haven_alice/shared@main".into(),
            "haven_alice/tools@main".into(),
            "haven_bob/secret@main".into(),
            "haven_nobody/none@main".into(),
            "haven_alice/app@main".into(),
        ],
    )
    .await
    .unwrap();
    assert!(got[0].sha.is_some(), "same owner, as visible as the job's repository: {got:?}");
    assert!(got[1].sha.is_none(), "a private one is narrower than an internal caller");
    // Another owner's private repository and a missing one give the same answer.
    assert_eq!(
        got[2].error.as_deref().map(|e| e.replace("bob/secret", "X")),
        got[3].error.as_deref().map(|e| e.replace("nobody/none", "X"))
    );
    assert!(got[4].sha.is_some(), "the job's own repository");
    // A private repository of the same owner serves a private caller, but only when whoever
    // started the run can read it: the same owner is not enough on its own.
    let private_app = repo(&h, &a, "private-app", Visibility::Private).await;
    let j2 = job(&h, &a, private_app, 2).await;
    let as_alice = resolve_for_job(&ctx, &j2, &["haven_alice/tools@main".into()]).await.unwrap();
    assert!(as_alice[0].sha.is_some(), "{as_alice:?}");
    let j3 = job(&h, &a, private_app, 3).await;
    sqlx::query("UPDATE workflow_runs SET actor_id = $2 WHERE id = $1").bind(j3.run_id).bind(b.0.0).execute(&h.pool).await.unwrap();
    let as_bob = resolve_for_job(&ctx, &j3, &["haven_alice/tools@main".into()]).await.unwrap();
    assert!(as_bob[0].sha.is_none(), "{as_bob:?}");
    // A scheduled run has no person behind it. It serves its own repository and public ones, and
    // nothing else that is merely the same owner's: whoever pushed the workflow may not be able
    // to read that repository.
    let j4 = job(&h, &a, private_app, 4).await;
    sqlx::query("UPDATE workflow_runs SET actor_id = NULL WHERE id = $1").bind(j4.run_id).execute(&h.pool).await.unwrap();
    let as_nobody = resolve_for_job(&ctx, &j4, &["haven_alice/tools@main".into(), "haven_alice/private-app@main".into()]).await.unwrap();
    assert!(as_nobody[0].sha.is_none(), "same owner, no person: {as_nobody:?}");
    assert!(as_nobody[1].sha.is_some(), "its own repository: {as_nobody:?}");
    // Never a pull request's ref.
    let refs = resolve_for_job(&ctx, &j, &["haven_alice/shared@refs/pull/1/head".into()]).await.unwrap();
    assert!(refs[0].sha.is_none());
}
