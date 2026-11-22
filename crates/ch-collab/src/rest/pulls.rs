//! Pull requests, reviews, review comments, requested reviewers and the merge gate.

use super::dto::Ctx;
use super::{
    ApiJson, Client, CurrentActor, OptionalJson, Paging, created, no_content, ok, open_of, page_of, repo_of, repository_dto, sort_of,
};
use axum::Router;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use ch_api_types::issues::CreateReaction;
use ch_api_types::pulls as api;
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, CommentId, Error, FieldError, Result, ReviewId, TeamId, UserId};
use serde::Deserialize;
use std::collections::HashMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{repo}/pulls", get(list_pulls).post(create_pull))
        .route("/repos/{owner}/{repo}/pulls/{number}", get(get_pull).patch(patch_pull))
        .route("/repos/{owner}/{repo}/pulls/{number}/files", get(pull_files))
        .route("/repos/{owner}/{repo}/pulls/{number}/commits", get(pull_commits))
        .route("/repos/{owner}/{repo}/pulls/{number}/merge", get(is_merged).put(merge_pull))
        .route("/repos/{owner}/{repo}/pulls/{number}/update-branch", put(update_branch))
        .route("/repos/{owner}/{repo}/pulls/{number}/merge-gate", get(merge_gate))
        .route("/repos/{owner}/{repo}/pulls/{number}/auto-merge", put(enable_auto_merge).delete(disable_auto_merge))
        .route("/repos/{owner}/{repo}/pulls/{number}/ready_for_review", post(ready_for_review))
        .route("/repos/{owner}/{repo}/pulls/{number}/convert_to_draft", post(convert_to_draft))
        .route("/repos/{owner}/{repo}/pulls/{number}/reviews", get(list_reviews).post(create_review))
        .route("/repos/{owner}/{repo}/pulls/{number}/reviews/{id}", get(get_review).delete(delete_pending_review))
        .route("/repos/{owner}/{repo}/pulls/{number}/reviews/{id}/events", post(submit_review))
        .route("/repos/{owner}/{repo}/pulls/{number}/reviews/{id}/dismissals", put(dismiss_review))
        .route("/repos/{owner}/{repo}/pulls/{number}/reviews/{id}/comments", get(review_comments_of))
        .route("/repos/{owner}/{repo}/pulls/{number}/comments", get(list_review_comments).post(create_review_comment))
        .route("/repos/{owner}/{repo}/pulls/{number}/threads", get(list_threads))
        .route("/repos/{owner}/{repo}/pulls/{number}/threads/{thread}/resolve", put(resolve_thread).delete(unresolve_thread))
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/requested_reviewers",
            get(requested_reviewers).post(request_reviewers).delete(remove_reviewers),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/comments/{id}",
            get(get_review_comment).patch(edit_review_comment).delete(delete_review_comment),
        )
        .route("/repos/{owner}/{repo}/pulls/comments/{id}/apply-suggestion", post(apply_suggestion))
        .route("/repos/{owner}/{repo}/pulls/comments/{id}/reactions", get(rc_reactions).post(rc_add_reaction))
        .route("/repos/{owner}/{repo}/pulls/comments/{id}/reactions/{rid}", delete(rc_del_reaction))
}

fn base(s: &AppState) -> &str {
    &s.config.server.external_url
}

/// Renders pull request views, looking each repository up once.
struct Render<'a> {
    s: &'a AppState,
    actor: &'a Actor,
    cache: HashMap<ch_core::RepoId, Option<ch_api_types::repos::Repository>>,
}

impl<'a> Render<'a> {
    fn new(s: &'a AppState, actor: &'a Actor) -> Self {
        Self { s, actor, cache: HashMap::new() }
    }

    async fn repo(&mut self, id: ch_core::RepoId) -> Option<ch_api_types::repos::Repository> {
        if let Some(r) = self.cache.get(&id) {
            return r.clone();
        }
        let dto = match self.s.repos.get(self.actor, id).await {
            Ok(r) => repository_dto(self.s, &r).await.ok(),
            Err(_) => None,
        };
        self.cache.insert(id, dto.clone());
        dto
    }

    async fn pull(&mut self, ctx: &Ctx, v: &PullView) -> api::PullRequest {
        let head = match v.head_repo.as_ref().map(|h| h.id).or(Some(v.base_repo.id)) {
            Some(id) => self.repo(id).await,
            None => None,
        };
        let base = self.repo(v.base_repo.id).await;
        ctx.pull(v, head, base)
    }
}

// ---------------------------------------------------------------- list, create, get, patch

#[derive(Debug, Default, Deserialize)]
struct PullQuery {
    state: Option<String>,
    head: Option<String>,
    base: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    q: Option<String>,
}

async fn list_pulls(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PullQuery>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let filter = PullFilter {
        issue: IssueFilter {
            open: open_of(q.state.as_deref()),
            query: q.q,
            sort: sort_of(q.sort.as_deref(), q.direction.as_deref()),
            page: paging.page(),
            per_page: paging.per_page(),
            ..Default::default()
        },
        head: q.head,
        base: q.base,
    };
    let views = s.pulls.views(&actor, repo.id, filter).await?;
    let mut r = Render::new(&s, &actor);
    let mut out = Vec::with_capacity(views.len());
    for v in &views {
        out.push(r.pull(&ctx, v).await);
    }
    Ok(page_of(out, &uri, base(&s), paging))
}

async fn create_pull(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<api::CreatePull>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let (head_owner, head_ref) = match body.head.split_once(':') {
        Some((o, b)) => (Some(o.to_string()), b.to_string()),
        None => (None, body.head.clone()),
    };
    let head_repo_id = match (&body.head_repo, &head_owner) {
        (Some(full), _) => {
            let (o, n) =
                full.split_once('/').ok_or_else(|| Error::Validation(vec![FieldError::new("PullRequest", "head_repo", "invalid")]))?;
            s.repos
                .get_by_name(&actor, o, n)
                .await
                .map_err(|_| Error::Validation(vec![FieldError::new("PullRequest", "head_repo", "invalid")]))?
                .id
        }
        (None, Some(o)) if !o.eq_ignore_ascii_case(&repo.owner_name) => {
            s.repos
                .get_by_name(&actor, o, &repo.name)
                .await
                .map_err(|_| Error::Validation(vec![FieldError::new("PullRequest", "head", "invalid")]))?
                .id
        }
        _ => repo.id,
    };
    let pr = s
        .pulls
        .create(
            &actor,
            NewPull {
                repo_id: repo.id,
                title: body.title,
                body: body.body.unwrap_or_default(),
                head_repo_id,
                head_ref,
                base_ref: body.base,
                draft: body.draft.unwrap_or(false),
            },
            &client,
        )
        .await?;
    let view = s.pulls.view(&actor, repo.id, pr.issue.number).await?;
    Ok(created(Render::new(&s, &actor).pull(&ctx, &view).await))
}

async fn get_pull(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.pulls.view(&actor, repo.id, number).await?;
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &v).await))
}

async fn patch_pull(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::UpdatePull>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let state_change = match body.state.as_deref() {
        None => None,
        Some("open") => Some(true),
        Some("closed") => Some(false),
        Some(_) => return Err(Error::invalid("PullRequest", "state", "state must be open or closed")),
    };
    let mut view = if body.title.is_some() || body.body.is_some() || body.base.is_some() {
        s.pulls.update(&actor, repo.id, number, PullPatch { title: body.title, body: body.body, base: body.base }, &client).await?
    } else {
        s.pulls.view(&actor, repo.id, number).await?
    };
    match state_change {
        Some(false) if view.pull.issue.open => {
            s.pulls.close(&actor, repo.id, number, &client).await?;
            view = s.pulls.view(&actor, repo.id, number).await?;
        }
        Some(true) if !view.pull.issue.open => view = s.pulls.reopen(&actor, repo.id, number, &client).await?,
        _ => {}
    }
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &view).await))
}

// ---------------------------------------------------------------- files, commits

async fn pull_files(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let view = s.pulls.view(&actor, repo.id, number).await?;
    let diff = s.pulls.files(&actor, repo.id, number).await?;
    let per = paging.per_page() as usize;
    let page: Vec<_> =
        diff.files.iter().skip((paging.page() as usize - 1) * per).take(per).map(|f| ctx.pull_file(f, &view.pull.head_sha)).collect();
    Ok(page_of(page, &uri, base(&s), paging))
}

async fn pull_commits(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let commits = s.pulls.commits(&actor, repo.id, number).await?;
    let per = paging.per_page() as usize;
    let page: Vec<_> = commits.iter().skip((paging.page() as usize - 1) * per).take(per).map(|c| ctx.commit(c)).collect();
    Ok(page_of(page, &uri, base(&s), paging))
}

// ---------------------------------------------------------------- merge

async fn is_merged(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.pulls.view(&actor, repo.id, number).await?;
    if v.pull.merged_at.is_some() { Ok(no_content()) } else { Err(Error::NotFound) }
}

#[derive(Debug, Default, Deserialize)]
struct MergeBody {
    merge_method: Option<String>,
    commit_title: Option<String>,
    commit_message: Option<String>,
    /// The head sha the caller reviewed.
    sha: Option<String>,
    delete_branch: Option<bool>,
}

fn parse_method(m: &str) -> Result<MergeMethod> {
    match m {
        "merge" => Ok(MergeMethod::Merge),
        "squash" => Ok(MergeMethod::Squash),
        "rebase" => Ok(MergeMethod::Rebase),
        _ => Err(Error::invalid("PullRequest", "merge_method", "merge_method must be merge, squash or rebase")),
    }
}

async fn merge_pull(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    OptionalJson(body): OptionalJson<MergeBody>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let method = body.merge_method.as_deref().map(parse_method).transpose()?;
    let opts = MergeOptions {
        method,
        title: body.commit_title,
        message: body.commit_message,
        expected_head_sha: body.sha,
        delete_branch: body.delete_branch,
    };
    let v = s.pulls.merge_with(&actor, repo.id, number, opts, &client).await?;
    Ok(ok(api::MergeResult {
        sha: v.pull.merge_commit_sha.unwrap_or_default(),
        merged: true,
        message: "Pull Request successfully merged".into(),
    }))
}

async fn update_branch(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    s.pulls.update_branch(&actor, repo.id, number, &client).await?;
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({ "message": "Updating pull request branch.", "url": ctx.api(&format!("/pulls/{number}")) })),
    )
        .into_response())
}

async fn merge_gate(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let g = s.pulls.merge_gate(&actor, repo.id, number).await?;
    Ok(ok(serde_json::json!({ "mergeable": g.mergeable, "reasons": g.reasons })))
}

#[derive(Debug, Default, Deserialize)]
struct AutoMergeBody {
    merge_method: Option<String>,
}

async fn enable_auto_merge(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    OptionalJson(body): OptionalJson<AutoMergeBody>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let method = parse_method(body.merge_method.as_deref().unwrap_or("merge"))?;
    let v = s.pulls.set_auto_merge(&actor, repo.id, number, Some(method), &client).await?;
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &v).await))
}

async fn disable_auto_merge(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.pulls.set_auto_merge(&actor, repo.id, number, None, &client).await?;
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &v).await))
}

async fn ready_for_review(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.pulls.set_draft(&actor, repo.id, number, false, &client).await?;
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &v).await))
}

async fn convert_to_draft(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.pulls.set_draft(&actor, repo.id, number, true, &client).await?;
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &v).await))
}

// ---------------------------------------------------------------- reviews

fn event_state(e: Option<&str>) -> Result<ReviewState> {
    match e.map(str::to_ascii_uppercase).as_deref() {
        None | Some("") | Some("PENDING") => Ok(ReviewState::Pending),
        Some("COMMENT") => Ok(ReviewState::Commented),
        Some("APPROVE") => Ok(ReviewState::Approved),
        Some("REQUEST_CHANGES") => Ok(ReviewState::ChangesRequested),
        Some(_) => Err(Error::invalid("PullRequestReview", "event", "event must be COMMENT, APPROVE or REQUEST_CHANGES")),
    }
}

async fn review_dto(s: &AppState, actor: &Actor, ctx: &Ctx, repo: ch_core::RepoId, number: i64, id: ReviewId) -> Result<api::Review> {
    let all = s.reviews.views(actor, repo, number).await?;
    let v = all.iter().find(|v| v.review.id == id).ok_or(Error::NotFound)?;
    Ok(ctx.review(v, number))
}

async fn list_reviews(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(s.reviews.views(&actor, repo.id, number).await?.iter().map(|v| ctx.review(v, number)).collect::<Vec<_>>()))
}

async fn get_review(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(review_dto(&s, &actor, &ctx, repo.id, number, ReviewId(id)).await?))
}

async fn create_review(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::CreateReview>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let state = event_state(body.event.as_deref())?;
    let mut comments = Vec::new();
    for c in body.comments {
        let line = c.line.ok_or_else(|| Error::Validation(vec![FieldError::new("PullRequestReviewComment", "line", "missing_field")]))?;
        comments.push(NewReviewComment {
            path: c.path,
            side: c.side.unwrap_or_else(|| "RIGHT".into()),
            line,
            start_line: c.start_line,
            body: c.body,
            in_reply_to: None,
        });
    }
    let review = s.reviews.submit(&actor, repo.id, number, state, body.body.as_deref().unwrap_or(""), comments, &client).await?;
    Ok(ok(review_dto(&s, &actor, &ctx, repo.id, number, review.id).await?))
}

async fn submit_review(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    ApiJson(body): ApiJson<api::SubmitReview>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let state = event_state(Some(&body.event))?;
    let r = s.reviews.submit_pending(&actor, repo.id, number, ReviewId(id), state, body.body.as_deref().unwrap_or(""), &client).await?;
    Ok(ok(review_dto(&s, &actor, &ctx, repo.id, number, r.id).await?))
}

async fn delete_pending_review(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let dto = review_dto(&s, &actor, &ctx, repo.id, number, ReviewId(id)).await?;
    s.reviews.delete_pending(&actor, repo.id, number, ReviewId(id), &client).await?;
    Ok(ok(dto))
}

#[derive(Debug, Deserialize)]
struct DismissBody {
    message: String,
}

async fn dismiss_review(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    ApiJson(body): ApiJson<DismissBody>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let r = s.reviews.dismiss(&actor, repo.id, number, ReviewId(id), &body.message, &client).await?;
    Ok(ok(review_dto(&s, &actor, &ctx, repo.id, number, r.id).await?))
}

async fn review_comments_of(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let threads = s.reviews.threads(&actor, repo.id, number).await?;
    let out: Vec<_> = threads
        .iter()
        .flat_map(|t| t.comments.iter())
        .filter(|c| c.comment.review_id == Some(ReviewId(id)))
        .map(|c| ctx.review_comment(c, number))
        .collect();
    Ok(ok(out))
}

// ---------------------------------------------------------------- review comments and threads

async fn list_review_comments(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let threads = s.reviews.threads(&actor, repo.id, number).await?;
    let mut all: Vec<&ReviewCommentView> = threads.iter().flat_map(|t| t.comments.iter()).collect();
    all.sort_by_key(|c| (c.comment.created_at, c.comment.id));
    let per = paging.per_page() as usize;
    let page: Vec<_> = all.into_iter().skip((paging.page() as usize - 1) * per).take(per).map(|c| ctx.review_comment(c, number)).collect();
    Ok(page_of(page, &uri, base(&s), paging))
}

async fn create_review_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::CreateReviewComment>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let in_reply_to = body.in_reply_to.map(CommentId);
    let line = match (body.line, in_reply_to) {
        (Some(l), _) => l,
        (None, Some(_)) => 1,
        (None, None) => return Err(Error::Validation(vec![FieldError::new("PullRequestReviewComment", "line", "missing_field")])),
    };
    let c = NewReviewComment {
        path: body.path,
        side: body.side.unwrap_or_else(|| "RIGHT".into()),
        line,
        start_line: body.start_line,
        body: body.body,
        in_reply_to,
    };
    let v = s.reviews.add_comment(&actor, repo.id, number, c, None, &client).await?;
    Ok(created(ctx.review_comment(&v, number)))
}

/// A review comment does not know its pull request's number, only its row id.
async fn number_of(s: &AppState, repo: ch_core::RepoId, v: &ReviewCommentView) -> Result<i64> {
    sqlx::query_scalar("SELECT number FROM issues WHERE id = $1 AND repo_id = $2")
        .bind(v.comment.pull_id.0)
        .bind(repo)
        .fetch_optional(&s.db)
        .await?
        .ok_or(Error::NotFound)
}

async fn get_review_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.reviews.comment(&actor, repo.id, CommentId(id)).await?;
    let n = number_of(&s, repo.id, &v).await?;
    Ok(ok(ctx.review_comment(&v, n)))
}

#[derive(Debug, Deserialize)]
struct EditComment {
    body: String,
}

async fn edit_review_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<EditComment>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.reviews.edit_comment(&actor, repo.id, CommentId(id), &body.body, &client).await?;
    let n = number_of(&s, repo.id, &v).await?;
    Ok(ok(ctx.review_comment(&v, n)))
}

async fn delete_review_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.reviews.delete_comment(&actor, repo.id, CommentId(id), &client).await?;
    Ok(no_content())
}

async fn apply_suggestion(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let sha = s.reviews.apply_suggestion(&actor, repo.id, CommentId(id), &client).await?;
    Ok(ok(serde_json::json!({ "sha": sha })))
}

async fn list_threads(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let threads = s.reviews.threads(&actor, repo.id, number).await?;
    let out: Vec<_> = threads
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "path": t.path,
                "resolved": t.resolved,
                "resolved_by": t.resolved_by.as_ref().map(|u| ctx.user(u)),
                "outdated": t.outdated,
                "comments": t.comments.iter().map(|c| ctx.review_comment(c, number)).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(ok(out))
}

async fn resolve_thread(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, _number, thread)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.reviews.resolve_thread(&actor, repo.id, thread, true, &client).await?;
    Ok(no_content())
}

async fn unresolve_thread(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, _number, thread)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.reviews.resolve_thread(&actor, repo.id, thread, false, &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- reactions on review comments

async fn rc_reactions(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(s
        .issues
        .reactions(&actor, repo.id, ReactionTarget::ReviewComment(CommentId(id)))
        .await?
        .iter()
        .map(|r| ctx.reaction(r))
        .collect::<Vec<_>>()))
}

async fn rc_add_reaction(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<CreateReaction>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(created(ctx.reaction(&s.issues.react(&actor, repo.id, ReactionTarget::ReviewComment(CommentId(id)), &body.content, &client).await?)))
}

async fn rc_del_reaction(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id, rid)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.unreact(&actor, repo.id, ReactionTarget::ReviewComment(CommentId(id)), rid, &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- requested reviewers

async fn requested_reviewers(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let r = s.reviews.requested(&actor, repo.id, number).await?;
    Ok(ok(api::RequestedReviewers { users: r.users.iter().map(|u| ctx.user(u)).collect(), teams: ctx.requested_teams(&r.teams) }))
}

async fn reviewer_ids(s: &AppState, actor: &Actor, owner: &str, body: &api::RequestReviewers) -> Result<(Vec<UserId>, Vec<TeamId>)> {
    let mut users = Vec::new();
    for l in &body.reviewers {
        match s.accounts.get_by_username(actor, l.trim_start_matches('@')).await {
            Ok(u) => users.push(u.id),
            Err(Error::NotFound) => {
                return Err(Error::Validation(vec![FieldError::custom("PullRequest", "reviewers", format!("{l} is not a known user"))]));
            }
            Err(e) => return Err(e),
        }
    }
    let mut teams = Vec::new();
    for slug in &body.team_reviewers {
        let id: Option<TeamId> = sqlx::query_scalar(
            "SELECT t.id FROM teams t JOIN users o ON o.id = t.org_id WHERE o.username = $1::citext AND t.name = $2::citext",
        )
        .bind(owner)
        .bind(slug)
        .fetch_optional(&s.db)
        .await?;
        teams.push(id.ok_or_else(|| {
            Error::Validation(vec![FieldError::custom("PullRequest", "team_reviewers", format!("{slug} is not a team of {owner}"))])
        })?);
    }
    Ok((users, teams))
}

async fn request_reviewers(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::RequestReviewers>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let (users, teams) = reviewer_ids(&s, &actor, &repo.owner_name, &body).await?;
    s.reviews.request_review(&actor, repo.id, number, users, teams, &client).await?;
    let v = s.pulls.view(&actor, repo.id, number).await?;
    Ok(created(Render::new(&s, &actor).pull(&ctx, &v).await))
}

async fn remove_reviewers(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::RequestReviewers>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let (users, teams) = reviewer_ids(&s, &actor, &repo.owner_name, &body).await?;
    s.reviews.remove_request(&actor, repo.id, number, users, teams, &client).await?;
    let v = s.pulls.view(&actor, repo.id, number).await?;
    Ok(ok(Render::new(&s, &actor).pull(&ctx, &v).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_events() {
        assert_eq!(event_state(None).unwrap(), ReviewState::Pending);
        assert_eq!(event_state(Some("approve")).unwrap(), ReviewState::Approved);
        assert_eq!(event_state(Some("REQUEST_CHANGES")).unwrap(), ReviewState::ChangesRequested);
        assert_eq!(event_state(Some("COMMENT")).unwrap(), ReviewState::Commented);
        assert!(event_state(Some("LGTM")).is_err());
    }

    #[test]
    fn merge_methods() {
        assert_eq!(parse_method("squash").unwrap(), MergeMethod::Squash);
        assert!(parse_method("fast-forward").is_err());
    }
}
