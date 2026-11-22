#![allow(clippy::unwrap_used)]

mod common;

use ch_collab::sync;
use ch_core::Error;
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use common::*;
use sqlx::PgPool;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn label_crud_and_rules(pool: PgPool) {
    let w = World::new(pool).await;
    let l = w
        .core
        .create_label(
            &w.dave_a(),
            w.repo,
            LabelDraft { name: " Priority ".into(), color: Some("#FF8800".into()), description: Some("urgent".into()) },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!((l.name.as_str(), l.color.as_str(), l.description.as_str()), ("Priority", "ff8800", "urgent"));
    // Names are unique ignoring case.
    let dup =
        w.core.create_label(&w.dave_a(), w.repo, LabelDraft { name: "priority".into(), color: None, description: None }, &w.c()).await;
    assert!(matches!(dup, Err(Error::Validation(_))));
    assert!(matches!(
        w.core.create_label(&w.carol_a(), w.repo, LabelDraft { name: "x".into(), color: None, description: None }, &w.c()).await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        w.core
            .create_label(&w.dave_a(), w.repo, LabelDraft { name: "x".into(), color: Some("red".into()), description: None }, &w.c())
            .await,
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        w.core.create_label(&w.eve_a(), w.repo, LabelDraft { name: "x".into(), color: None, description: None }, &w.c()).await,
        Err(Error::NotFound)
    ));

    let renamed = w
        .core
        .update_label(
            &w.dave_a(),
            w.repo,
            "PRIORITY",
            LabelEdit { new_name: Some("urgent".into()), color: None, description: None },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!((renamed.id, renamed.name.as_str(), renamed.color.as_str()), (l.id, "urgent", "ff8800"));
    w.label("other").await;
    let clash = w
        .core
        .update_label(&w.dave_a(), w.repo, "other", LabelEdit { new_name: Some("URGENT".into()), ..Default::default() }, &w.c())
        .await;
    assert!(matches!(clash, Err(Error::Validation(_))));

    // Deleting a label takes it off the issues that had it.
    let n = w.issue(w.carol, "x", "").await.issue.number;
    w.core.issue_add_labels(&w.dave_a(), w.repo, n, vec!["urgent".into()], &w.c()).await.unwrap();
    w.core.delete_label(&w.dave_a(), w.repo, "urgent", &w.c()).await.unwrap();
    assert!(w.core.issue_view(&w.carol_a(), w.repo, n).await.unwrap().labels.is_empty());
    assert!(matches!(w.core.label(&w.carol_a(), w.repo, "urgent").await, Err(Error::NotFound)));
    assert!(w.audit_actions().await.contains(&"label.deleted".to_string()));
    let all = w.core.labels(&w.carol_a(), w.repo).await.unwrap();
    assert_eq!(all.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["other"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn new_repositories_get_the_starting_labels_once(pool: PgPool) {
    let w = World::new(pool).await;
    let fresh = w.env.repo(w.alice, "fresh").await;
    let mut tx = w.pool.begin().await.unwrap();
    w.core.d.events.emit(&mut tx, DomainEvent::RepoCreated { repo_id: fresh, actor: Some(w.alice) }).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(sync::run_defaults_once(&w.core).await.unwrap(), 1);
    let labels = w.core.labels(&w.alice_a(), fresh).await.unwrap();
    assert_eq!(labels.len(), ch_collab::labels::DEFAULT_LABELS.len());
    assert!(labels.iter().all(|l| l.is_default));
    // Seeding again adds nothing, and a label someone made first is kept as theirs.
    assert_eq!(w.core.seed_default_labels(fresh).await.unwrap(), 0);
    assert!(w.core.labels(&w.alice_a(), w.repo).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn milestones_number_per_repository_and_close(pool: PgPool) {
    let w = World::new(pool).await;
    let other = w.env.repo(w.alice, "other").await;
    let mk = |repo, title: &str| {
        let core = w.core.clone();
        let c = w.c();
        let actor = w.alice_a();
        let title = title.to_string();
        async move { core.create_milestone(&actor, repo, MilestoneDraft { title, description: None, due_on: None, open: None }, &c).await }
    };
    assert_eq!(mk(w.repo, "v1").await.unwrap().number, 1);
    assert_eq!(mk(w.repo, "v2").await.unwrap().number, 2);
    assert_eq!(mk(other, "v1").await.unwrap().number, 1, "a separate sequence in each repository");
    assert!(matches!(mk(w.repo, "V1").await, Err(Error::Validation(_))), "titles are unique per repository");

    let due = chrono::Utc::now() + chrono::Duration::days(7);
    let m = w
        .core
        .update_milestone(&w.alice_a(), w.repo, 2, MilestoneEdit { due_on: Some(Some(due)), ..Default::default() }, &w.c())
        .await
        .unwrap();
    assert!(m.due_on.is_some());
    let m =
        w.core.update_milestone(&w.alice_a(), w.repo, 2, MilestoneEdit { open: Some(false), ..Default::default() }, &w.c()).await.unwrap();
    assert!(!m.open && m.closed_at.is_some());
    let m = w
        .core
        .update_milestone(&w.alice_a(), w.repo, 2, MilestoneEdit { open: Some(true), due_on: Some(None), ..Default::default() }, &w.c())
        .await
        .unwrap();
    assert!(m.open && m.closed_at.is_none() && m.due_on.is_none());

    let open = w.core.milestones(&w.carol_a(), w.repo, Some(true)).await.unwrap();
    assert_eq!(open.len(), 2);
    w.core.update_milestone(&w.alice_a(), w.repo, 1, MilestoneEdit { open: Some(false), ..Default::default() }, &w.c()).await.unwrap();
    assert_eq!(w.core.milestones(&w.carol_a(), w.repo, Some(false)).await.unwrap().len(), 1);
    assert_eq!(w.core.milestones(&w.carol_a(), w.repo, None).await.unwrap().len(), 2);

    let i = w.issue(w.carol, "x", "").await;
    w.core.issue_set_milestone(&w.dave_a(), w.repo, i.issue.number, Some(2), &w.c()).await.unwrap();
    w.core.delete_milestone(&w.alice_a(), w.repo, 2, &w.c()).await.unwrap();
    assert!(w.core.issue_view(&w.carol_a(), w.repo, i.issue.number).await.unwrap().milestone.is_none());
    assert!(matches!(w.core.milestone(&w.carol_a(), w.repo, 2).await, Err(Error::NotFound)));
    assert!(matches!(w.core.delete_milestone(&w.carol_a(), w.repo, 1, &w.c()).await, Err(Error::Forbidden(_))));
}
