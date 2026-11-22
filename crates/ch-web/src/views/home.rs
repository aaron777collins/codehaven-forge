//! The dashboard: your repositories, your unread notifications, your organizations, and
//! what was recently updated across the server. A side card whose service is not built yet
//! simply does not appear; the repository list says so instead of failing the page.

use super::people::{HitRow, NotificationRow, PersonRow, RepoCard, hide_unavailable, hit_rows, notification_rows, person_row, repo_cards};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::WebResult;
use askama::Template;
use axum::response::Response;

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    c: Chrome,
    repos: Vec<RepoCard>,
    repos_ok: bool,
    notifications: Vec<NotificationRow>,
    show_notifications: bool,
    orgs: Vec<PersonRow>,
    show_orgs: bool,
    recent: Vec<HitRow>,
    show_recent: bool,
}

pub async fn home(ctx: Ctx) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let repos = hide_unavailable(ctx.state.repos.list_for_owner(&ctx.actor, uid).await)?;
    let notes = hide_unavailable(ctx.state.notifications.inbox(&ctx.actor, true, 1, 8).await)?;
    let notifications = match notes {
        Some(list) => Some(notification_rows(&ctx, &list).await),
        None => None,
    };
    let orgs: Option<Vec<PersonRow>> =
        hide_unavailable(ctx.state.people.orgs_of(&ctx.actor, uid).await)?.map(|v| v.iter().map(|o| person_row(o, "", "")).collect());
    let recent = hide_unavailable(ctx.state.search.repos(&ctx.actor, "", 6).await)?.map(|v| hit_rows(&v, true));
    let c = ctx.chrome("Home").await.nav("home");
    ctx.page(&HomePage {
        c,
        repos_ok: repos.is_some(),
        repos: repos.map(|v| repo_cards(&v)).unwrap_or_default(),
        show_notifications: notifications.is_some(),
        notifications: notifications.unwrap_or_default(),
        show_orgs: orgs.is_some(),
        orgs: orgs.unwrap_or_default(),
        show_recent: recent.is_some(),
        recent: recent.unwrap_or_default(),
    })
}

// ROUTES: none to add. `GET / -> home` is already registered in routes.rs.
