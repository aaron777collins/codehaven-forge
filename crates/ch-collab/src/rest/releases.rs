//! Releases and their assets.

use super::dto::Ctx;
use super::{ApiJson, Client, CurrentActor, OptionalJson, Paging, created, no_content, ok, page_slice, repo_of};
use axum::Router;
use axum::extract::{OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, header};
use axum::response::Response;
use axum::routing::{get, post};
use bytes::Bytes;
use ch_api_types::releases as api;
use ch_core::collab::*;
use ch_core::range::{Serve, file_response};
use ch_core::state::AppState;
use ch_core::{Actor, Error, FieldError, ReleaseId, Result};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;

fn base(s: &AppState) -> &str {
    &s.config.server.external_url
}

/// `make_latest` and `discussion_category_name` of a release body. Discussions are not built, so
/// a value for that field is refused by name.
fn latest_and_discussion(make_latest: Option<&str>, discussion: Option<&str>) -> Result<Option<MakeLatest>> {
    if discussion.is_some_and(|d| !d.trim().is_empty()) {
        return Err(Error::Validation(vec![FieldError::custom(
            "Release",
            "discussion_category_name",
            "Discussions are not available here, so discussion_category_name is not supported",
        )]));
    }
    match make_latest {
        None => Ok(None),
        Some(m) => MakeLatest::parse(m).map(Some).ok_or_else(|| {
            Error::Validation(vec![FieldError::custom("Release", "make_latest", "make_latest must be true, false or legacy")])
        }),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{repo}/releases", get(list_releases).post(create_release))
        .route("/repos/{owner}/{repo}/releases/latest", get(latest_release))
        .route("/repos/{owner}/{repo}/releases/tags/{*tag}", get(release_by_tag))
        .route("/repos/{owner}/{repo}/releases/generate-notes", post(generate_notes))
        .route("/repos/{owner}/{repo}/releases/{id}", get(get_release).patch(update_release).delete(delete_release))
        .route("/repos/{owner}/{repo}/releases/{id}/assets", get(list_assets).post(upload_asset))
        .route("/repos/{owner}/{repo}/releases/assets/{id}", get(get_asset).patch(update_asset).delete(delete_asset))
}

async fn list_releases(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let all: Vec<_> = s.releases.views(&actor, repo.id).await?.iter().map(|v| ctx.release(v)).collect();
    Ok(page_slice(all, &uri, base(&s), paging))
}

async fn latest_release(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(ctx.release(&s.releases.latest_view(&actor, repo.id).await?)))
}

async fn release_by_tag(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, tag)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(ctx.release(&s.releases.view_by_tag(&actor, repo.id, &tag).await?)))
}

async fn get_release(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(ctx.release(&s.releases.view(&actor, repo.id, ReleaseId(id)).await?)))
}

async fn create_release(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<api::CreateRelease>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let make_latest = latest_and_discussion(body.make_latest.as_deref(), body.discussion_category_name.as_deref())?;
    let draft = ReleaseDraft {
        tag_name: body.tag_name,
        target_commitish: body.target_commitish,
        name: body.name,
        body: body.body,
        draft: body.draft.unwrap_or(false),
        prerelease: body.prerelease.unwrap_or(false),
        generate_notes: body.generate_release_notes.unwrap_or(false),
        make_latest: make_latest.unwrap_or_default(),
    };
    Ok(created(ctx.release(&s.releases.create(&actor, repo.id, draft, &client).await?)))
}

async fn update_release(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::UpdateRelease>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let make_latest = latest_and_discussion(body.make_latest.as_deref(), body.discussion_category_name.as_deref())?;
    let edit = ReleaseEdit {
        tag_name: body.tag_name,
        target_commitish: body.target_commitish,
        name: body.name,
        body: body.body,
        draft: body.draft,
        prerelease: body.prerelease,
        generate_notes: body.generate_release_notes,
        make_latest,
    };
    Ok(ok(ctx.release(&s.releases.update(&actor, repo.id, ReleaseId(id), edit, &client).await?)))
}

async fn delete_release(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.releases.delete(&actor, repo.id, ReleaseId(id), &client).await?;
    Ok(no_content())
}

async fn generate_notes(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    OptionalJson(body): OptionalJson<GenerateBody>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let n = s
        .releases
        .generate_notes(&actor, repo.id, &body.tag_name, body.previous_tag_name.as_deref(), body.target_commitish.as_deref())
        .await?;
    Ok(ok(api::GeneratedNotes { name: n.name, body: n.body }))
}

#[derive(Debug, Default, Deserialize)]
struct GenerateBody {
    #[serde(default)]
    tag_name: String,
    previous_tag_name: Option<String>,
    target_commitish: Option<String>,
}

// ---------------------------------------------------------------- assets

async fn asset_dto(s: &AppState, actor: &Actor, ctx: &Ctx, repo: ch_core::RepoId, a: &AssetInfo) -> Result<api::ReleaseAsset> {
    let v = s.releases.view(actor, repo, a.release_id).await?;
    Ok(ctx.asset(a, &v.release.tag_name))
}

async fn list_assets(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.releases.view(&actor, repo.id, ReleaseId(id)).await?;
    let all: Vec<_> = v.assets.iter().map(|a| ctx.asset(a, &v.release.tag_name)).collect();
    Ok(page_slice(all, &uri, base(&s), paging))
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    name: String,
    label: Option<String>,
}

/// The file is the request body, with its type in `Content-Type`, as GitHub's upload endpoint does.
async fn upload_asset(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    req: Request,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let idle = std::time::Duration::from_secs(s.config.http.upload_idle_secs.max(1));
    let stream = idle_timeout(req.into_body().into_data_stream().map_err(std::io::Error::other).boxed(), idle);
    let a = s
        .releases
        .upload_asset(&actor, repo.id, ReleaseId(id), &q.name, q.label.as_deref(), &content_type, Box::pin(stream), &client)
        .await?;
    Ok(created(asset_dto(&s, &actor, &ctx, repo.id, &a).await?))
}

async fn get_asset(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let wants_file = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("application/octet-stream"));
    let info = s.releases.asset(&actor, repo.id, id).await?;
    if !wants_file {
        return Ok(ok(asset_dto(&s, &actor, &ctx, repo.id, &info).await?));
    }
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let size = u64::try_from(info.size).unwrap_or(0);
    let serve = Serve::plan(range, size, method == Method::HEAD);
    // A download is counted only when it reads the file from the start, and a HEAD or a refused
    // range reads nothing.
    let stream = if serve.needs_body() {
        let (_, stream) = s.releases.open_asset_by_id(&actor, repo.id, id).await?;
        Some(stream)
    } else {
        None
    };
    Ok(file_response(&info.name, size, &info.sha256, serve, stream))
}

async fn update_asset(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::UpdateAsset>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let a = s.releases.update_asset(&actor, repo.id, id, body.name.as_deref(), body.label.as_deref(), &client).await?;
    Ok(ok(asset_dto(&s, &actor, &ctx, repo.id, &a).await?))
}

async fn delete_asset(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.releases.delete_asset(&actor, repo.id, id, &client).await?;
    Ok(no_content())
}

/// Ends a body that stays silent for `idle` between two chunks with a time-out error. The whole
/// upload may take as long as the sender keeps sending.
pub(crate) fn idle_timeout(
    stream: BoxStream<'static, std::io::Result<Bytes>>,
    idle: std::time::Duration,
) -> BoxStream<'static, std::io::Result<Bytes>> {
    futures::stream::unfold((stream, false), move |(mut s, done)| async move {
        if done {
            return None;
        }
        match tokio::time::timeout(idle, s.next()).await {
            Ok(Some(item)) => Some((item, (s, false))),
            Ok(None) => None,
            Err(_) => Some((Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the upload stopped sending")), (s, true))),
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn a_slow_body_is_fine_and_a_silent_one_is_cut() {
        // Three chunks, each 40 s after the one before, with a 60 s idle limit: no single gap is long.
        let slow = futures::stream::unfold(0u8, |n| async move {
            if n == 3 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(40)).await;
            Some((Ok(Bytes::from_static(b"x")), n + 1))
        })
        .boxed();
        let got: Vec<_> = idle_timeout(slow, Duration::from_secs(60)).collect().await;
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(Result::is_ok));
        // One that goes quiet for longer than the limit ends with an error, once.
        let quiet = futures::stream::once(async {
            tokio::time::sleep(Duration::from_secs(90)).await;
            Ok(Bytes::from_static(b"late"))
        })
        .boxed();
        let got: Vec<_> = idle_timeout(quiet, Duration::from_secs(60)).collect().await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_ref().unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }
}
