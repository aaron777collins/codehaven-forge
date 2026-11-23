//! The route table. Built through a small registry so tests can see every method and path.

use crate::assets;
use crate::views::{
    self, actions, admin, apps, auth, issues, people, pulls, releases, repo, repo_settings as rs, settings, visibility, workspace as ws,
};
use axum::Router;
use axum::handler::Handler;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ch_core::state::AppState;

pub struct Registry {
    pub router: Router<AppState>,
    pub table: Vec<(&'static str, &'static str)>,
}

impl Registry {
    fn new() -> Self {
        Registry { router: Router::new(), table: Vec::new() }
    }

    fn get<H, T>(mut self, path: &'static str, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.table.push(("GET", path));
        self.router = self.router.route(path, get(h));
        self
    }

    fn post<H, T>(mut self, path: &'static str, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.table.push(("POST", path));
        self.router = self.router.route(path, post(h));
        self
    }

    /// GET and POST on one path (a form page).
    fn form<G, TG, P, TP>(mut self, path: &'static str, g: G, p: P) -> Self
    where
        G: Handler<TG, AppState>,
        TG: 'static,
        P: Handler<TP, AppState>,
        TP: 'static,
    {
        self.table.push(("GET", path));
        self.table.push(("POST", path));
        self.router = self.router.route(path, get(g).post(p));
        self
    }
}

pub fn build() -> Registry {
    build_for(true)
}

/// The route table. With `mcp` false the two OAuth pages do not exist at all.
pub fn build_for(mcp: bool) -> Registry {
    let mut r = Registry::new()
        .get("/favicon.svg", favicon)
        .get("/static/{*path}", assets::serve)
        .form("/login", auth::login_get, auth::login_post)
        .form("/login/two-factor", auth::two_factor_get, auth::two_factor_post)
        .post("/logout", auth::logout_post)
        .form("/invite/{token}", auth::invite_get, auth::invite_post)
        .form("/password/{token}", auth::password_get, auth::password_post)
        .get("/", views::home::home)
        .get("/admin", admin::overview)
        .get("/admin/invites", admin::invites_get)
        .post("/admin/invites", admin::invites_create)
        .post("/admin/invites/{id}/revoke", admin::invite_revoke)
        .get("/admin/users", admin::users_get)
        .post("/admin/users/{id}/suspend", admin::user_suspend)
        .post("/admin/users/{id}/ci", admin::user_ci)
        .post("/admin/users/{id}/unlock", admin::user_unlock)
        .post("/admin/users/{id}/signout", admin::user_signout)
        .post("/admin/users/{id}/reset-password", admin::user_reset_password)
        .get("/admin/runners", admin::runners_get)
        .post("/admin/runners/{id}/revoke", admin::runner_revoke)
        .post("/admin/runners/token", admin::runner_token)
        .get("/admin/audit", admin::audit_get)
        .get("/admin/settings", visibility::admin_settings_get)
        .post("/admin/settings", visibility::admin_settings_post)
        .get("/admin/workspace", visibility::admin_workspace_get)
        .post("/admin/workspace", visibility::admin_workspace_post)
        .get("/settings", settings::index)
        .form("/settings/profile", settings::profile_get, settings::profile_post)
        .form("/settings/password", settings::password_get, settings::password_post)
        .get("/settings/security", settings::security_get)
        .post("/settings/security/2fa/start", settings::two_factor_start)
        .post("/settings/security/2fa/confirm", settings::two_factor_confirm)
        .post("/settings/security/2fa/disable", settings::two_factor_disable)
        .get("/settings/sessions", settings::sessions_get)
        .post("/settings/sessions/{id}/revoke", settings::session_revoke)
        .form("/settings/keys", settings::keys_get, settings::keys_post)
        .post("/settings/keys/{id}/delete", settings::key_delete)
        .form("/settings/tokens", settings::tokens_get, settings::tokens_post)
        .post("/settings/tokens/{id}/delete", settings::token_delete)
        .get("/settings/apps", apps::apps_get)
        .post("/settings/apps/{id}/revoke", apps::app_revoke)
        .form("/settings/appearance", settings::appearance_get, settings::appearance_post)
        .get("/explore", people::explore)
        .get("/search", people::search)
        .get("/notifications", people::notifications)
        .post("/notifications/read", people::mark_all_read)
        .post("/notifications/{id}/read", people::mark_one_read)
        .get("/orgs/{org}/people", people::org_people)
        .post("/orgs/{org}/people/add", people::org_people_add)
        .post("/orgs/{org}/people/{username}/remove", people::org_people_remove)
        .post("/orgs/{org}/people/{username}/role", people::org_people_role)
        .get("/orgs/{org}/teams", people::org_teams)
        .post("/orgs/{org}/teams/new", people::org_team_new)
        .get("/orgs/{org}/teams/{team}", people::org_team)
        .post("/orgs/{org}/teams/{team}/permission", people::org_team_permission)
        .post("/orgs/{org}/teams/{team}/members/add", people::org_team_member_add)
        .post("/orgs/{org}/teams/{team}/members/{username}/remove", people::org_team_member_remove)
        .post("/orgs/{org}/teams/{team}/repos/add", people::org_team_repo_add)
        .post("/orgs/{org}/teams/{team}/repos/{repo_id}/remove", people::org_team_repo_remove)
        .post("/orgs/{org}/teams/{team}/delete", people::org_team_delete)
        .form("/orgs/{org}/settings", people::org_settings_get, people::org_settings_post)
        .post("/orgs/{org}/settings/delete", people::org_settings_delete)
        .form("/new", people::new_repo_get, people::new_repo_post)
        .form("/new/org", people::new_org_get, people::new_org_post)
        .get("/avatar/{name}", avatar)
        .post("/preview", repo::preview)
        .get("/{owner}/{repo}", repo::home)
        // The browser workspace. Keep in step with `views::workspace::ROUTES`.
        .get("/{owner}/{repo}/workspace", ws::page::shell)
        .get("/{owner}/{repo}/workspace/assets/{build}/{*file}", ws::assets::get)
        .get("/{owner}/{repo}/workspace/terminal", ws::terminal::frame)
        .get("/{owner}/{repo}/workspace/terminal/source.tar", ws::terminal::source)
        .get("/{owner}/{repo}/workspace/boot", ws::files::boot)
        .get("/{owner}/{repo}/workspace/tree", ws::files::tree)
        .get("/{owner}/{repo}/workspace/file", ws::files::file)
        .post("/{owner}/{repo}/workspace/search", ws::files::search)
        .get("/{owner}/{repo}/workspace/snapshot", ws::files::snapshot)
        .post("/{owner}/{repo}/workspace/commit", ws::commit::commit)
        .post("/{owner}/{repo}/workspace/branches", ws::commit::branches)
        .get("/{owner}/{repo}/workspace/runs", ws::runs::runs)
        .get("/{owner}/{repo}/workspace/runs/{run}", ws::runs::run)
        .get("/{owner}/{repo}/workspace/runs/{run}/jobs/{job}/log/stream", ws::runs::log_stream)
        .get("/{owner}/{repo}/workspace/workflows", ws::runs::workflows)
        .post("/{owner}/{repo}/workspace/dispatch", ws::runs::dispatch)
        .post("/{owner}/{repo}/workspace/runs/{run}/rerun", ws::runs::rerun)
        .post("/{owner}/{repo}/workspace/runs/{run}/cancel", ws::runs::cancel)
        .get("/{owner}/{repo}/tree/{*rest}", repo::tree)
        .get("/{owner}/{repo}/blob/{*rest}", repo::blob)
        .get("/{owner}/{repo}/find/{*rest}", repo::find)
        .get("/{owner}/{repo}/raw/{*rest}", repo::raw)
        .get("/{owner}/{repo}/blame/{*rest}", repo::blame)
        .get("/{owner}/{repo}/archive/{*rest}", repo::archive)
        .get("/{owner}/{repo}/commits", repo::commits_default)
        .get("/{owner}/{repo}/commits/{*rest}", repo::commits)
        .get("/{owner}/{repo}/commit/{sha}", repo::commit)
        .get("/{owner}/{repo}/branches", repo::branches)
        .post("/{owner}/{repo}/branches/delete", repo::branch_delete)
        .get("/{owner}/{repo}/tags", repo::tags)
        .post("/{owner}/{repo}/tags/delete", repo::tag_delete)
        .get("/{owner}/{repo}/compare", repo::compare_form)
        .get("/{owner}/{repo}/compare/{*spec}", repo::compare)
        .form("/{owner}/{repo}/edit/{*rest}", repo::edit_get, repo::edit_post)
        .form("/{owner}/{repo}/new/{*rest}", repo::new_get, repo::new_post)
        .form("/{owner}/{repo}/delete/{*rest}", repo::delete_get, repo::delete_post)
        .get("/{owner}/{repo}/issues", issues::list)
        .form("/{owner}/{repo}/issues/new", issues::new_get, issues::new_post)
        .get("/{owner}/{repo}/issues/{number}", issues::show)
        .post("/{owner}/{repo}/issues/{number}/comment", issues::comment_post)
        .post("/{owner}/{repo}/issues/{number}/edit", issues::edit_post)
        .post("/{owner}/{repo}/issues/{number}/labels", issues::labels_post)
        .post("/{owner}/{repo}/issues/{number}/milestone", issues::milestone_post)
        .post("/{owner}/{repo}/issues/{number}/assignees", issues::assignees_post)
        .get("/{owner}/{repo}/labels", issues::labels_get)
        .post("/{owner}/{repo}/labels", issues::label_create)
        .post("/{owner}/{repo}/labels/delete", issues::label_delete)
        .get("/{owner}/{repo}/milestones", issues::milestones_get)
        .post("/{owner}/{repo}/milestones", issues::milestone_create)
        .post("/{owner}/{repo}/milestones/{id}/state", issues::milestone_state)
        .get("/{owner}/{repo}/actions", actions::runs)
        .form("/{owner}/{repo}/actions/run", actions::dispatch_get, actions::dispatch_post)
        .get("/{owner}/{repo}/actions/runs/{run}", actions::run_get)
        .get("/{owner}/{repo}/actions/runs/{run}/jobs/{job}", actions::job_get)
        .get("/{owner}/{repo}/actions/runs/{run}/jobs/{job}/log/stream", actions::job_stream)
        .get("/{owner}/{repo}/actions/runs/{run}/jobs/{job}/log.txt", actions::job_log_txt)
        .post("/{owner}/{repo}/actions/runs/{run}/cancel", actions::run_cancel)
        .post("/{owner}/{repo}/actions/runs/{run}/approve", actions::run_approve)
        .post("/{owner}/{repo}/actions/runs/{run}/rerun", actions::run_rerun)
        .get("/{owner}/{repo}/actions/runs/{run}/artifacts/{id}", actions::artifact_download)
        .form("/{owner}/{repo}/settings/secrets", actions::secrets_get, actions::secrets_post)
        .post("/{owner}/{repo}/settings/secrets/{name}/delete", actions::secret_delete)
        .post("/{owner}/{repo}/settings/variables/{name}/delete", actions::variable_delete)
        .get("/{owner}/{repo}/settings/runners", actions::runners_get)
        .get("/{owner}/{repo}/releases", releases::list)
        .get("/{owner}/{repo}/releases/tag/{tag}", releases::show)
        .get("/{owner}/{repo}/releases/download/{tag}/{name}", releases::download)
        .form("/{owner}/{repo}/releases/new", releases::new_get, releases::new_post)
        .post("/{owner}/{repo}/releases/{id}/delete", releases::delete)
        .form("/{owner}/{repo}/settings", rs::general_get, rs::general_post)
        .get("/{owner}/{repo}/settings/visibility", visibility::visibility_get)
        .post("/{owner}/{repo}/settings/visibility", visibility::visibility_post)
        .post("/{owner}/{repo}/settings/contributions", visibility::contributions_post)
        .post("/{owner}/{repo}/settings/archive", rs::archive_post)
        .post("/{owner}/{repo}/settings/rename", rs::rename_post)
        .post("/{owner}/{repo}/settings/transfer", rs::transfer_post)
        .post("/{owner}/{repo}/settings/delete", rs::delete_post)
        .get("/{owner}/{repo}/settings/collaborators", rs::collaborators_get)
        .post("/{owner}/{repo}/settings/collaborators", rs::collaborator_add)
        .post("/{owner}/{repo}/settings/collaborators/{user_id}/remove", rs::collaborator_remove)
        .get("/{owner}/{repo}/settings/branches", rs::branches_get)
        .post("/{owner}/{repo}/settings/branches", rs::branch_protect)
        .post("/{owner}/{repo}/settings/branches/remove", rs::branch_unprotect)
        .get("/{owner}/{repo}/settings/hooks", rs::hooks_get)
        .post("/{owner}/{repo}/settings/hooks", rs::hook_create)
        .post("/{owner}/{repo}/settings/hooks/{id}/delete", rs::hook_delete)
        .get("/{owner}/{repo}/pulls", pulls::list)
        .form("/{owner}/{repo}/pulls/new", pulls::new_get, pulls::new_post)
        .get("/{owner}/{repo}/pulls/{number}", pulls::show)
        .get("/{owner}/{repo}/pulls/{number}/commits", pulls::commits)
        .get("/{owner}/{repo}/pulls/{number}/checks", pulls::checks)
        .get("/{owner}/{repo}/pulls/{number}/files", pulls::files)
        .post("/{owner}/{repo}/pulls/{number}/comment", pulls::comment_post)
        .post("/{owner}/{repo}/pulls/{number}/edit", pulls::edit_post)
        .post("/{owner}/{repo}/pulls/{number}/labels", pulls::labels_post)
        .post("/{owner}/{repo}/pulls/{number}/merge", pulls::merge_post)
        .post("/{owner}/{repo}/pulls/{number}/review", pulls::review_post)
        .post("/{owner}/{repo}/pulls/{number}/inline", pulls::inline_post)
        .post("/{owner}/{repo}/pulls/{number}/resolve", pulls::resolve_post)
        .get("/{owner}", people::profile);
    if mcp {
        r = r.get("/oauth/authorize", apps::authorize_get).post("/oauth/consent", apps::consent_post);
    }
    r
}

async fn favicon() -> Response {
    let mut r = (StatusCode::OK, crate::FAVICON_SVG).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
    r
}

/// Every `(method, path)` this crate registers.
pub fn route_table() -> Vec<(&'static str, &'static str)> {
    build().table
}

/// Generated SVG avatars. They are deterministic from the name, so they cache for a day.
async fn avatar(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let name = name.trim_end_matches(".svg");
    let mut r = (StatusCode::OK, ch_render::avatar::svg(name)).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    r
}
