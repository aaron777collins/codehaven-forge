//! What both access suites check: every workspace route, the least it needs, and whether it
//! needs the workspace key. `WORKSPACE_ROUTES` must equal the router's workspace routes.

#![allow(dead_code)]

pub mod surface;

pub const WORKSPACE_ROUTES: &[(&str, &str, &str, bool)] = &[
    ("GET", "/{owner}/{repo}/workspace", "read", false),
    ("GET", "/{owner}/{repo}/workspace/assets/{build}/{*file}", "read", false),
    ("GET", "/{owner}/{repo}/workspace/terminal", "read", false),
    ("GET", "/{owner}/{repo}/workspace/terminal/source.tar", "read", false),
    ("GET", "/{owner}/{repo}/workspace/boot", "read", true),
    ("GET", "/{owner}/{repo}/workspace/tree", "read", true),
    ("GET", "/{owner}/{repo}/workspace/file", "read", true),
    ("POST", "/{owner}/{repo}/workspace/search", "read", true),
    ("GET", "/{owner}/{repo}/workspace/snapshot", "read", true),
    ("POST", "/{owner}/{repo}/workspace/commit", "write", true),
    ("POST", "/{owner}/{repo}/workspace/branches", "write", true),
    ("GET", "/{owner}/{repo}/workspace/runs", "read", true),
    ("GET", "/{owner}/{repo}/workspace/runs/{run}", "read", true),
    ("GET", "/{owner}/{repo}/workspace/runs/{run}/jobs/{job}/log/stream", "read", true),
    ("GET", "/{owner}/{repo}/workspace/workflows", "read", true),
    ("POST", "/{owner}/{repo}/workspace/dispatch", "write", true),
    ("POST", "/{owner}/{repo}/workspace/runs/{run}/rerun", "write", true),
    ("POST", "/{owner}/{repo}/workspace/runs/{run}/cancel", "write", true),
];

/// The admin page for the workspace switches. Site admin with two-factor only.
pub const ADMIN_WORKSPACE_ROUTES: &[(&str, &str)] = &[("GET", "/admin/workspace"), ("POST", "/admin/workspace")];

/// A request path for a pattern, with this owner and repository.
pub fn path_for(pattern: &str, owner: &str, repo: &str) -> String {
    pattern
        .replace("{owner}", owner)
        .replace("{repo}", repo)
        .replace("{build}", "b")
        .replace("{*file}", "workspace.js")
        .replace("{run}", "1")
        .replace("{job}", "1")
}
