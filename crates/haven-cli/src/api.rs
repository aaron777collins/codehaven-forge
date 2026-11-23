//! The REST v3 client. Every typed call decodes into a `ch-api-types` DTO, so the CLI stops
//! compiling when the contract changes.

use anyhow::{Context, Result, anyhow};
use ch_api_types::common::ApiError;
use reqwest::{Method, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};

/// A non-2xx answer from the server.
#[derive(Debug, thiserror::Error)]
pub struct HttpError {
    pub status: StatusCode,
    pub message: String,
    /// `resource.field: code` lines from the server's validation errors.
    pub details: Vec<String>,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (HTTP {})", self.message, self.status.as_u16())?;
        for d in &self.details {
            write!(f, "\n  {d}")?;
        }
        if self.status == StatusCode::UNAUTHORIZED {
            write!(f, "\n  Run `haven auth login` to sign in.")?;
        }
        Ok(())
    }
}

/// A response kept as bytes, for `haven api` and downloads.
pub struct RawResponse {
    pub status: StatusCode,
    pub headers: header::HeaderMap,
    pub body: bytes::Bytes,
}

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    /// `https://host`, no trailing slash.
    base: String,
    token: Option<String>,
}

impl ApiClient {
    pub fn new(base: &str, token: Option<String>) -> Result<ApiClient> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("haven/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("building the HTTP client")?;
        Ok(ApiClient { http, base: base.trim_end_matches('/').to_string(), token })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// `path` is `/user`, `/repos/o/r`, ... relative to `/api/v3`. An absolute URL on the same
    /// host (a `Link` header, an `upload_url`) is used as is.
    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}/api/v3/{}", self.base, path.trim_start_matches('/'))
        }
    }

    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, url).header(header::ACCEPT, "application/json");
        // A token goes only to the host it was issued for, never to a link that points elsewhere.
        if let Some(t) = &self.token
            && url.starts_with(&self.base)
        {
            r = r.header(header::AUTHORIZATION, format!("token {t}"));
        }
        r
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = rb.send().await.map_err(|e| anyhow!("cannot reach {}: {}", self.base, e))?;
        if resp.status().is_success() || resp.status().is_redirection() {
            return Ok(resp);
        }
        Err(Self::to_error(resp).await.into())
    }

    async fn to_error(resp: reqwest::Response) -> HttpError {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<ApiError>(&text) {
            Ok(e) => HttpError {
                status,
                message: e.message,
                details: e
                    .errors
                    .iter()
                    .map(|f| match &f.message {
                        Some(m) => format!("{}.{}: {} ({m})", f.resource, f.field, f.code),
                        None => format!("{}.{}: {}", f.resource, f.field, f.code),
                    })
                    .collect(),
            },
            Err(_) => HttpError {
                status,
                message: if text.trim().is_empty() {
                    status.canonical_reason().unwrap_or("request failed").to_string()
                } else {
                    text.trim().chars().take(300).collect()
                },
                details: vec![],
            },
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.get_query(path, &[] as &[(&str, &str)]).await
    }

    pub async fn get_query<T: DeserializeOwned, Q: Serialize + ?Sized>(&self, path: &str, query: &Q) -> Result<T> {
        let resp = self.send(self.request(Method::GET, &self.url(path)).query(query)).await?;
        decode(resp).await
    }

    pub async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self.send(self.request(Method::POST, &self.url(path)).json(body)).await?;
        decode(resp).await
    }

    pub async fn patch<T: DeserializeOwned, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self.send(self.request(Method::PATCH, &self.url(path)).json(body)).await?;
        decode(resp).await
    }

    pub async fn put<T: DeserializeOwned, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self.send(self.request(Method::PUT, &self.url(path)).json(body)).await?;
        decode(resp).await
    }

    /// A DELETE that decodes a JSON answer (turning auto-merge off answers with the pull request).
    pub async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self.send(self.request(Method::DELETE, &self.url(path))).await?;
        decode(resp).await
    }

    /// A POST whose answer carries nothing we need (`202 Accepted`, `204 No Content`).
    pub async fn post_empty<B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<()> {
        self.send(self.request(Method::POST, &self.url(path)).json(body)).await?;
        Ok(())
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.send(self.request(Method::DELETE, &self.url(path))).await?;
        Ok(())
    }

    /// Follows `Link: <...>; rel="next"` until `limit` items are collected (0 means all).
    /// `path` may already carry a query string.
    pub async fn get_paginated<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)], limit: usize) -> Result<Vec<T>> {
        let mut out: Vec<T> = Vec::new();
        let per_page = if limit == 0 || limit > 100 { 100 } else { limit.max(1) };
        let mut url = self.url(path);
        let mut first = true;
        loop {
            let mut rb = self.request(Method::GET, &url);
            if first {
                rb = rb.query(query).query(&[("per_page", per_page.to_string())]);
                first = false;
            }
            let resp = self.send(rb).await?;
            let next = next_link(resp.headers());
            let mut page: Vec<T> = decode(resp).await?;
            out.append(&mut page);
            if limit != 0 && out.len() >= limit {
                out.truncate(limit);
                break;
            }
            match next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(out)
    }

    /// Like `get_paginated` for the `{ "total_count": n, "<key>": [...] }` envelopes
    /// (workflow runs, workflows, jobs). `pick` takes the items out of one decoded page.
    pub async fn get_paginated_wrapped<P: DeserializeOwned, T>(
        &self,
        path: &str,
        query: &[(&str, String)],
        limit: usize,
        pick: impl Fn(P) -> Vec<T>,
    ) -> Result<Vec<T>> {
        let mut out: Vec<T> = Vec::new();
        let per_page = if limit == 0 || limit > 100 { 100 } else { limit.max(1) };
        let mut url = self.url(path);
        let mut first = true;
        loop {
            let mut rb = self.request(Method::GET, &url);
            if first {
                rb = rb.query(query).query(&[("per_page", per_page.to_string())]);
                first = false;
            }
            let resp = self.send(rb).await?;
            let next = next_link(resp.headers());
            let page: P = decode(resp).await?;
            out.extend(pick(page));
            if limit != 0 && out.len() >= limit {
                out.truncate(limit);
                break;
            }
            match next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(out)
    }

    /// Any method, any body, any extra headers. Returns the response whatever its status, which
    /// is what `haven api` wants.
    pub async fn raw(&self, method: Method, path: &str, body: Option<Vec<u8>>, headers: &[(String, String)]) -> Result<RawResponse> {
        let mut rb = self.request(method, &self.url(path));
        for (k, v) in headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(b) = body {
            rb = rb.body(b);
        }
        let resp = rb.send().await.map_err(|e| anyhow!("cannot reach {}: {}", self.base, e))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await?;
        Ok(RawResponse { status, headers, body })
    }

    /// Downloads a body (a patch, a log, an archive) as text.
    pub async fn get_text(&self, path: &str, accept: Option<&str>) -> Result<String> {
        let mut rb = self.request(Method::GET, &self.url(path));
        if let Some(a) = accept {
            rb = rb.header(header::ACCEPT, a);
        }
        let resp = self.send(rb).await?;
        Ok(resp.text().await?)
    }

    /// Uploads bytes to an absolute URL on this host (release assets).
    pub async fn upload<T: DeserializeOwned>(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<T> {
        let rb = self.request(Method::POST, url).header(header::CONTENT_TYPE, content_type).body(body);
        decode(self.send(rb).await?).await
    }
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| anyhow!("the server answered {} with something unexpected: {e}", status.as_u16()))
}

/// The `rel="next"` target of a `Link` header.
pub fn next_link(h: &header::HeaderMap) -> Option<String> {
    for v in h.get_all(header::LINK) {
        let s = v.to_str().ok()?;
        for part in s.split(',') {
            let part = part.trim();
            if part.contains("rel=\"next\"") {
                let start = part.find('<')? + 1;
                let end = part.find('>')?;
                return Some(part[start..end].to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_header_next() {
        let mut h = header::HeaderMap::new();
        h.insert(header::LINK, "<https://x/api/v3/a?page=2>; rel=\"next\", <https://x/api/v3/a?page=9>; rel=\"last\"".parse().unwrap());
        assert_eq!(next_link(&h).as_deref(), Some("https://x/api/v3/a?page=2"));
        let mut h = header::HeaderMap::new();
        h.insert(header::LINK, "<https://x/api/v3/a?page=1>; rel=\"prev\"".parse().unwrap());
        assert_eq!(next_link(&h), None);
        assert_eq!(next_link(&header::HeaderMap::new()), None);
    }

    #[test]
    fn urls_join_under_the_api_root() {
        let c = ApiClient::new("https://h.example/", None).unwrap();
        assert_eq!(c.url("/user"), "https://h.example/api/v3/user");
        assert_eq!(c.url("repos/a/b"), "https://h.example/api/v3/repos/a/b");
        assert_eq!(c.url("https://h.example/api/v3/x?page=2"), "https://h.example/api/v3/x?page=2");
    }
}
