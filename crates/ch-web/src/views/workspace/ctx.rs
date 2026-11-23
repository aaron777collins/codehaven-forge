//! `WorkspaceCtx`: the checks every workspace route runs, in the spec's order (section 5).
//!
//! 1. The gate: only a browser session reaches a page route at all.
//! 2. Repository lookup and read: otherwise the same 404 as a missing repository.
//! 3. The canonical owner and name, because CSP paths are case sensitive.
//! 4. The workspace is on, otherwise 404.
//! 5. The workspace key, where the route needs it, otherwise 409 `stale`.
//! 6. Write, and not archived, where the route needs it, otherwise 403.
//! 7. Limits, then the body, then validation (in the handlers).

use super::keys;
use super::limits::{Kind, Limits, Permit, Refused};
use crate::ctx::Ctx;
use crate::error::WebError;
use crate::views::repo_common::RepoCtx;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ch_core::model::WorkspaceSettings;
use ch_core::{Permission, UserId};
use serde::de::DeserializeOwned;

/// Whether a route needs the `X-Workspace-Key` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Needed,
    None,
}

/// The least a route needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Read,
    Write,
}

/// A refusal with a machine code, for data routes. Pages and assets use `WebError` instead.
#[derive(Debug)]
pub enum WsError {
    Web(WebError),
    Code { status: StatusCode, code: &'static str, message: String, field: Option<String>, retry_after: Option<u64> },
}

impl From<WebError> for WsError {
    fn from(e: WebError) -> Self {
        WsError::Web(e)
    }
}

impl From<ch_core::Error> for WsError {
    fn from(e: ch_core::Error) -> Self {
        use ch_core::Error as E;
        match e {
            E::NotFound => WsError::Web(WebError::not_found()),
            E::Conflict(m) => WsError::code(StatusCode::CONFLICT, "head_moved", m),
            E::Forbidden(m) => WsError::code(StatusCode::FORBIDDEN, "protected", m),
            E::Validation(errs) => {
                let first = errs.first();
                let message =
                    first.map(|f| f.message.clone().unwrap_or_else(|| f.code.clone())).unwrap_or_else(|| "That is not valid.".into());
                let field = first.map(|f| f.field.clone());
                WsError::Code { status: StatusCode::UNPROCESSABLE_ENTITY, code: "invalid", message, field, retry_after: None }
            }
            E::RateLimited { retry_after_secs } => WsError::Code {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "rate_limited",
                message: "Too many requests. Wait a moment and try again.".into(),
                field: None,
                retry_after: Some(retry_after_secs),
            },
            other => WsError::Web(WebError::Core(other)),
        }
    }
}

impl WsError {
    pub fn code(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        WsError::Code { status, code, message: message.into(), field: None, retry_after: None }
    }
    pub fn invalid(field: &str, message: impl Into<String>) -> Self {
        WsError::Code {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid",
            message: message.into(),
            field: Some(field.into()),
            retry_after: None,
        }
    }
    pub fn stale() -> Self {
        WsError::code(StatusCode::CONFLICT, "stale", "This tab is out of date. Reload the page.")
    }
    pub fn too_large(message: impl Into<String>) -> Self {
        WsError::code(StatusCode::PAYLOAD_TOO_LARGE, "too_large", message)
    }
    fn refused(r: Refused) -> Self {
        let (message, retry) = match r {
            Refused::Rate(s) => ("Too many requests. Wait a moment and try again.", Some(s)),
            Refused::Busy => ("Another request like this is still running. Wait for it to finish.", Some(1)),
        };
        WsError::Code {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            message: message.into(),
            field: None,
            retry_after: retry,
        }
    }
}

/// `Cache-Control: no-store` and `nosniff`, on every data response.
pub fn data_headers(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

impl IntoResponse for WsError {
    fn into_response(self) -> Response {
        match self {
            WsError::Web(e) => {
                let mut r = e.into_response();
                data_headers(r.headers_mut());
                r
            }
            WsError::Code { status, code, message, field, retry_after } => {
                let mut body = serde_json::json!({ "code": code, "message": message });
                if let Some(f) = field {
                    body["field"] = serde_json::Value::String(f);
                }
                let mut r = (status, axum::Json(body)).into_response();
                data_headers(r.headers_mut());
                if let Some(s) = retry_after
                    && let Ok(v) = HeaderValue::from_str(&s.to_string())
                {
                    r.headers_mut().insert(header::RETRY_AFTER, v);
                }
                r
            }
        }
    }
}

pub type WsResult<T> = Result<T, WsError>;

/// A JSON success with the data headers.
pub fn json<T: serde::Serialize>(value: &T) -> Response {
    let mut r = axum::Json(value).into_response();
    data_headers(r.headers_mut());
    r
}

pub struct WorkspaceCtx {
    pub ctx: Ctx,
    pub rc: RepoCtx,
    pub user: UserId,
    pub settings: WorkspaceSettings,
}

impl WorkspaceCtx {
    /// Runs checks 2 to 6. Check 1 (the gate) ran before routing.
    pub async fn load(ctx: Ctx, owner: &str, name: &str, key: Key, need: Need, headers: &HeaderMap) -> WsResult<Self> {
        // A page route is only reached by a session, but say so here too: a handler that runs
        // without one must not go further.
        let user = match &ctx.actor {
            ch_core::Actor::User { id, via: ch_core::AuthVia::Session, .. } => *id,
            _ => return Err(WebError::not_found().into()),
        };
        let rc = RepoCtx::load(&ctx, owner, name).await?;
        if rc.repo.owner_name != owner || rc.repo.name != name {
            return Err(WebError::not_found().into());
        }
        let settings = ctx.state.repos.workspace_settings().await?;
        if !settings.editor_on(&ctx.state.config) {
            return Err(WebError::not_found().into());
        }
        if key == Key::Needed {
            let supplied = headers.get("x-workspace-key").and_then(|v| v.to_str().ok()).unwrap_or("");
            let session = ctx.session_cookie().unwrap_or("");
            if session.is_empty() || !keys::workspace_key_valid(&ctx.state.config, session, user, rc.id(), supplied) {
                return Err(WsError::stale());
            }
        }
        if need == Need::Write {
            if !rc.perm.allows(Permission::Write) {
                return Err(WsError::code(StatusCode::FORBIDDEN, "read_only", "You can read this repository but not change it."));
            }
            if rc.repo.archived {
                return Err(WsError::code(StatusCode::FORBIDDEN, "read_only", "This repository is archived, so it is read only."));
            }
        }
        Ok(WorkspaceCtx { ctx, rc, user, settings })
    }

    /// `load` for a POST: the exact Origin and the `X-CSRF-Token` header come first, so a
    /// forged request is refused the same way for every repository, before anything else.
    pub async fn load_post(ctx: Ctx, owner: &str, name: &str, need: Need, headers: &HeaderMap) -> WsResult<Self> {
        check_origin_and_token(&ctx, headers)?;
        Self::load(ctx, owner, name, Key::Needed, need, headers).await
    }

    /// The workspace base path, `/{owner}/{repo}/workspace`, canonical.
    pub fn base(&self) -> String {
        format!("{}/workspace", self.rc.base())
    }

    /// The absolute base URL for CSP sources.
    pub fn abs_base(&self) -> String {
        format!("{}{}", self.ctx.state.config.origin(), self.base())
    }

    pub fn can_write(&self) -> bool {
        self.rc.perm.allows(Permission::Write) && !self.rc.repo.archived
    }

    /// The terminal is allowed by the config file and switched on by an admin.
    pub fn terminal_on(&self) -> bool {
        self.settings.terminal_on(&self.ctx.state.config)
    }

    /// A rate and concurrency slot, or `rate_limited`.
    pub fn limit(&self, kind: Kind) -> WsResult<Permit> {
        Limits::of(&self.ctx.state).enter(self.user, kind).map_err(WsError::refused)
    }

    pub fn charge_commit_bytes(&self, n: u64) -> WsResult<()> {
        Limits::of(&self.ctx.state).charge_commit_bytes(self.user, n).map_err(WsError::refused)
    }
}

/// `JsonData`: the JSON counterpart of `FormData`, read after the route's other checks. The
/// exact Origin (no Referer fallback), `X-CSRF-Token`, `application/json` and a body cap are all
/// required. Unknown fields are refused by `#[serde(deny_unknown_fields)]` on every body type.
pub struct JsonData<T>(pub T);

/// The exact Origin, no Referer fallback, and a valid `X-CSRF-Token` header.
pub fn check_origin_and_token(ctx: &Ctx, headers: &HeaderMap) -> WsResult<()> {
    let refuse = || WsError::code(StatusCode::FORBIDDEN, "stale", "This request did not come from this page. Reload the page.");
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if origin != Some(ctx.state.config.origin()) {
        return Err(refuse());
    }
    let token = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ctx.csrf_valid(token) {
        return Err(refuse());
    }
    Ok(())
}

impl<T: DeserializeOwned> JsonData<T> {
    pub async fn read(ws: &WorkspaceCtx, headers: &HeaderMap, body: Body, max: usize) -> WsResult<Self> {
        check_origin_and_token(&ws.ctx, headers)?;
        let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let mime = ct.split(';').next().unwrap_or("").trim();
        if mime != "application/json" {
            return Err(WsError::code(StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid", "Send JSON."));
        }
        if let Some(n) = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok())
            && n > max
        {
            return Err(WsError::too_large("That request is too large."));
        }
        let bytes = to_bytes(body, max).await.map_err(|_| WsError::too_large("That request is too large."))?;
        let value = serde_json::from_slice::<T>(&bytes)
            .map_err(|e| WsError::invalid("body", format!("That request is not valid: {}", short(&e))))?;
        Ok(JsonData(value))
    }
}

/// A serde error without the input echoed back.
fn short(e: &serde_json::Error) -> String {
    let s = e.to_string();
    s.split(" at line").next().unwrap_or("").chars().take(120).collect()
}
