//! What the access suites share: one seeded world (a private repository with an issue, a pull
//! request, a release and Actions history, a hidden repository with planted secrets, an
//! organization with an internal repository), and the helpers that turn the router's registered
//! patterns into requests. `public_surface.rs` sweeps it signed out; `access_matrix.rs` sweeps it
//! with every kind of credential.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use axum::Router;
use ch_core::collab::{LabelDraft, MilestoneDraft, ReleaseDraft};
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, RepoId, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use sqlx::PgPool;
use std::sync::Arc;

pub const PLANTED: &[&str] = &[
    "alice-planted@example.org",
    "PLANTED-SECRET-ISSUE",
    "hidden-proj",
    "bobplanted",
    "PLANTED-PRIVATE-BODY",
    // Actions history recorded while the repository was private (ch-4pn.12).
    "PLANTED-PRIVATE-RUN",
    "PLANTED-PRIVATE-JOB",
    "PLANTED-PRIVATE-CHECK",
    "PLANTED-PRIVATE-STATUS",
    "PLANTED-PRIVATE-ARTIFACT",
];

pub struct World {
    pub app: Router,
    pub state: AppState,
    pub _dir: tempfile::TempDir,
    pub proj: RepoId,
    pub sha: String,
    pub blob: String,
    pub comment: i64,
    pub release: i64,
    pub asset: i64,
    /// Actions history recorded at the public tier: a workflow, a run, its job, check run and
    /// artifact. The private run sits beside it and must never show.
    pub workflow: i64,
    pub run: i64,
    pub job: i64,
    pub check: i64,
    pub artifact: i64,
    /// The private run and its job.
    pub hidden_run: i64,
    pub hidden_job: i64,
}

pub fn alice_actor(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

pub fn c() -> ClientInfo {
    ClientInfo::local()
}

pub async fn user(state: &AppState, name: &str, email: &str) -> UserId {
    state
        .accounts
        .create_user(
            &Actor::System,
            NewUser {
                username: name.into(),
                email: email.into(),
                display_name: Some(format!("{name} display")),
                is_admin: false,
                can_run_ci: false,
            },
            &c(),
        )
        .await
        .unwrap()
        .id
}

pub async fn seed(pool: PgPool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    // The sweep covers the connector's routes too, so they must be registered.
    cfg.mcp.enabled = true;
    let cfg = Arc::new(cfg);
    // Cheap password hashing: the matrix sends a Basic credential with every request, and the
    // production cost would make that minutes of hashing. What is hashed is not what is tested.
    let auth = ch_auth::AuthOptions { hash_params: ch_auth::password::HashParams::FAST_FOR_TESTS, ..Default::default() };
    let state = wiring::build_state_with(cfg.clone(), pool.clone(), auth).await.unwrap();
    let app = build_router(state.clone(), Metrics::new(), None);

    let alice = user(&state, "alice", "alice-planted@example.org").await;
    let bob = user(&state, "bobplanted", "bob@example.org").await;
    let a = alice_actor(alice);
    let new = |name: &str| NewRepo {
        owner_id: alice,
        name: name.into(),
        description: format!("{name} description"),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    let proj = state.repos.create(&a, new("proj"), &c()).await.unwrap();
    let hidden = state.repos.create(&a, new("hidden-proj"), &c()).await.unwrap();
    let sig = Signature { name: "Alice".into(), email: "alice@users.noreply.code.example.com".into(), at: chrono::Utc::now() };
    state
        .git
        .commit_file_change(
            &a,
            CommitFileChange {
                repo_id: proj.id,
                branch: "main".into(),
                new_branch: Some("feature".into()),
                expected_sha: None,
                changes: vec![FileChange { path: "src/lib.rs".into(), content: Some(b"pub fn x() {}\n".to_vec()), renamed_from: None }],
                message: "Add lib".into(),
                author: sig.clone(),
            },
            &c(),
        )
        .await
        .unwrap();
    let sha = state.git.resolve(proj.id, "refs/heads/main").await.unwrap();
    let blob = state.git.blob(proj.id, "main", "README.md", 1 << 20).await.unwrap().sha;
    let label = LabelDraft { name: "bug".into(), color: Some("d73a4a".into()), description: None };
    state.issues.create_label(&a, proj.id, label, &c()).await.unwrap();
    let ms = MilestoneDraft { title: "v1".into(), description: None, due_on: None, open: Some(true) };
    state.issues.create_milestone(&a, proj.id, ms, &c()).await.unwrap();
    let issue = NewIssue {
        repo_id: proj.id,
        title: "A public issue".into(),
        body: "hello".into(),
        labels: vec![],
        assignees: vec![],
        milestone_id: None,
    };
    state.issues.create(&a, issue, &c()).await.unwrap();
    let comment = state.issues.add_comment(&a, proj.id, 1, "a comment", &c()).await.unwrap().id.0;
    let pull = NewPull {
        repo_id: proj.id,
        title: "A pull request".into(),
        body: "please".into(),
        head_repo_id: proj.id,
        head_ref: "feature".into(),
        base_ref: "main".into(),
        draft: false,
    };
    state.pulls.create(&a, pull, &c()).await.unwrap();
    let draft = ReleaseDraft {
        tag_name: "v1".into(),
        target_commitish: Some("main".into()),
        name: Some("First".into()),
        body: Some("notes".into()),
        draft: false,
        prerelease: false,
        generate_notes: false,
        make_latest: Default::default(),
    };
    let release = state.releases.create(&a, proj.id, draft, &c()).await.unwrap().release.id;
    let body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> =
        Box::pin(futures::stream::once(async { Ok(bytes::Bytes::from_static(b"file contents\n")) }));
    let asset = state.releases.upload_asset(&a, proj.id, release, "notes.txt", None, "text/plain", body, &c()).await.unwrap().id;

    // What a stranger must never see: a private repository and its issue, an email, a member.
    let secret = NewIssue {
        repo_id: hidden.id,
        title: "PLANTED-SECRET-ISSUE".into(),
        body: "PLANTED-PRIVATE-BODY".into(),
        labels: vec![],
        assignees: vec![],
        milestone_id: None,
    };
    state.issues.create(&a, secret, &c()).await.unwrap();
    let org = state.people.create_org(&a, "harbor", "Harbor Works", &c()).await.unwrap();
    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'member') ON CONFLICT DO NOTHING")
        .bind(org.id.0)
        .bind(bob.0)
        .execute(&pool)
        .await
        .unwrap();
    let tools = NewRepo { owner_id: org.id, ..new("tools") };
    let tools = state.repos.create(&a, tools, &c()).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = 'internal' WHERE id = $1").bind(tools.id.0).execute(&pool).await.unwrap();

    let (workflow, ids) = seed_actions(&pool, proj.id, &sha).await;
    // A container image linked to the repository, and one with no repository, which stays its
    // owner's alone whatever the repository's tier (ch-4pn.13).
    sqlx::query(
        "INSERT INTO packages (owner_id, repo_id, kind, name) VALUES ($1, $2, 'container', 'proj'), ($1, NULL, 'container', 'hidden-proj')",
    )
    .bind(alice.0)
    .bind(proj.id.0)
    .execute(&pool)
    .await
    .unwrap();
    let ((run, job, check, artifact), (hidden_run, hidden_job, _, _)) = (ids[0], ids[1]);

    World {
        app,
        state,
        _dir: dir,
        proj: proj.id,
        sha,
        blob,
        comment,
        release: release.0,
        asset,
        workflow,
        run,
        job,
        check,
        artifact,
        hidden_run,
        hidden_job,
    }
}

/// One run of each recorded tier on the same commit: public (with a job, a check run, a status
/// and an artifact) and private (the same, with planted names). Rows are written directly with
/// their recorded tier, as if each had run while the repository was at that tier.
pub async fn seed_actions(pool: &PgPool, repo: RepoId, sha: &str) -> (i64, Vec<(i64, i64, i64, i64)>) {
    let workflow: i64 =
        sqlx::query_scalar("INSERT INTO workflows (repo_id, path, name) VALUES ($1, '.codehaven/workflows/ci.yml', 'ci') RETURNING id")
            .bind(repo.0)
            .fetch_one(pool)
            .await
            .unwrap();
    let mut ids = Vec::new();
    for (n, tier, label) in [(1_i64, "public", "public"), (2, "private", "PLANTED-PRIVATE")] {
        let run: i64 = sqlx::query_scalar(
            "INSERT INTO workflow_runs (repo_id, workflow_id, run_number, event, head_sha, head_branch, title, status, conclusion, recorded_tier)
             VALUES ($1, $2, $3, 'push', $4, 'main', $5, 'completed', 'success', $6) RETURNING id",
        )
        .bind(repo.0)
        .bind(workflow)
        .bind(n)
        .bind(sha)
        .bind(format!("{label}-RUN title"))
        .bind(tier)
        .fetch_one(pool)
        .await
        .unwrap();
        let job: i64 = sqlx::query_scalar(
            "INSERT INTO workflow_jobs (run_id, repo_id, job_key, name, status, conclusion) VALUES ($1, $2, 'build', $3, 'completed', 'success') RETURNING id",
        )
        .bind(run)
        .bind(repo.0)
        .bind(format!("{label}-JOB build"))
        .fetch_one(pool)
        .await
        .unwrap();
        let check: i64 = sqlx::query_scalar(
            "INSERT INTO check_runs (repo_id, head_sha, name, status, conclusion, job_id, recorded_tier)
             VALUES ($1, $2, $3, 'completed', 'success', $4, $5) RETURNING id",
        )
        .bind(repo.0)
        .bind(sha)
        .bind(format!("{label}-CHECK ci"))
        .bind(job)
        .bind(tier)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO commit_statuses (repo_id, sha, context, state, recorded_tier) VALUES ($1, $2, $3, 'success', $4)")
            .bind(repo.0)
            .bind(sha)
            .bind(format!("{label}-STATUS lint"))
            .bind(tier)
            .execute(pool)
            .await
            .unwrap();
        let artifact: i64 = sqlx::query_scalar(
            "INSERT INTO artifacts (run_id, job_id, name, blob_sha256, size, expires_at) VALUES ($1, $2, $3, $4, 1, now() + interval '1 day') RETURNING id",
        )
        .bind(run)
        .bind(job)
        .bind(format!("{label}-ARTIFACT out"))
        .bind("0".repeat(64))
        .fetch_one(pool)
        .await
        .unwrap();
        ids.push((run, job, check, artifact));
    }
    (workflow, ids)
}

/// A concrete path for a registered pattern, from seeded values. Every parameter name used by
/// any route must be known here: an unknown one fails the test with the route.
pub fn concrete(pattern: &str, w: &World, owner: &str, repo: &str) -> String {
    let p = pattern;
    // The missing world names an organization that does not exist either.
    let org = if owner == "nobody" { "nobody" } else { "harbor" };
    pattern
        .split('/')
        .map(|s| -> String {
            match s {
                "{username}" => owner.into(),
                "{owner}" | "{login}" | "{user}" => {
                    if p.starts_with("/api/v3/orgs/") || p.starts_with("/orgs/") {
                        org.into()
                    } else {
                        owner.into()
                    }
                }
                "{org}" => org.into(),
                "{repo}" => repo.into(),
                "{*rest}" => {
                    if p.starts_with("/v2/") {
                        // A pull of the image linked to the repository the world names.
                        format!("{owner}/{repo}/tags/list")
                    } else if p.contains("/archive/") {
                        "main.zip".into()
                    } else if p.contains("/commits/") || p.contains("/find/") || p.contains("/tree/") || p.contains("/branches/") {
                        "main".into()
                    } else {
                        "main/README.md".into()
                    }
                }
                "{*spec}" | "{basehead}" => "main...feature".into(),
                "{*path}" => {
                    if p.starts_with("/static/") {
                        "app.css".into()
                    } else {
                        "README.md".into()
                    }
                }
                "{*prefix}" | "{*name}" => "heads/main".into(),
                "{*file}" => "workspace.js".into(),
                "{*tag}" | "{tag}" => "v1".into(),
                "{sha}" => {
                    if p.ends_with("/git/blobs/{sha}") {
                        w.blob.clone()
                    } else {
                        w.sha.clone()
                    }
                }
                "{ref}" | "{reference}" => "main".into(),
                "{number}" => {
                    if p.contains("/pulls/") {
                        "2".into()
                    } else {
                        "1".into()
                    }
                }
                "{id}" => {
                    if p.contains("/actions/runs/") {
                        w.run.to_string()
                    } else if p.contains("/actions/jobs/") {
                        w.job.to_string()
                    } else if p.contains("/check-runs/") {
                        w.check.to_string()
                    } else if p.contains("/actions/artifacts/") {
                        w.artifact.to_string()
                    } else if p.contains("/issues/comments/") {
                        w.comment.to_string()
                    } else if p.contains("/releases/assets/") {
                        w.asset.to_string()
                    } else if p.contains("/releases/") {
                        w.release.to_string()
                    } else {
                        "1".into()
                    }
                }
                "{name}" => {
                    if p.contains("/labels/") {
                        "bug".into()
                    } else if p.contains("/releases/download/") || p.contains("/releases/latest/download/") {
                        "notes.txt".into()
                    } else if p.starts_with("/avatar/") {
                        "alice".into()
                    } else {
                        "x".into()
                    }
                }
                "{asset}" => w.asset.to_string(),
                "{run}" => w.run.to_string(),
                "{job}" => w.job.to_string(),
                "{workflow}" => w.workflow.to_string(),
                "{rid}" | "{delivery}" | "{thread}" | "{token}" | "{provider}" | "{user_id}" | "{repo_id}" | "{slug}" | "{team}"
                | "{key}" | "{run_id}" | "{jid}" | "{wid}" | "{did}" | "{digest}" | "{kind}" | "{n}" | "{layer}" | "{issue_number}"
                | "{env}" | "{build}" | "{grant}" | "{archive}" | "{method}" | "{version}" | "{file}" | "{leaf}" => "1".into(),
                other if other.starts_with('{') => panic!("route {pattern}: no fixture for parameter {other}"),
                other => other.into(),
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Pulls every registered path out of axum's `Debug` output for the router, as the surface test
/// next door does.
pub fn registered_paths(router: &Router<AppState>) -> Vec<String> {
    let dbg = format!("{router:?}");
    let start = dbg.find("path_router: PathRouter").expect("path_router in Debug output");
    let rest = &dbg[start..];
    let end = rest.find("fallback_router").unwrap_or(rest.len());
    let section = &rest[..end];
    let paths_at = section.find("paths: {").expect("paths in Debug output");
    let tail = &section[paths_at..];
    let mut out = vec![];
    let mut chars = tail.char_indices();
    while let Some((i, ch)) = chars.next() {
        if ch == '"' {
            let from = i + 1;
            let mut to = from;
            for (j, c2) in chars.by_ref() {
                if c2 == '"' {
                    to = j;
                    break;
                }
            }
            out.push(tail[from..to].to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}
