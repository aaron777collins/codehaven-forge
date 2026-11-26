//! The REST edge: `/api/v3` endpoints shaped like GitHub's, over the `ch_core::services` traits.
//!
//! Handlers do three things: find the repository (a repository the caller cannot read is a
//! plain 404, the same as one that does not exist), call a service, and turn the answer into
//! the DTOs of `ch-api-types`. All permission checks live in the services.

pub mod dto;
mod issues;
mod pulls;
mod releases;
mod search;

use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, OriginalUri, Request};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use ch_core::model::{ClientInfo, Repo};
use ch_core::state::AppState;
use ch_core::{Actor, Error, FieldError, Result};
use dto::Ctx;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::convert::Infallible;

pub fn routes() -> Router<AppState> {
    Router::new().merge(issues::routes()).merge(pulls::routes()).merge(releases::routes()).merge(search::routes())
}

/// Who is calling, from the gate.
pub use ch_core::access::CurrentActor;

/// The caller's address and user agent, for the audit log. The `codehaven` binary puts a
/// `ClientInfo` into the request extensions; a handler that runs without one (a unit test)
/// gets the local address.
#[derive(Debug, Clone)]
pub struct Client(pub ClientInfo);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> std::result::Result<Self, Self::Rejection> {
        Ok(Client(parts.extensions.get::<ClientInfo>().cloned().unwrap_or_else(ClientInfo::local)))
    }
}

/// A JSON body that fails as a GitHub-shaped 422 instead of axum's plain text.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = Error;
    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(Error::Validation(vec![FieldError::custom("Request", "body", ch_core::error::json_problem(&e.body_text()))])),
        }
    }
}

/// A body that may be empty, such as `PUT /merge` with no options.
pub struct OptionalJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Default> FromRequest<S> for OptionalJson<T> {
    type Rejection = Error;
    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|_| Error::invalid("Request", "body", "The body could not be read"))?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(OptionalJson(T::default()));
        }
        serde_json::from_slice(&bytes)
            .map(OptionalJson)
            .map_err(|e| Error::Validation(vec![FieldError::custom("Request", "body", format!("Problems parsing JSON: {e}"))]))
    }
}

/// `page` and `per_page`, as GitHub names them.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct Paging {
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

impl Paging {
    pub fn page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }
    pub fn per_page(&self) -> u32 {
        match self.per_page {
            None | Some(0) => 30,
            Some(n) => n.min(100),
        }
    }
}

pub fn ok<T: Serialize>(v: T) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

pub fn created<T: Serialize>(v: T) -> Response {
    (StatusCode::CREATED, axum::Json(v)).into_response()
}

pub fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// A list with a `Link` header. There is no total count, so `next` is offered whenever the
/// page came back full.
pub fn page_of<T: Serialize>(items: Vec<T>, uri: &Uri, ext_url: &str, paging: Paging) -> Response {
    let full = items.len() as u32 >= paging.per_page();
    let mut resp = ok(items);
    if let Some(link) = link_header(uri, ext_url, paging, full)
        && let Ok(v) = HeaderValue::from_str(&link)
    {
        resp.headers_mut().insert(header::LINK, v);
    }
    resp
}

/// One page of a list that is all in memory: the `Link` header says exactly whether there is a
/// next page and which is the last.
pub fn page_slice<T: Serialize>(all: Vec<T>, uri: &Uri, ext_url: &str, paging: Paging) -> Response {
    let per = paging.per_page() as usize;
    let total = all.len();
    let start = (paging.page() as usize - 1).saturating_mul(per);
    let has_next = start.saturating_add(per) < total;
    let last = total.div_ceil(per).max(1) as u32;
    let items: Vec<T> = all.into_iter().skip(start).take(per).collect();
    let mut resp = ok(items);
    if let Some(link) = link_header_with(uri, ext_url, paging, has_next, Some(last))
        && let Ok(v) = HeaderValue::from_str(&link)
    {
        resp.headers_mut().insert(header::LINK, v);
    }
    resp
}

fn link_header(uri: &Uri, ext_url: &str, paging: Paging, has_next: bool) -> Option<String> {
    link_header_with(uri, ext_url, paging, has_next, None)
}

fn link_header_with(uri: &Uri, ext_url: &str, paging: Paging, has_next: bool, last: Option<u32>) -> Option<String> {
    let page = paging.page();
    if !has_next && page == 1 {
        return None;
    }
    let base = format!("{}{}", ext_url.trim_end_matches('/'), uri.path());
    let others: Vec<(String, String)> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k != "page" && k != "per_page").then(|| (k.to_string(), v.to_string()))
        })
        .collect();
    let make = |n: u32| {
        let mut q: Vec<String> = others.iter().map(|(k, v)| format!("{k}={v}")).collect();
        q.push(format!("per_page={}", paging.per_page()));
        q.push(format!("page={n}"));
        format!("<{base}?{}>", q.join("&"))
    };
    let mut parts = Vec::new();
    if has_next {
        parts.push(format!("{}; rel=\"next\"", make(page + 1)));
        if let Some(l) = last {
            parts.push(format!("{}; rel=\"last\"", make(l)));
        }
    }
    if page > 1 {
        parts.push(format!("{}; rel=\"prev\"", make(page - 1)));
        parts.push(format!("{}; rel=\"first\"", make(1)));
    }
    Some(parts.join(", "))
}

/// Finds the repository for `owner/name` as the caller. `NotFound` for one they cannot read.
/// A signed-out caller reaches this only through the gate's public list, and then only for a
/// repository `Authz` says is public now.
pub async fn repo_of(state: &AppState, actor: &Actor, owner: &str, name: &str) -> Result<(Repo, Ctx)> {
    let repo = match state.repos.get_by_name(actor, owner, name).await {
        Ok(r) => r,
        // Signed out, anything that is not public is the gate's rejection, never a 404.
        Err(Error::NotFound) if actor.is_anonymous() => return Err(Error::unauthorized()),
        Err(e) => return Err(e),
    };
    let ctx = Ctx::new(&state.config.server.external_url, &repo.owner_name, &repo.name);
    Ok((repo, ctx))
}

/// What `OriginalUri` gives, for building `Link` headers behind a nested router.
pub type Orig = OriginalUri;

/// Quotes a value for the search language so a label with spaces stays one term.
pub fn quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An `Option<Option<T>>` field: absent is `None`, `null` is `Some(None)`.
pub fn double_option<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

use serde::Deserialize;

/// `state=open|closed|all` as an `open` filter. Open is the default.
pub fn open_of(state: Option<&str>) -> Option<bool> {
    match state {
        Some("closed") => Some(false),
        Some("all") => None,
        _ => Some(true),
    }
}

/// `sort` and `direction` as the filter's `created-desc` style.
pub fn sort_of(sort: Option<&str>, direction: Option<&str>) -> Option<String> {
    let field = match sort {
        Some("created") => "created",
        Some("updated") => "updated",
        Some("comments") => "comments",
        _ => return None,
    };
    Some(format!("{field}-{}", if direction == Some("asc") { "asc" } else { "desc" }))
}

/// A repository in the shape of `GET /repos/{owner}/{repo}`, with real counts. The git stream
/// owns the full repository endpoints. This is the copy pull request branches and search
/// results need, so collab does not depend on it.
pub async fn repository_dto(s: &AppState, repo: &Repo) -> Result<ch_api_types::repos::Repository> {
    use chrono::{DateTime, Utc};
    #[allow(clippy::type_complexity)]
    let row: (i64, bool, bool, bool, bool, bool, DateTime<Utc>, String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT r.size_kb, r.has_issues, r.allow_merge_commit, r.allow_squash_merge, r.allow_rebase_merge, r.delete_branch_on_merge, r.updated_at, u.kind,
                (SELECT count(*) FROM stars WHERE repo_id = r.id), (SELECT count(*) FROM watches WHERE repo_id = r.id AND level <> 'ignore'),
                (SELECT count(*) FROM repositories f WHERE f.fork_parent_id = r.id),
                (SELECT count(*) FROM issues i WHERE i.repo_id = r.id AND i.open AND NOT i.is_pull)
         FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(repo.id)
    .fetch_one(&s.db)
    .await?;
    let base = s.config.server.external_url.trim_end_matches('/').to_string();
    let ctx = Ctx::new(&base, &repo.owner_name, &repo.name);
    let owner = ctx.user(&ch_core::collab::UserRef {
        id: repo.owner_id,
        username: repo.owner_name.clone(),
        display_name: String::new(),
        kind: ch_core::model::UserKind::parse(&row.7).unwrap_or(ch_core::model::UserKind::User),
    });
    Ok(ch_api_types::repos::Repository {
        id: repo.id.0,
        name: repo.name.clone(),
        full_name: repo.full_name(),
        owner,
        private: repo.visibility.is_private_flag(),
        html_url: ctx.html(""),
        description: if repo.description.is_empty() { None } else { Some(repo.description.clone()) },
        fork: repo.fork_parent_id.is_some(),
        url: ctx.api(""),
        clone_url: format!("{base}/{}/{}.git", repo.owner_name, repo.name),
        ssh_url: format!("ssh://git@{}/{}/{}.git", s.config.ssh.external, repo.owner_name, repo.name),
        default_branch: repo.default_branch.clone(),
        visibility: repo.visibility.as_str().into(),
        contribution_policy: repo.contribution_policy.as_str().into(),
        archived: repo.archived,
        disabled: false,
        topics: repo.topics.clone(),
        has_issues: row.1,
        created_at: repo.created_at,
        updated_at: row.6,
        pushed_at: repo.pushed_at,
        size: row.0,
        stargazers_count: row.8,
        watchers_count: row.9,
        forks_count: row.10,
        open_issues_count: row.11,
        permissions: None,
        parent: None,
        empty: repo.is_empty,
        allow_squash_merge: row.3,
        allow_merge_commit: row.2,
        allow_rebase_merge: row.4,
        delete_branch_on_merge: row.5,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn paging_defaults_and_clamps() {
        let p = Paging::default();
        assert_eq!((p.page(), p.per_page()), (1, 30));
        let p = Paging { page: Some(0), per_page: Some(500) };
        assert_eq!((p.page(), p.per_page()), (1, 100));
    }

    #[test]
    fn link_header_next_and_prev() {
        let p = Paging { page: Some(2), per_page: Some(10) };
        let l = link_header(&uri("/api/v3/repos/o/r/issues?state=open&page=2&per_page=10"), "https://h.example/", p, true).unwrap();
        assert!(l.contains("<https://h.example/api/v3/repos/o/r/issues?state=open&per_page=10&page=3>; rel=\"next\""));
        assert!(l.contains("page=1>; rel=\"prev\""));
        assert!(l.contains("rel=\"first\""));
    }

    #[test]
    fn no_link_when_alone() {
        assert!(link_header(&uri("/x"), "https://h", Paging::default(), false).is_none());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("good first issue"), "\"good first issue\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn double_option_distinguishes_null() {
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "double_option")]
            m: Option<Option<i64>>,
        }
        assert_eq!(serde_json::from_str::<P>("{}").unwrap().m, None);
        assert_eq!(serde_json::from_str::<P>("{\"m\":null}").unwrap().m, Some(None));
        assert_eq!(serde_json::from_str::<P>("{\"m\":3}").unwrap().m, Some(Some(3)));
    }
}
