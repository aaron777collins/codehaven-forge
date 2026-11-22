#![allow(clippy::unwrap_used)]

mod common;

use ch_core::Actor;
use ch_core::model::*;
use common::*;
use sqlx::PgPool;

fn titles(hits: &[SearchHit]) -> Vec<String> {
    hits.iter().map(|h| h.title.clone()).collect()
}

async fn world(pool: PgPool) -> World {
    let w = World::new(pool).await;
    let lib = w.env.public_repo(w.alice, "public-lib").await;
    sqlx::query("UPDATE repositories SET description = 'A forge for friends' WHERE id = $1").bind(lib).execute(&w.pool).await.unwrap();
    w.env.repo(w.eve, "secret").await;
    w
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn repository_search_never_shows_what_the_caller_cannot_read(pool: PgPool) {
    let w = world(pool).await;
    let hits = w.core.search_repos(&w.carol_a(), "demo", 10).await.unwrap();
    assert_eq!(titles(&hits), vec!["alice/demo"]);
    assert_eq!(hits[0].kind, "repo");
    assert!(w.core.search_repos(&w.carol_a(), "secret", 10).await.unwrap().is_empty());
    assert_eq!(titles(&w.core.search_repos(&w.eve_a(), "secret", 10).await.unwrap()), vec!["eve/secret"]);
    // Eve is a signed-in stranger: she sees the public one and her own, not alice's private repo.
    let all = titles(&w.core.search_repos(&w.eve_a(), "", 10).await.unwrap());
    assert!(
        all.contains(&"alice/public-lib".to_string())
            && all.contains(&"eve/secret".to_string())
            && !all.contains(&"alice/demo".to_string())
    );
    assert_eq!(
        titles(&w.core.search_repos(&w.carol_a(), "forge", 10).await.unwrap()),
        vec!["alice/public-lib"],
        "descriptions are searched too"
    );
    assert_eq!(titles(&w.core.search_repos(&w.carol_a(), "pub", 10).await.unwrap()), vec!["alice/public-lib"], "and partial names");
    assert_eq!(w.core.search_repos(&w.carol_a(), "", 1).await.unwrap().len(), 1, "the limit holds");
    assert!(w.core.search_repos(&Actor::Anonymous, "demo", 10).await.unwrap().is_empty());
    // Odd text is only text.
    for q in ["%", "_", "'; select 1; --", "\\", "\u{0}", "a & b | !c"] {
        assert!(w.core.search_repos(&w.carol_a(), q, 10).await.is_ok(), "{q:?}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_hidden_match_does_not_use_up_the_limit(pool: PgPool) {
    let w = world(pool).await;
    // Many private repositories that match, then one public one the caller can read.
    for i in 0..40 {
        w.env.repo(w.eve, &format!("zzz-private-{i}")).await;
    }
    w.env.public_repo(w.eve, "zzz-public").await;
    let hits = w.core.search_repos(&w.carol_a(), "zzz", 3).await.unwrap();
    assert_eq!(titles(&hits), vec!["eve/zzz-public"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn issue_search_is_ranked_and_filtered_by_permission(pool: PgPool) {
    let w = world(pool).await;
    let lib = w.env.repo(w.alice, "scratch").await;
    let mk = |repo, actor: Actor, title: &str, body: &str| {
        let core = w.core.clone();
        let c = w.c();
        let (title, body) = (title.to_string(), body.to_string());
        async move {
            core.issue_create(&actor, NewIssue { repo_id: repo, title, body, labels: vec![], assignees: vec![], milestone_id: None }, &c)
                .await
                .unwrap()
        }
    };
    mk(w.repo, w.alice_a(), "Login crash", "").await;
    mk(w.repo, w.alice_a(), "Crash report", "crash crash").await;
    mk(w.repo, w.alice_a(), "Unrelated", "nothing here").await;
    let secret_repo = w.env.repo(w.eve, "hidden").await;
    mk(secret_repo, w.eve_a(), "Crash in secret", "").await;
    mk(lib, w.alice_a(), "Crash in scratch", "").await;
    w.pull(w.bob, "A crash fix", "").await;

    let hits = w.core.search_issues(&w.carol_a(), "crash", 10).await.unwrap();
    let got = titles(&hits);
    assert_eq!(got[0], "Crash report", "more matches rank first");
    assert!(got.contains(&"Login crash".to_string()) && got.contains(&"A crash fix".to_string()));
    assert!(!got.contains(&"Crash in secret".to_string()), "carol cannot read eve's repository");
    assert!(!got.contains(&"Crash in scratch".to_string()), "scratch is alice's private repository and carol was never added");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn issue_search_understands_qualifiers(pool: PgPool) {
    let w = world(pool).await;
    w.issue(w.alice, "Login crash", "").await;
    w.issue(w.alice, "Crash report", "").await;
    w.pull(w.bob, "A crash fix", "").await;
    let a = w.carol_a();
    assert_eq!(
        w.core.search_issues(&a, "is:pr crash", 10).await.unwrap().iter().map(|h| h.kind.as_str()).collect::<Vec<_>>(),
        vec!["pull"]
    );
    assert_eq!(w.core.search_issues(&a, "is:issue crash", 10).await.unwrap().len(), 2);
    assert_eq!(w.core.search_issues(&a, "repo:alice/demo is:issue", 10).await.unwrap().len(), 2);
    assert!(w.core.search_issues(&a, "repo:alice/nothing", 10).await.unwrap().is_empty());
    assert_eq!(w.core.search_issues(&a, "author:bob", 10).await.unwrap().len(), 1);
    assert!(w.core.search_issues(&Actor::Anonymous, "crash", 10).await.unwrap().is_empty());
    let first = w.core.search_issues(&a, "is:issue sort:created-asc", 10).await.unwrap();
    assert_eq!(titles(&first), vec!["Login crash", "Crash report"]);
    assert_eq!(w.core.search_issues(&a, "is:issue", 1).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn user_search_matches_names_and_hides_the_rest(pool: PgPool) {
    let w = world(pool).await;
    sqlx::query("UPDATE users SET display_name = 'Dave Dribble' WHERE id = $1").bind(w.dave).execute(&w.pool).await.unwrap();
    w.env.org("acme-labs").await;
    sqlx::query("INSERT INTO users (kind, username) VALUES ('bot', 'acme-bot')").execute(&w.pool).await.unwrap();
    let gone = w.env.user("acme-gone").await;
    sqlx::query("UPDATE users SET suspended_at = now() WHERE id = $1").bind(gone).execute(&w.pool).await.unwrap();

    let a = w.carol_a();
    assert_eq!(titles(&w.core.search_users(&a, "ali", 10).await.unwrap()), vec!["alice"]);
    assert_eq!(titles(&w.core.search_users(&a, "dribble", 10).await.unwrap()), vec!["dave"], "display names count");
    assert_eq!(titles(&w.core.search_users(&a, "acme", 10).await.unwrap()), vec!["acme-labs"], "orgs yes, bots and suspended accounts no");
    assert!(w.core.search_users(&a, "", 10).await.unwrap().is_empty());
    assert!(w.core.search_users(&Actor::Anonymous, "ali", 10).await.unwrap().is_empty());
    assert_eq!(w.core.search_users(&a, "a", 2).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn code_search_asks_git_and_checks_access_first(pool: PgPool) {
    let w = world(pool).await;
    w.git.st.lock().unwrap().grep_hits =
        vec![SearchHit { kind: "code".into(), id: 1, title: "src/lib.rs".into(), snippet: "fn needle() {}".into() }];
    let hits = w.core.search_code(&w.carol_a(), w.repo, "needle", 10).await.unwrap();
    assert_eq!(hits[0].title, "src/lib.rs");
    assert!(
        w.core.search_code(&w.eve_a(), w.repo, "needle", 10).await.unwrap().is_empty(),
        "no access, no hits and no error to learn from"
    );
    assert!(w.core.search_code(&w.carol_a(), w.repo, "  ", 10).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_trait_objects_agree_with_the_engine(pool: PgPool) {
    use ch_core::services::Search;
    let w = world(pool).await;
    let state = w.app_state().await;
    let s: &dyn Search = &*state.search;
    assert_eq!(titles(&s.repos(&w.carol_a(), "demo", 5).await.unwrap()), vec!["alice/demo"]);
    assert_eq!(titles(&s.users(&w.carol_a(), "ali", 5).await.unwrap()), vec!["alice"]);
    assert!(s.issues(&w.carol_a(), "nothing", 5).await.unwrap().is_empty());
    assert!(s.code(&w.carol_a(), w.repo, "x", 5).await.unwrap().is_empty());
}
