//! Releases for release actions (ch-ctk.10): which release is the latest, asset renames and
//! digests, and generated notes that follow the commits between two tags.
#![allow(clippy::unwrap_used)]

mod common;

use bytes::Bytes;
use ch_core::Error;
use ch_core::collab::*;
use ch_core::model::*;
use common::*;
use futures::StreamExt;
use futures::stream;
use sqlx::PgPool;

fn draft(tag: &str) -> ReleaseDraft {
    ReleaseDraft {
        tag_name: tag.into(),
        target_commitish: None,
        name: None,
        body: None,
        draft: false,
        prerelease: false,
        generate_notes: false,
        make_latest: MakeLatest::True,
    }
}

fn body(data: &'static [u8]) -> futures::stream::BoxStream<'static, std::io::Result<Bytes>> {
    stream::iter(vec![Ok(Bytes::from_static(data))]).boxed()
}

async fn latest(w: &World) -> Option<String> {
    w.core.release_latest_view(&w.carol_a(), w.repo).await.ok().map(|v| v.release.tag_name)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn latest_follows_the_mark_not_the_clock(pool: PgPool) {
    let w = World::new(pool).await;
    let b = w.bob_a();
    w.core.release_create(&b, w.repo, draft("v1.0.0"), &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v1.0.0"));

    // `false` keeps a newer release from becoming the latest.
    w.core.release_create(&b, w.repo, ReleaseDraft { make_latest: MakeLatest::False, ..draft("v1.1.0") }, &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v1.0.0"));

    // `true` (the default) takes it.
    w.core.release_create(&b, w.repo, draft("v2.0.0"), &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v2.0.0"));

    // `legacy` marks only a release whose version is the highest published so far.
    w.core.release_create(&b, w.repo, ReleaseDraft { make_latest: MakeLatest::Legacy, ..draft("v1.5.0") }, &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v2.0.0"), "an older version does not take the mark");
    w.core.release_create(&b, w.repo, ReleaseDraft { make_latest: MakeLatest::Legacy, ..draft("v3.0.0") }, &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v3.0.0"));

    // An edit can move the mark back to an older release.
    let old = w.core.release_view_by_tag(&b, w.repo, "v1.0.0").await.unwrap().release.id;
    w.core
        .release_update(&b, w.repo, old, ReleaseEdit { make_latest: Some(MakeLatest::True), ..Default::default() }, &w.c())
        .await
        .unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v1.0.0"));

    // A prerelease and a draft are never the latest, whatever they ask for.
    w.core.release_create(&b, w.repo, ReleaseDraft { prerelease: true, ..draft("v9.0.0-rc.1") }, &w.c()).await.unwrap();
    w.core.release_create(&b, w.repo, ReleaseDraft { draft: true, ..draft("v9.0.0") }, &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v1.0.0"));

    // Publishing the draft marks it, as creating a release does.
    let d = w.core.release_view_by_tag(&b, w.repo, "v9.0.0").await.unwrap().release.id;
    w.core.release_update(&b, w.repo, d, ReleaseEdit { draft: Some(false), ..Default::default() }, &w.c()).await.unwrap();
    assert_eq!(latest(&w).await.as_deref(), Some("v9.0.0"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_release_that_declines_the_mark_is_not_the_latest_even_alone(pool: PgPool) {
    let w = World::new(pool).await;
    w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { make_latest: MakeLatest::False, ..draft("v1.0.0") }, &w.c()).await.unwrap();
    assert_eq!(latest(&w).await, None);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_asset_has_a_digest_and_can_be_renamed(pool: PgPool) {
    let w = World::new(pool).await;
    let r = w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap().release.id;
    let a = w.core.release_upload_asset(&w.bob_a(), w.repo, r, "a.txt", None, "text/plain", body(b"hello world"), &w.c()).await.unwrap();
    assert_eq!(a.sha256, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    let b = w.core.release_upload_asset(&w.bob_a(), w.repo, r, "b.txt", None, "text/plain", body(b"other"), &w.c()).await.unwrap();

    let renamed = w.core.release_update_asset(&w.bob_a(), w.repo, a.id, Some("renamed.txt"), Some("A label"), &w.c()).await.unwrap();
    assert_eq!((renamed.name.as_str(), renamed.label.as_str()), ("renamed.txt", "A label"));
    assert_eq!(renamed.sha256, a.sha256, "the bytes did not change");
    // Only the label.
    let relabel = w.core.release_update_asset(&w.bob_a(), w.repo, a.id, None, Some(""), &w.c()).await.unwrap();
    assert_eq!((relabel.name.as_str(), relabel.label.as_str()), ("renamed.txt", ""));
    // The name of another asset of the release is taken.
    let clash = w.core.release_update_asset(&w.bob_a(), w.repo, a.id, Some("b.txt"), None, &w.c()).await;
    assert!(matches!(clash, Err(Error::Validation(_))), "{clash:?}");
    // Keeping its own name is no clash.
    assert!(w.core.release_update_asset(&w.bob_a(), w.repo, b.id, Some("b.txt"), None, &w.c()).await.is_ok());
    for bad in ["", "a/b", "..", "a\nb"] {
        let r = w.core.release_update_asset(&w.bob_a(), w.repo, a.id, Some(bad), None, &w.c()).await;
        assert!(matches!(r, Err(Error::Validation(_))), "{bad:?}");
    }
    assert!(matches!(w.core.release_update_asset(&w.carol_a(), w.repo, a.id, Some("x"), None, &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.release_update_asset(&w.eve_a(), w.repo, a.id, Some("x"), None, &w.c()).await, Err(Error::NotFound)));
    assert!(matches!(w.core.release_update_asset(&w.bob_a(), w.repo, 99_999, Some("x"), None, &w.c()).await, Err(Error::NotFound)));
    // The download by the new name works and the old name is gone.
    assert!(w.core.release_open_asset(&w.carol_a(), w.repo, "v1", "renamed.txt").await.is_ok());
    assert!(matches!(w.core.release_open_asset(&w.carol_a(), w.repo, "v1", "a.txt").await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn notes_follow_the_commits_between_the_tags_not_the_clock(pool: PgPool) {
    let w = World::new(pool).await;
    w.label("bug").await;
    w.label("enhancement").await;
    w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap();
    let a = w.pull_from(w.carol, "feature", "Add search", "").await.pull.issue.number;
    w.git.branch(w.repo, "other", &sha_of(14));
    let b = w.pull_from(w.carol, "other", "Fix crash", "").await.pull.issue.number;
    w.git.branch(w.repo, "old", &sha_of(15));
    let old = w.pull_from(w.carol, "old", "Merged on another branch", "").await.pull.issue.number;
    w.core.issue_add_labels(&w.dave_a(), w.repo, a, vec!["enhancement".into()], &w.c()).await.unwrap();
    w.core.issue_add_labels(&w.dave_a(), w.repo, b, vec!["bug".into()], &w.c()).await.unwrap();
    for n in [a, b, old] {
        w.core.pull_merge(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    }
    let merge_sha = |n: i64| {
        let pool = w.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT merge_commit_sha FROM pull_requests WHERE issue_id = (SELECT id FROM issues WHERE number = $1)",
            )
            .bind(n)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    // The commits from v1 to the target hold the first two merges and not the third, though all
    // three were merged after v1 was cut.
    let commits = vec![commit_info(&merge_sha(a).await, "Merge a"), commit_info(&merge_sha(b).await, "Merge b")];
    w.git.set_compare(
        w.repo,
        "refs/tags/v1",
        "main",
        ch_core::model::Compare {
            base_sha: sha_of(1),
            head_sha: sha_of(2),
            merge_base: sha_of(1),
            ahead_by: 2,
            behind_by: 0,
            commits,
            diff: default_diff(),
        },
    );
    let notes = w.core.release_generate_notes(&w.bob_a(), w.repo, "v2", None, Some("main")).await.unwrap();
    assert!(notes.body.contains("* Add search (#"), "{}", notes.body);
    assert!(notes.body.contains("* Fix crash (#"), "{}", notes.body);
    assert!(!notes.body.contains("Merged on another branch"), "{}", notes.body);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn release_yml_names_the_sections_and_leaves_pull_requests_out(pool: PgPool) {
    let w = World::new(pool).await;
    for l in ["bug", "enhancement", "skip-notes"] {
        w.label(l).await;
    }
    w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap();
    let a = w.pull_from(w.carol, "feature", "Add search", "").await.pull.issue.number;
    w.git.branch(w.repo, "other", &sha_of(14));
    let b = w.pull_from(w.carol, "other", "Fix crash", "").await.pull.issue.number;
    w.git.branch(w.repo, "third", &sha_of(15));
    let c3 = w.pull_from(w.carol, "third", "Tidy the build", "").await.pull.issue.number;
    w.core.issue_add_labels(&w.dave_a(), w.repo, a, vec!["enhancement".into()], &w.c()).await.unwrap();
    w.core.issue_add_labels(&w.dave_a(), w.repo, b, vec!["bug".into()], &w.c()).await.unwrap();
    w.core.issue_add_labels(&w.dave_a(), w.repo, c3, vec!["skip-notes".into()], &w.c()).await.unwrap();
    for n in [a, b, c3] {
        w.core.pull_merge(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    }
    let mut commits = Vec::new();
    for n in [a, b, c3] {
        let sha: String =
            sqlx::query_scalar("SELECT merge_commit_sha FROM pull_requests WHERE issue_id = (SELECT id FROM issues WHERE number = $1)")
                .bind(n)
                .fetch_one(&w.pool)
                .await
                .unwrap();
        commits.push(commit_info(&sha, "Merge"));
    }
    w.git.set_compare(
        w.repo,
        "refs/tags/v1",
        "main",
        ch_core::model::Compare {
            base_sha: sha_of(1),
            head_sha: sha_of(2),
            merge_base: sha_of(1),
            ahead_by: 3,
            behind_by: 0,
            commits,
            diff: default_diff(),
        },
    );
    // Without a file the built-in groups are used.
    let plain = w.core.release_generate_notes(&w.bob_a(), w.repo, "v2", None, Some("main")).await.unwrap();
    assert!(plain.body.contains("## New features") && plain.body.contains("Tidy the build"), "{}", plain.body);
    // With one, its sections and its exclusions rule.
    w.git.put_file(
        w.repo,
        ".github/release.yml",
        "changelog:\n  exclude:\n    labels: [skip-notes]\n  categories:\n    - title: Shiny things\n      labels: [enhancement]\n    - title: Everything else\n      labels: ['*']\n",
    );
    let notes = w.core.release_generate_notes(&w.bob_a(), w.repo, "v2", None, Some("main")).await.unwrap();
    assert!(notes.body.contains("## Shiny things\n\n* Add search (#"), "{}", notes.body);
    assert!(notes.body.contains("## Everything else\n\n* Fix crash (#"), "{}", notes.body);
    assert!(!notes.body.contains("Tidy the build") && !notes.body.contains("New features"), "{}", notes.body);
    assert!(notes.body.contains("**Full changelog**: https://test.example/alice/demo/compare/v1...v2"));
    // A file that does not parse is the built-in grouping, never an error.
    w.git.put_file(w.repo, ".github/release.yml", "changelog: [not, a, mapping]\n");
    let fallback = w.core.release_generate_notes(&w.bob_a(), w.repo, "v2", None, Some("main")).await.unwrap();
    assert!(fallback.body.contains("## New features"), "{}", fallback.body);
}
