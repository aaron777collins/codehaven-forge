//! The per-request context every handler takes: who is calling, from where, which cookies
//! they sent, plus helpers to build pages, redirects and cookies.

use crate::chrome::{Chrome, Flash, NavUser};
use crate::csrf;
use crate::error::{WebError, WebResult};
use askama::Template;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ch_core::access;
use ch_core::model::ClientInfo;
use ch_core::net;
use ch_core::state::AppState;
use ch_core::{Actor, UserId};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;

pub const THEME_COOKIE: &str = "ch_theme";
pub const FLASH_COOKIE: &str = "ch_flash";
pub const PENDING_COOKIE: &str = "ch_2fa";

#[derive(Clone)]
pub struct Ctx {
    pub state: AppState,
    pub actor: Actor,
    pub client: ClientInfo,
    pub cookies: HashMap<String, String>,
    /// Path and query, for `?next=` and for marking the current link.
    pub path: String,
    /// An htmx request (not a boosted navigation).
    pub hx: bool,
    pub origin: Option<String>,
    pub referer: Option<String>,
    /// The token the request guard in front of the router expects for this request. Empty when
    /// there is no guard (the demo and the page tests), and then `csrf_token` makes its own.
    pub guard_csrf: String,
}

impl FromRequestParts<AppState> for Ctx {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let actor = parts.extensions.get::<Actor>().cloned().unwrap_or(Actor::Anonymous);
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or_else(|| [0, 0, 0, 0].into());
        let xff = parts.headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
        let ip = net::client_ip(peer, if xff.is_empty() { None } else { Some(&xff) }, &state.config.http.trusted_proxies);
        let user_agent = parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(300).collect());
        let mut cookies = HashMap::new();
        for h in parts.headers.get_all(header::COOKIE) {
            let Ok(s) = h.to_str() else { continue };
            for part in s.split(';') {
                if let Some((k, v)) = part.trim().split_once('=') {
                    cookies.entry(k.to_string()).or_insert_with(|| v.to_string());
                }
            }
        }
        let hdr = |n: &str| parts.headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        let hx = hdr("hx-request").is_some() && hdr("hx-boosted").is_none();
        let path = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        Ok(Ctx {
            state: state.clone(),
            actor,
            client: ClientInfo { ip, user_agent },
            cookies,
            path,
            hx,
            origin: hdr("origin"),
            referer: hdr("referer"),
            guard_csrf: parts.extensions.get::<ch_core::identity::CsrfToken>().map(|t| t.0.clone()).unwrap_or_default(),
        })
    }
}

impl Ctx {
    pub fn https(&self) -> bool {
        self.state.config.is_https()
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(String::as_str)
    }

    pub fn session_cookie(&self) -> Option<&str> {
        self.cookie(access::session_cookie_name(self.https()))
    }

    pub fn signed_in(&self) -> bool {
        matches!(self.actor, Actor::User { .. })
    }

    /// The signed in user's id, or the error that sends a visitor to the sign-in page.
    pub fn user_id(&self) -> WebResult<UserId> {
        match &self.actor {
            Actor::User { id, .. } => Ok(*id),
            _ => Err(WebError::Core(ch_core::Error::unauthorized())),
        }
    }

    pub fn theme(&self) -> String {
        match self.cookie(THEME_COOKIE) {
            Some(t @ ("light" | "dark")) => t.to_string(),
            _ => "system".to_string(),
        }
    }

    fn key(&self) -> &'static [u8; 32] {
        csrf::key(&self.state.config)
    }

    /// The token a form on this page must carry. Signed in pages bind it to the session. Signed
    /// out pages bind it to the double-submit cookie, which `anon_csrf` creates.
    pub fn csrf_token(&self) -> String {
        // The guard checks every unsafe request before a handler runs, with its own token. A
        // form must carry that one, so it is the one pages embed.
        if !self.guard_csrf.is_empty() {
            return self.guard_csrf.clone();
        }
        match self.session_cookie() {
            Some(sess) if self.signed_in() => csrf::session_token(self.key(), sess),
            _ => self
                .cookie(access::csrf_cookie_name(self.https()))
                .filter(|v| csrf::anon_value_valid(self.key(), v))
                .unwrap_or("")
                .to_string(),
        }
    }

    /// For signed out forms: the existing valid cookie value, or a new one and the
    /// `Set-Cookie` that stores it.
    pub fn anon_csrf(&self) -> (String, Option<String>) {
        // With the guard in front, it hands out the cookie itself (on the response of the first
        // page a visitor loads) and the token follows from it.
        if !self.guard_csrf.is_empty() {
            return (self.guard_csrf.clone(), None);
        }
        let name = access::csrf_cookie_name(self.https());
        if let Some(v) = self.cookie(name)
            && csrf::anon_value_valid(self.key(), v)
        {
            return (v.to_string(), None);
        }
        let v = csrf::new_anon_value(self.key());
        let c = self.set_cookie(name, &v, Some(8 * 3600), true, "Lax", "/");
        (v, Some(c))
    }

    /// Checks a submitted token against the one this request should carry.
    pub fn csrf_valid(&self, supplied: &str) -> bool {
        if supplied.is_empty() {
            return false;
        }
        let expected = self.csrf_token();
        !expected.is_empty() && csrf::ct_eq(&expected, supplied)
    }

    pub fn origin_valid(&self) -> bool {
        csrf::origin_ok(&self.state.config, self.origin.as_deref(), self.referer.as_deref())
    }

    /// A `Set-Cookie` value. `Secure` follows the public origin.
    pub fn set_cookie(&self, name: &str, value: &str, max_age: Option<i64>, http_only: bool, same_site: &str, path: &str) -> String {
        let mut s = format!("{name}={value}; Path={path}; SameSite={same_site}");
        if let Some(m) = max_age {
            s.push_str(&format!("; Max-Age={m}"));
        }
        if http_only {
            s.push_str("; HttpOnly");
        }
        if self.https() {
            s.push_str("; Secure");
        }
        s
    }

    pub fn clear_cookie(&self, name: &str, path: &str) -> String {
        self.set_cookie(name, "", Some(0), true, "Lax", path)
    }

    pub fn flash_cookie(&self, kind: &str, text: &str) -> String {
        let signed = csrf::sign(self.key(), "flash", &format!("{kind}|{text}"));
        self.set_cookie(FLASH_COOKIE, &signed, Some(60), true, "Lax", "/")
    }

    fn read_flash(&self) -> Option<Flash> {
        let raw = self.cookie(FLASH_COOKIE)?;
        let text = csrf::unsign(self.key(), "flash", raw)?;
        let (kind, text) = text.split_once('|')?;
        let kind = if matches!(kind, "ok" | "error" | "info") { kind } else { "info" };
        Some(Flash { kind: kind.into(), text: text.into() })
    }

    /// Builds the chrome for a full page: assets, the user in the header, the flash message.
    pub async fn chrome(&self, title: &str) -> Chrome {
        let instance = self.state.config.server.name.clone();
        let mut c = Chrome::base(title, &instance, &self.theme());
        c.csrf = self.csrf_token();
        c.flash = self.read_flash();
        if let Actor::User { id, is_admin, has_2fa, .. } = &self.actor {
            let user = self.state.accounts.get_user(*id).await.ok();
            let unread = self.state.notifications.unread_count(&self.actor).await.unwrap_or(0);
            c.user = Some(NavUser {
                username: user.as_ref().map(|u| u.username.clone()).unwrap_or_default(),
                display_name: user.map(|u| u.display_name).unwrap_or_default(),
                is_admin: self.actor.is_site_admin(),
                admin_needs_2fa: *is_admin && !*has_2fa,
                unread,
            });
            if *is_admin && !*has_2fa {
                c.banner = Some("Turn on two-factor sign-in to use the admin pages.".into());
            }
        }
        c
    }

    /// Renders a template into a no-store HTML response, clearing the flash cookie it showed.
    pub fn page<T: Template>(&self, t: &T) -> WebResult<Response> {
        let body = t.render().map_err(|e| WebError::Core(ch_core::Error::internal(anyhow_from(e))))?;
        let mut r = (StatusCode::OK, body).into_response();
        self.finish(&mut r);
        if self.cookie(FLASH_COOKIE).is_some() {
            add_cookie(&mut r, &self.clear_cookie(FLASH_COOKIE, "/"));
        }
        Ok(r)
    }

    /// Same, with a status code (a form that failed validation).
    pub fn page_status<T: Template>(&self, status: StatusCode, t: &T) -> WebResult<Response> {
        let mut r = self.page(t)?;
        *r.status_mut() = status;
        Ok(r)
    }

    /// A fragment for htmx, with no layout and no flash handling.
    pub fn fragment<T: Template>(&self, t: &T) -> WebResult<Response> {
        let body = t.render().map_err(|e| WebError::Core(ch_core::Error::internal(anyhow_from(e))))?;
        let mut r = (StatusCode::OK, body).into_response();
        self.finish(&mut r);
        Ok(r)
    }

    fn finish(&self, r: &mut Response) {
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }

    pub fn redirect(&self, to: &str) -> Response {
        redirect(to)
    }

    pub fn redirect_flash(&self, to: &str, kind: &str, text: &str) -> Response {
        let mut r = redirect(to);
        add_cookie(&mut r, &self.flash_cookie(kind, text));
        r
    }
}

fn anyhow_from(e: askama::Error) -> anyhow::Error {
    anyhow::anyhow!("template: {e}")
}

pub fn redirect(to: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub fn add_cookie(r: &mut Response, cookie: &str) {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        r.headers_mut().append(HeaderName::from_static("set-cookie"), v);
    }
}
