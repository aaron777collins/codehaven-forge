//! Code search inside one repository (ch-tnt). It needs an account: the route is not on the
//! signed-out list, and the service answers nothing to anyone without one.

use super::people::{CodeRow, code_rows};
use super::repo_common::RepoCtx;
use super::{Params, Query};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::WebResult;
use askama::Template;
use axum::extract::Path;
use axum::response::Response;
use ch_core::model::CodeHit;

#[derive(Template)]
#[template(path = "repo/search.html")]
struct RepoSearchPage {
    c: Chrome,
    base: String,
    q: String,
    branch: String,
    rows: Vec<CodeRow>,
}

pub async fn search_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    ctx.user_id()?;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let query = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let mut rows = Vec::new();
    if !query.is_empty() {
        let hits = ctx.state.search.code(&ctx.actor, rc.id(), &query, 100).await?;
        let full = rc.repo.full_name();
        let as_code: Vec<CodeHit> = hits
            .into_iter()
            .map(|h| CodeHit {
                repo_id: rc.id(),
                repo: full.clone(),
                branch: rc.repo.default_branch.clone(),
                path: h.title,
                line: h.id,
                text: h.snippet,
            })
            .collect();
        rows = code_rows(&as_code);
    }
    let c = rc.chrome(&ctx, "Search code", "search").await;
    ctx.page(&RepoSearchPage { c, base: rc.base(), q: query, branch: rc.repo.default_branch.clone(), rows })
}
