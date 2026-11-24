//! Explore, search, notifications, profiles and organizations, plus the new repository and
//! new organization forms. The row types the dashboard shares (repositories, search hits,
//! notification rows, people) live here too.

use super::{FieldErrors, Pager, Params, Query};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::model::{NewRepo, Notification, Repo, SearchHit, User, UserKind, Visibility};
use ch_core::services::OrgRole;
use ch_core::{Error, NotificationId, OrgId, Permission, RepoId, TeamId, UserId};
use std::collections::HashMap;

// ---------------------------------------------------------------- shared rows and helpers

/// A side card's data, or nothing when the service behind it is not built yet. Any other
/// error still fails the page.
pub fn hide_unavailable<T>(r: ch_core::Result<T>) -> ch_core::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Error::Unavailable(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Why a notification exists, in plain words.
pub fn reason_text(reason: &str) -> &'static str {
    match reason {
        "assign" | "assigned" => "You were assigned",
        "mention" => "You were mentioned",
        "team_mention" => "Your team was mentioned",
        "review_requested" => "Review requested",
        "comment" => "You commented",
        "author" => "You opened this",
        "subscribed" => "You watch this repository",
        "ci_activity" => "A workflow ran",
        "security_alert" => "Security alert",
        "invitation" => "You were invited",
        "manual" => "You subscribed",
        "state_change" => "Its state changed",
        _ => "Activity",
    }
}

/// Where a notification's subject lives, when the repository is known. A commit subject
/// carries no sha in the notification, so there is nowhere to link it; an unknown kind
/// lands on the repository home.
fn subject_href(kind: &str, subject_id: i64, owner: &str, name: &str) -> Option<String> {
    let base = format!("/{}/{}", fmt::enc(owner), fmt::enc(name));
    match kind {
        "issue" => Some(format!("{base}/issues/{subject_id}")),
        "pull" | "pull_request" => Some(format!("{base}/pulls/{subject_id}")),
        "commit" => None,
        _ => Some(base),
    }
}

/// One notification as a row.
pub struct NotificationRow {
    pub id: i64,
    pub unread: bool,
    pub title: String,
    /// The title with `code` spans drawn, escaped.
    pub title_html: String,
    /// `open`, `closed` or `merged` for an issue or pull request, as a chip.
    pub state: Option<(&'static str, &'static str)>,
    /// The page the subject lives on, when the repository can be resolved.
    pub href: Option<String>,
    /// `owner/name`, when the repository can be resolved.
    pub repo: Option<String>,
    pub reason: &'static str,
    pub when: TimeView,
}

/// Notification rows, looking each distinct repository up once per page.
pub async fn notification_rows(ctx: &Ctx, notes: &[Notification]) -> Vec<NotificationRow> {
    let mut seen: HashMap<RepoId, Option<Repo>> = HashMap::new();
    let mut out = Vec::with_capacity(notes.len());
    for n in notes {
        let repo = match n.repo_id {
            Some(rid) => {
                if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(rid) {
                    slot.insert(ctx.state.repos.get(&ctx.actor, rid).await.ok());
                }
                seen.get(&rid).cloned().flatten()
            }
            None => None,
        };
        let (href, repo_name) = match &repo {
            // An issue or pull request links by its number. Without one (an old row whose
            // subject is gone) there is no page to link to.
            Some(r) => match (n.subject_type.as_str(), n.number) {
                ("issue" | "pull" | "pull_request", None) => (None, Some(r.full_name())),
                (_, number) => (subject_href(&n.subject_type, number.unwrap_or(n.subject_id), &r.owner_name, &r.name), Some(r.full_name())),
            },
            None => (None, None),
        };
        out.push(NotificationRow {
            id: n.id.0,
            unread: n.unread,
            title: n.title.clone(),
            title_html: fmt::title_html(&n.title),
            state: match n.state.as_deref() {
                Some("open") => Some(("open", "Open")),
                Some("closed") => Some(("closed", "Closed")),
                Some("merged") => Some(("merged", "Merged")),
                _ => None,
            },
            href,
            repo: repo_name,
            reason: reason_text(&n.reason),
            when: fmt::time(n.updated_at),
        });
    }
    out
}

/// A search hit as a row. `link` says whether the title can point somewhere: a repository
/// or account hit knows its address from its title, an issue hit does not.
pub struct HitRow {
    pub href: Option<String>,
    pub title: String,
    pub snippet: String,
    pub kind: String,
}

pub fn hit_rows(hits: &[SearchHit], link: bool) -> Vec<HitRow> {
    hits.iter()
        .map(|h| HitRow {
            href: if link { Some(format!("/{}", fmt::enc_path(&h.title))) } else { None },
            title: h.title.clone(),
            snippet: h.snippet.clone(),
            kind: h.kind.clone(),
        })
        .collect()
}

/// A person or an organization as a list row.
pub struct PersonRow {
    pub username: String,
    pub display: String,
    pub href: String,
    /// A chip such as Owner, Member or Organization. Empty means none.
    pub chip: String,
    /// Extra chip classes, such as `chip-muted`.
    pub chip_class: String,
}

pub fn display_of(u: &User) -> String {
    if u.display_name.is_empty() { u.username.clone() } else { u.display_name.clone() }
}

pub fn person_row(u: &User, chip: &str, chip_class: &str) -> PersonRow {
    PersonRow {
        username: u.username.clone(),
        display: display_of(u),
        href: format!("/{}", fmt::enc(&u.username)),
        chip: chip.into(),
        chip_class: chip_class.into(),
    }
}

/// A repository as a dashboard or profile row.
pub struct RepoCard {
    pub href: String,
    pub name: String,
    pub private: bool,
    /// The chip: `Private`, `Internal` or `Public`, and its explanation.
    pub tier: &'static str,
    pub tier_title: &'static str,
    pub description: String,
    /// When it was last pushed to. Empty repositories have none.
    pub updated: Option<TimeView>,
}

pub fn repo_cards(repos: &[Repo]) -> Vec<RepoCard> {
    repos
        .iter()
        .map(|r| RepoCard {
            href: format!("/{}/{}", fmt::enc(&r.owner_name), fmt::enc(&r.name)),
            name: r.name.clone(),
            private: r.visibility == Visibility::Private,
            tier: super::visibility::chip(r.visibility).0,
            tier_title: super::visibility::chip(r.visibility).1,
            description: r.description.clone(),
            updated: fmt::time_opt(r.pushed_at),
        })
        .collect()
}

/// A tab in a row of tabs.
pub struct TabItem {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

/// The trimmed query, empty when blank.
fn query_of(q: &Params) -> String {
    q.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_default()
}

fn page_of(q: &Params) -> u32 {
    q.get("page").and_then(|p| p.parse().ok()).filter(|p| *p >= 1).unwrap_or(1)
}

/// The search tab asked for. Anything unknown reads as repositories.
fn search_type(t: Option<&str>) -> &'static str {
    match t {
        Some("issues") => "issues",
        Some("users") => "users",
        Some("code") => "code",
        _ => "repos",
    }
}

fn search_tabs(q: &str, current: &str) -> Vec<TabItem> {
    let href = |t: &str| {
        if q.is_empty() { format!("/search?type={t}") } else { format!("/search?q={}&type={t}", fmt::enc(q)) }
    };
    [("repos", "Repositories"), ("issues", "Issues"), ("code", "Code"), ("users", "People")]
        .into_iter()
        .map(|(t, label)| TabItem { label, href: href(t), current: t == current })
        .collect()
}

/// What is wrong with a repository name, or nothing. The rules: 1 to 100 characters of
/// letters, digits, dots, dashes and underscores, no leading dot, no `.git` ending.
pub fn repo_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("Give the repository a name.");
    }
    if name.chars().count() > 100 {
        return Some("Use up to 100 characters.");
    }
    if name.starts_with('.') {
        return Some("The name cannot start with a dot.");
    }
    if name.to_lowercase().ends_with(".git") {
        return Some("The name cannot end in .git.");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        return Some("Use letters, numbers, dots, dashes and underscores.");
    }
    None
}

/// The choice on the new repository form. Anything unknown is private.
pub(crate) fn visibility_of(s: &str) -> Visibility {
    Visibility::parse(s).unwrap_or(Visibility::Private)
}

/// The first letter uppercased, for words like "Read" and "Write".
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

// ------------------------------------------------------------------ org form helpers

/// One `<option>` of a select.
struct SelectOption {
    value: String,
    label: String,
    selected: bool,
}

const PERMISSIONS: [(&str, &str); 5] =
    [("read", "Read"), ("triage", "Triage"), ("write", "Write"), ("maintain", "Maintain"), ("admin", "Admin")];

/// The permission choices for a team, in order, with one marked as the current selection.
fn permission_options(current: &str) -> Vec<SelectOption> {
    let mut list: Vec<SelectOption> = PERMISSIONS
        .iter()
        .map(|(v, l)| SelectOption { value: (*v).to_string(), label: (*l).to_string(), selected: *v == current })
        .collect();
    if !current.is_empty() && !list.iter().any(|o| o.selected) {
        list.push(SelectOption { value: current.to_string(), label: capitalized(current), selected: true });
    }
    list
}

/// What organization members can do with a new repository by default. The short list is the
/// one GitHub offers; a value set through the API still shows up as a choice.
fn base_permission_options(current: &str) -> Vec<SelectOption> {
    let mut list: Vec<SelectOption> = [("none", "None"), ("read", "Read"), ("write", "Write"), ("admin", "Admin")]
        .iter()
        .map(|(v, l)| SelectOption { value: (*v).to_string(), label: (*l).to_string(), selected: *v == current })
        .collect();
    if !current.is_empty() && !list.iter().any(|o| o.selected) {
        list.push(SelectOption { value: current.to_string(), label: capitalized(current), selected: true });
    }
    list
}

/// A permission field as the service accepts it. "none" and unknown words are refused: a
/// team always carries a real level of access.
fn team_permission_of(raw: &str) -> Option<Permission> {
    Permission::parse(raw).filter(|p| *p != Permission::None)
}

/// A service error as one flash line. A validation error states its rule; the bare heading
/// "Validation Failed" helps nobody.
pub fn flash_error(e: &Error) -> String {
    match e {
        Error::Validation(list) => list.first().and_then(|f| f.message.clone()).unwrap_or_else(|| "Please check what you entered.".into()),
        other => other.public_message(),
    }
}

/// The viewer's role in an organization. A role that cannot be read counts as no role.
async fn viewer_role(ctx: &Ctx, org: OrgId) -> Option<OrgRole> {
    ctx.state.authz.org_role(&ctx.actor, org).await.ok().flatten()
}

/// A person's account by username, or none. An organization is not a person.
async fn find_person(ctx: &Ctx, username: &str) -> WebResult<Option<User>> {
    match ctx.state.accounts.get_by_username(&ctx.actor, username).await {
        Ok(u) if u.kind == UserKind::User => Ok(Some(u)),
        Ok(_) | Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ------------------------------------------------------------------ explore

#[derive(Template)]
#[template(path = "people/explore.html")]
struct ExplorePage {
    c: Chrome,
    q: String,
    repos: Vec<HitRow>,
    show_repos: bool,
    people: Vec<PersonRow>,
    show_people: bool,
}

pub async fn explore(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    let qs = query_of(&q);
    let repos = hide_unavailable(ctx.state.search.repos(&ctx.actor, &qs, 30).await)?;
    // No people search signed out: a stranger sees public repositories and nothing else.
    let users = if ctx.actor.is_anonymous() {
        None
    } else {
        hide_unavailable(ctx.state.people.list_users(&ctx.actor, if qs.is_empty() { None } else { Some(qs.as_str()) }, 30).await)?
    };
    let c = ctx.chrome("Explore").await.nav("explore");
    ctx.page(&ExplorePage {
        c,
        q: qs.clone(),
        show_repos: repos.is_some(),
        repos: repos.map(|v| hit_rows(&v, true)).unwrap_or_default(),
        show_people: users.is_some(),
        people: users
            .map(|v| v.iter().map(|u| person_row(u, if u.kind == UserKind::Org { "Organization" } else { "" }, "")).collect())
            .unwrap_or_default(),
    })
}

// ------------------------------------------------------------------ search

#[derive(Template)]
#[template(path = "people/search.html")]
struct SearchPage {
    c: Chrome,
    q: String,
    kind: &'static str,
    tabs: Vec<TabItem>,
    hits: Vec<HitRow>,
    show_kind: bool,
    code: Vec<CodeRow>,
}

/// One matching line of code, linked to its line on the default branch.
pub struct CodeRow {
    pub repo: String,
    pub repo_href: String,
    pub location: String,
    pub href: String,
    pub text: String,
}

pub fn code_rows(hits: &[ch_core::model::CodeHit]) -> Vec<CodeRow> {
    hits.iter()
        .map(|h| {
            let base = format!("/{}", fmt::enc_path(&h.repo));
            CodeRow {
                repo: h.repo.clone(),
                href: format!("{base}/blob/{}/{}#LC{}", fmt::enc_path(&h.branch), fmt::enc_path(&h.path), h.line),
                repo_href: base,
                location: format!("{}:{}", h.path, h.line),
                text: h.text.clone(),
            }
        })
        .collect()
}

pub async fn search(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    let qs = query_of(&q);
    let kind = search_type(q.get("type").map(String::as_str));
    let mut code = Vec::new();
    let hits = if qs.is_empty() {
        Vec::new()
    } else {
        let found = match kind {
            "issues" => ctx.state.search.issues(&ctx.actor, &qs, 30).await?,
            "users" => ctx.state.search.users(&ctx.actor, &qs, 30).await?,
            "code" => {
                code = code_rows(&ctx.state.search.code_all(&ctx.actor, &qs, 50).await?);
                Vec::new()
            }
            _ => ctx.state.search.repos(&ctx.actor, &qs, 30).await?,
        };
        hit_rows(&found, kind != "issues")
    };
    let c = ctx.chrome("Search").await;
    ctx.page(&SearchPage { c, tabs: search_tabs(&qs, kind), q: qs, kind, hits, show_kind: kind == "issues", code })
}

// ------------------------------------------------------------------ notifications

#[derive(Template)]
#[template(path = "people/notifications.html")]
struct NotificationsPage {
    c: Chrome,
    all: bool,
    rows: Vec<NotificationRow>,
    pager: Pager,
}

pub async fn notifications(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    let all = q.get("all").map(String::as_str) == Some("1");
    let page = page_of(&q);
    let mut notes = ctx.state.notifications.inbox(&ctx.actor, !all, page, 31).await?;
    let has_more = notes.len() > 30;
    notes.truncate(30);
    let rows = notification_rows(&ctx, &notes).await;
    let base = if all { "/notifications?all=1".to_string() } else { "/notifications".to_string() };
    let c = ctx.chrome("Notifications").await.nav("notifications");
    ctx.page(&NotificationsPage { c, all, rows, pager: Pager::new(&base, page, has_more) })
}

pub async fn mark_one_read(ctx: Ctx, Path(id): Path<i64>, _form: FormData) -> WebResult<Response> {
    ctx.state.notifications.mark_read(&ctx.actor, Some(NotificationId(id))).await?;
    Ok(ctx.redirect("/notifications"))
}

pub async fn mark_all_read(ctx: Ctx, _form: FormData) -> WebResult<Response> {
    ctx.state.notifications.mark_read(&ctx.actor, None).await?;
    Ok(ctx.redirect("/notifications"))
}

// ------------------------------------------------------------------ profile and organizations

/// An organization looked up by its login. Anything else is the same 404 as a missing
/// account, so a person's profile page is never shown on an organization address.
async fn org_by_name(ctx: &Ctx, name: &str) -> WebResult<User> {
    let org = ctx.state.accounts.get_by_username(&ctx.actor, name).await?;
    if org.kind == UserKind::Org { Ok(org) } else { Err(WebError::not_found()) }
}

#[derive(Template)]
#[template(path = "people/profile.html")]
struct ProfilePage {
    /// A signed-out visitor: no join date.
    signed_out: bool,
    c: Chrome,
    login: String,
    login_enc: String,
    display: String,
    base: String,
    is_org: bool,
    org_owner: bool,
    org_id: i64,
    joined: TimeView,
    repos: Vec<RepoCard>,
    orgs: Vec<PersonRow>,
}

pub async fn profile(ctx: Ctx, Path(owner): Path<String>) -> WebResult<Response> {
    // Signed out, an account exists only while it owns a public repository, and its page shows
    // its names and its public repositories and nothing else: no join date, no organizations.
    let signed_out = ctx.actor.is_anonymous();
    let user = if signed_out {
        ctx.state.accounts.get_by_username(&ch_core::Actor::System, &owner).await.map_err(|_| WebError::Core(Error::unauthorized()))?
    } else {
        ctx.state.accounts.get_by_username(&ctx.actor, &owner).await?
    };
    let is_org = user.kind == UserKind::Org;
    let org_owner = !signed_out && is_org && viewer_role(&ctx, OrgId(user.id.0)).await == Some(OrgRole::Owner);
    let listed = ctx.state.repos.list_for_owner(&ctx.actor, user.id).await?;
    if signed_out && listed.is_empty() {
        return Err(WebError::Core(Error::unauthorized()));
    }
    let repos = repo_cards(&listed);
    let orgs = if is_org || signed_out {
        Vec::new()
    } else {
        hide_unavailable(ctx.state.people.orgs_of(&ctx.actor, user.id).await)?
            .unwrap_or_default()
            .iter()
            .map(|o| person_row(o, "", ""))
            .collect()
    };
    let login = user.username.clone();
    let login_enc = fmt::enc(&login);
    let c = ctx.chrome(&display_of(&user)).await;
    ctx.page(&ProfilePage {
        c,
        login,
        base: format!("/{login_enc}"),
        display: display_of(&user),
        is_org,
        org_owner,
        org_id: user.id.0,
        joined: fmt::time(user.created_at),
        signed_out,
        repos,
        orgs,
        login_enc,
    })
}

#[derive(Template)]
#[template(path = "people/org_people.html")]
struct OrgPeoplePage {
    c: Chrome,
    login: String,
    login_enc: String,
    display: String,
    base: String,
    is_owner: bool,
    rows: Vec<OrgPersonRow>,
}

/// One member row on the people tab, with the actions that row offers.
struct OrgPersonRow {
    user_id: i64,
    username: String,
    display: String,
    href: String,
    is_owner: bool,
    is_viewer: bool,
}

pub async fn org_people(ctx: Ctx, Path(org): Path<String>) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let display = display_of(&org);
    let org_id = OrgId(org.id.0);
    let is_owner = viewer_role(&ctx, org_id).await == Some(OrgRole::Owner);
    let viewer = ctx.user_id().ok();
    let members = ctx.state.people.org_members(&ctx.actor, org_id).await?;
    let rows = members
        .iter()
        .map(|m| OrgPersonRow {
            user_id: m.user.id.0,
            username: m.user.username.clone(),
            display: display_of(&m.user),
            href: format!("/{}", fmt::enc(&m.user.username)),
            is_owner: m.role == OrgRole::Owner,
            is_viewer: viewer == Some(m.user.id),
        })
        .collect();
    let login = org.username.clone();
    let login_enc = fmt::enc(&login);
    let c = ctx.chrome(&format!("People - {display}")).await;
    ctx.page(&OrgPeoplePage { c, login, base: format!("/{login_enc}"), login_enc, display, is_owner, rows })
}

pub async fn org_people_add(ctx: Ctx, Path(org): Path<String>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/people", fmt::enc(&org.username));
    let username = form.get("username").trim().to_string();
    let role = if form.get("role") == "owner" { OrgRole::Owner } else { OrgRole::Member };
    let Some(user) = find_person(&ctx, &username).await? else {
        return Ok(ctx.redirect_flash(&back, "error", "No one has that username."));
    };
    match ctx.state.orgs.set_member(&ctx.actor, OrgId(org.id.0), user.id, role, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Added {} to the organization.", user.username))),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_people_remove(ctx: Ctx, Path((org, username)): Path<(String, String)>, _form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/people", fmt::enc(&org.username));
    let Some(user) = find_person(&ctx, &username).await? else {
        return Ok(ctx.redirect_flash(&back, "error", "No one has that username."));
    };
    // Owners remove anyone; a member removing themselves is leaving. The last owner rule is
    // the service's to state, as a flash.
    match ctx.state.orgs.remove_member(&ctx.actor, OrgId(org.id.0), user.id, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Removed {} from the organization.", user.username))),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_people_role(ctx: Ctx, Path((org, username)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/people", fmt::enc(&org.username));
    let role = match form.get("role") {
        "owner" => OrgRole::Owner,
        "member" => OrgRole::Member,
        _ => return Ok(ctx.redirect_flash(&back, "error", "Pick Member or Owner.")),
    };
    let Some(user) = find_person(&ctx, &username).await? else {
        return Ok(ctx.redirect_flash(&back, "error", "No one has that username."));
    };
    match ctx.state.orgs.set_member(&ctx.actor, OrgId(org.id.0), user.id, role, &ctx.client).await {
        Ok(()) => {
            let what = if role == OrgRole::Owner { "an owner" } else { "a member" };
            Ok(ctx.redirect_flash(&back, "ok", &format!("{} is {what} now.", user.username)))
        }
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

struct TeamRow {
    name: String,
    description: String,
    permission: String,
    members_text: String,
    href: String,
}

#[derive(Template)]
#[template(path = "people/org_teams.html")]
struct OrgTeamsPage {
    c: Chrome,
    login: String,
    login_enc: String,
    display: String,
    base: String,
    is_owner: bool,
    perm_options: Vec<SelectOption>,
    rows: Vec<TeamRow>,
}

pub async fn org_teams(ctx: Ctx, Path(org): Path<String>) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let display = display_of(&org);
    let login = org.username.clone();
    let login_enc = fmt::enc(&login);
    let is_owner = viewer_role(&ctx, OrgId(org.id.0)).await == Some(OrgRole::Owner);
    let teams = ctx.state.people.teams(&ctx.actor, OrgId(org.id.0)).await?;
    let rows = teams
        .iter()
        .map(|t| TeamRow {
            name: t.name.clone(),
            description: t.description.clone(),
            permission: capitalized(t.permission.as_str()),
            members_text: fmt::plural(t.members, "member", "members"),
            href: format!("/orgs/{login_enc}/teams/{}", t.id.0),
        })
        .collect();
    let c = ctx.chrome(&format!("Teams - {display}")).await;
    ctx.page(&OrgTeamsPage {
        c,
        login,
        base: format!("/{login_enc}"),
        login_enc,
        display,
        is_owner,
        perm_options: permission_options("read"),
        rows,
    })
}

pub async fn org_team_new(ctx: Ctx, Path(org): Path<String>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let login_enc = fmt::enc(&org.username);
    let teams_href = format!("/orgs/{login_enc}/teams");
    let name = form.get("name").trim().to_string();
    let description = form.get("description").trim().to_string();
    let Some(perm) = team_permission_of(form.get("permission")) else {
        return Ok(ctx.redirect_flash(&teams_href, "error", "Use read, triage, write, maintain or admin."));
    };
    match ctx.state.orgs.create_team(&ctx.actor, OrgId(org.id.0), &name, &description, perm, &ctx.client).await {
        Ok(t) => Ok(ctx.redirect_flash(&format!("/orgs/{login_enc}/teams/{}", t.id.0), "ok", &format!("Team {} created.", t.name))),
        Err(e) => Ok(ctx.redirect_flash(&teams_href, "error", &flash_error(&e))),
    }
}

#[derive(Template)]
#[template(path = "people/org_team.html")]
struct OrgTeamPage {
    c: Chrome,
    org_login: String,
    org_login_enc: String,
    org_href: String,
    team_href: String,
    is_owner: bool,
    name: String,
    description: String,
    permission: String,
    members_text: String,
    perm_options: Vec<SelectOption>,
    rows: Vec<TeamMemberRow>,
    repo_rows: Vec<TeamRepoRow>,
    repo_choices: Vec<RepoChoice>,
}

struct TeamMemberRow {
    username: String,
    display: String,
    href: String,
}

struct TeamRepoRow {
    repo_id: i64,
    name: String,
    href: String,
    permission: String,
}

/// A repository of the organization the team does not have yet.
struct RepoChoice {
    repo_id: i64,
    name: String,
}

pub async fn org_team(ctx: Ctx, Path((org, team)): Path<(String, i64)>) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let org_id = OrgId(org.id.0);
    let org_login = org.username.clone();
    let login_enc = fmt::enc(&org_login);
    let is_owner = viewer_role(&ctx, org_id).await == Some(OrgRole::Owner);
    let teams = ctx.state.people.teams(&ctx.actor, org_id).await?;
    let Some(team) = teams.iter().find(|t| t.id.0 == team) else { return Err(WebError::not_found()) };
    let team_id = TeamId(team.id.0);
    let team_href = format!("/orgs/{login_enc}/teams/{}", team.id.0);
    let members = ctx.state.people.team_members(&ctx.actor, team_id).await?;
    let rows = members
        .iter()
        .map(|u| TeamMemberRow { username: u.username.clone(), display: display_of(u), href: format!("/{}", fmt::enc(&u.username)) })
        .collect();
    // A repository the viewer cannot read is skipped: its name would be all we could show.
    let team_repos = hide_unavailable(ctx.state.orgs.team_repos(&ctx.actor, team_id).await)?.unwrap_or_default();
    let mut repo_rows = Vec::with_capacity(team_repos.len());
    for tr in &team_repos {
        if let Ok(r) = ctx.state.repos.get(&ctx.actor, tr.repo_id).await {
            repo_rows.push(TeamRepoRow {
                repo_id: tr.repo_id.0,
                name: r.name.clone(),
                href: format!("/{}/{}", fmt::enc(&r.owner_name), fmt::enc(&r.name)),
                permission: capitalized(tr.permission.as_str()),
            });
        }
    }
    let mut repo_choices = Vec::new();
    if is_owner {
        for r in ctx.state.repos.list_for_owner(&ctx.actor, UserId(org.id.0)).await? {
            if !team_repos.iter().any(|tr| tr.repo_id.0 == r.id.0) {
                repo_choices.push(RepoChoice { repo_id: r.id.0, name: r.name.clone() });
            }
        }
    }
    let c = ctx.chrome(&format!("{} - {}", team.name, display_of(&org))).await;
    ctx.page(&OrgTeamPage {
        c,
        org_login,
        org_login_enc: login_enc.clone(),
        org_href: format!("/{login_enc}"),
        team_href,
        is_owner,
        name: team.name.clone(),
        description: team.description.clone(),
        permission: capitalized(team.permission.as_str()),
        members_text: fmt::plural(team.members, "member", "members"),
        perm_options: permission_options(team.permission.as_str()),
        rows,
        repo_rows,
        repo_choices,
    })
}

pub async fn org_team_permission(ctx: Ctx, Path((org, team)): Path<(String, i64)>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/teams/{team}", fmt::enc(&org.username));
    let Some(perm) = team_permission_of(form.get("permission")) else {
        return Ok(ctx.redirect_flash(&back, "error", "Use read, triage, write, maintain or admin."));
    };
    match ctx.state.orgs.set_team_permission(&ctx.actor, TeamId(team), perm, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", "Team permission saved.")),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_team_member_add(ctx: Ctx, Path((org, team)): Path<(String, i64)>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/teams/{team}", fmt::enc(&org.username));
    let username = form.get("username").trim().to_string();
    let Some(user) = find_person(&ctx, &username).await? else {
        return Ok(ctx.redirect_flash(&back, "error", "No one has that username."));
    };
    match ctx.state.orgs.add_team_member(&ctx.actor, TeamId(team), user.id, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Added {} to the team.", user.username))),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_team_member_remove(
    ctx: Ctx,
    Path((org, team, username)): Path<(String, i64, String)>,
    _form: FormData,
) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/teams/{team}", fmt::enc(&org.username));
    let Some(user) = find_person(&ctx, &username).await? else {
        return Ok(ctx.redirect_flash(&back, "error", "No one has that username."));
    };
    match ctx.state.orgs.remove_team_member(&ctx.actor, TeamId(team), user.id, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", &format!("Removed {} from the team.", user.username))),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_team_repo_add(ctx: Ctx, Path((org, team)): Path<(String, i64)>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/teams/{team}", fmt::enc(&org.username));
    let Some(repo_id) = form.int("repo_id") else {
        return Ok(ctx.redirect_flash(&back, "error", "Pick a repository."));
    };
    let Some(perm) = team_permission_of(form.get("permission")) else {
        return Ok(ctx.redirect_flash(&back, "error", "Use read, triage, write, maintain or admin."));
    };
    match ctx.state.orgs.set_team_repo(&ctx.actor, TeamId(team), RepoId(repo_id), perm, &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", "Repository added to the team.")),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_team_repo_remove(ctx: Ctx, Path((org, team, repo_id)): Path<(String, i64, i64)>, _form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let back = format!("/orgs/{}/teams/{team}", fmt::enc(&org.username));
    match ctx.state.orgs.remove_team_repo(&ctx.actor, TeamId(team), RepoId(repo_id), &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&back, "ok", "Repository removed from the team.")),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

pub async fn org_team_delete(ctx: Ctx, Path((org, team)): Path<(String, i64)>, form: FormData) -> WebResult<Response> {
    let org = org_by_name(&ctx, &org).await?;
    let teams_href = format!("/orgs/{}/teams", fmt::enc(&org.username));
    if !form.flag("confirm") {
        return Ok(ctx.redirect_flash(&format!("{teams_href}/{team}"), "error", "Tick the box to say you are sure, then try again."));
    }
    match ctx.state.orgs.delete_team(&ctx.actor, TeamId(team), &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&teams_href, "ok", "Team deleted.")),
        Err(e) => Ok(ctx.redirect_flash(&teams_href, "error", &flash_error(&e))),
    }
}

// ------------------------------------------------------------------ organization settings

#[derive(Template)]
#[template(path = "people/org_settings.html")]
struct OrgSettingsPage {
    c: Chrome,
    login: String,
    login_enc: String,
    display: String,
    base: String,
    display_name: String,
    description: String,
    base_options: Vec<SelectOption>,
    errors: FieldErrors,
}

/// An organization for its owner. Anyone else, member included, gets the same 404 as for an
/// organization that does not exist: settings are not there to be discovered.
async fn org_for_owner(ctx: &Ctx, name: &str) -> WebResult<User> {
    let org = org_by_name(ctx, name).await?;
    if viewer_role(ctx, OrgId(org.id.0)).await != Some(OrgRole::Owner) {
        return Err(WebError::not_found());
    }
    Ok(org)
}

async fn org_settings_page(
    ctx: &Ctx,
    org: &User,
    display_name: String,
    description: String,
    base: String,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let display = display_of(org);
    let login_enc = fmt::enc(&org.username);
    let c = ctx.chrome(&format!("Settings - {display}")).await;
    ctx.page_status(
        status,
        &OrgSettingsPage {
            c,
            login: org.username.clone(),
            base: format!("/{login_enc}"),
            login_enc,
            display,
            display_name,
            description,
            base_options: base_permission_options(&base),
            errors,
        },
    )
}

pub async fn org_settings_get(ctx: Ctx, Path(org): Path<String>) -> WebResult<Response> {
    let org = org_for_owner(&ctx, &org).await?;
    // The display name lives on the account; the description and the default repository
    // permission only the Orgs service knows, and reading them is not worth failing the
    // page over.
    let details = hide_unavailable(ctx.state.orgs.get(&ctx.actor, &org.username).await)?;
    let display_name = details.as_ref().map(|o| o.display_name.clone()).unwrap_or_else(|| org.display_name.clone());
    let description = details.as_ref().map(|o| o.description.clone()).unwrap_or_default();
    let base = details.as_ref().map(|o| o.base_permission.as_str().to_string()).unwrap_or_else(|| "read".into());
    org_settings_page(&ctx, &org, display_name, description, base, FieldErrors::default(), StatusCode::OK).await
}

pub async fn org_settings_post(ctx: Ctx, Path(org): Path<String>, form: FormData) -> WebResult<Response> {
    let org = org_for_owner(&ctx, &org).await?;
    let display_name = form.get("display_name").trim().to_string();
    let description = form.get("description").trim().to_string();
    let base_raw = form.get("base_permission").trim().to_string();
    let mut errors = FieldErrors::default();
    if display_name.chars().count() > 100 {
        errors = errors.with("display_name", "Keep the display name to 100 characters or fewer.");
    }
    if description.chars().count() > 500 {
        errors = errors.with("description", "Keep the description to 500 characters or fewer.");
    }
    let base = Permission::parse(&base_raw);
    if base.is_none() {
        errors = errors.with("base_permission", "Pick a default repository permission.");
    }
    if errors.any() {
        return org_settings_page(&ctx, &org, display_name, description, base_raw, errors, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    let back = format!("/orgs/{}/settings", fmt::enc(&org.username));
    match ctx.state.orgs.update(&ctx.actor, OrgId(org.id.0), Some(display_name.clone()), Some(description.clone()), base, &ctx.client).await
    {
        Ok(_) => Ok(ctx.redirect_flash(&back, "ok", "Settings saved.")),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => {
                org_settings_page(&ctx, &org, display_name, description, base_raw, errors, StatusCode::UNPROCESSABLE_ENTITY).await
            }
            None => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
        },
    }
}

pub async fn org_settings_delete(ctx: Ctx, Path(org): Path<String>, form: FormData) -> WebResult<Response> {
    let org = org_for_owner(&ctx, &org).await?;
    let back = format!("/orgs/{}/settings", fmt::enc(&org.username));
    if form.get("confirm").trim() != org.username {
        return Ok(ctx.redirect_flash(&back, "error", "Type the organization name to confirm."));
    }
    match ctx.state.orgs.delete(&ctx.actor, OrgId(org.id.0), &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash("/", "ok", &format!("Deleted the organization {}.", org.username))),
        Err(e) => Ok(ctx.redirect_flash(&back, "error", &flash_error(&e))),
    }
}

// ------------------------------------------------------------------ new repository

pub(crate) struct OwnerChoice {
    pub id: i64,
    pub login: String,
    pub selected: bool,
}

/// Yourself, plus every organization you own: the owners a new repository can belong to.
/// An organization whose role cannot be checked is simply left out.
pub(crate) async fn owner_choices(ctx: &Ctx, uid: UserId, picked: Option<i64>) -> Vec<OwnerChoice> {
    let mut out = Vec::new();
    if let Ok(me) = ctx.state.accounts.get_user(uid).await {
        out.push(OwnerChoice { id: uid.0, login: me.username, selected: Some(uid.0) == picked });
    }
    if let Ok(orgs) = ctx.state.people.orgs_of(&ctx.actor, uid).await {
        for org in orgs {
            if !matches!(ctx.state.authz.org_role(&ctx.actor, OrgId(org.id.0)).await, Ok(Some(OrgRole::Owner))) {
                continue;
            }
            out.push(OwnerChoice { id: org.id.0, login: org.username, selected: Some(org.id.0) == picked });
        }
    }
    out
}

#[derive(Template)]
#[template(path = "people/new_repo.html")]
struct NewRepoPage {
    c: Chrome,
    owners: Vec<OwnerChoice>,
    name: String,
    description: String,
    /// The chosen tier: `private`, `internal` or `public`.
    tier: String,
    switch_on: bool,
    text_private: &'static str,
    text_internal: &'static str,
    text_public: &'static str,
    auto_init: bool,
    errors: FieldErrors,
}

#[allow(clippy::too_many_arguments)]
async fn new_repo_page(
    ctx: &Ctx,
    owners: Vec<OwnerChoice>,
    name: &str,
    description: &str,
    tier: Visibility,
    auto_init: bool,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let c = ctx.chrome("New repository").await;
    let switch_on = ctx.state.repos.public_repos_allowed().await.unwrap_or(false);
    ctx.page_status(
        status,
        &NewRepoPage {
            c,
            owners,
            name: name.into(),
            description: description.into(),
            tier: tier.as_str().into(),
            switch_on,
            text_private: super::visibility::TEXT_PRIVATE,
            text_internal: super::visibility::TEXT_INTERNAL,
            text_public: super::visibility::TEXT_PUBLIC,
            auto_init,
            errors,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn new_repo_again(
    ctx: &Ctx,
    owners: Vec<OwnerChoice>,
    name: &str,
    description: &str,
    tier: Visibility,
    auto_init: bool,
    errors: FieldErrors,
) -> WebResult<Response> {
    new_repo_page(ctx, owners, name, description, tier, auto_init, errors, StatusCode::UNPROCESSABLE_ENTITY).await
}

pub async fn new_repo_get(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    // An organization page's button names the owner to preselect. It only takes effect when
    // the id is one of the viewer's choices.
    let picked = q.get("owner").and_then(|o| o.parse::<i64>().ok()).or(Some(uid.0));
    let owners = owner_choices(&ctx, uid, picked).await;
    new_repo_page(&ctx, owners, "", "", Visibility::Private, false, FieldErrors::default(), StatusCode::OK).await
}

pub async fn new_repo_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let picked = form.int("owner_id");
    let name = form.get("name").trim().to_string();
    let description = form.get("description").trim().to_string();
    let public = visibility_of(form.get("visibility"));
    let auto_init = form.flag("auto_init");
    let owners = owner_choices(&ctx, uid, picked).await;
    let Some(owner) = owners.iter().find(|o| Some(o.id) == picked) else {
        return new_repo_again(
            &ctx,
            owners,
            &name,
            &description,
            public,
            auto_init,
            FieldErrors::default().with("owner_id", "Pick an owner you can create repositories for."),
        )
        .await;
    };
    if let Some(msg) = repo_name_error(&name) {
        return new_repo_again(&ctx, owners, &name, &description, public, auto_init, FieldErrors::default().with("name", msg)).await;
    }
    let new = NewRepo {
        owner_id: UserId(owner.id),
        name: name.clone(),
        description: description.clone(),
        // Nothing is created public. A repository meant to be public is created private and
        // the person lands on the page that checks it first.
        visibility: public.min(Visibility::Internal),
        default_branch: None,
        auto_init,
    };
    match ctx.state.repos.create(&ctx.actor, new, &ctx.client).await {
        Ok(repo) if public == Visibility::Public => {
            Ok(ctx.redirect(&format!("/{}/{}/settings/visibility", fmt::enc(&repo.owner_name), fmt::enc(&repo.name))))
        }
        Ok(repo) => Ok(ctx.redirect(&format!("/{}/{}", fmt::enc(&repo.owner_name), fmt::enc(&repo.name)))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => new_repo_again(&ctx, owners, &name, &description, public, auto_init, errors).await,
            None => Err(e.into()),
        },
    }
}

// ------------------------------------------------------------------ import a repository

/// The name an address suggests for the repository: its last path segment, minus a trailing
/// `.git`. An address with no path leaves nothing, and the empty name is reported as usual.
fn name_from_url(url: &str) -> String {
    let last = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

#[derive(Template)]
#[template(path = "people/import_repo.html")]
struct ImportRepoPage {
    c: Chrome,
    owners: Vec<OwnerChoice>,
    url: String,
    name: String,
    description: String,
    /// The chosen tier: `private` or `internal`.
    tier: String,
    text_private: &'static str,
    text_internal: &'static str,
    errors: FieldErrors,
}

#[allow(clippy::too_many_arguments)]
async fn import_repo_page(
    ctx: &Ctx,
    owners: Vec<OwnerChoice>,
    url: &str,
    name: &str,
    description: &str,
    tier: Visibility,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let c = ctx.chrome("Import a repository").await;
    ctx.page_status(
        status,
        &ImportRepoPage {
            c,
            owners,
            url: url.into(),
            name: name.into(),
            description: description.into(),
            tier: tier.as_str().into(),
            text_private: super::visibility::TEXT_PRIVATE,
            text_internal: super::visibility::TEXT_INTERNAL,
            errors,
        },
    )
}

pub async fn import_repo_get(ctx: Ctx, Query(q): Query<Params>) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    // An organization page's button names the owner to preselect, exactly as on /new.
    let picked = q.get("owner").and_then(|o| o.parse::<i64>().ok()).or(Some(uid.0));
    let owners = owner_choices(&ctx, uid, picked).await;
    import_repo_page(&ctx, owners, "", "", "", Visibility::Private, FieldErrors::default(), StatusCode::OK).await
}

pub async fn import_repo_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let uid = ctx.user_id()?;
    let picked = form.int("owner_id");
    let url = form.get("url").trim().to_string();
    let mut name = form.get("name").trim().to_string();
    let description = form.get("description").trim().to_string();
    // Nothing is imported public either, so the choice stops at internal.
    let tier = visibility_of(form.get("visibility")).min(Visibility::Internal);
    let owners = owner_choices(&ctx, uid, picked).await;
    if url.is_empty() {
        return import_repo_page(
            &ctx,
            owners,
            &url,
            &name,
            &description,
            tier,
            FieldErrors::default().with("url", "Give the address of the repository to import."),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    // The address can name the repository when the form does not.
    if name.is_empty() {
        name = name_from_url(&url);
    }
    let Some(owner) = owners.iter().find(|o| Some(o.id) == picked) else {
        return import_repo_page(
            &ctx,
            owners,
            &url,
            &name,
            &description,
            tier,
            FieldErrors::default().with("owner_id", "Pick an owner you can create repositories for."),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    };
    if let Some(msg) = repo_name_error(&name) {
        return import_repo_page(
            &ctx,
            owners,
            &url,
            &name,
            &description,
            tier,
            FieldErrors::default().with("name", msg),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let new = NewRepo {
        owner_id: UserId(owner.id),
        name: name.clone(),
        description: description.clone(),
        visibility: tier,
        default_branch: None,
        auto_init: false,
    };
    match ctx.state.repos.import(&ctx.actor, new, &url, &ctx.client).await {
        Ok(repo) => Ok(ctx.redirect(&format!("/{}/{}", fmt::enc(&repo.owner_name), fmt::enc(&repo.name)))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => import_repo_page(&ctx, owners, &url, &name, &description, tier, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

// ------------------------------------------------------------------ new organization

#[derive(Template)]
#[template(path = "people/new_org.html")]
struct NewOrgPage {
    c: Chrome,
    name: String,
    display: String,
    errors: FieldErrors,
}

async fn new_org_page(ctx: &Ctx, name: &str, display: &str, errors: FieldErrors, status: StatusCode) -> WebResult<Response> {
    let c = ctx.chrome("New organization").await;
    ctx.page_status(status, &NewOrgPage { c, name: name.into(), display: display.into(), errors })
}

pub async fn new_org_get(ctx: Ctx) -> WebResult<Response> {
    new_org_page(&ctx, "", "", FieldErrors::default(), StatusCode::OK).await
}

pub async fn new_org_post(ctx: Ctx, form: FormData) -> WebResult<Response> {
    let name = form.get("name").trim().to_string();
    let display = form.get("display_name").trim().to_string();
    if name.is_empty() {
        return new_org_page(
            &ctx,
            &name,
            &display,
            FieldErrors::default().with("name", "Give the organization a name."),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    match ctx.state.people.create_org(&ctx.actor, &name, &display, &ctx.client).await {
        Ok(org) => Ok(ctx.redirect(&format!("/{}", fmt::enc(&org.username)))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => new_org_page(&ctx, &name, &display, errors, StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

// ROUTES:
// GET /explore -> explore
// GET /search -> search
// GET /notifications -> notifications
// POST /notifications/read -> mark_all_read
// POST /notifications/{id}/read -> mark_one_read
// GET /orgs/{org}/people -> org_people
// POST /orgs/{org}/people/add -> org_people_add
// POST /orgs/{org}/people/{username}/remove -> org_people_remove
// POST /orgs/{org}/people/{username}/role -> org_people_role
// GET /orgs/{org}/teams -> org_teams
// POST /orgs/{org}/teams/new -> org_team_new
// GET /orgs/{org}/teams/{team} -> org_team
// POST /orgs/{org}/teams/{team}/permission -> org_team_permission
// POST /orgs/{org}/teams/{team}/members/add -> org_team_member_add
// POST /orgs/{org}/teams/{team}/members/{username}/remove -> org_team_member_remove
// POST /orgs/{org}/teams/{team}/repos/add -> org_team_repo_add
// POST /orgs/{org}/teams/{team}/repos/{repo_id}/remove -> org_team_repo_remove
// POST /orgs/{org}/teams/{team}/delete -> org_team_delete
// GET+POST /orgs/{org}/settings -> org_settings_get, org_settings_post
// POST /orgs/{org}/settings/delete -> org_settings_delete
// GET+POST /new -> new_repo_get, new_repo_post
// GET+POST /new/import -> import_repo_get, import_repo_post
// GET+POST /new/org -> new_org_get, new_org_post
// GET /{owner} -> profile (a catch-all: register it after every literal route)

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Params {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn user() -> User {
        User {
            id: UserId(1),
            kind: UserKind::User,
            username: "alice".into(),
            display_name: String::new(),
            email: None,
            is_admin: false,
            can_run_ci: true,
            suspended: false,
            has_2fa: false,
            has_password: true,
            invited_by: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn reasons_read_as_sentences() {
        assert_eq!(reason_text("assign"), "You were assigned");
        assert_eq!(reason_text("mention"), "You were mentioned");
        assert_eq!(reason_text("review_requested"), "Review requested");
        assert_eq!(reason_text("comment"), "You commented");
        assert_eq!(reason_text("author"), "You opened this");
        assert_eq!(reason_text("subscribed"), "You watch this repository");
        assert_eq!(reason_text("team_mention"), "Your team was mentioned");
        assert_eq!(reason_text("state_change"), "Its state changed");
        assert_eq!(reason_text("assigned"), "You were assigned", "the fan-out writes this spelling");
        assert_eq!(reason_text("something_new"), "Activity");
        assert_eq!(reason_text(""), "Activity");
    }

    #[test]
    fn subject_links_point_at_issues_and_pulls() {
        assert_eq!(subject_href("issue", 7, "alice", "demo").as_deref(), Some("/alice/demo/issues/7"));
        assert_eq!(subject_href("pull", 9, "alice", "demo").as_deref(), Some("/alice/demo/pulls/9"));
        assert_eq!(subject_href("pull_request", 9, "alice", "demo").as_deref(), Some("/alice/demo/pulls/9"));
        assert_eq!(subject_href("commit", 1, "alice", "demo"), None);
        assert_eq!(subject_href("release", 3, "alice", "demo").as_deref(), Some("/alice/demo"));
        assert_eq!(subject_href("issue", 7, "a b", "demo").as_deref(), Some("/a%20b/demo/issues/7"));
    }

    #[test]
    fn repo_names_follow_the_rules() {
        assert_eq!(repo_name_error("widgets"), None);
        assert_eq!(repo_name_error("my-widget_2.v3"), None);
        assert_eq!(repo_name_error(&"x".repeat(100)), None);
        assert_eq!(repo_name_error(""), Some("Give the repository a name."));
        assert_eq!(repo_name_error(&"x".repeat(101)), Some("Use up to 100 characters."));
        assert_eq!(repo_name_error(".hidden"), Some("The name cannot start with a dot."));
        assert_eq!(repo_name_error("repo.git"), Some("The name cannot end in .git."));
        assert_eq!(repo_name_error("repo.GIT"), Some("The name cannot end in .git."));
        assert_eq!(repo_name_error("two words"), Some("Use letters, numbers, dots, dashes and underscores."));
    }

    #[test]
    fn search_types_pages_and_queries() {
        assert_eq!(search_type(None), "repos");
        assert_eq!(search_type(Some("repos")), "repos");
        assert_eq!(search_type(Some("issues")), "issues");
        assert_eq!(search_type(Some("users")), "users");
        assert_eq!(search_type(Some("code")), "code");
        assert_eq!(search_type(Some("nonsense")), "repos");
        assert_eq!(page_of(&params(&[])), 1);
        assert_eq!(page_of(&params(&[("page", "3")])), 3);
        assert_eq!(page_of(&params(&[("page", "0")])), 1);
        assert_eq!(page_of(&params(&[("page", "x")])), 1);
        assert_eq!(query_of(&params(&[("q", "  widgets ")])), "widgets");
        assert_eq!(query_of(&params(&[("q", "   ")])), "");
        assert_eq!(query_of(&params(&[])), "");
    }

    #[test]
    fn search_tabs_carry_the_query_and_mark_the_current_type() {
        let tabs = search_tabs("a b", "issues");
        assert_eq!(tabs[0].href, "/search?q=a%20b&type=repos");
        assert_eq!(tabs[1].href, "/search?q=a%20b&type=issues");
        assert_eq!(tabs[1].label, "Issues");
        assert!(tabs[1].current && !tabs[0].current && !tabs[2].current);
        let tabs = search_tabs("", "repos");
        assert_eq!(tabs[0].href, "/search?type=repos");
        assert!(tabs[0].current);
    }

    #[test]
    fn hit_rows_link_repositories_and_accounts_but_not_issues() {
        let hits = vec![SearchHit { kind: "repo".into(), id: 1, title: "alice/demo".into(), snippet: "A demo".into() }];
        let linked = hit_rows(&hits, true);
        assert_eq!(linked[0].href.as_deref(), Some("/alice/demo"));
        assert_eq!(linked[0].title, "alice/demo");
        let plain = hit_rows(&hits, false);
        assert_eq!(plain[0].href, None);
        assert_eq!(plain[0].kind, "repo");
    }

    #[test]
    fn person_rows_show_a_name_and_optional_chip() {
        let u = user();
        let p = person_row(&u, "", "");
        assert_eq!(p.display, "alice");
        assert_eq!(p.href, "/alice");
        assert!(p.chip.is_empty());
        let mut org = user();
        org.display_name = "Acme Widgets".into();
        org.kind = UserKind::Org;
        let p = person_row(&org, "Organization", "");
        assert_eq!(p.display, "Acme Widgets");
        assert_eq!(p.chip, "Organization");
    }

    #[test]
    fn words_get_capitalized() {
        assert_eq!(capitalized("read"), "Read");
        assert_eq!(capitalized("maintain"), "Maintain");
        assert_eq!(capitalized(""), "");
    }

    #[test]
    fn visibility_defaults_to_private() {
        assert_eq!(visibility_of("public"), Visibility::Public);
        assert_eq!(visibility_of("internal"), Visibility::Internal);
        assert_eq!(visibility_of("private"), Visibility::Private);
        assert_eq!(visibility_of(""), Visibility::Private);
        assert_eq!(visibility_of("Internal"), Visibility::Private, "exact spelling only");
    }

    #[test]
    fn unavailable_services_hide_their_cards_but_other_errors_do_not() {
        assert!(hide_unavailable::<i64>(Ok(3)).unwrap().is_some());
        assert!(hide_unavailable::<i64>(Err(Error::unavailable("nope"))).unwrap().is_none());
        assert!(hide_unavailable::<i64>(Err(Error::NotFound)).is_err());
    }

    #[test]
    fn team_permissions_parse_but_none_is_refused() {
        assert_eq!(team_permission_of("read"), Some(Permission::Read));
        assert_eq!(team_permission_of("write"), Some(Permission::Write));
        assert_eq!(team_permission_of("maintain"), Some(Permission::Maintain));
        assert_eq!(team_permission_of("none"), None);
        assert_eq!(team_permission_of(""), None);
        assert_eq!(team_permission_of("bogus"), None);
    }

    #[test]
    fn permission_selects_mark_the_current_value() {
        let o = permission_options("write");
        assert_eq!(o.len(), 5);
        assert_eq!(o[2].label, "Write");
        assert!(o[2].selected && !o[0].selected);
        assert!(permission_options("read")[0].selected);
        assert!(!permission_options("read")[2].selected);
    }

    #[test]
    fn base_permission_selects_cover_the_api_values() {
        let o = base_permission_options("none");
        assert_eq!(o[0].value, "none");
        assert!(o[0].selected);
        assert_eq!(o.len(), 4);
        // A value outside the short list, set through the API, still shows up.
        let o = base_permission_options("triage");
        assert_eq!(o.len(), 5);
        assert!(o.last().unwrap().selected);
        assert!(!base_permission_options("read").iter().any(|o| o.value == "triage"));
    }

    #[test]
    fn flash_errors_name_the_rule() {
        let e = Error::Validation(vec![ch_core::FieldError::custom("Team", "permission", "Use read, triage, write, maintain or admin.")]);
        assert_eq!(flash_error(&e), "Use read, triage, write, maintain or admin.");
        assert_eq!(flash_error(&Error::Conflict("The last owner cannot leave.".into())), "The last owner cannot leave.");
        assert_eq!(flash_error(&Error::NotFound), "Not Found");
        let bare = Error::Validation(vec![ch_core::FieldError::new("Team", "name", "missing")]);
        assert_eq!(flash_error(&bare), "Please check what you entered.");
    }
}
