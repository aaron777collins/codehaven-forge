//! `GET B`: the shell. It holds no file content: the page fetches everything after `boot`.

use super::assets::assets_url;
use super::ctx::{Key, Need, WorkspaceCtx, WsError, WsResult};
use super::headers::shell_headers;
use super::keys::workspace_key;
use crate::assets;
use crate::ctx::Ctx;
use crate::error::WebError;
use askama::Template;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use std::collections::HashMap;

#[derive(Template)]
#[template(path = "workspace/shell.html")]
struct Shell {
    theme: String,
    title: String,
    css_app: String,
    assets: String,
    base: String,
    origin: String,
    key: String,
    csrf: String,
    full_name: String,
    branch: String,
    path: String,
}

/// A branch or path from the URL, shown to the page as a starting point. Anything that does not
/// look like a plain name is dropped: a URL can name where to look, never what to do.
fn plain(v: Option<&String>, max: usize) -> String {
    match v {
        Some(s) if s.len() <= max && !s.chars().any(|c| c.is_control()) => s.clone(),
        _ => String::new(),
    }
}

pub async fn shell(
    ctx: Ctx,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> WsResult<Response> {
    // The one redirect under the workspace: a non-canonical spelling of a repository the caller
    // can read moves to the canonical URL. The query is not carried, so nothing rides along.
    let canonical = match WorkspaceCtx::load(ctx.clone(), &owner, &name, Key::None, Need::Read, &headers).await {
        Ok(ws) => ws,
        Err(WsError::Web(WebError::Core(ch_core::Error::NotFound))) => {
            let rc = crate::views::repo_common::RepoCtx::load(&ctx, &owner, &name).await?;
            let s = ctx.state.repos.workspace_settings().await?;
            if !s.editor_on(&ctx.state.config) || (rc.repo.owner_name == owner && rc.repo.name == name) {
                return Err(WebError::not_found().into());
            }
            let mut r = StatusCode::SEE_OTHER.into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("{}/workspace", rc.base())) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(r);
        }
        Err(e) => return Err(e),
    };
    let ws = canonical;
    let session = ws.ctx.session_cookie().unwrap_or("").to_string();
    let base = ws.base();
    let page = Shell {
        theme: ws.ctx.theme(),
        title: format!("Workspace - {}", ws.rc.repo.full_name()),
        css_app: assets::url("app.css"),
        assets: assets_url(&base),
        origin: ws.ctx.state.config.origin().to_string(),
        key: workspace_key(&ws.ctx.state.config, &session, ws.user, ws.rc.id()),
        csrf: ws.ctx.csrf_token(),
        full_name: ws.rc.repo.full_name(),
        branch: plain(q.get("branch"), 255),
        path: plain(q.get("path"), 4096),
        base,
    };
    let html = page.render().map_err(|e| WebError::Core(ch_core::Error::internal(anyhow::anyhow!(e))))?;
    let mut r = Html(html).into_response();
    shell_headers(r.headers_mut(), ws.ctx.state.config.origin(), &ws.abs_base(), ws.terminal_on());
    Ok(r)
}
