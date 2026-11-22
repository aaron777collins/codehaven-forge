//! Releases and their assets.

use super::dto::Ctx;
use super::{ApiJson, Client, CurrentActor, OptionalJson, created, no_content, ok, repo_of};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ch_api_types::releases as api;
use ch_core::collab::*;
use ch_core::state::AppState;
use ch_core::{Actor, ReleaseId, Result};
use futures::TryStreamExt;
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{repo}/releases", get(list_releases).post(create_release))
        .route("/repos/{owner}/{repo}/releases/latest", get(latest_release))
        .route("/repos/{owner}/{repo}/releases/tags/{*tag}", get(release_by_tag))
        .route("/repos/{owner}/{repo}/releases/generate-notes", post(generate_notes))
        .route("/repos/{owner}/{repo}/releases/{id}", get(get_release).patch(update_release).delete(delete_release))
        .route("/repos/{owner}/{repo}/releases/{id}/assets", get(list_assets).post(upload_asset))
        .route("/repos/{owner}/{repo}/releases/assets/{id}", get(get_asset).patch(rename_asset_unsupported).delete(delete_asset))
}

async fn list_releases(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(s.releases.views(&actor, repo.id).await?.iter().map(|v| ctx.release(v)).collect::<Vec<_>>()))
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
    let draft = ReleaseDraft {
        tag_name: body.tag_name,
        target_commitish: body.target_commitish,
        name: body.name,
        body: body.body,
        draft: body.draft.unwrap_or(false),
        prerelease: body.prerelease.unwrap_or(false),
        generate_notes: body.generate_release_notes.unwrap_or(false),
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
    let edit = ReleaseEdit {
        tag_name: body.tag_name,
        target_commitish: body.target_commitish,
        name: body.name,
        body: body.body,
        draft: body.draft,
        prerelease: body.prerelease,
        generate_notes: body.generate_release_notes,
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
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.releases.view(&actor, repo.id, ReleaseId(id)).await?;
    Ok(ok(v.assets.iter().map(|a| ctx.asset(a, &v.release.tag_name)).collect::<Vec<_>>()))
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
    let stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
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
    headers: HeaderMap,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let wants_file = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("application/octet-stream"));
    if !wants_file {
        let a = s.releases.asset(&actor, repo.id, id).await?;
        return Ok(ok(asset_dto(&s, &actor, &ctx, repo.id, &a).await?));
    }
    let (info, stream) = s.releases.open_asset_by_id(&actor, repo.id, id).await?;
    Ok(file_response(&info, stream))
}

/// Streams an asset. The name goes into `Content-Disposition` with quotes and control
/// characters removed, and `nosniff` stops a browser from guessing a different type.
pub fn file_response(info: &AssetInfo, stream: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>>) -> Response {
    let safe: String = info.name.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').collect();
    let mut r = Response::new(Body::from_stream(stream));
    *r.status_mut() = StatusCode::OK;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&info.size.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    r
}

async fn rename_asset_unsupported() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        axum::Json(serde_json::json!({ "message": "Renaming an asset is not supported. Delete it and upload it again" })),
    )
        .into_response()
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
