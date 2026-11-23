//! The browser workspace at `/{owner}/{repo}/workspace` (epic ch-1ke). A full-screen editor that
//! reads the repository at a pinned commit, keeps drafts in the browser and commits many files
//! at once. Every route is signed in only: none is in `PUBLIC_ROUTES` or `PUBLIC_REPO_ROUTES`.
//! See `ctx.rs` for the order of checks every route runs.

pub mod assets;
pub mod commit;
pub mod ctx;
pub mod files;
pub mod headers;
pub mod keys;
pub mod limits;
pub mod page;
pub mod runs;
pub mod terminal;

/// Every workspace route: method, path pattern, the least permission, and whether it needs the
/// workspace key. The access suites check this table against the router.
pub const ROUTES: &[(&str, &str, &str, bool)] = &[
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
