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
/// The one answer to "forgot your password", whether or not an account matched.
const RESET_SENT: &str = "If an account here matches, we have emailed it a link to choose a new password. The link works for an hour.";
const BAD_PASSKEY: &str = "That passkey did not work. Try again, or use a recovery code.";
const BAD_CODE: &str = "That code did not work. Try the next one, or use a recovery code.";

fn notice_text(code: &str) -> Option<&'static str> {
    match code {
        "signed-out" => Some("You are signed out."),
        "invite-accepted" => Some("Your account is ready. Sign in to start."),
        "password-set" => Some("Your password is saved. Sign in with it."),
        "reset-sent" => Some(RESET_SENT),
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
    public_on: bool,
}

async fn login_page(ctx: &Ctx, username: &str, next: &str, error: &str, notice: &str) -> WebResult<Response> {
    let no_accounts = ctx.state.accounts.count_users().await.map(|n| n == 0).unwrap_or(false);
    let public_on = ctx.state.repos.public_repos_allowed().await.unwrap_or(false);
    let (token, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome("Sign in").await;
    c.csrf = token.clone();
    let page = LoginPage {
        c,
        csrf: token,
        username: username.into(),
        next: next.into(),
        error: error.into(),
        notice: notice.into(),
        no_accounts,
        public_on,
    };
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
    /// The account has an authenticator app (a recovery code works whatever this says).
    totp: bool,
    /// The account has a passkey and this site can ask for it.
    passkeys: bool,
}

async fn two_factor_page(ctx: &Ctx, next: &str, error: &str) -> WebResult<Response> {
    let (token, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome("Two-factor code").await;
    c.csrf = token.clone();
    // What the pending sign-in can answer with. When it cannot be looked up, ask for a code.
    let pending = ctx.cookie(PENDING_COOKIE).unwrap_or("").to_string();
    let (totp, passkeys) = match ctx.state.sessions.pending_factors(&pending).await {
        Ok(Some(f)) => (f.totp, f.passkeys),
        _ => (true, false),
    };
    let mut r = ctx.page(&TwoFactorPage { c, csrf: token, next: next.into(), error: error.into(), totp, passkeys })?;
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

/// First half of a passkey sign-in: the options the browser asks the authenticator with. Needs the
/// pending cookie the password step set, and the same CSRF check as every post.
pub async fn passkey_begin(ctx: Ctx, _form: FormData) -> WebResult<Response> {
    let Some(pending) = ctx.cookie(PENDING_COOKIE).map(str::to_string) else {
        return Ok(super::settings::json_error(&Error::unauthorized()));
    };
    match ctx.state.sessions.begin_passkey_sign_in(&pending, &ctx.client).await {
        Ok(options) => {
            let mut r = axum::response::IntoResponse::into_response((StatusCode::OK, options));
            r.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
            r.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
            Ok(r)
        }
        Err(e) => Ok(super::settings::json_error(&e)),
    }
}

/// Second half: the authenticator's answer arrives as a form field, like a code does.
pub async fn passkey_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let next = next_of(form.get("next"));
    let Some(pending) = ctx.cookie(PENDING_COOKIE).map(str::to_string) else { return Ok(redirect("/login")) };
    let credential = form.get("credential");
    if credential.is_empty() {
        return two_factor_page(&ctx, &next, BAD_PASSKEY).await;
    }
    match ctx.state.sessions.complete_passkey_sign_in(&pending, credential, &ctx.client).await? {
        Some(s) => {
            let mut r = redirect(&next);
            add_cookie(&mut r, &session_cookie(&ctx, &s));
            add_cookie(&mut r, &ctx.clear_cookie(PENDING_COOKIE, "/login"));
            Ok(r)
        }
        None => two_factor_page(&ctx, &next, BAD_PASSKEY).await,
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

// ------------------------------------------------------------------ forgot password (ch-bfa)

#[derive(Template)]
#[template(path = "auth/forgot.html")]
struct ForgotPage {
    c: Chrome,
    csrf: String,
    mail_on: bool,
}

pub async fn forgot_get(ctx: Ctx) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/settings/password"));
    }
    let (token, cookie) = ctx.anon_csrf();
    let mut c = ctx.chrome("Forgot your password").await;
    c.csrf = token.clone();
    let mut r = ctx.page(&ForgotPage { c, csrf: token, mail_on: ctx.state.mail.enabled() })?;
    if let Some(cookie) = cookie {
        add_cookie(&mut r, &cookie);
    }
    Ok(r)
}

/// Always the same answer, sent before any mail goes out, so neither the words nor the time
/// taken say whether an account matched. With mail off nothing is looked up at all.
pub async fn forgot_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    if ctx.signed_in() {
        return Ok(redirect("/settings/password"));
    }
    if !ctx.state.mail.enabled() {
        return Ok(redirect("/password/forgot"));
    }
    let login = form.get("login").trim().to_string();
    if !login.is_empty() {
        let state = ctx.state.clone();
        let client = ctx.client.clone();
        let base = state.config.server.external_url.trim_end_matches('/').to_string();
        let instance = ctx.chrome("").await.instance;
        tokio::spawn(async move {
            match state.accounts.request_password_reset(&login, &client).await {
                Ok(Some((email, link))) => {
                    let body = reset_mail(&instance, &format!("{base}{}", link.path));
                    if let Err(e) = state.mail.send(&email, &format!("Choose a new password for {instance}"), &body).await {
                        tracing::warn!(error = %e, "a password reset mail was not sent");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "a password reset request failed"),
            }
        });
    }
    Ok(redirect("/login?notice=reset-sent"))
}

/// The reset mail. Plain text, the link on a line of its own.
pub fn reset_mail(instance: &str, url: &str) -> String {
    format!(
        "Someone asked to reset the password of your account on {instance}.\n\nTo choose a new password, open this link within an hour:\n\n{url}\n\nUsing it signs your account out everywhere else. If you did not ask for this, ignore this mail: your password stays as it is.\n"
    )
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
        // Signed in straight away with the new password, through the same steps as the sign-in
        // form, so two-factor still asks for a code (ch-9vo).
        Ok(()) => match ctx.state.sessions.sign_in(&peek.username, form.get("password"), &ctx.client).await {
            Ok(SignInOutcome::Session(s)) => {
                let mut r = redirect("/");
                add_cookie(&mut r, &session_cookie(&ctx, &s));
                Ok(r)
            }
            Ok(SignInOutcome::TwoFactorRequired { pending }) => {
                let mut r = redirect("/login/two-factor?next=%2F");
                add_cookie(&mut r, &ctx.set_cookie(PENDING_COOKIE, &pending, Some(300), true, "Lax", "/login"));
                Ok(r)
            }
            _ => Ok(redirect("/login?notice=password-set")),
        },
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
    fn the_reset_mail_holds_the_link_on_its_own_line() {
        let m = reset_mail("Codehaven", "https://code.example/password/cho_x");
        assert!(m.contains("\n\nhttps://code.example/password/cho_x\n\n"));
        assert!(m.contains("If you did not ask for this"));
    }

    #[test]
    fn only_known_notices_have_text() {
        assert!(notice_text("signed-out").is_some());
        assert!(notice_text("<script>").is_none());
    }
}
