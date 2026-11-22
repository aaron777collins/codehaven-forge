//! `/search/repositories`, `/search/issues`, `/search/users` and `/search/code`.

use super::dto::Ctx;
use super::{CurrentActor, Paging, ok, repo_of, repository_dto};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use ch_api_types::search::{CodeSearchItem, SearchResult, TextMatch};
use ch_core::collab::UserRef;
use ch_core::model::UserKind;
use ch_core::state::AppState;
use ch_core::{Error, FieldError, IssueId, RepoId, Result, UserId};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/search/repositories", get(repositories))
        .route("/search/issues", get(issues))
        .route("/search/users", get(users))
        .route("/search/code", get(code))
}

#[derive(Debug, Default, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

fn window<T>(items: Vec<T>, paging: Paging) -> Vec<T> {
    let per = paging.per_page() as usize;
    items.into_iter().skip((paging.page() as usize - 1) * per).take(per).collect()
}

/// How many hits to ask for so that `page` is full.
fn want(paging: Paging) -> u32 {
    (paging.page() * paging.per_page()).min(1000)
}

fn envelope<T>(items: Vec<T>) -> SearchResult<T> {
    SearchResult { total_count: items.len() as i64, incomplete_results: false, items }
}

async fn repositories(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<SearchQuery>,
    Query(paging): Query<Paging>,
) -> Result<Response> {
    let hits = window(s.search.repos(&actor, &q.q, want(paging)).await?, paging);
    let mut items = Vec::new();
    for h in hits {
        if let Ok(r) = s.repos.get(&actor, RepoId(h.id)).await
            && let Ok(dto) = repository_dto(&s, &r).await
        {
            items.push(dto);
        }
    }
    Ok(ok(envelope(items)))
}

async fn issues(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<SearchQuery>,
    Query(paging): Query<Paging>,
) -> Result<Response> {
    let hits = window(s.search.issues(&actor, &q.q, want(paging)).await?, paging);
    let ids: Vec<IssueId> = hits.iter().map(|h| IssueId(h.id)).collect();
    let views = s.issues.views_by_ids(&actor, &ids).await?;
    let base = s.config.server.external_url.clone();
    Ok(ok(envelope(views.iter().map(|v| Ctx::from_full_name(&base, &v.repo_full_name).issue(v)).collect::<Vec<_>>())))
}

async fn users(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<SearchQuery>,
    Query(paging): Query<Paging>,
) -> Result<Response> {
    let hits = window(s.search.users(&actor, &q.q, want(paging)).await?, paging);
    let ctx = Ctx::new(&s.config.server.external_url, "", "");
    let mut items = Vec::new();
    for h in hits {
        if let Ok(u) = s.accounts.get_user(UserId(h.id)).await {
            items.push(ctx.user(&UserRef { id: u.id, username: u.username, display_name: u.display_name, kind: u.kind }));
        }
    }
    let _ = UserKind::User;
    Ok(ok(envelope(items)))
}

/// Code search is per repository for now: put `repo:owner/name` in the query.
async fn code(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<SearchQuery>,
    Query(paging): Query<Paging>,
) -> Result<Response> {
    let parsed = crate::query::parse(&q.q);
    let Some((owner, name)) = parsed.repo else {
        return Err(Error::Validation(vec![FieldError::custom(
            "Search",
            "q",
            "Code search needs a repository. Add repo:owner/name to the query",
        )]));
    };
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let hits = window(s.search.code(&actor, repo.id, &parsed.text, want(paging)).await?, paging);
    let sha = s.git.resolve(repo.id, &repo.default_branch).await.unwrap_or_default();
    let dto = repository_dto(&s, &repo).await?;
    let items = hits
        .into_iter()
        .map(|h| {
            let path = h.title.clone();
            CodeSearchItem {
                name: path.rsplit('/').next().unwrap_or(&path).to_string(),
                url: ctx.api(&format!("/contents/{path}")),
                html_url: ctx.html(&format!("/blob/{}/{path}", repo.default_branch)),
                path,
                sha: sha.clone(),
                repository: dto.clone(),
                text_matches: vec![TextMatch { fragment: h.snippet }],
            }
        })
        .collect();
    Ok(ok(envelope(items)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_and_wants() {
        let p = Paging { page: Some(2), per_page: Some(2) };
        assert_eq!(want(p), 4);
        assert_eq!(window(vec![1, 2, 3, 4, 5], p), vec![3, 4]);
        assert_eq!(want(Paging { page: Some(100), per_page: Some(100) }), 1000);
    }

    #[test]
    fn envelope_counts() {
        let e = envelope(vec![1, 2]);
        assert_eq!(e.total_count, 2);
        assert!(!e.incomplete_results);
    }
}
