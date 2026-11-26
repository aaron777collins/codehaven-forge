//! The deny-by-default gate's rulebook.
//!
//! `PUBLIC_ROUTES` is the complete list of routes a signed-out request may reach
//! (ARCHITECTURE.md sections 4.1a and 7.3). Everything else is rejected before a handler
//! runs. A new route is private until someone adds it here on purpose, and every change
//! to this table is reviewed by Claude.

use crate::actor::Actor;
use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::request::Parts;
use std::convert::Infallible;

/// What a rejected signed-out request gets, decided only by the shape of the path so that
/// paths that exist and paths that do not look identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// A web page: `303` to `/login?next=...`.
    Page,
    /// `/api/v3/*`: `401` with a JSON body.
    Api,
    /// git smart HTTP: `401` with a Basic challenge.
    GitHttp,
    /// `/v2/*` container registry: `401` with a Bearer challenge.
    Registry,
    /// `/api/runner/v1/*`: `401` unless a runner credential or job token is present.
    Runner,
    /// The MCP endpoint, `/mcp`: `401` with a `WWW-Authenticate` challenge that points at the
    /// protected resource document. Accepts only an OAuth access token of a connected app.
    Mcp,
    /// The signed-out OAuth surface: the three discovery documents and the token and
    /// revocation endpoints. No credential is read and no cookie is set.
    OAuth,
}

/// The exact path of the MCP endpoint.
pub const MCP_PATH: &str = "/mcp";
/// The protected resource documents, with and without the resource path suffix.
pub const WELL_KNOWN_RESOURCE_MCP: &str = "/.well-known/oauth-protected-resource/mcp";
pub const WELL_KNOWN_RESOURCE: &str = "/.well-known/oauth-protected-resource";
pub const WELL_KNOWN_AUTH_SERVER: &str = "/.well-known/oauth-authorization-server";
pub const OAUTH_TOKEN_PATH: &str = "/oauth/token";
pub const OAUTH_REVOKE_PATH: &str = "/oauth/revoke";

/// True for the seven paths that exist only while MCP is on: the endpoint, the three discovery
/// documents, the token and revocation endpoints, and the signed-in authorize and consent pages.
pub fn is_mcp_path(path: &str) -> bool {
    matches!(
        path,
        MCP_PATH
            | WELL_KNOWN_RESOURCE_MCP
            | WELL_KNOWN_RESOURCE
            | WELL_KNOWN_AUTH_SERVER
            | OAUTH_TOKEN_PATH
            | OAUTH_REVOKE_PATH
            | "/oauth/authorize"
            | "/oauth/consent"
    )
}

/// A static method and path pattern.
///
/// Pattern syntax: literal segments, `{name}` for exactly one non-empty segment, and a
/// final `*` for one or more remaining segments.
#[derive(Debug, Clone, Copy)]
pub struct PublicRoute {
    pub methods: &'static [&'static str],
    pub pattern: &'static str,
    pub why: &'static str,
}

pub const PUBLIC_ROUTES: &[PublicRoute] = &[
    PublicRoute { methods: &["GET", "POST"], pattern: "/login", why: "Signing in" },
    PublicRoute { methods: &["GET", "POST"], pattern: "/login/two-factor", why: "Second step of signing in" },
    PublicRoute {
        methods: &["POST"],
        pattern: "/login/two-factor/passkey/begin",
        why: "Second step of signing in with a passkey (ch-wf0): hands out a challenge only to a half signed-in session, and says nothing otherwise",
    },
    PublicRoute {
        methods: &["POST"],
        pattern: "/login/two-factor/passkey",
        why: "Second step of signing in with a passkey (ch-wf0): needs the half signed-in session, counts as a wrong code when it fails",
    },
    PublicRoute { methods: &["GET", "POST"], pattern: "/invite/{token}", why: "Accepting an invite" },
    PublicRoute {
        methods: &["GET", "POST"],
        pattern: "/password/forgot",
        why: "Asks for a reset link by email (ch-bfa). The answer is the same whether or not an account matches, and with mail off nothing is looked up",
    },
    PublicRoute { methods: &["GET", "POST"], pattern: "/password/{token}", why: "One-time set-password and reset links" },
    PublicRoute { methods: &["GET"], pattern: "/login/oidc/{provider}", why: "Only answers when a provider is configured" },
    PublicRoute { methods: &["GET"], pattern: "/login/oidc/{provider}/callback", why: "Only answers when a provider is configured" },
    PublicRoute { methods: &["GET"], pattern: "/static/*", why: "Styles, scripts and icons. Nothing user-generated" },
    PublicRoute { methods: &["GET"], pattern: "/favicon.svg", why: "The mark" },
    PublicRoute { methods: &["GET"], pattern: "/robots.txt", why: "Disallow: /" },
    PublicRoute { methods: &["GET"], pattern: "/healthz", why: "Says ok and nothing else" },
    PublicRoute {
        methods: &["POST"],
        pattern: "/api/runner/v1/register",
        why: "Rejects anything without an admin-issued registration token",
    },
    PublicRoute {
        methods: &["GET", "PUT"],
        pattern: "/api/runner/v1/toolkit/blobs/{grant}",
        why: "The Actions toolkit's cache archive and artifact zip (GET) and artifact upload (PUT), which the toolkit moves with no credential: a one-hour link this server signed for one entry or one open upload. Anything else is a 404",
    },
    // The five below exist only while `mcp.enabled` is true. Each is static or answers only for
    // a code or token made at the consent page. There is no client registration route.
    PublicRoute {
        methods: &["GET"],
        pattern: "/.well-known/oauth-protected-resource/mcp",
        why: "Static JSON from config: where the MCP endpoint is and who signs people in",
    },
    PublicRoute { methods: &["GET"], pattern: "/.well-known/oauth-protected-resource", why: "The same document" },
    PublicRoute {
        methods: &["GET"],
        pattern: "/.well-known/oauth-authorization-server",
        why: "Static JSON from config: the OAuth endpoints",
    },
    PublicRoute {
        methods: &["POST"],
        pattern: "/oauth/token",
        why: "Answers only for a code made at the consent page or a live refresh token",
    },
    PublicRoute { methods: &["POST"], pattern: "/oauth/revoke", why: "Empty answer for any token, known or not" },
];

/// What a signed-out route on the public list is about. The gate looks the target up before
/// any handler runs, and lets the request through only when that target is public now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicTarget {
    /// Listed only while the instance allows public repositories. No lookup.
    Discovery,
    /// The `{owner}` (or `{login}`, `{user}`, `{org}`) segment must own a public repository.
    Owner,
    /// `{owner}/{repo}` must be a public repository.
    Repo,
    /// A registry path (`/v2/{*rest}`): a pull of an image linked to a public repository, or
    /// the catalog (ch-4pn.13). Never an upload.
    Image,
}

/// How much a request may cost the shared server, for the signed-out caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cost {
    /// An ordinary page or JSON answer.
    Light,
    /// Blame, compare, diffs and file lists of a pull request.
    Heavy,
    /// A raw file, a blob or a release file.
    Raw,
    /// A zip or tar of a tree.
    Archive,
    /// A clone or fetch.
    Git,
}

/// One route a signed-out visitor may reach on a public repository (epic ch-4pn, spec section
/// 5). The pattern is written exactly as the router registers it, so the gate and the router
/// agree on which route a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicRepoRoute {
    pub methods: &'static [&'static str],
    pub pattern: &'static str,
    pub target: PublicTarget,
    pub cost: Cost,
}

const fn r(pattern: &'static str, target: PublicTarget, cost: Cost) -> PublicRepoRoute {
    PublicRepoRoute { methods: &["GET"], pattern, target, cost }
}

use Cost::{Archive, Git, Heavy, Light, Raw};
use PublicTarget::{Discovery, Image, Owner, Repo as RepoT};

/// The complete list of what a signed-out visitor may read while the instance allows public
/// repositories. HEAD follows GET, except on git and archive routes. Everything else stays
/// closed. Changing this list is a security decision, reviewed by Claude.
pub const PUBLIC_REPO_ROUTES: &[PublicRepoRoute] = &[
    // Discovery
    r("/explore", Discovery, Light),
    r("/avatar/{name}", Discovery, Light),
    r("/api/v3/search/repositories", Discovery, Light),
    // Owners that have a public repository
    r("/{owner}", Owner, Light),
    r("/api/v3/users/{login}", Owner, Light),
    r("/api/v3/users/{user}/repos", Owner, Light),
    r("/api/v3/orgs/{org}", Owner, Light),
    r("/api/v3/orgs/{org}/repos", Owner, Light),
    // Repository pages
    r("/{owner}/{repo}", RepoT, Light),
    r("/{owner}/{repo}/tree/{*rest}", RepoT, Light),
    r("/{owner}/{repo}/blob/{*rest}", RepoT, Light),
    r("/{owner}/{repo}/find/{*rest}", RepoT, Light),
    r("/{owner}/{repo}/raw/{*rest}", RepoT, Raw),
    r("/{owner}/{repo}/blame/{*rest}", RepoT, Heavy),
    r("/{owner}/{repo}/archive/{*rest}", RepoT, Archive),
    r("/{owner}/{repo}/commits", RepoT, Light),
    r("/{owner}/{repo}/commits/{*rest}", RepoT, Light),
    r("/{owner}/{repo}/commit/{sha}", RepoT, Heavy),
    r("/{owner}/{repo}/branches", RepoT, Light),
    r("/{owner}/{repo}/tags", RepoT, Light),
    r("/{owner}/{repo}/compare", RepoT, Light),
    r("/{owner}/{repo}/compare/{*spec}", RepoT, Heavy),
    r("/{owner}/{repo}/issues", RepoT, Light),
    r("/{owner}/{repo}/issues/{number}", RepoT, Light),
    r("/{owner}/{repo}/labels", RepoT, Light),
    r("/{owner}/{repo}/milestones", RepoT, Light),
    r("/{owner}/{repo}/pulls", RepoT, Light),
    r("/{owner}/{repo}/pulls/{number}", RepoT, Light),
    r("/{owner}/{repo}/pulls/{number}/commits", RepoT, Light),
    r("/{owner}/{repo}/pulls/{number}/files", RepoT, Heavy),
    r("/{owner}/{repo}/releases", RepoT, Light),
    r("/{owner}/{repo}/releases/tag/{tag}", RepoT, Light),
    r("/{owner}/{repo}/releases/download/{tag}/{name}", RepoT, Raw),
    // Actions history recorded at the public tier (ch-4pn.12). The handlers show only rows whose
    // recorded tier covers the viewer, see `history_tiers`.
    r("/{owner}/{repo}/actions", RepoT, Light),
    r("/{owner}/{repo}/actions/runs/{run}", RepoT, Light),
    r("/{owner}/{repo}/actions/runs/{run}/jobs/{job}", RepoT, Light),
    r("/{owner}/{repo}/actions/runs/{run}/jobs/{job}/log.txt", RepoT, Raw),
    r("/{owner}/{repo}/pulls/{number}/checks", RepoT, Light),
    // Repository API
    r("/api/v3/repos/{owner}/{repo}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/topics", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/forks", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/branches", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/branches/{*rest}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/tags", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/commits", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/commits/{ref}", RepoT, Heavy),
    r("/api/v3/repos/{owner}/{repo}/compare/{basehead}", RepoT, Heavy),
    r("/api/v3/repos/{owner}/{repo}/contents", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/contents/{*path}", RepoT, Raw),
    r("/api/v3/repos/{owner}/{repo}/readme", RepoT, Light),
    PublicRepoRoute { methods: &["GET"], pattern: "/api/v3/repos/{owner}/{repo}/zipball", target: RepoT, cost: Archive },
    PublicRepoRoute { methods: &["GET"], pattern: "/api/v3/repos/{owner}/{repo}/zipball/{ref}", target: RepoT, cost: Archive },
    PublicRepoRoute { methods: &["GET"], pattern: "/api/v3/repos/{owner}/{repo}/tarball", target: RepoT, cost: Archive },
    PublicRepoRoute { methods: &["GET"], pattern: "/api/v3/repos/{owner}/{repo}/tarball/{ref}", target: RepoT, cost: Archive },
    r("/api/v3/repos/{owner}/{repo}/git/refs", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/git/matching-refs/{*prefix}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/git/ref/{*name}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/git/refs/{*name}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/git/trees/{sha}", RepoT, Heavy),
    r("/api/v3/repos/{owner}/{repo}/git/blobs/{sha}", RepoT, Raw),
    r("/api/v3/repos/{owner}/{repo}/git/commits/{sha}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/{number}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/{number}/comments", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/comments/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/{number}/labels", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/{number}/events", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/{number}/timeline", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/{number}/reactions", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issues/comments/{id}/reactions", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/issue-templates", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/labels", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/labels/{name}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/milestones", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/milestones/{number}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/files", RepoT, Heavy),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/commits", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/merge", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/reviews", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/reviews/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/reviews/{id}/comments", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/comments", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/{number}/threads", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/comments/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/pulls/comments/{id}/reactions", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/releases", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/releases/latest", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/releases/tags/{*tag}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/releases/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/releases/{id}/assets", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/releases/assets/{id}", RepoT, Raw),
    // Actions history, statuses and checks recorded at the public tier (ch-4pn.12). Variables,
    // secrets, caches, runners, the run log zip, artifact downloads and live streams stay closed.
    r("/api/v3/repos/{owner}/{repo}/commits/{reference}/statuses", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/commits/{reference}/status", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/commits/{reference}/check-runs", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/check-runs/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/workflows", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/workflows/{workflow}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/workflows/{workflow}/runs", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/runs", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/runs/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/runs/{id}/jobs", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/runs/{id}/artifacts", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/jobs/{id}", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/jobs/{id}/logs", RepoT, Raw),
    r("/api/v3/repos/{owner}/{repo}/actions/artifacts", RepoT, Light),
    r("/api/v3/repos/{owner}/{repo}/actions/artifacts/{id}", RepoT, Light),
    // The registry (ch-4pn.13): `/v2/token` hands a signed-out client the literal pull-only token
    // `anonymous`, and a pull of an image linked to a public repository, or the catalog, gets
    // in. `/v2/` itself still challenges, so `docker login` works. Uploads never match: their
    // paths end in `/`, `uploads` or an upload id, which the gate's lookup refuses.
    r("/v2/token", Discovery, Light),
    r("/v2/{*rest}", Image, Raw),
    // Git over HTTPS: clone and fetch only. Push always challenges.
    PublicRepoRoute { methods: &["GET"], pattern: "/{owner}/{repo}/info/refs", target: RepoT, cost: Git },
    PublicRepoRoute { methods: &["POST"], pattern: "/{owner}/{repo}/git-upload-pack", target: RepoT, cost: Git },
];

/// Which recorded tiers of Actions history a viewer may see (spec section 4, "Actions"). A run,
/// its jobs, logs, artifacts, checks and statuses show only when `min(recorded_tier,
/// access_tier)` covers the viewer. The caller has already required View, so the repository's
/// tier covers them; what is left is the tier recorded on the row. `None` means all of it, and
/// an empty list means none.
///
/// All of it needs Read that comes from a grant (`grant`), or a job of the repository itself.
/// Read that comes only from the tier (any account on an internal or public repository) is not
/// enough (ch-6j4). Without a grant, a signed-out visitor sees rows recorded public, and an
/// account (a session, an app, or a token that may read repositories) sees rows recorded
/// internal or public. A token without a repo scope reads only what anyone may read.
pub fn history_tiers(actor: &Actor, perm: crate::Permission, grant: bool) -> Option<&'static [&'static str]> {
    const PUBLIC: &[&str] = &["public"];
    const SIGNED_IN: &[&str] = &["internal", "public"];
    const NONE: &[&str] = &[];
    if perm >= crate::Permission::Read && (grant || matches!(actor, Actor::Job { .. } | Actor::System)) {
        return None;
    }
    if perm < crate::Permission::View {
        return Some(NONE);
    }
    Some(match actor {
        Actor::User { .. } | Actor::App { .. } => SIGNED_IN,
        Actor::Token { scopes, .. } if scopes.repo_cap() >= crate::Permission::Read => SIGNED_IN,
        _ => PUBLIC,
    })
}

/// The target a public route names: the owner segment and, for a repository route, the repo
/// segment, as written in the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicMatch<'a> {
    pub rule: &'static PublicRepoRoute,
    pub owner: Option<&'a str>,
    pub repo: Option<&'a str>,
    /// The path as sent, for targets the segments alone do not name (a registry path).
    pub path: &'a str,
}

/// How specific a pattern segment is, the way the router ranks them: a literal beats a
/// parameter, which beats a catch-all.
fn rank_of(seg: &str) -> u8 {
    if seg.starts_with("{*") || seg == "*" {
        0
    } else if seg.starts_with('{') {
        1
    } else {
        2
    }
}

/// The public-repository route a signed-out request with this method, path and query would
/// reach. Only the shape is checked here. The gate then looks the target up, and a layer inside
/// the router confirms the router chose this same pattern.
pub fn public_repo_route<'a>(method: &Method, path: &'a str, query: Option<&str>) -> Option<PublicMatch<'a>> {
    let segs = safe_segments(path)?;
    let head = *method == Method::HEAD;
    let m = if head { "GET" } else { method.as_str() };
    let mut best: Option<(&'static PublicRepoRoute, Vec<u8>)> = None;
    for rule in PUBLIC_REPO_ROUTES {
        if !rule.methods.contains(&m) || !pattern_matches(rule.pattern, &segs) {
            continue;
        }
        let ranks: Vec<u8> = rule.pattern.trim_start_matches('/').split('/').map(rank_of).collect();
        if best.as_ref().is_none_or(|(_, b)| ranks > *b) {
            best = Some((rule, ranks));
        }
    }
    let (rule, _) = best?;
    if head && matches!(rule.cost, Cost::Git | Cost::Archive) {
        return None;
    }
    // `info/refs` is open only for exactly one `service=git-upload-pack`.
    if rule.pattern.ends_with("/info/refs") && query != Some("service=git-upload-pack") {
        return None;
    }
    // A branch, never its protection settings.
    if rule.pattern.ends_with("/branches/{*rest}") && segs.last().is_some_and(|s| *s == "protection") {
        return None;
    }
    let pats: Vec<&str> = rule.pattern.trim_start_matches('/').split('/').collect();
    let mut owner = None;
    let mut repo = None;
    for (i, p) in pats.iter().enumerate() {
        match *p {
            "{owner}" | "{login}" | "{user}" | "{org}" => owner = segs.get(i).copied(),
            "{repo}" => repo = segs.get(i).copied(),
            _ => {}
        }
    }
    Some(PublicMatch { rule, owner, repo, path })
}

/// An account name as the users table allows it: 1 to 39 letters, digits and inner dashes.
pub fn valid_owner_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 39 && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-') && b[0] != b'-' && b[b.len() - 1] != b'-'
}

/// A repository name as the repositories table allows it, with an optional `.git` suffix
/// removed first. `None` when it is not one.
pub fn valid_repo_name(s: &str) -> Option<&str> {
    let n = s.strip_suffix(".git").unwrap_or(s);
    let ok = !n.is_empty()
        && n.len() <= 100
        && n != "."
        && n != ".."
        && n.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    ok.then_some(n)
}

/// The key a signed-out limit counts by: the address, or its /64 for IPv6, so one host cannot
/// take a fresh budget from each of its addresses.
pub fn address_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

/// `address_key`, but an IPv6 client counts by its /56, the block one home or office is usually
/// given, so it cannot spread over 256 /64s (ch-6yg). Used where a cap holds open connections,
/// such as the Pages host's replies per address.
pub fn wide_address_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/56", s[0], s[1], s[2], s[3] & 0xff00)
        }
        other => address_key(other),
    }
}

/// True when `router_path` (a registered pattern such as `/{owner}/{repo}/tree/{*rest}`) is the
/// rule's own pattern. The router layer checks this, so a path the gate admitted can only run
/// the handler the rule names.
pub fn same_route(rule: &PublicRepoRoute, router_path: &str) -> bool {
    normalize_pattern(rule.pattern) == normalize_pattern(router_path)
}

/// A route pattern with parameter names dropped, so `/{a}/x` and `/{b}/x` compare equal.
pub fn normalize_route(p: &str) -> String {
    normalize_pattern(p)
}

/// Set by the gate on a signed-out request it admitted to a public route. A handler or
/// extractor that would refuse a signed-out caller lets it through only with this present, and
/// `Authz` still has to say `View` for the repository.
#[derive(Debug, Clone)]
pub struct PublicRead {
    pub pattern: &'static str,
    pub cost: Cost,
}

/// Splits a path into segments, or `None` when the path is not safe to match at all:
/// empty segments, dot segments, encoded slashes or backslashes, control characters.
fn safe_segments(path: &str) -> Option<Vec<&str>> {
    if !path.starts_with('/') || path.len() > 2048 {
        return None;
    }
    if path.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\') {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%00") || lower.contains("%2e") {
        return None;
    }
    if path == "/" {
        return Some(vec![]);
    }
    let segs: Vec<&str> = path[1..].split('/').collect();
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(segs)
}

fn pattern_matches(pattern: &str, segs: &[&str]) -> bool {
    let pats: Vec<&str> = if pattern == "/" { vec![] } else { pattern[1..].split('/').collect() };
    let mut i = 0;
    for (n, p) in pats.iter().enumerate() {
        if *p == "*" || p.starts_with("{*") {
            // `*` must be last and needs at least one segment.
            return n == pats.len() - 1 && segs.len() > n;
        }
        let Some(s) = segs.get(i) else { return false };
        if p.starts_with('{') && p.ends_with('}') {
            // Any single non-empty segment.
        } else if p != s {
            return false;
        }
        i += 1;
    }
    i == segs.len()
}

/// True when a signed-out request with this method and path may reach a handler.
pub fn is_public(method: &Method, path: &str) -> bool {
    let Some(segs) = safe_segments(path) else { return false };
    let m = if *method == Method::HEAD { "GET" } else { method.as_str() };
    PUBLIC_ROUTES.iter().any(|r| r.methods.contains(&m) && pattern_matches(r.pattern, &segs))
}

/// True when `pattern` (as written in `PUBLIC_ROUTES`) is the same route as a registered
/// router path such as `/invite/{token}`. Used by the surface test.
pub fn is_public_pattern(method: &str, router_path: &str) -> bool {
    let m = if method == "HEAD" { "GET" } else { method };
    PUBLIC_ROUTES.iter().any(|r| r.methods.contains(&m) && normalize_pattern(r.pattern) == normalize_pattern(router_path))
}

fn normalize_pattern(p: &str) -> String {
    p.split('/')
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                if s.starts_with("{*") { "*".to_string() } else { "{}".to_string() }
            } else {
                s.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `classify` for an instance where MCP may be off. With MCP off the seven MCP paths are
/// ordinary unknown pages, so a signed-out visitor gets exactly what any other unknown path gets.
pub fn classify_for(path: &str, mcp_enabled: bool) -> RouteKind {
    if !mcp_enabled && is_mcp_path(path) {
        return RouteKind::Page;
    }
    classify(path)
}

/// `is_public` for an instance where MCP may be off: the five public MCP routes are public only
/// while MCP is on.
pub fn is_public_for(method: &Method, path: &str, mcp_enabled: bool) -> bool {
    if !mcp_enabled && is_mcp_path(path) {
        return false;
    }
    is_public(method, path)
}

/// Decides the rejection kind from the path alone.
pub fn classify(path: &str) -> RouteKind {
    if path == MCP_PATH {
        return RouteKind::Mcp;
    }
    if matches!(path, WELL_KNOWN_RESOURCE_MCP | WELL_KNOWN_RESOURCE | WELL_KNOWN_AUTH_SERVER | OAUTH_TOKEN_PATH | OAUTH_REVOKE_PATH) {
        return RouteKind::OAuth;
    }
    if path == "/api/runner/v1" || path.starts_with("/api/runner/") || path.starts_with("/twirp/") {
        return RouteKind::Runner;
    }
    if path == "/api/v3" || path.starts_with("/api/v3/") {
        return RouteKind::Api;
    }
    if path == "/v2" || path.starts_with("/v2/") {
        return RouteKind::Registry;
    }
    // The private NuGet feed (ch-ctk.13): NuGet clients answer a Basic challenge, not a Bearer one.
    if path.starts_with("/nuget/") {
        return RouteKind::GitHttp;
    }
    if is_git_http_path(path) {
        return RouteKind::GitHttp;
    }
    RouteKind::Page
}

/// `/{owner}/{repo}(.git)/info/refs`, `/git-upload-pack` and `/git-receive-pack`.
pub fn is_git_http_path(path: &str) -> bool {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    matches!(segs.as_slice(), [_, _, "info", "refs"] | [_, _, "git-upload-pack"] | [_, _, "git-receive-pack"])
}

/// A local path that is safe to put in `?next=`. Anything else becomes `None`.
pub fn safe_next(path_and_query: &str) -> Option<&str> {
    let p = path_and_query;
    // An authorize URL carries the client id, redirect, challenge and state, so it needs more
    // room than an ordinary page. Nothing else gets it.
    let cap = if p.starts_with("/oauth/authorize?") { 4096 } else { 512 };
    if !p.starts_with('/') || p.starts_with("//") || p.len() > cap {
        return None;
    }
    if p.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\') {
        return None;
    }
    // Never bounce back to the sign-in pages.
    if p == "/login" || p.starts_with("/login/") || p.starts_with("/login?") {
        return None;
    }
    Some(p)
}

/// Percent-encodes everything outside the unreserved set, for use as a query value.
pub fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Session cookie name. The `__Host-` prefix needs `Secure`, so it is used only when the
/// public origin is https. Plain http (local development) gets the bare name.
pub fn session_cookie_name(https: bool) -> &'static str {
    if https { "__Host-ch_session" } else { "ch_session" }
}

/// Signed double-submit CSRF cookie for forms that have no session yet (sign-in, invite).
pub fn csrf_cookie_name(https: bool) -> &'static str {
    if https { "__Host-ch_csrf" } else { "ch_csrf" }
}

/// Reads one cookie from a request's `Cookie` headers. Empty and oversized values are `None`.
pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=')
                && k == name
                && !v.is_empty()
                && v.len() <= 256
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The `Set-Cookie` value for a session: `HttpOnly`, `SameSite=Lax`, `Path=/`, and `Secure`
/// when the public origin is https (which the `__Host-` prefix requires).
pub fn session_set_cookie(value: &str, max_age_secs: i64, https: bool) -> String {
    let name = session_cookie_name(https);
    let secure = if https { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure}")
}

/// Expires the session cookie.
pub fn session_clear_cookie(https: bool) -> String {
    session_set_cookie("", 0, https)
}

/// Which credential kinds a route family accepts. A credential that is valid but not
/// accepted here is treated exactly like no credential, so a personal access token opens no
/// web page and a browser session opens no git or registry endpoint.
pub fn actor_accepted(actor: &Actor, kind: RouteKind) -> bool {
    use crate::actor::AuthVia;
    match (actor, kind) {
        (Actor::Anonymous, _) => false,
        (Actor::User { via: AuthVia::Session, .. }, RouteKind::Page | RouteKind::Api) => true,
        (Actor::User { via: AuthVia::Basic, .. }, RouteKind::Api | RouteKind::GitHttp | RouteKind::Registry) => true,
        (Actor::User { via: AuthVia::Ssh, .. }, _) => false,
        (Actor::Token { .. }, RouteKind::Api | RouteKind::GitHttp | RouteKind::Registry) => true,
        (Actor::Job { .. }, RouteKind::Api | RouteKind::GitHttp | RouteKind::Runner | RouteKind::Registry) => true,
        (Actor::Runner { .. }, RouteKind::Runner) => true,
        // An app token opens the MCP endpoint and nothing else, and nothing else opens it.
        (Actor::App { .. }, RouteKind::Mcp) => true,
        _ => false,
    }
}

/// Web file downloads a personal access token also opens (ch-8ee): a raw file, an archive and a
/// release file, GET or HEAD only, so a script can fetch with a token what a person downloads in
/// a browser. No other page accepts a token. The pattern is written as the router registers it,
/// and the layer inside the router checks the request really reached that route.
pub const TOKEN_FILE_ROUTES: &[&str] =
    &["/{owner}/{repo}/raw/{*rest}", "/{owner}/{repo}/archive/{*rest}", "/{owner}/{repo}/releases/download/{tag}/{name}"];

/// The token file route this request is shaped like, if any.
pub fn token_file_route(method: &Method, path: &str) -> Option<&'static str> {
    if *method != Method::GET && *method != Method::HEAD {
        return None;
    }
    let segs = safe_segments(path)?;
    TOKEN_FILE_ROUTES.iter().copied().find(|p| pattern_matches(p, &segs))
}

/// Set by the gate when a token was let onto a page route for a file download. The layer inside
/// the router lets the request run only on that route.
#[derive(Debug, Clone)]
pub struct TokenFileRead {
    pub pattern: &'static str,
}

/// The caller's API quota once this request is counted. The guard puts it on the request so
/// `GET /rate_limit` reports the same numbers as the `X-RateLimit-*` headers (ch-iw8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiQuota {
    pub limit: u32,
    pub remaining: u32,
    /// Unix time the window ends.
    pub reset: i64,
}

/// Extractor for the caller. The gate inserts the `Actor` it resolved; a handler that
/// somehow runs without one sees `Anonymous`.
#[derive(Debug, Clone)]
pub struct CurrentActor(pub Actor);

impl<S: Send + Sync> FromRequestParts<S> for CurrentActor {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentActor(parts.extensions.get::<Actor>().cloned().unwrap_or(Actor::Anonymous)))
    }
}

/// The caller's address and user agent, worked out by the request-info layer from the socket
/// and, from trusted proxies only, `X-Forwarded-For`. Handlers pass it to services for the
/// audit log and rate limits.
#[derive(Debug, Clone)]
pub struct Client(pub crate::model::ClientInfo);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Client(parts.extensions.get::<crate::model::ClientInfo>().cloned().unwrap_or_else(crate::model::ClientInfo::local)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn app() -> Actor {
        use crate::{AppGrant, GrantId, RepoSet, UserId};
        Actor::App {
            user_id: UserId(1),
            grant_id: GrantId(1),
            grant: AppGrant { read: RepoSet::All, write: RepoSet::none(), workflows: RepoSet::none() },
        }
    }

    #[test]
    fn all_history_needs_a_grant_not_just_read_from_the_tier() {
        use crate::{AuthVia, JobId, JobPerms, Permission, RepoId, UserId};
        let user = Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Session };
        // Read that comes only from an internal or public tier (ch-6j4).
        assert_eq!(history_tiers(&user, Permission::Read, false), Some(&["internal", "public"][..]));
        assert_eq!(history_tiers(&user, Permission::Read, true), None);
        assert_eq!(history_tiers(&user, Permission::Admin, true), None);
        assert_eq!(history_tiers(&Actor::Anonymous, Permission::View, false), Some(&["public"][..]));
        assert_eq!(history_tiers(&user, Permission::None, false), Some(&[][..]));
        let job = Actor::Job { job_id: JobId(1), repo_id: RepoId(1), perms: JobPerms::default_token(true) };
        assert_eq!(history_tiers(&job, Permission::Read, false), None, "a job reads its own repository's history");
    }

    #[test]
    fn the_connector_adds_exactly_five_public_routes_and_no_registration() {
        let mcp: Vec<&str> = PUBLIC_ROUTES
            .iter()
            .filter(|r| r.pattern.starts_with("/.well-known/") || r.pattern.starts_with("/oauth/"))
            .map(|r| r.pattern)
            .collect();
        assert_eq!(
            mcp,
            [
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource",
                "/.well-known/oauth-authorization-server",
                "/oauth/token",
                "/oauth/revoke",
            ]
        );
        assert_eq!(
            PUBLIC_ROUTES.len(),
            11 + 5 + 1 + 1 + 2,
            "the eleven that existed before, plus five, plus forgot password (ch-bfa), plus the toolkit cache link (ch-ctk.6), plus the two passkey steps of sign-in (ch-wf0)"
        );
        for p in [
            "/oauth/register",
            "/register",
            "/oauth2/register",
            "/oauth/clients",
            "/.well-known/oauth-client",
            "/oauth/authorize",
            "/oauth/consent",
            "/mcp",
        ] {
            for m in [Method::GET, Method::POST] {
                assert!(!is_public(&m, p), "{m} {p} must not be public");
            }
        }
        assert!(is_public(&Method::GET, "/.well-known/oauth-protected-resource/mcp"));
        assert!(is_public(&Method::HEAD, "/.well-known/oauth-authorization-server"), "HEAD follows GET");
        assert!(!is_public(&Method::POST, "/.well-known/oauth-authorization-server"));
        assert!(is_public(&Method::POST, "/oauth/token") && !is_public(&Method::GET, "/oauth/token"));
        assert!(is_public(&Method::POST, "/oauth/revoke") && !is_public(&Method::GET, "/oauth/revoke"));
        assert!(!is_public(&Method::GET, "/.well-known/oauth-protected-resource/other"));
        assert!(!is_public(&Method::GET, "/.well-known/oauth-protected-resource/mcp/"));
        assert!(!is_public(&Method::POST, "/oauth/token/x"));
    }

    #[test]
    fn mcp_routes_have_their_own_kinds_and_vanish_when_mcp_is_off() {
        assert_eq!(classify("/mcp"), RouteKind::Mcp);
        assert_eq!(classify("/mcp/"), RouteKind::Page, "only the exact path");
        for p in [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
            "/oauth/token",
            "/oauth/revoke",
        ] {
            assert_eq!(classify(p), RouteKind::OAuth, "{p}");
        }
        assert_eq!(classify("/oauth/authorize"), RouteKind::Page, "signed-in pages redirect to sign in");
        assert_eq!(classify("/oauth/consent"), RouteKind::Page);
        // With MCP off, all eight are ordinary unknown pages and nothing is public.
        for p in [
            "/mcp",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
            "/oauth/token",
            "/oauth/revoke",
            "/oauth/authorize",
            "/oauth/consent",
        ] {
            assert_eq!(classify_for(p, false), RouteKind::Page, "{p}");
            for m in [Method::GET, Method::POST, Method::HEAD] {
                assert!(!is_public_for(&m, p, false), "{m} {p}");
            }
            assert_eq!(classify_for(p, true), classify(p));
        }
        assert_eq!(classify_for("/login", false), RouteKind::Page);
        assert!(is_public_for(&Method::GET, "/login", false));
    }

    #[test]
    fn an_app_token_opens_the_mcp_endpoint_and_nothing_else_opens_it() {
        use crate::actor::{AuthVia, JobPerms};
        use crate::{JobId, RepoId, RunnerId, Scopes, TokenId, UserId};
        let kinds =
            [RouteKind::Page, RouteKind::Api, RouteKind::GitHttp, RouteKind::Registry, RouteKind::Runner, RouteKind::Mcp, RouteKind::OAuth];
        for k in kinds {
            assert_eq!(actor_accepted(&app(), k), k == RouteKind::Mcp, "app on {k:?}");
        }
        let others = [
            Actor::User { id: UserId(1), is_admin: true, has_2fa: true, via: AuthVia::Session },
            Actor::User { id: UserId(1), is_admin: true, has_2fa: true, via: AuthVia::Basic },
            Actor::Token { user_id: UserId(1), token_id: TokenId(1), scopes: Scopes::new(["repo"]) },
            Actor::Job { job_id: JobId(1), repo_id: RepoId(1), perms: JobPerms::default_token(false) },
            Actor::Runner { runner_id: RunnerId(1) },
            Actor::System,
            Actor::Anonymous,
        ];
        for a in &others {
            assert!(!actor_accepted(a, RouteKind::Mcp), "{a:?} must not open /mcp");
            assert!(!actor_accepted(a, RouteKind::OAuth), "the OAuth routes are public, never credentialed: {a:?}");
        }
    }

    #[test]
    fn only_the_authorize_url_may_be_long_in_next() {
        let long = format!("/oauth/authorize?client_id={}", "a".repeat(3000));
        assert!(safe_next(&long).is_some(), "the authorize URL keeps its parameters through sign-in");
        assert!(safe_next(&format!("/oauth/authorize?x={}", "a".repeat(4100))).is_none(), "but not without limit");
        assert!(safe_next(&format!("/oauth/authorizex?x={}", "a".repeat(600))).is_none());
        assert!(safe_next(&format!("/settings/{}", "a".repeat(600))).is_none(), "every other path keeps the 512 cap");
        assert!(safe_next("/oauth/authorize?a=1\nb").is_none(), "control characters are never accepted");
        assert!(safe_next("/oauth/authorize?\u{7}").is_none());
        assert!(safe_next("//evil.example/oauth/authorize?x=1").is_none());
    }

    #[test]
    fn listed_routes_are_public() {
        assert!(is_public(&Method::GET, "/login"));
        assert!(is_public(&Method::POST, "/login"));
        assert!(is_public(&Method::HEAD, "/login"));
        assert!(is_public(&Method::GET, "/login/two-factor"));
        assert!(is_public(&Method::POST, "/login/two-factor/passkey/begin"));
        assert!(is_public(&Method::POST, "/login/two-factor/passkey"));
        assert!(!is_public(&Method::GET, "/login/two-factor/passkey"));
        assert!(is_public(&Method::GET, "/invite/chi_abc"));
        assert!(is_public(&Method::POST, "/invite/chi_abc"));
        assert!(is_public(&Method::GET, "/password/tok"));
        assert!(is_public(&Method::GET, "/static/app.123.css"));
        assert!(is_public(&Method::GET, "/static/css/app.css"));
        assert!(is_public(&Method::GET, "/healthz"));
        assert!(is_public(&Method::GET, "/robots.txt"));
        assert!(is_public(&Method::GET, "/favicon.svg"));
        assert!(is_public(&Method::POST, "/api/runner/v1/register"));
        let a: std::net::IpAddr = "2001:db8:1:2a00::1".parse().unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
        let b: std::net::IpAddr = "2001:db8:1:2aff:ffff::9".parse().unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
        let c: std::net::IpAddr = "2001:db8:1:2b00::1".parse().unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
        assert_eq!(wide_address_key(a), wide_address_key(b), "one /56 is one client");
        assert_ne!(wide_address_key(a), wide_address_key(c));
        assert_ne!(address_key(a), address_key(b), "the /64 key still tells them apart");
        assert_eq!(wide_address_key(std::net::IpAddr::from([192, 0, 2, 1])), "192.0.2.1");
        assert!(is_public(&Method::GET, "/api/runner/v1/toolkit/blobs/c.1.2.abc"));
        assert!(is_public(&Method::PUT, "/api/runner/v1/toolkit/blobs/u.1.2.abc"), "an artifact upload link writes");
        assert!(!is_public(&Method::POST, "/api/runner/v1/toolkit/blobs/u.1.2.abc"), "and only with PUT");
        assert!(!is_public(&Method::DELETE, "/api/runner/v1/toolkit/blobs/u.1.2.abc"));
        assert!(
            !is_public(&Method::POST, "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact"),
            "the artifact service needs the runtime token"
        );
        assert_eq!(classify("/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact"), RouteKind::Runner);
        assert_eq!(classify("/nuget/alice/index.json"), RouteKind::GitHttp, "the feed asks for Basic credentials");
        assert!(!is_public(&Method::GET, "/nuget/alice/index.json"), "there is no public feed");
        assert!(!is_public(&Method::PUT, "/nuget/alice/push"));
        assert!(!is_public(&Method::GET, "/api/runner/v1/toolkit/_apis/artifactcache/cache"), "the cache API needs the runtime token");
        assert!(is_public(&Method::GET, "/login/oidc/github/callback"));
    }

    #[test]
    fn everything_else_is_private() {
        for (m, p) in [
            (Method::GET, "/"),
            (Method::GET, "/explore"),
            (Method::GET, "/signup"),
            (Method::GET, "/register"),
            (Method::POST, "/signup"),
            (Method::GET, "/root"),
            (Method::GET, "/nobody/nothing"),
            (Method::GET, "/api/v3/user"),
            (Method::GET, "/api/v3/meta"),
            (Method::GET, "/api/v3"),
            (Method::GET, "/v2/"),
            (Method::GET, "/metrics"),
            (Method::GET, "/readyz"),
            (Method::GET, "/api/runner/v1/jobs/request"),
            (Method::GET, "/api/runner/v1/register"),
            (Method::PUT, "/login"),
            (Method::DELETE, "/invite/x"),
            (Method::GET, "/invite"),
            (Method::GET, "/invite/a/b"),
            (Method::GET, "/static"),
            (Method::GET, "/static/"),
            (Method::GET, "/healthz/x"),
            (Method::POST, "/healthz"),
            (Method::GET, "/login/"),
            (Method::GET, "/login/oidc"),
            (Method::GET, "/x/y.git/info/refs"),
        ] {
            assert!(!is_public(&m, p), "{m} {p} must be private");
        }
    }

    #[test]
    fn tricky_paths_are_never_public() {
        for p in [
            "/static/../api/v3/user",
            "/static/%2e%2e/admin",
            "/static/a%2Fb",
            "/static/a\\b",
            "/login//",
            "//login",
            "/static//x",
            "/static/a\0b",
            "/static/./x",
        ] {
            assert!(!is_public(&Method::GET, p), "{p:?}");
        }
    }

    #[test]
    fn classify_by_shape() {
        assert_eq!(classify("/"), RouteKind::Page);
        assert_eq!(classify("/alice/demo"), RouteKind::Page);
        assert_eq!(classify("/api/v3/user"), RouteKind::Api);
        assert_eq!(classify("/api/v3"), RouteKind::Api);
        assert_eq!(classify("/api/runner/v1/jobs/request"), RouteKind::Runner);
        assert_eq!(classify("/v2/"), RouteKind::Registry);
        assert_eq!(classify("/v2/alice/img/manifests/latest"), RouteKind::Registry);
        assert_eq!(classify("/x/y.git/info/refs"), RouteKind::GitHttp);
        assert_eq!(classify("/x/y/git-upload-pack"), RouteKind::GitHttp);
        assert_eq!(classify("/x/y.git/git-receive-pack"), RouteKind::GitHttp);
        assert_eq!(classify("/x/y/info"), RouteKind::Page);
    }

    #[test]
    fn next_only_accepts_local_paths() {
        assert_eq!(safe_next("/alice/demo?tab=1"), Some("/alice/demo?tab=1"));
        assert_eq!(safe_next("//evil.example/x"), None);
        assert_eq!(safe_next("https://evil.example"), None);
        assert_eq!(safe_next("/\\evil"), None);
        assert_eq!(safe_next("/login"), None);
        assert_eq!(safe_next("/login?next=/x"), None);
        assert_eq!(safe_next("/a\r\nSet-Cookie: x"), None);
    }

    #[test]
    fn credentials_only_open_their_own_doors() {
        use crate::actor::{AuthVia, JobPerms, Scopes};
        use crate::ids::*;
        let session = Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Session };
        let basic = Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Basic };
        let token = Actor::Token { user_id: UserId(1), token_id: TokenId(1), scopes: Scopes::new(["repo"]) };
        let job = Actor::Job { job_id: JobId(1), repo_id: RepoId(1), perms: JobPerms::default_token(false) };
        let runner = Actor::Runner { runner_id: RunnerId(1) };
        assert!(actor_accepted(&session, RouteKind::Page));
        assert!(!actor_accepted(&session, RouteKind::GitHttp));
        assert!(!actor_accepted(&session, RouteKind::Registry));
        assert!(!actor_accepted(&basic, RouteKind::Page));
        assert!(actor_accepted(&basic, RouteKind::GitHttp));
        assert!(!actor_accepted(&token, RouteKind::Page));
        assert!(actor_accepted(&token, RouteKind::Api));
        assert!(!actor_accepted(&token, RouteKind::Runner));
        assert!(!actor_accepted(&job, RouteKind::Page));
        assert!(actor_accepted(&job, RouteKind::Runner));
        assert!(!actor_accepted(&runner, RouteKind::Api));
        assert!(!actor_accepted(&runner, RouteKind::Page));
        assert!(actor_accepted(&runner, RouteKind::Runner));
        assert!(!actor_accepted(&Actor::Anonymous, RouteKind::Page));
        assert!(!actor_accepted(&Actor::System, RouteKind::Page));
    }

    #[test]
    fn cookie_names_follow_scheme() {
        assert_eq!(session_cookie_name(true), "__Host-ch_session");
        assert_eq!(session_cookie_name(false), "ch_session");
        assert_eq!(csrf_cookie_name(true), "__Host-ch_csrf");
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(encode_query_value("/a b?c=d&e"), "/a%20b%3Fc%3Dd%26e");
    }

    #[test]
    fn router_pattern_comparison() {
        assert!(is_public_pattern("GET", "/invite/{token}"));
        assert!(is_public_pattern("GET", "/invite/{t}"));
        assert!(is_public_pattern("GET", "/static/{*path}"));
        assert!(!is_public_pattern("GET", "/"));
        assert!(!is_public_pattern("DELETE", "/login"));
    }

    proptest! {
        // Whatever the path, a path that is not in the table is never public.
        #[test]
        fn random_paths_under_api_are_never_public(tail in "[a-zA-Z0-9/._%-]{0,40}") {
            let p = format!("/api/v3/{tail}");
            prop_assert!(!is_public(&Method::GET, &p));
            let q = format!("/v2/{tail}");
            prop_assert!(!is_public(&Method::GET, &q));
        }
    }
}
