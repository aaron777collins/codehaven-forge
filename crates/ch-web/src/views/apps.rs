//! Connected apps: the consent page an MCP client sends a person to, the page that returns them
//! to the app, and the list of what they have connected.
//!
//! Nothing the client claimed is trusted. Its name, redirect and state are escaped like any
//! other text, and the redirect was already matched exactly by `Apps::begin_authorize`.

use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::{Path, RawQuery};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ch_core::apps::*;
use ch_core::{GrantId, Permission, RepoId};

// ---------------------------------------------------------------- pure helpers

/// The query of an authorize request. A name sent twice makes the whole request refused.
pub(crate) fn parse_authorize(query: &str) -> Result<AuthorizeParams, &'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut p = AuthorizeParams::default();
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        if !seen.insert(k.to_string()) {
            return Err("A parameter was sent twice, so the request was refused.");
        }
        let v = v.into_owned();
        match k.as_ref() {
            "client_id" => p.client_id = v,
            "redirect_uri" => p.redirect_uri = v,
            "response_type" => p.response_type = v,
            "code_challenge" => p.code_challenge = v,
            "code_challenge_method" => p.code_challenge_method = v,
            "state" => p.state = Some(v),
            "scope" => p.scope = Some(v),
            "resource" => p.resource = Some(v),
            _ => {}
        }
    }
    Ok(p)
}

fn ids(form: &FormData, name: &str) -> Vec<RepoId> {
    form.all(name).into_iter().filter_map(|s| s.parse::<i64>().ok()).map(RepoId).collect()
}

/// Reads the consent form into choices. `Err` is a sentence for the person.
pub(crate) fn parse_choices(form: &FormData) -> Result<ConsentChoices, &'static str> {
    let read = match form.get("read") {
        "all" => RepoChoice::All,
        "selected" => RepoChoice::Selected(ids(form, "read_repos")),
        _ => return Err("Choose which repositories the app may read."),
    };
    let write = match form.get("write") {
        "all" => RepoChoice::All,
        "selected" => RepoChoice::Selected(ids(form, "write_repos")),
        "" | "none" => RepoChoice::None,
        _ => return Err("Choose what the app may write."),
    };
    let workflows = match form.get("workflows") {
        "all" => RepoChoice::All,
        "selected" => RepoChoice::Selected(ids(form, "workflow_repos")),
        "" | "none" => RepoChoice::None,
        _ => return Err("Choose whether the app may change workflows."),
    };
    // An empty selection is no selection.
    let norm = |c: RepoChoice| match c {
        RepoChoice::Selected(v) if v.is_empty() => RepoChoice::None,
        other => other,
    };
    let expires_days = form.get("expires_days").trim().parse().map_err(|_| "Choose how long the connection lasts.")?;
    Ok(ConsentChoices {
        request_id: form.get("request_id").to_string(),
        read,
        write: norm(write),
        workflows: norm(workflows),
        expires_days,
    })
}

fn scope_words(scopes: &[String]) -> String {
    scopes
        .iter()
        .map(|s| match s.as_str() {
            "read" => "read",
            "write" => "write",
            "workflows" => "run workflows",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------- authorize and consent

struct RepoOpt {
    id: i64,
    name: String,
    private: bool,
}

#[derive(Template)]
#[template(path = "oauth/consent.html")]
struct ConsentPage {
    c: Chrome,
    section: String,
    v: ConsentView,
    repos: Vec<RepoOpt>,
    writable: Vec<RepoOpt>,
    asked: String,
}

#[derive(Template)]
#[template(path = "oauth/refused.html")]
struct RefusedPage {
    c: Chrome,
    section: String,
    title: String,
    message: String,
    two_factor: bool,
}

#[derive(Template)]
#[template(path = "oauth/return.html")]
struct ReturnPage {
    css_app: String,
    css_forge: String,
    url: String,
    host: String,
    denied: bool,
}

async fn refused(ctx: &Ctx, title: &str, message: &str, two_factor: bool) -> WebResult<Response> {
    let c = ctx.chrome("Cannot connect").await.nav("settings");
    ctx.page_status(
        StatusCode::BAD_REQUEST,
        &RefusedPage { c, section: "apps".into(), title: title.into(), message: message.into(), two_factor },
    )
}

pub async fn authorize_get(ctx: Ctx, RawQuery(query): RawQuery) -> WebResult<Response> {
    ctx.user_id()?;
    let Some(session) = ctx.session_cookie().map(str::to_string) else { return Err(WebError::Core(ch_core::Error::unauthorized())) };
    let params = match parse_authorize(query.as_deref().unwrap_or("")) {
        Ok(p) => p,
        Err(m) => return refused(&ctx, "Cannot connect", m, false).await,
    };
    match ctx.state.apps.begin_authorize(&ctx.actor, &session, params, &ctx.client).await? {
        AuthorizeOutcome::NeedsTwoFactor => {
            refused(&ctx, "Turn on two-factor, then connect again", "Connecting an app needs two-factor sign-in on your account.", true)
                .await
        }
        AuthorizeOutcome::Refused(m) => refused(&ctx, "Cannot connect", &m, false).await,
        AuthorizeOutcome::Consent(v) => {
            let all = ctx.state.repos.list_accessible(&ctx.actor, 1, 100).await?;
            let mut repos = Vec::new();
            let mut writable = Vec::new();
            for r in all {
                let can_write =
                    ctx.state.authz.repo_permission(&ctx.actor, r.id).await.map(|p| p.allows(Permission::Write)).unwrap_or(false)
                        && !r.archived;
                let opt = || RepoOpt { id: r.id.0, name: r.full_name(), private: r.visibility == ch_core::model::Visibility::Private };
                if can_write {
                    writable.push(opt());
                }
                repos.push(opt());
            }
            let asked = scope_words(&v.requested_scopes.iter().filter(|s| *s != SCOPE_OFFLINE).cloned().collect::<Vec<_>>());
            let c = ctx.chrome("Connect an app").await.nav("settings");
            let page = ConsentPage { c, section: "apps".into(), asked, v, repos, writable };
            // The consent page keeps the site's `same-origin` referrer policy, otherwise the
            // browser sends `Origin: null` with the form and the origin check refuses it.
            ctx.page(&page)
        }
    }
}

pub async fn consent_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    let Some(session) = ctx.session_cookie().map(str::to_string) else { return Err(WebError::Core(ch_core::Error::unauthorized())) };
    let request_id = form.get("request_id").to_string();
    let target = if form.get("decision") == "deny" {
        ctx.state.apps.deny(&ctx.actor, &session, &request_id, &ctx.client).await?
    } else {
        let choices = parse_choices(&form).map_err(|m| WebError::Bad(m.to_string()))?;
        ctx.state.apps.approve(&ctx.actor, &session, choices, &ctx.client).await?
    };
    // A 303 from this POST would be blocked by `form-action 'self'` in the CSP, so the browser is
    // sent on by a page with a meta refresh and a link.
    let page = ReturnPage {
        css_app: crate::assets::url("app.css"),
        css_forge: crate::assets::url("forge.css"),
        url: target.url,
        host: target.host,
        denied: form.get("decision") == "deny",
    };
    let body = page.render().map_err(|e| WebError::Core(ch_core::Error::internal(anyhow::anyhow!("template: {e}"))))?;
    let mut r = (StatusCode::OK, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    Ok(r)
}

// ---------------------------------------------------------------- settings: connected apps

struct GrantRepoRow {
    name: String,
    access: &'static str,
}

struct GrantRow {
    id: i64,
    host: String,
    name: String,
    redirect_host: String,
    created: TimeView,
    last_used: Option<TimeView>,
    expires: TimeView,
    scopes: String,
    all_read: bool,
    all_write: bool,
    repos: Vec<GrantRepoRow>,
}

#[derive(Template)]
#[template(path = "settings/apps.html")]
struct AppsPage {
    c: Chrome,
    section: String,
    rows: Vec<GrantRow>,
    enabled: bool,
    mcp_url: String,
}

pub async fn apps_get(ctx: Ctx) -> WebResult<Response> {
    ctx.user_id()?;
    let grants = ctx.state.apps.list_grants(&ctx.actor).await?;
    let rows = grants
        .iter()
        .map(|g| GrantRow {
            id: g.id.0,
            host: g.client_host.clone(),
            name: g.client_name.clone(),
            redirect_host: g.redirect_host.clone(),
            created: fmt::time(g.created_at),
            last_used: g.last_used_at.map(fmt::time),
            expires: fmt::time(g.expires_at),
            scopes: scope_words(&g.scopes),
            all_read: g.read_all,
            all_write: g.write_all,
            repos: g
                .repos
                .iter()
                .map(|r| GrantRepoRow {
                    name: if r.full_name.is_empty() { "a repository you can no longer see".into() } else { r.full_name.clone() },
                    access: if r.can_workflows {
                        "read, write and workflows"
                    } else if r.can_write {
                        "read and write"
                    } else {
                        "read"
                    },
                })
                .collect(),
        })
        .collect();
    let c = ctx.chrome("Connected apps").await.nav("settings");
    ctx.page(&AppsPage { c, section: "apps".into(), rows, enabled: ctx.state.config.mcp.enabled, mcp_url: ctx.state.config.mcp_resource() })
}

pub async fn app_revoke(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    ctx.user_id()?;
    ctx.state.apps.revoke_grant(&ctx.actor, GrantId(id), &ctx.client).await?;
    Ok(ctx.redirect_flash("/settings/apps", "ok", "App disconnected."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_query_refuses_duplicates() {
        let q =
            "client_id=a&redirect_uri=b&response_type=code&code_challenge=c&code_challenge_method=S256&state=s&scope=read+write&resource=r";
        let p = parse_authorize(q).unwrap();
        assert_eq!((p.client_id.as_str(), p.state.as_deref(), p.scope.as_deref()), ("a", Some("s"), Some("read write")));
        assert!(parse_authorize("client_id=a&client_id=b").is_err());
        assert!(parse_authorize("state=a&state=a").is_err(), "even identical copies");
        assert!(parse_authorize("").unwrap().client_id.is_empty());
    }

    #[test]
    fn consent_form_choices() {
        let f = |s: &str| FormData::parse(s.as_bytes());
        // nothing is preselected for read
        assert!(parse_choices(&f("request_id=x&expires_days=90")).is_err());
        let c = parse_choices(&f(
            "request_id=x&read=selected&read_repos=1&read_repos=2&write=selected&write_repos=2&workflows=none&expires_days=90",
        ))
        .unwrap();
        assert_eq!(c.read, RepoChoice::Selected(vec![RepoId(1), RepoId(2)]));
        assert_eq!(c.write, RepoChoice::Selected(vec![RepoId(2)]));
        assert_eq!(c.workflows, RepoChoice::None);
        // write defaults to none
        let c = parse_choices(&f("request_id=x&read=all&expires_days=180")).unwrap();
        assert_eq!((c.read, c.write, c.workflows, c.expires_days), (RepoChoice::All, RepoChoice::None, RepoChoice::None, 180));
        // an empty selection is no selection, and junk ids are dropped
        let c = parse_choices(&f("request_id=x&read=all&write=selected&write_repos=abc&expires_days=90")).unwrap();
        assert_eq!(c.write, RepoChoice::None);
        assert!(parse_choices(&f("request_id=x&read=all&write=bogus&expires_days=90")).is_err());
        assert!(parse_choices(&f("request_id=x&read=all")).is_err(), "expiry is required");
    }
}
