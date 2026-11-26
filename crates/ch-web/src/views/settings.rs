//! A user's own settings: profile, password, two-factor sign-in, sessions, SSH keys, access
//! tokens and theme. Every page is for the signed in user only. Things shown once and never
//! again (a new token's secret, two-factor recovery codes) appear only in the response of
//! the POST that created them, never in a redirect, a cookie or a URL.

use super::FieldErrors;
use super::auth::session_cookie;
use crate::chrome::Chrome;
use crate::ctx::{Ctx, THEME_COOKIE, add_cookie};
use crate::error::WebResult;
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::audit::{Outcome, StoredAudit, action};
use ch_core::web_contract::UpdateProfile;
use ch_core::{SessionId, SshKeyId, TokenId};

// ---------------------------------------------------------------- pure helpers

/// The audit `action` constant in words for the user's own security log. Actions the page
/// does not know pass through unchanged, so nothing is hidden by an upgrade.
fn friendly_action(name: &str) -> &str {
    match name {
        action::SIGN_IN => "Signed in",
        action::SIGN_IN_FAILED => "Sign in failed",
        action::TWO_FACTOR_FAILED => "Two-factor code failed",
        action::LOCKOUT => "Locked out after failed sign-ins",
        action::UNLOCK => "Lockout lifted",
        action::SIGN_OUT => "Signed out",
        action::PASSWORD_SET => "Password set",
        action::PASSWORD_CHANGED => "Password changed",
        action::PASSWORD_RESET => "Password reset",
        action::TWO_FACTOR_ON => "Two-factor turned on",
        action::TWO_FACTOR_OFF => "Two-factor turned off",
        action::RECOVERY_CODE_USED => "Recovery code used",
        action::PASSKEY_ADDED => "Passkey added",
        action::PASSKEY_REMOVED => "Passkey removed",
        action::PASSKEY_USED => "Signed in with a passkey",
        action::SESSION_REVOKED => "Session signed out",
        action::TOKEN_CREATED => "Access token created",
        action::TOKEN_DELETED => "Access token deleted",
        action::SSH_KEY_ADDED => "SSH key added",
        action::SSH_KEY_REMOVED => "SSH key removed",
        other => other,
    }
}

/// Success is green and failure is red. This is the user's own log, so anything that failed
/// is worth their attention.
fn outcome_chip(outcome: Outcome) -> (&'static str, &'static str) {
    match outcome {
        Outcome::Success => ("chip-ok", "success"),
        Outcome::Failure => ("chip-fail", "failed"),
    }
}

/// The browser family of a user agent, by simple substring checks. Edge and Opera say
/// "Chrome" too, so they are looked for first.
fn browser_name(ua: &str) -> Option<&'static str> {
    let ua = ua.to_ascii_lowercase();
    if ua.contains("edg/") || ua.contains("edge") {
        Some("Edge")
    } else if ua.contains("opr/") || ua.contains("opera") {
        Some("Opera")
    } else if ua.contains("firefox") || ua.contains("fxios") {
        Some("Firefox")
    } else if ua.contains("chrome") || ua.contains("crios") {
        Some("Chrome")
    } else if ua.contains("safari") {
        Some("Safari")
    } else {
        None
    }
}

/// The operating system of a user agent. Phones and tablets are named before the desktop
/// systems they imitate.
fn os_name(ua: &str) -> Option<&'static str> {
    let ua = ua.to_ascii_lowercase();
    if ua.contains("iphone") || ua.contains("ipad") {
        Some("iOS")
    } else if ua.contains("android") {
        Some("Android")
    } else if ua.contains("windows") {
        Some("Windows")
    } else if ua.contains("mac os x") || ua.contains("macintosh") {
        Some("macOS")
    } else if ua.contains("cros") {
        Some("ChromeOS")
    } else if ua.contains("linux") || ua.contains("x11") {
        Some("Linux")
    } else {
        None
    }
}

/// "Firefox on macOS" for a session's user agent, or "Unknown browser" when there is nothing
/// to go on.
fn browser_label(ua: Option<&str>) -> String {
    let Some(ua) = ua.filter(|u| !u.trim().is_empty()) else { return "Unknown browser".into() };
    match (browser_name(ua), os_name(ua)) {
        (Some(browser), Some(os)) => format!("{browser} on {os}"),
        (Some(browser), None) => browser.into(),
        (None, Some(os)) => format!("Unknown browser on {os}"),
        (None, None) => "Unknown browser".into(),
    }
}

/// The service calls the password fields `current` and `new`; the form ids are
/// `current_password` and `new_password`. Copy the messages across so each lands under its
/// own input.
fn password_errors(mut e: FieldErrors) -> FieldErrors {
    for (from, to) in [("current", "current_password"), ("new", "new_password")] {
        let msg = e.get(from);
        if !msg.is_empty() {
            e = e.with(to, &msg);
        }
    }
    e
}

/// Every scope the token form may grant, with the plain words shown next to it.
const SCOPE_CHOICES: [(&str, &str, &str); 10] = [
    ("repo", "Repositories", "Read and write your repositories."),
    ("repo:read", "Repositories, read only", "Read your repositories, including private ones."),
    ("user", "Profile", "Read and update your profile."),
    ("read:org", "Organizations, read only", "See the organizations and teams you belong to."),
    ("admin:org", "Organizations", "Manage the organizations and teams you belong to."),
    ("write:packages", "Packages", "Upload and delete packages."),
    ("read:packages", "Packages, read only", "Download packages."),
    ("workflow", "Workflows", "Update workflow files and trigger runs."),
    ("admin:repo_hook", "Repository webhooks", "Manage webhooks on your repositories."),
    ("notifications", "Notifications", "Read your notifications."),
];

/// One checkbox on the token form.
struct ScopeOpt {
    value: &'static str,
    label: &'static str,
    help: &'static str,
    checked: bool,
}

fn scope_options(chosen: &[String]) -> Vec<ScopeOpt> {
    SCOPE_CHOICES.iter().map(|(value, label, help)| ScopeOpt { value, label, help, checked: chosen.iter().any(|c| c == value) }).collect()
}

/// At least one scope, and nothing outside the list the page offered.
fn scopes_valid(chosen: &[String]) -> bool {
    !chosen.is_empty() && chosen.iter().all(|c| SCOPE_CHOICES.iter().any(|(v, _, _)| c == v))
}

const EXPIRY_CHOICES: [(u32, &str); 4] = [(30, "30 days"), (90, "90 days"), (180, "180 days"), (365, "1 year")];

/// The expiry the form sends, with 90 days for anything else. 90 is the default choice, so a
/// doctored value buys nothing.
fn expiry_days(value: &str) -> u32 {
    match value.trim() {
        "30" => 30,
        "180" => 180,
        "365" => 365,
        _ => 90,
    }
}

/// One option in the expiry select.
struct ExpiryOpt {
    days: u32,
    label: &'static str,
    selected: bool,
}

fn expiry_options(selected: u32) -> Vec<ExpiryOpt> {
    EXPIRY_CHOICES.iter().map(|(days, label)| ExpiryOpt { days: *days, label, selected: *days == selected }).collect()
}

// ---------------------------------------------------------------- index

pub async fn index(ctx: Ctx) -> WebResult<Response> {
    ctx.user_id()?;
    Ok(ctx.redirect("/settings/profile"))
}

// ---------------------------------------------------------------- profile

#[derive(Template)]
#[template(path = "settings/profile.html")]
struct ProfilePage {
    c: Chrome,
    section: String,
    username: String,
    display_name: String,
    email: String,
    errors: FieldErrors,
}

async fn profile_page(ctx: &Ctx, display_name: String, email: String, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let user = ctx.state.accounts.get_user(ctx.user_id()?).await?;
    let c = ctx.chrome("Profile").await.nav("settings");
    let page = ProfilePage { c, section: "profile".into(), username: user.username, display_name, email, errors };
    ctx.page_status(status, &page)
}

pub async fn profile_get(ctx: Ctx) -> WebResult<Response> {
    let user = ctx.state.accounts.get_user(ctx.user_id()?).await?;
    profile_page(&ctx, user.display_name, user.email.unwrap_or_default(), FieldErrors::default(), StatusCode::OK).await
}

pub async fn profile_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let display_name = form.get("display_name").trim().to_string();
    let email = form.get("email").trim().to_string();
    let update = UpdateProfile {
        display_name: (!display_name.is_empty()).then(|| display_name.clone()),
        email: (!email.is_empty()).then(|| email.clone()),
    };
    match ctx.state.sessions.update_profile(&ctx.actor, update, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash("/settings/profile", "ok", "Profile saved.")),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => profile_page(&ctx, display_name, email, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------- password

#[derive(Template)]
#[template(path = "settings/password.html")]
struct PasswordPage {
    c: Chrome,
    section: String,
    errors: FieldErrors,
}

async fn password_page(ctx: &Ctx, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    ctx.user_id()?;
    let c = ctx.chrome("Password").await.nav("settings");
    ctx.page_status(status, &PasswordPage { c, section: "password".into(), errors })
}

pub async fn password_get(ctx: Ctx) -> WebResult<Response> {
    password_page(&ctx, FieldErrors::default(), StatusCode::OK).await
}

pub async fn password_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    if form.get("new_password") != form.get("confirm_password") {
        let errors = FieldErrors::default().with("confirm_password", "The two passwords are not the same.");
        return password_page(&ctx, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let current = ctx.session_cookie().unwrap_or("").to_string();
    match ctx
        .state
        .sessions
        .change_password(&ctx.actor, &current, form.get("current_password"), form.get("new_password"), &ctx.client)
        .await
    {
        Ok(session) => {
            // Every session ended, this one included. The new cookie goes out with the redirect.
            let mut r = ctx.redirect_flash("/settings/password", "ok", "Password changed. Your other sessions were signed out.");
            add_cookie(&mut r, &session_cookie(&ctx, &session));
            Ok(r)
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => password_page(&ctx, password_errors(errors), StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------- two-factor and security

struct SecurityRow {
    at: TimeView,
    what: String,
    ip: String,
    chip_class: &'static str,
    chip_label: &'static str,
}

fn security_row(event: &StoredAudit) -> SecurityRow {
    let (chip_class, chip_label) = outcome_chip(event.event.outcome);
    SecurityRow {
        at: fmt::time(event.at),
        what: friendly_action(&event.event.action).to_string(),
        ip: event.event.client_ip.map(|ip| ip.to_string()).unwrap_or_else(|| "-".into()),
        chip_class,
        chip_label,
    }
}

struct PasskeyRow {
    id: i64,
    name: String,
    created: TimeView,
    last_used: Option<TimeView>,
    /// Removing it takes away the last second factor, so a recovery code is asked for too.
    needs_code: bool,
}

#[derive(Template)]
#[template(path = "settings/security.html")]
struct SecurityPage {
    c: Chrome,
    section: String,
    has_2fa: bool,
    /// The authenticator app is on.
    totp: bool,
    passkeys_available: bool,
    passkeys: Vec<PasskeyRow>,
    recovery_left: u32,
    admin_needs_2fa: bool,
    rows: Vec<SecurityRow>,
    errors: FieldErrors,
}

async fn security_page(ctx: &Ctx, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let user = ctx.state.accounts.get_user(ctx.user_id()?).await?;
    let log = ctx.state.sessions.security_log(&ctx.actor, 30).await?;
    let rows = log.iter().map(security_row).collect();
    let factors = ctx.state.sessions.two_factor_status(&ctx.actor).await.ok();
    let totp = factors.as_ref().map(|f| f.totp).unwrap_or(user.has_2fa);
    let list = ctx.state.sessions.passkeys(&ctx.actor).await.unwrap_or_default();
    let last_is_passkey = !totp && list.len() == 1;
    let passkeys = list
        .into_iter()
        .map(|k| PasskeyRow {
            id: k.id,
            name: k.name,
            created: fmt::time(k.created_at),
            last_used: k.last_used_at.map(fmt::time),
            needs_code: last_is_passkey,
        })
        .collect();
    let c = ctx.chrome("Two-factor and security").await.nav("settings");
    ctx.page_status(
        status,
        &SecurityPage {
            c,
            section: "security".into(),
            has_2fa: user.has_2fa,
            totp,
            passkeys_available: ctx.state.sessions.passkeys_available().await,
            passkeys,
            recovery_left: factors.map(|f| f.recovery_codes_left).unwrap_or(0),
            admin_needs_2fa: ctx.actor.is_admin_without_2fa(),
            rows,
            errors,
        },
    )
}

/// Errors a security form can show: a field or general message, and a wrong password or code
/// (`Unauthorized`) as the message under the password.
fn security_errors(e: &ch_core::Error) -> Option<FieldErrors> {
    match e {
        ch_core::Error::Unauthorized(m) => Some(FieldErrors::default().with("password", m)),
        other => FieldErrors::from_error(other),
    }
}

pub async fn security_get(ctx: Ctx) -> WebResult<Response> {
    security_page(&ctx, FieldErrors::default(), StatusCode::OK).await
}

#[derive(Template)]
#[template(path = "settings/two_factor_setup.html")]
struct TwoFactorSetupPage {
    c: Chrome,
    section: String,
    secret: String,
    otpauth_url: String,
    qr_svg: String,
    error: String,
}

/// The page the start and confirm posts render in the response. On a wrong code the same
/// secret is shown again: the hidden fields carry it back for display only, and the code is
/// checked against the secret the server stored.
async fn setup_page(ctx: &Ctx, secret: String, otpauth_url: String, error: &str, status: StatusCode) -> WebResult<Response> {
    ctx.user_id()?;
    let c = ctx.chrome("Set up two-factor").await.nav("settings");
    let qr_svg = crate::qr::svg(&otpauth_url);
    ctx.page_status(status, &TwoFactorSetupPage { c, section: "security".into(), secret, otpauth_url, qr_svg, error: error.into() })
}

pub async fn two_factor_start(ctx: Ctx, _form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let setup = ctx.state.sessions.begin_two_factor_setup(&ctx.actor, &ctx.client).await?;
    setup_page(&ctx, setup.secret, setup.otpauth_url, "", StatusCode::OK).await
}

#[derive(Template)]
#[template(path = "settings/two_factor_done.html")]
struct TwoFactorDonePage {
    c: Chrome,
    section: String,
    heading: String,
    lead: String,
    codes: Vec<String>,
    codes_text: String,
}

pub async fn two_factor_confirm(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let secret = form.get("secret").to_string();
    let otpauth_url = form.get("otpauth_url").to_string();
    let code: String = form.get("code").chars().filter(|c| !c.is_whitespace()).collect();
    if code.is_empty() {
        return setup_page(&ctx, secret, otpauth_url, "Enter the six digit code from your app.", StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let current = ctx.session_cookie().unwrap_or("").to_string();
    match ctx.state.sessions.confirm_two_factor_setup(&ctx.actor, &current, &code, &ctx.client).await {
        Ok(on) => {
            let codes = on.recovery_codes;
            let codes_text = codes.join("\n");
            let mut c = ctx.chrome("Two-factor is on").await.nav("settings");
            // This request still carries the session from before two-factor, so the chrome
            // would ask for what was just done (ch-9vo). The new session has it on.
            c.banner = None;
            if let Some(u) = c.user.as_mut() {
                u.admin_needs_2fa = false;
            }
            let mut r = ctx.page(&TwoFactorDonePage {
                c,
                section: "security".into(),
                heading: "Two-factor is on".into(),
                lead: "From now on, signing in asks for a code from your app as well as your password.".into(),
                codes,
                codes_text,
            })?;
            // Turning two-factor on ended every session. This is the new one.
            add_cookie(&mut r, &session_cookie(&ctx, &on.session));
            Ok(r)
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => {
                let message = if errors.general.is_empty() {
                    "That code did not work. Check it and try the next one."
                } else {
                    errors.general.as_str()
                };
                setup_page(&ctx, secret, otpauth_url, message, StatusCode::UNPROCESSABLE_ENTITY).await
            }
            None => Err(e.into()),
        },
    }
}

pub async fn two_factor_disable(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let current = ctx.session_cookie().unwrap_or("").to_string();
    let code: String = form.get("code").chars().filter(|c| !c.is_whitespace()).collect();
    match ctx.state.sessions.disable_two_factor(&ctx.actor, &current, form.get("password"), &code, &ctx.client).await {
        Ok(session) => {
            let mut r = ctx.redirect_flash("/settings/security", "ok", "The authenticator app is off.");
            add_cookie(&mut r, &session_cookie(&ctx, &session));
            Ok(r)
        }
        Err(e) => match security_errors(&e) {
            Some(errors) => security_page(&ctx, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------- passkeys

/// A JSON answer for the page script. Never cached.
fn json_response(status: StatusCode, body: String) -> Response {
    let mut r = axum::response::IntoResponse::into_response((status, body));
    r.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
    r.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    r
}

/// What the script shows when a step fails: a plain sentence, never a detail of the ceremony.
pub(super) fn json_error(e: &ch_core::Error) -> Response {
    let (status, text) = match e {
        ch_core::Error::Conflict(m) | ch_core::Error::Unavailable(m) => (StatusCode::CONFLICT, m.clone()),
        ch_core::Error::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "Too many tries. Wait a little and try again.".to_string()),
        ch_core::Error::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "Your sign-in timed out. Sign in again.".to_string()),
        _ => (StatusCode::BAD_REQUEST, "That did not work. Try again.".to_string()),
    };
    json_response(status, serde_json::json!({ "error": text }).to_string())
}

/// First half of adding a passkey: the options the browser asks the authenticator with.
pub async fn passkey_begin(ctx: Ctx, _form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    match ctx.state.sessions.begin_passkey_registration(&ctx.actor, &ctx.client).await {
        Ok(options) => Ok(json_response(StatusCode::OK, options)),
        Err(e) => Ok(json_error(&e)),
    }
}

/// Second half: the answer comes in as a form field so the page can show recovery codes and set
/// the new session cookie in an ordinary response when this was the first second factor.
pub async fn passkey_finish(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let current = ctx.session_cookie().unwrap_or("").to_string();
    match ctx.state.sessions.finish_passkey_registration(&ctx.actor, &current, form.get("name"), form.get("credential"), &ctx.client).await
    {
        Ok(on) => match on.session {
            Some(session) => {
                let codes = on.recovery_codes;
                let codes_text = codes.join("\n");
                let mut c = ctx.chrome("Two-factor is on").await.nav("settings");
                // This request still carries the session from before two-factor (ch-9vo).
                c.banner = None;
                if let Some(u) = c.user.as_mut() {
                    u.admin_needs_2fa = false;
                }
                let mut r = ctx.page(&TwoFactorDonePage {
                    c,
                    section: "security".into(),
                    heading: "Passkey added, two-factor is on".into(),
                    lead: "From now on, signing in asks for this passkey as well as your password.".into(),
                    codes,
                    codes_text,
                })?;
                add_cookie(&mut r, &session_cookie(&ctx, &session));
                Ok(r)
            }
            None => Ok(ctx.redirect_flash("/settings/security", "ok", "Passkey added.")),
        },
        Err(e) => match security_errors(&e) {
            Some(errors) => security_page(&ctx, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn passkey_remove(ctx: Ctx, Path(id): Path<i64>, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let current = ctx.session_cookie().unwrap_or("").to_string();
    let code: String = form.get("code").chars().filter(|c| !c.is_whitespace()).collect();
    match ctx.state.sessions.remove_passkey(&ctx.actor, &current, id, form.get("password"), &code, &ctx.client).await {
        Ok(Some(session)) => {
            let mut r = ctx.redirect_flash("/settings/security", "ok", "Passkey removed. Two-factor is off.");
            add_cookie(&mut r, &session_cookie(&ctx, &session));
            Ok(r)
        }
        Ok(None) => Ok(ctx.redirect_flash("/settings/security", "ok", "Passkey removed.")),
        Err(e) => match security_errors(&e) {
            Some(errors) => security_page(&ctx, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn recovery_codes_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    match ctx.state.sessions.regenerate_recovery_codes(&ctx.actor, form.get("password"), &ctx.client).await {
        Ok(codes) => {
            let codes_text = codes.join("\n");
            let c = ctx.chrome("New recovery codes").await.nav("settings");
            ctx.page(&TwoFactorDonePage {
                c,
                section: "security".into(),
                heading: "New recovery codes".into(),
                lead: "The old codes no longer work.".into(),
                codes,
                codes_text,
            })
        }
        Err(e) => match security_errors(&e) {
            Some(errors) => security_page(&ctx, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------- sessions

struct SessionRow {
    id: i64,
    browser: String,
    ip: String,
    signed_in: TimeView,
    last_active: TimeView,
    current: bool,
}

#[derive(Template)]
#[template(path = "settings/sessions.html")]
struct SessionsPage {
    c: Chrome,
    section: String,
    rows: Vec<SessionRow>,
}

pub async fn sessions_get(ctx: Ctx) -> WebResult<Response> {
    let current = ctx.session_cookie().unwrap_or("").to_string();
    let list = ctx.state.sessions.sessions(&ctx.actor, &current).await?;
    let rows = list
        .iter()
        .map(|s| SessionRow {
            id: s.id.0,
            browser: browser_label(s.user_agent.as_deref()),
            ip: s.ip.clone(),
            signed_in: fmt::time(s.created_at),
            last_active: fmt::time(s.last_seen_at),
            current: s.current,
        })
        .collect();
    let c = ctx.chrome("Sessions").await.nav("settings");
    ctx.page(&SessionsPage { c, section: "sessions".into(), rows })
}

pub async fn session_revoke(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    ctx.state.sessions.revoke_session(&ctx.actor, SessionId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash("/settings/sessions", "ok", "That session is signed out."))
}

// ---------------------------------------------------------------- SSH keys

#[derive(Debug, Clone, Default)]
struct KeyForm {
    title: String,
    public_key: String,
}

impl KeyForm {
    fn from_form(form: &FormData) -> Self {
        KeyForm { title: form.get("title").trim().to_string(), public_key: form.get("public_key").trim().to_string() }
    }
}

struct KeyRow {
    id: i64,
    title: String,
    key_type: String,
    fingerprint: String,
    added: TimeView,
    last_used: Option<TimeView>,
}

#[derive(Template)]
#[template(path = "settings/keys.html")]
struct KeysPage {
    c: Chrome,
    section: String,
    rows: Vec<KeyRow>,
    form: KeyForm,
    ssh_example: String,
    ssh_tunnel: bool,
    errors: FieldErrors,
}

async fn keys_page(ctx: &Ctx, form: KeyForm, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let keys = ctx.state.accounts.list_ssh_keys(&ctx.actor, uid).await?;
    let rows = keys
        .iter()
        .map(|k| KeyRow {
            id: k.id.0,
            title: k.title.clone(),
            key_type: k.key_type.clone(),
            fingerprint: k.fingerprint.clone(),
            added: fmt::time(k.created_at),
            last_used: fmt::time_opt(k.last_used_at),
        })
        .collect();
    let username = ctx.state.accounts.get_user(uid).await.map(|u| u.username).unwrap_or_default();
    let ssh_example = format!("git@{}:{}/repo.git", ctx.state.config.ssh.external, username);
    let ssh_tunnel = ctx.state.config.ssh.tunnel_only();
    let c = ctx.chrome("SSH keys").await.nav("settings");
    ctx.page_status(status, &KeysPage { c, section: "keys".into(), rows, form, ssh_example, ssh_tunnel, errors })
}

pub async fn keys_get(ctx: Ctx) -> WebResult<Response> {
    keys_page(&ctx, KeyForm::default(), FieldErrors::default(), StatusCode::OK).await
}

pub async fn keys_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let held = KeyForm::from_form(&form);
    let mut errors = FieldErrors::default();
    if held.title.is_empty() {
        errors = errors.with("title", "Give the key a name, such as the device it lives on.");
    }
    if held.public_key.is_empty() {
        errors = errors.with("public_key", "Paste the public key here.");
    }
    if errors.any() {
        return keys_page(&ctx, held, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    match ctx.state.accounts.add_ssh_key(&ctx.actor, uid, &held.title, &held.public_key, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash("/settings/keys", "ok", "SSH key added.")),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => keys_page(&ctx, held, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn key_delete(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    ctx.state.accounts.delete_ssh_key(&ctx.actor, SshKeyId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash("/settings/keys", "ok", "SSH key removed."))
}

// ---------------------------------------------------------------- access tokens

struct TokenRow {
    id: i64,
    name: String,
    prefix: String,
    scopes: Vec<String>,
    expires: TimeView,
    expires_soon: bool,
    last_used: Option<TimeView>,
}

/// The secret of a token just created, shown once in the response that made it.
struct SecretView {
    name: String,
    secret: String,
}

#[derive(Template)]
#[template(path = "settings/tokens.html")]
struct TokensPage {
    c: Chrome,
    section: String,
    rows: Vec<TokenRow>,
    name: String,
    scopes: Vec<ScopeOpt>,
    options: Vec<ExpiryOpt>,
    new_secret: Option<SecretView>,
    errors: FieldErrors,
}

async fn tokens_page(
    ctx: &Ctx,
    name: String,
    chosen: Vec<String>,
    chosen_days: u32,
    new_secret: Option<SecretView>,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let tokens = ctx.state.accounts.list_tokens(&ctx.actor, uid).await?;
    let soon_after = chrono::Utc::now() + chrono::Duration::days(7);
    let rows = tokens
        .iter()
        .map(|t| TokenRow {
            id: t.id.0,
            name: t.name.clone(),
            prefix: t.prefix.clone(),
            scopes: t.scopes.clone(),
            expires: fmt::time(t.expires_at),
            expires_soon: t.expires_at < soon_after,
            last_used: fmt::time_opt(t.last_used_at),
        })
        .collect();
    let c = ctx.chrome("Access tokens").await.nav("settings");
    ctx.page_status(
        status,
        &TokensPage {
            c,
            section: "tokens".into(),
            rows,
            name,
            scopes: scope_options(&chosen),
            options: expiry_options(chosen_days),
            new_secret,
            errors,
        },
    )
}

pub async fn tokens_get(ctx: Ctx) -> WebResult<Response> {
    tokens_page(&ctx, String::new(), Vec::new(), 90, None, FieldErrors::default(), StatusCode::OK).await
}

pub async fn tokens_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let name = form.get("name").trim().to_string();
    let chosen: Vec<String> = form.all("scopes").into_iter().map(str::to_string).collect();
    let mut errors = FieldErrors::default();
    if name.is_empty() {
        errors = errors.with("name", "Give the token a name, such as the script that uses it.");
    }
    if chosen.is_empty() {
        errors = errors.with("scopes", "Choose at least one scope.");
    } else if !scopes_valid(&chosen) {
        errors = errors.with("scopes", "Choose only the scopes listed.");
    }
    if errors.any() {
        let days = expiry_days(form.get("expires_in_days"));
        return tokens_page(&ctx, name, chosen, days, None, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let days = expiry_days(form.get("expires_in_days"));
    match ctx.state.accounts.create_token(&ctx.actor, uid, &name, chosen.clone(), days, &ctx.client).await {
        Ok((token, secret)) => {
            let new_secret = SecretView { name: token.name.clone(), secret };
            tokens_page(&ctx, String::new(), Vec::new(), 90, Some(new_secret), FieldErrors::default(), StatusCode::OK).await
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => tokens_page(&ctx, name, chosen, days, None, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn token_delete(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    ctx.state.accounts.delete_token(&ctx.actor, TokenId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash("/settings/tokens", "ok", "Token deleted."))
}

// ---------------------------------------------------------------- appearance

#[derive(Template)]
#[template(path = "settings/appearance.html")]
struct AppearancePage {
    c: Chrome,
    section: String,
    theme: String,
    errors: FieldErrors,
}

async fn appearance_page(ctx: &Ctx, theme: &str, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    ctx.user_id()?;
    let c = ctx.chrome("Appearance").await.nav("settings");
    ctx.page_status(status, &AppearancePage { c, section: "appearance".into(), theme: theme.into(), errors })
}

pub async fn appearance_get(ctx: Ctx) -> WebResult<Response> {
    let theme = ctx.theme();
    appearance_page(&ctx, &theme, FieldErrors::default(), StatusCode::OK).await
}

pub async fn appearance_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let value = form.get("theme");
    if !matches!(value, "system" | "light" | "dark") {
        let errors = FieldErrors::default().with("theme", "Choose system, light or dark.");
        let theme = ctx.theme();
        return appearance_page(&ctx, &theme, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    // Not HttpOnly: app.js reads this cookie to set the theme before first paint.
    let cookie = ctx.set_cookie(THEME_COOKIE, value, Some(31_536_000), false, "Lax", "/");
    let mut r = ctx.redirect_flash("/settings/appearance", "ok", "Theme saved.");
    add_cookie(&mut r, &cookie);
    Ok(r)
}

// ---------------------------------------------------------------- notifications

#[derive(Template)]
#[template(path = "settings/notifications.html")]
struct NotificationsPage {
    c: Chrome,
    section: String,
    on: bool,
    mail_on: bool,
}

pub async fn notifications_get(ctx: Ctx) -> WebResult<Response> {
    ctx.user_id()?;
    let on = ctx.state.notifications.email_on(&ctx.actor).await?;
    let c = ctx.chrome("Notifications").await.nav("settings");
    ctx.page(&NotificationsPage { c, section: "notifications".into(), on, mail_on: ctx.state.mail.enabled() })
}

pub async fn notifications_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    ctx.state.notifications.set_email_on(&ctx.actor, form.flag("email")).await?;
    Ok(ctx.redirect_flash("/settings/notifications", "ok", "Saved."))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_actions_get_plain_words_and_unknown_pass_through() {
        assert_eq!(friendly_action(action::SIGN_IN), "Signed in");
        assert_eq!(friendly_action(action::SIGN_IN_FAILED), "Sign in failed");
        assert_eq!(friendly_action(action::PASSWORD_CHANGED), "Password changed");
        assert_eq!(friendly_action(action::TWO_FACTOR_ON), "Two-factor turned on");
        assert_eq!(friendly_action(action::TWO_FACTOR_OFF), "Two-factor turned off");
        assert_eq!(friendly_action(action::TOKEN_CREATED), "Access token created");
        assert_eq!(friendly_action(action::SSH_KEY_REMOVED), "SSH key removed");
        assert_eq!(friendly_action(action::SESSION_REVOKED), "Session signed out");
        assert_eq!(friendly_action("something.new"), "something.new");
    }

    #[test]
    fn user_agents_shorten_to_a_browser_and_an_os() {
        assert_eq!(
            browser_label(Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:130.0) Gecko/20100101 Firefox/130.0")),
            "Firefox on macOS"
        );
        assert_eq!(
            browser_label(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36"
            )),
            "Chrome on Windows"
        );
        assert_eq!(
            browser_label(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36 Edg/128.0.0.0"
            )),
            "Edge on Windows"
        );
        assert_eq!(
            browser_label(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"
            )),
            "Safari on iOS"
        );
        assert_eq!(
            browser_label(Some(
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Mobile Safari/537.36"
            )),
            "Chrome on Android"
        );
        assert_eq!(browser_label(Some("Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0")), "Firefox on Linux");
        assert_eq!(browser_label(Some("curl/8.7.1")), "Unknown browser");
        assert_eq!(browser_label(Some("")), "Unknown browser");
        assert_eq!(browser_label(Some("   ")), "Unknown browser");
        assert_eq!(browser_label(None), "Unknown browser");
    }

    #[test]
    fn outcomes_expiries_and_scopes() {
        assert_eq!(outcome_chip(Outcome::Success), ("chip-ok", "success"));
        assert_eq!(outcome_chip(Outcome::Failure), ("chip-fail", "failed"));

        assert_eq!(expiry_days("30"), 30);
        assert_eq!(expiry_days("90"), 90);
        assert_eq!(expiry_days(" 180 "), 180);
        assert_eq!(expiry_days("365"), 365);
        assert_eq!(expiry_days("7"), 90);
        assert_eq!(expiry_days("nonsense"), 90);
        let options = expiry_options(90);
        assert_eq!(options.len(), 4);
        assert!(options.iter().find(|o| o.days == 90).unwrap().selected);
        assert!(!options.iter().find(|o| o.days == 30).unwrap().selected);

        assert!(scopes_valid(&["repo".into(), "user".into()]));
        assert!(!scopes_valid(&[]));
        assert!(!scopes_valid(&["repo".into(), "root".into()]));
        let options = scope_options(&["user".into()]);
        assert_eq!(options.len(), 10);
        assert!(options.iter().find(|o| o.value == "user").unwrap().checked);
        assert!(!options.iter().find(|o| o.value == "repo").unwrap().checked);
    }

    #[test]
    fn password_errors_move_to_the_form_fields() {
        let e = password_errors(FieldErrors::default().with("current", "That password is wrong."));
        assert_eq!(e.get("current_password"), "That password is wrong.");
        let e = password_errors(FieldErrors::default().with("new", "Choose a password that is harder to guess."));
        assert_eq!(e.get("new_password"), "Choose a password that is harder to guess.");
    }
}

// ROUTES:
// GET /settings -> index
// GET+POST /settings/profile -> profile_get, profile_post
// GET+POST /settings/password -> password_get, password_post
// GET /settings/security -> security_get
// POST /settings/security/2fa/start -> two_factor_start
// POST /settings/security/2fa/confirm -> two_factor_confirm
// POST /settings/security/2fa/disable -> two_factor_disable
// POST /settings/security/passkeys/begin -> passkey_begin
// POST /settings/security/passkeys/finish -> passkey_finish
// POST /settings/security/passkeys/{id}/remove -> passkey_remove
// POST /settings/security/recovery-codes -> recovery_codes_post
// GET /settings/sessions -> sessions_get
// POST /settings/sessions/{id}/revoke -> session_revoke
// GET+POST /settings/keys -> keys_get, keys_post
// POST /settings/keys/{id}/delete -> key_delete
// GET+POST /settings/tokens -> tokens_get, tokens_post
// POST /settings/tokens/{id}/delete -> token_delete
// GET+POST /settings/appearance -> appearance_get, appearance_post
// GET+POST /settings/notifications -> notifications_get, notifications_post
