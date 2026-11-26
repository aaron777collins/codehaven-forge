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
        make_latest: Default::default(),
    }
}

fn body(data: &'static [u8]) -> futures::stream::BoxStream<'static, std::io::Result<Bytes>> {
    stream::iter(vec![Ok(Bytes::from_static(data))]).boxed()
}

async fn read_all(mut s: futures::stream::BoxStream<'static, std::io::Result<Bytes>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn publishing_creates_the_tag_and_announces_it(pool: PgPool) {
    let w = World::new(pool).await;
    let v = w
        .core
        .release_create(
            &w.bob_a(),
            w.repo,
            ReleaseDraft { name: Some("First".into()), body: Some("Notes".into()), ..draft("v1.0.0") },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!((v.release.tag_name.as_str(), v.release.name.as_str(), v.release.body.as_str()), ("v1.0.0", "First", "Notes"));
    assert!(v.release.published_at.is_some() && !v.release.draft);
    assert_eq!(v.target_commitish, "main");
    assert_eq!(v.author.username, "bob");
    assert_eq!(w.git.created_refs(), vec![(w.repo.0, "refs/tags/v1.0.0".to_string(), sha_of(1))]);
    assert_eq!(w.count_events("release_published").await, 1);
    assert!(w.audit_actions().await.contains(&"release.published".to_string()));
    // The name defaults to the tag.
    let v2 = w.core.release_create(&w.bob_a(), w.repo, draft("v1.1.0"), &w.c()).await.unwrap();
    assert_eq!(v2.release.name, "v1.1.0");
    // A tag that already exists is used as it is.
    w.git.set_ref(w.repo, "refs/tags/v2.0.0", &sha_of(9));
    w.core.release_create(&w.bob_a(), w.repo, draft("v2.0.0"), &w.c()).await.unwrap();
    assert_eq!(w.git.created_refs().len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_releases_are_refused(pool: PgPool) {
    let w = World::new(pool).await;
    w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap();
    assert!(matches!(w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await, Err(Error::Validation(_))), "duplicate tag");
    for bad in ["", "has space", "a..b", "-x", "x.lock"] {
        assert!(matches!(w.core.release_create(&w.bob_a(), w.repo, draft(bad), &w.c()).await, Err(Error::Validation(_))), "{bad:?}");
    }
    let r =
        w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { target_commitish: Some("nowhere".into()), ..draft("v3") }, &w.c()).await;
    assert!(matches!(r, Err(Error::Validation(_))));
    assert_eq!(w.env.count("releases").await, 1, "a failed publish leaves no row");
    assert!(matches!(w.core.release_create(&w.carol_a(), w.repo, draft("v4"), &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.release_create(&w.dave_a(), w.repo, draft("v4"), &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.release_create(&w.eve_a(), w.repo, draft("v4"), &w.c()).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn drafts_are_for_writers_until_published(pool: PgPool) {
    let w = World::new(pool).await;
    let d = w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { draft: true, ..draft("v1") }, &w.c()).await.unwrap();
    assert!(d.release.draft && d.release.published_at.is_none());
    assert!(w.git.created_refs().is_empty(), "no tag until it is published");
    assert_eq!(w.count_events("release_published").await, 0);

    assert_eq!(w.core.release_views(&w.bob_a(), w.repo).await.unwrap().len(), 1);
    assert!(w.core.release_views(&w.carol_a(), w.repo).await.unwrap().is_empty());
    assert!(matches!(w.core.release_view_by_tag(&w.carol_a(), w.repo, "v1").await, Err(Error::NotFound)));
    assert!(matches!(w.core.release_view(&w.carol_a(), w.repo, d.release.id).await, Err(Error::NotFound)));
    assert!(matches!(w.core.release_latest_view(&w.bob_a(), w.repo).await, Err(Error::NotFound)));

    let p = w
        .core
        .release_update(&w.bob_a(), w.repo, d.release.id, ReleaseEdit { draft: Some(false), ..Default::default() }, &w.c())
        .await
        .unwrap();
    assert!(!p.release.draft && p.release.published_at.is_some());
    assert_eq!(w.git.created_refs().len(), 1);
    assert_eq!(w.count_events("release_published").await, 1);
    assert_eq!(w.core.release_view_by_tag(&w.carol_a(), w.repo, "v1").await.unwrap().release.id, d.release.id);
    // Editing a published release does not announce it again.
    w.core
        .release_update(&w.bob_a(), w.repo, d.release.id, ReleaseEdit { body: Some("edited".into()), ..Default::default() }, &w.c())
        .await
        .unwrap();
    assert_eq!(w.count_events("release_published").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn latest_skips_prereleases_and_drafts(pool: PgPool) {
    let w = World::new(pool).await;
    w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap();
    w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { prerelease: true, ..draft("v2-rc1") }, &w.c()).await.unwrap();
    w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { draft: true, ..draft("v3") }, &w.c()).await.unwrap();
    assert_eq!(w.core.release_latest_view(&w.carol_a(), w.repo).await.unwrap().release.tag_name, "v1");
    let listed = w.core.release_views(&w.bob_a(), w.repo).await.unwrap();
    assert_eq!(listed.len(), 3);
    // Renaming into another release's tag is refused.
    let id = listed.iter().find(|v| v.release.tag_name == "v1").unwrap().release.id;
    let r =
        w.core.release_update(&w.bob_a(), w.repo, id, ReleaseEdit { tag_name: Some("v2-rc1".into()), ..Default::default() }, &w.c()).await;
    assert!(matches!(r, Err(Error::Validation(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn assets_are_stored_counted_and_removed(pool: PgPool) {
    let w = World::new(pool).await;
    let r = w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap().release.id;
    let a = w
        .core
        .release_upload_asset(&w.bob_a(), w.repo, r, "haven.tar.gz", Some("Source"), "application/gzip", body(b"hello world"), &w.c())
        .await
        .unwrap();
    assert_eq!(
        (a.name.as_str(), a.size, a.content_type.as_str(), a.label.as_str(), a.download_count),
        ("haven.tar.gz", 11, "application/gzip", "Source", 0)
    );
    assert_eq!(a.uploader.as_ref().unwrap().username, "bob");

    let dup = w.core.release_upload_asset(&w.bob_a(), w.repo, r, "haven.tar.gz", None, "", body(b"other"), &w.c()).await;
    assert!(matches!(dup, Err(Error::Validation(_))));
    for bad in ["", "a/b", "..", "a\nb"] {
        assert!(
            matches!(
                w.core.release_upload_asset(&w.bob_a(), w.repo, r, bad, None, "", body(b"x"), &w.c()).await,
                Err(Error::Validation(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        w.core.release_upload_asset(&w.carol_a(), w.repo, r, "x", None, "", body(b"x"), &w.c()).await,
        Err(Error::Forbidden(_))
    ));

    let (info, s) = w.core.release_open_asset(&w.carol_a(), w.repo, "v1", "haven.tar.gz").await.unwrap();
    assert_eq!(info.download_count, 1);
    assert_eq!(read_all(s).await, b"hello world");
    let (_, s2) = w.core.release_open_asset_by_id(&w.carol_a(), w.repo, a.id).await.unwrap();
    assert_eq!(read_all(s2).await, b"hello world");
    assert_eq!(w.core.release_asset(&w.carol_a(), w.repo, a.id).await.unwrap().download_count, 2);
    assert!(matches!(w.core.release_open_asset(&w.eve_a(), w.repo, "v1", "haven.tar.gz").await, Err(Error::NotFound)));
    assert!(matches!(w.core.release_open_asset(&w.carol_a(), w.repo, "v1", "missing").await, Err(Error::NotFound)));
    let view = w.core.release_view(&w.carol_a(), w.repo, r).await.unwrap();
    assert_eq!(view.assets.len(), 1);

    // The same bytes under two names share one stored file until both are gone.
    let b = w.core.release_upload_asset(&w.bob_a(), w.repo, r, "copy.tar.gz", None, "", body(b"hello world"), &w.c()).await.unwrap();
    let refcount =
        |pool: PgPool| async move { sqlx::query_scalar::<_, i64>("SELECT refcount::bigint FROM blobs").fetch_one(&pool).await.unwrap() };
    assert_eq!(refcount(w.pool.clone()).await, 2);
    w.core.release_delete_asset(&w.bob_a(), w.repo, b.id, &w.c()).await.unwrap();
    assert_eq!(refcount(w.pool.clone()).await, 1);
    assert!(matches!(w.core.release_delete_asset(&w.carol_a(), w.repo, a.id, &w.c()).await, Err(Error::Forbidden(_))));
    w.core.release_delete(&w.bob_a(), w.repo, r, &w.c()).await.unwrap();
    assert_eq!(refcount(w.pool.clone()).await, 0);
    assert_eq!(w.env.count("release_assets").await, 0);
    assert!(w.audit_actions().await.contains(&"release.deleted".to_string()));
    // Whatever happened, the git tag is left alone.
    assert!(w.git.deleted_refs().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn draft_assets_are_for_writers(pool: PgPool) {
    let w = World::new(pool).await;
    let r = w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { draft: true, ..draft("v1") }, &w.c()).await.unwrap().release.id;
    let a = w.core.release_upload_asset(&w.bob_a(), w.repo, r, "x.bin", None, "", body(b"x"), &w.c()).await.unwrap();
    assert!(matches!(w.core.release_asset(&w.carol_a(), w.repo, a.id).await, Err(Error::NotFound)));
    assert!(matches!(w.core.release_open_asset_by_id(&w.carol_a(), w.repo, a.id).await, Err(Error::NotFound)));
    assert!(matches!(w.core.release_open_asset(&w.carol_a(), w.repo, "v1", "x.bin").await, Err(Error::NotFound)));
    assert!(w.core.release_asset(&w.bob_a(), w.repo, a.id).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn notes_list_merged_pull_requests_since_the_last_release(pool: PgPool) {
    let w = World::new(pool).await;
    for l in ["bug", "enhancement", "breaking"] {
        w.label(l).await;
    }
    w.core.release_create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap();
    let a = w.pull_from(w.carol, "feature", "Add search", "").await.pull.issue.number;
    let b = w.pull_from(w.carol, "other", "Fix crash", "").await.pull.issue.number;
    w.git.branch(w.repo, "old", &sha_of(15));
    let old = w.pull_from(w.carol, "old", "Ancient history", "").await.pull.issue.number;
    w.core.issue_add_labels(&w.dave_a(), w.repo, a, vec!["enhancement".into()], &w.c()).await.unwrap();
    w.core.issue_add_labels(&w.dave_a(), w.repo, b, vec!["bug".into()], &w.c()).await.unwrap();
    for n in [a, b, old] {
        w.core.pull_merge(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    }
    // The commits from v1 to main hold the merges of the first two, and not the third (it went
    // in on another line of history, whatever the clock says).
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
    let commits = vec![commit_info(&merge_sha(a).await, "Merge a"), commit_info(&merge_sha(b).await, "Merge b")];
    w.git.set_compare(
        w.repo,
        "refs/tags/v1",
        "main",
        Compare {
            base_sha: sha_of(1),
            head_sha: sha_of(2),
            merge_base: sha_of(1),
            ahead_by: 2,
            behind_by: 0,
            commits,
            diff: default_diff(),
        },
    );

    let notes = w.core.release_generate_notes(&w.bob_a(), w.repo, "v2", None, None).await.unwrap();
    assert_eq!(notes.name, "v2");
    assert!(notes.body.contains("## New features\n\n* Add search (#"), "{}", notes.body);
    assert!(notes.body.contains("## Fixes\n\n* Fix crash (#"));
    assert!(notes.body.contains("by @carol"));
    assert!(!notes.body.contains("Ancient history"));
    assert!(notes.body.contains("**Full changelog**: https://test.example/alice/demo/compare/v1...v2"));

    assert!(matches!(w.core.release_generate_notes(&w.bob_a(), w.repo, "v2", Some("nope"), None).await, Err(Error::Validation(_))));
    assert!(matches!(w.core.release_generate_notes(&w.carol_a(), w.repo, "v2", None, None).await, Err(Error::Forbidden(_))));
    // Creating with `generate_notes` writes them into the release.
    let r = w.core.release_create(&w.bob_a(), w.repo, ReleaseDraft { generate_notes: true, ..draft("v2") }, &w.c()).await.unwrap();
    assert!(r.release.body.contains("Add search"));
    // Notes that were written by hand are not overwritten.
    let r = w
        .core
        .release_create(&w.bob_a(), w.repo, ReleaseDraft { generate_notes: true, body: Some("mine".into()), ..draft("v3") }, &w.c())
        .await
        .unwrap();
    assert_eq!(r.release.body, "mine");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_trait_objects_agree_with_the_engine(pool: PgPool) {
    use ch_core::services::Releases;
    let w = World::new(pool).await;
    let state = w.app_state().await;
    let r: &dyn Releases = &*state.releases;
    let created = r.create(&w.bob_a(), w.repo, draft("v1"), &w.c()).await.unwrap();
    assert_eq!(r.list(&w.carol_a(), w.repo).await.unwrap()[0].tag_name, "v1");
    assert_eq!(r.get_by_tag(&w.carol_a(), w.repo, "v1").await.unwrap().id, created.release.id);
    assert_eq!(r.latest(&w.carol_a(), w.repo).await.unwrap().id, created.release.id);
}
