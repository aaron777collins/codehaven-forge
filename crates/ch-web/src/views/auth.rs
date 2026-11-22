//! Signing in, two-factor, invite links and one-time password links. These are the only
//! pages a signed out visitor can reach, so they say as little as possible: a bad invite, an
//! expired link and a made-up one all get the same page.

use super::FieldErrors;
use crate::chrome::Chrome;
use crate::ctx::{Ctx, PENDING_COOKIE, add_cookie, redirect};
use crate::error::{WebError, WebResult};
use crate::forms::FormData;
use askama::Template;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::Error;
use ch_core::access;
use ch_core::model::{LinkPurpose, NewAccount};
use ch_core::web_contract::{NewSession, SignInOutcome};
use std::collections::HashMap;

const BAD_LOGIN: &str = "That username and password don't match.";
const BAD_CODE: &str = "That code did not work. Try the next one, or use a recovery code.";

fn notice_text(code: &str) -> Option<&'static str> {
    match code {
        "signed-out" => Some("You are signed out."),
        "invite-accepted" => Some("Your account is ready. Sign in to start."),
        "password-set" => Some("Your password is saved. Sign in with it."),
        _ => None,
    }
}

fn next_of(raw: &str) -> String {
    access::safe_next(raw).unwrap_or("/").to_string()
}

pub(super) fn session_cookie(ctx: &Ctx, s: &NewSession) -> String {
    ctx.set_cookie(access::session_cookie_name(ctx.https()), &s.cookie_value, Some(s.max_age_secs as i64), true, "Lax", "/")
}

// ------------------------------------------------------------------ sign in

#[derive(Template)]
#[template(path = "auth/login.html")]
struct LoginPage {
    c: Chrome,
    csrf: String,
    username: String,
    next: String,
    error: String,
    notice: String,
    no_accounts: bool,
}

async fn login_page(ctx: &Ctx, username: &str, next: &str, error: &str, notice: &str) -> WebResult<Response> {
    let no_accounts = ctx.state.accounts.count_users().await.map(|n| n == 0).unwrap_or(false);
    let (token, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome("Sign in").await;
    c.csrf = token.clone();
    let page =
        LoginPage { c, csrf: token, username: username.into(), next: next.into(), error: error.into(), notice: notice.into(), no_accounts };
    let mut r = ctx.page(&page)?;
    if let Some(cookie) = cookie {
        add_cookie(&mut r, &cookie);
    }
    Ok(r)
}

pub async fn login_get(ctx: Ctx, Query(q): Query<HashMap<String, String>>) -> WebResult<Response> {
    let next = next_of(q.get("next").map(String::as_str).unwrap_or(""));
    if ctx.signed_in() {
        return Ok(redirect(&next));
    }
    let notice = q.get("notice").and_then(|n| notice_text(n)).unwrap_or("");
    login_page(&ctx, "", &next, "", notice).await
}

pub async fn login_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let next = next_of(form.get("next"));
    if ctx.signed_in() {
        return Ok(redirect(&next));
    }
    let username = form.get("username").trim().to_string();
    if username.is_empty() || form.get("password").is_empty() {
        return login_page(&ctx, &username, &next, BAD_LOGIN, "").await;
    }
    match ctx.state.sessions.sign_in(&username, form.get("password"), &ctx.client).await? {
        SignInOutcome::Session(s) => {
            let mut r = redirect(&next);
            add_cookie(&mut r, &session_cookie(&ctx, &s));
            Ok(r)
        }
        SignInOutcome::TwoFactorRequired { pending } => {
            let mut r = redirect(&format!("/login/two-factor?next={}", access::encode_query_value(&next)));
            add_cookie(&mut r, &ctx.set_cookie(PENDING_COOKIE, &pending, Some(300), true, "Lax", "/login"));
            Ok(r)
        }
        SignInOutcome::Rejected => login_page(&ctx, &username, &next, BAD_LOGIN, "").await,
    }
}

// ------------------------------------------------------------------ two-factor

#[derive(Template)]
#[template(path = "auth/two_factor.html")]
struct TwoFactorPage {
    c: Chrome,
    csrf: String,
    next: String,
    error: String,
}

async fn two_factor_page(ctx: &Ctx, next: &str, error: &str) -> WebResult<Response> {
    let (token, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome("Two-factor code").await;
    c.csrf = token.clone();
    let mut r = ctx.page(&TwoFactorPage { c, csrf: token, next: next.into(), error: error.into() })?;
    if let Some(cookie) = cookie {
        add_cookie(&mut r, &cookie);
    }
    Ok(r)
}

pub async fn two_factor_get(ctx: Ctx, Query(q): Query<HashMap<String, String>>) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/"));
    }
    if ctx.cookie(PENDING_COOKIE).is_none() {
        return Ok(redirect("/login"));
    }
    two_factor_page(&ctx, &next_of(q.get("next").map(String::as_str).unwrap_or("")), "").await
}

pub async fn two_factor_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let next = next_of(form.get("next"));
    let Some(pending) = ctx.cookie(PENDING_COOKIE).map(str::to_string) else { return Ok(redirect("/login")) };
    let code: String = form.get("code").chars().filter(|c| !c.is_whitespace()).collect();
    if code.is_empty() {
        return two_factor_page(&ctx, &next, BAD_CODE).await;
    }
    match ctx.state.sessions.complete_two_factor(&pending, &code, &ctx.client).await? {
        Some(s) => {
            let mut r = redirect(&next);
            add_cookie(&mut r, &session_cookie(&ctx, &s));
            add_cookie(&mut r, &ctx.clear_cookie(PENDING_COOKIE, "/login"));
            Ok(r)
        }
        None => two_factor_page(&ctx, &next, BAD_CODE).await,
    }
}

pub async fn logout_post(ctx: Ctx, _form: FormData) -> WebResult<Response> {
    if let Some(cookie) = ctx.session_cookie() {
        let _ = ctx.state.sessions.sign_out(cookie, &ctx.client).await;
    }
    let mut r = redirect("/login?notice=signed-out");
    add_cookie(&mut r, &ctx.clear_cookie(access::session_cookie_name(ctx.https()), "/"));
    Ok(r)
}

// ------------------------------------------------------------------ invites

#[derive(Template)]
#[template(path = "auth/bad_link.html")]
struct BadLinkPage {
    c: Chrome,
    what: String,
}

async fn bad_link(ctx: &Ctx, what: &str) -> WebResult<Response> {
    let c = ctx.chrome("Link does not work").await;
    ctx.page_status(StatusCode::NOT_FOUND, &BadLinkPage { c, what: what.into() })
}

#[derive(Template)]
#[template(path = "auth/invite.html")]
struct InvitePage {
    c: Chrome,
    csrf: String,
    action: String,
    invited_by: String,
    email: String,
    email_locked: bool,
    username: String,
    display_name: String,
    errors: FieldErrors,
}

fn token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= 128 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[allow(clippy::too_many_arguments)]
async fn invite_page(
    ctx: &Ctx,
    token: &str,
    invited_by: &str,
    email: &str,
    locked: bool,
    username: &str,
    display_name: &str,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let (csrf, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome("Accept your invite").await;
    c.csrf = csrf.clone();
    let page = InvitePage {
        c,
        csrf,
        action: format!("/invite/{token}"),
        invited_by: invited_by.into(),
        email: email.into(),
        email_locked: locked,
        username: username.into(),
        display_name: display_name.into(),
        errors,
    };
    let mut r = ctx.page_status(status, &page)?;
    if let Some(cookie) = cookie {
        add_cookie(&mut r, &cookie);
    }
    Ok(r)
}

pub async fn invite_get(ctx: Ctx, Path(token): Path<String>) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/"));
    }
    if !token_ok(&token) {
        return bad_link(&ctx, "invite").await;
    }
    match ctx.state.invites.peek(&token).await {
        Ok(p) => {
            let email = p.email.clone().unwrap_or_default();
            invite_page(
                &ctx,
                &token,
                p.invited_by.as_deref().unwrap_or(""),
                &email,
                p.email.is_some(),
                "",
                "",
                FieldErrors::default(),
                StatusCode::OK,
            )
            .await
        }
        Err(Error::NotFound) => bad_link(&ctx, "invite").await,
        Err(e) => Err(e.into()),
    }
}

pub async fn invite_post(ctx: Ctx, Path(token): Path<String>, form: FormData) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/"));
    }
    if !token_ok(&token) {
        return bad_link(&ctx, "invite").await;
    }
    let peek = match ctx.state.invites.peek(&token).await {
        Ok(p) => p,
        Err(Error::NotFound) => return bad_link(&ctx, "invite").await,
        Err(e) => return Err(e.into()),
    };
    let username = form.get("username").trim().to_string();
    let display_name = form.get("display_name").trim().to_string();
    let email = peek.email.clone().unwrap_or_else(|| form.get("email").trim().to_string());
    let by = peek.invited_by.clone().unwrap_or_default();
    let locked = peek.email.is_some();
    let again = |errors: FieldErrors| {
        invite_page(&ctx, &token, &by, &email, locked, &username, &display_name, errors, StatusCode::UNPROCESSABLE_ENTITY)
    };

    if form.get("password") != form.get("password_confirm") {
        return again(FieldErrors::default().with("password_confirm", "The two passwords are not the same.")).await;
    }
    let account = NewAccount {
        username: username.clone(),
        email: email.clone(),
        password: form.get("password").to_string(),
        display_name: if display_name.is_empty() { None } else { Some(display_name.clone()) },
    };
    match ctx.state.invites.accept(&token, account, &ctx.client).await {
        Ok(_) => Ok(redirect("/login?notice=invite-accepted")),
        Err(Error::NotFound) => bad_link(&ctx, "invite").await,
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => again(errors).await,
            None => Err(e.into()),
        },
    }
}

// ------------------------------------------------------------------ one-time password links

#[derive(Template)]
#[template(path = "auth/password.html")]
struct PasswordPage {
    c: Chrome,
    csrf: String,
    action: String,
    username: String,
    first_time: bool,
    errors: FieldErrors,
}

pub async fn password_get(ctx: Ctx, Path(token): Path<String>) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/"));
    }
    if !token_ok(&token) {
        return bad_link(&ctx, "password link").await;
    }
    match ctx.state.sessions.peek_link(&token).await {
        Ok(p) => {
            password_page(&ctx, &token, &p.username, p.purpose == LinkPurpose::SetPassword, FieldErrors::default(), StatusCode::OK).await
        }
        Err(Error::NotFound) => bad_link(&ctx, "password link").await,
        Err(e) => Err(e.into()),
    }
}

async fn password_page(
    ctx: &Ctx,
    token: &str,
    username: &str,
    first: bool,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let (csrf, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome(if first { "Choose a password" } else { "Reset your password" }).await;
    c.csrf = csrf.clone();
    let page = PasswordPage { c, csrf, action: format!("/password/{token}"), username: username.into(), first_time: first, errors };
    let mut r = ctx.page_status(status, &page)?;
    if let Some(cookie) = cookie {
        add_cookie(&mut r, &cookie);
    }
    Ok(r)
}

pub async fn password_post(ctx: Ctx, Path(token): Path<String>, form: FormData) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/"));
    }
    if !token_ok(&token) {
        return bad_link(&ctx, "password link").await;
    }
    let peek = match ctx.state.sessions.peek_link(&token).await {
        Ok(p) => p,
        Err(Error::NotFound) => return bad_link(&ctx, "password link").await,
        Err(e) => return Err(e.into()),
    };
    let first = peek.purpose == LinkPurpose::SetPassword;
    if form.get("password") != form.get("password_confirm") {
        let errors = FieldErrors::default().with("password_confirm", "The two passwords are not the same.");
        return password_page(&ctx, &token, &peek.username, first, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    match ctx.state.sessions.redeem_link(&token, form.get("password"), &ctx.client).await {
        Ok(()) => Ok(redirect("/login?notice=password-set")),
        Err(Error::NotFound) => bad_link(&ctx, "password link").await,
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => password_page(&ctx, &token, &peek.username, first, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(WebError::Core(e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_always_a_local_path() {
        assert_eq!(next_of("/alice/demo"), "/alice/demo");
        assert_eq!(next_of("https://evil.example/"), "/");
        assert_eq!(next_of("//evil.example"), "/");
        assert_eq!(next_of("/login"), "/");
        assert_eq!(next_of(""), "/");
    }

    #[test]
    fn tokens_are_checked_before_any_lookup() {
        assert!(token_ok("chi_AbCdEf0123"));
        assert!(!token_ok(""));
        assert!(!token_ok("a/b"));
        assert!(!token_ok(&"x".repeat(200)));
        assert!(!token_ok("a b"));
    }

    #[test]
    fn only_known_notices_have_text() {
        assert!(notice_text("signed-out").is_some());
        assert!(notice_text("<script>").is_none());
    }
}
