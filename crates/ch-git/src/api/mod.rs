//! The repository REST endpoints under `/api/v3` (ARCHITECTURE.md section 11).
//!
//! Paths, methods, field names and status codes follow GitHub's REST v3 where they mean the
//! same thing. A handler gets the caller from the gate, finds the repository (which answers
//! `404` for one the caller cannot read), checks the permission the operation needs and calls
//! [`GitCore`]. Errors are the shared GitHub-shaped JSON.

use crate::core::GitCore;
use axum::Router;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use ch_core::access::CurrentActor;
use ch_core::model::{ClientInfo, Repo};
use ch_core::state::AppState;
use ch_core::{Actor, Error, Permission, Result};
use serde::Deserialize;
use std::sync::Arc;

pub mod code;
pub mod convert;
pub mod repos;

pub fn routes() -> Router<AppState> {
    Router::new().merge(repos::routes()).merge(code::routes())
}

/// Everything a handler needs from the request, resolved once.
pub struct Api {
    pub state: AppState,
    pub core: Arc<GitCore>,
    pub actor: Actor,
    pub client: ClientInfo,
}

impl Api {
    pub fn base(&self) -> &str {
        &self.state.config.server.external_url
    }

    /// The repository, with a read check. `NotFound` hides one the caller cannot read.
    pub async fn repo(&self, owner: &str, name: &str) -> Result<Repo> {
        self.core.do_get_by_name(&self.actor, owner, name).await
    }

    /// The repository, with the permission an operation needs.
    pub async fn repo_with(&self, owner: &str, name: &str, needed: Permission) -> Result<Repo> {
        let repo = self.repo(owner, name).await?;
        // `repo` proves only `View`. Anything more is checked here.
        if needed > Permission::View {
            self.core.require(&self.actor, &repo, needed).await?;
        }
        Ok(repo)
    }
}

impl FromRequestParts<AppState> for Api {
    type Rejection = Error;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> std::result::Result<Self, Error> {
        let CurrentActor(actor) = CurrentActor::from_request_parts(parts, state).await.unwrap_or(CurrentActor(Actor::Anonymous));
        // A signed-out caller only on a route the gate admitted from the public list. `Authz`
        // still decides each repository.
        if actor.is_anonymous() && parts.extensions.get::<ch_core::access::PublicRead>().is_none() {
            return Err(Error::unauthorized());
        }
        let core = state.ext.get::<GitCore>().ok_or_else(|| Error::unavailable("git is not built yet"))?;
        let client = parts.extensions.get::<ClientInfo>().cloned().unwrap_or_else(ClientInfo::local);
        Ok(Api { state: state.clone(), core, actor, client })
    }
}

/// `?page=` and `?per_page=` (default 30, at most 100).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PageQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

impl PageQuery {
    pub fn page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }
    pub fn per_page(&self) -> u32 {
        self.per_page.unwrap_or(30).clamp(1, 100)
    }
    pub fn skip(&self) -> u32 {
        (self.page() - 1) * self.per_page()
    }
}

/// A JSON list with a `Link` header for the next page when there may be one.
pub fn paged<T: serde::Serialize>(base_url: &str, q: PageQuery, items: Vec<T>, more: bool) -> Response {
    let mut resp = axum::Json(items).into_response();
    let mut links = Vec::new();
    if more {
        links.push(format!("<{base_url}?page={}&per_page={}>; rel=\"next\"", q.page() + 1, q.per_page()));
    }
    if q.page() > 1 {
        links.push(format!("<{base_url}?page={}&per_page={}>; rel=\"prev\"", q.page() - 1, q.per_page()));
        links.push(format!("<{base_url}?page=1&per_page={}>; rel=\"first\"", q.per_page()));
    }
    if !links.is_empty()
        && let Ok(v) = HeaderValue::from_str(&links.join(", "))
    {
        resp.headers_mut().insert(header::LINK, v);
    }
    resp
}

/// Cuts a page from `limit + 1` fetched rows: returns the rows to show and whether more exist.
pub fn trim_page<T>(mut rows: Vec<T>, q: PageQuery) -> (Vec<T>, bool) {
    let more = rows.len() > q.per_page() as usize;
    rows.truncate(q.per_page() as usize);
    (rows, more)
}

/// A JSON request body that reports a bad body in the API's own error shape.
pub struct ApiJson<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Error;
    async fn from_request(req: axum::extract::Request, state: &S) -> std::result::Result<Self, Error> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(Error::invalid("Request", "body", ch_core::error::json_problem(&e.body_text()))),
        }
    }
}

/// A JSON body that may be left out altogether (an empty body is `None`).
pub struct ApiJsonOpt<T>(pub Option<T>);

impl<T, S> axum::extract::FromRequest<S> for ApiJsonOpt<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Error;
    async fn from_request(req: axum::extract::Request, _state: &S) -> std::result::Result<Self, Error> {
        let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await
            .map_err(|_| Error::invalid("Request", "body", "the body is too large"))?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(ApiJsonOpt(None));
        }
        serde_json::from_slice(&bytes)
            .map(|v| ApiJsonOpt(Some(v)))
            .map_err(|e| Error::invalid("Request", "body", format!("Problems parsing JSON: {e}")))
    }
}

pub(crate) fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_defaults_and_limits() {
        let q = PageQuery { page: None, per_page: None };
        assert_eq!((q.page(), q.per_page(), q.skip()), (1, 30, 0));
        let q = PageQuery { page: Some(3), per_page: Some(500) };
        assert_eq!((q.page(), q.per_page(), q.skip()), (3, 100, 200));
        let q = PageQuery { page: Some(0), per_page: Some(0) };
        assert_eq!((q.page(), q.per_page()), (1, 1));
    }

    #[test]
    fn trim_reports_more() {
        let q = PageQuery { page: Some(1), per_page: Some(2) };
        assert_eq!(trim_page(vec![1, 2, 3], q), (vec![1, 2], true));
        assert_eq!(trim_page(vec![1, 2], q), (vec![1, 2], false));
    }

    #[test]
    fn link_header_has_next_and_prev() {
        let q = PageQuery { page: Some(2), per_page: Some(10) };
        let r = paged("http://x/api/v3/repos", q, vec![1], true);
        let link = r.headers().get(header::LINK).unwrap().to_str().unwrap().to_string();
        assert!(link.contains("page=3&per_page=10>; rel=\"next\""), "{link}");
        assert!(link.contains("page=1&per_page=10>; rel=\"prev\""), "{link}");
        let none = paged("http://x", PageQuery { page: None, per_page: None }, vec![1], false);
        assert!(none.headers().get(header::LINK).is_none());
    }
}
