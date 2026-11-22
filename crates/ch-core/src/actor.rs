//! Who is calling.

use crate::ids::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// How a user proved who they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthVia {
    Session,
    Basic,
    Ssh,
}

/// What a personal access token may do. A token never carries site admin powers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Scopes(pub BTreeSet<String>);

impl Scopes {
    pub const ALL: &'static [&'static str] = &[
        "repo",
        "repo:read",
        "user",
        "read:org",
        "admin:org",
        "write:packages",
        "read:packages",
        "workflow",
        "admin:repo_hook",
        "notifications",
    ];

    pub fn new<I: IntoIterator<Item = S>, S: Into<String>>(it: I) -> Self {
        Scopes(it.into_iter().map(Into::into).collect())
    }
    pub fn has(&self, scope: &str) -> bool {
        self.0.contains(scope)
    }
    pub fn is_known(scope: &str) -> bool {
        Self::ALL.contains(&scope)
    }
    /// The most a token with these scopes may do on a repository.
    pub fn repo_cap(&self) -> Permission {
        if self.has("repo") {
            Permission::Admin
        } else if self.has("repo:read") {
            Permission::Read
        } else {
            Permission::None
        }
    }
}

/// What a CI job token may do, always on its own repo only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPerms {
    /// Read repository contents.
    pub contents_read: bool,
    /// Write statuses, checks, artifacts and caches. False for fork pull request jobs.
    pub write_checks: bool,
}

/// A set of repositories a connected app was given: every repository the person can reach, now
/// and later, or exactly these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoSet {
    All,
    Only(BTreeSet<RepoId>),
}

impl RepoSet {
    pub fn none() -> Self {
        RepoSet::Only(BTreeSet::new())
    }
    pub fn has(&self, repo: RepoId) -> bool {
        match self {
            RepoSet::All => true,
            RepoSet::Only(s) => s.contains(&repo),
        }
    }
    pub fn is_all(&self) -> bool {
        matches!(self, RepoSet::All)
    }
    pub fn is_empty(&self) -> bool {
        matches!(self, RepoSet::Only(s) if s.is_empty())
    }
}

/// What a person let a connected app do. `workflows` is a subset of `write`, and `write` a
/// subset of `read`. The database enforces the same rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppGrant {
    pub read: RepoSet,
    pub write: RepoSet,
    pub workflows: RepoSet,
}

/// Where a change came from when a connected app made it. Carried on push and pull request
/// events so a run started by an app can wait for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppOrigin {
    pub grant_id: GrantId,
    /// False when the app had no workflows permission on the repository.
    pub ci_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Actor {
    /// No valid credential. Services grant it nothing, ever.
    Anonymous,
    User {
        id: UserId,
        is_admin: bool,
        has_2fa: bool,
        via: AuthVia,
    },
    /// A personal access token.
    Token {
        user_id: UserId,
        token_id: TokenId,
        scopes: Scopes,
    },
    /// A CI job token, valid until the job ends.
    Job {
        job_id: JobId,
        repo_id: RepoId,
        perms: JobPerms,
    },
    Runner {
        runner_id: RunnerId,
    },
    /// A connected app (an MCP client) acting for a person, within what they granted. It has
    /// no `user_id()` on purpose: every check that asks "is this the person" fails closed.
    App {
        user_id: UserId,
        grant_id: GrantId,
        grant: AppGrant,
    },
    /// Hooks, schedulers, migrations and the `admin` CLI.
    System,
}

impl Actor {
    pub fn is_anonymous(&self) -> bool {
        matches!(self, Actor::Anonymous)
    }
    pub fn is_system(&self) -> bool {
        matches!(self, Actor::System)
    }
    /// The account behind this actor, when there is one.
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            Actor::User { id, .. } => Some(*id),
            Actor::Token { user_id, .. } => Some(*user_id),
            _ => None,
        }
    }
    /// The person whose work this is, for recording who made a change and for listings that
    /// are already narrowed to what the actor may see. Unlike `user_id` it includes a
    /// connected app. Never use it for a self-service, owner, org or account check.
    pub fn author_id(&self) -> Option<UserId> {
        match self {
            Actor::App { user_id, .. } => Some(*user_id),
            other => other.user_id(),
        }
    }
    /// Where a change came from when a connected app made it: which grant, and whether it may
    /// start CI on this repository. `None` for everyone else.
    pub fn app_origin(&self, repo: RepoId) -> Option<AppOrigin> {
        match self {
            Actor::App { grant_id, grant, .. } => Some(AppOrigin { grant_id: *grant_id, ci_allowed: grant.workflows.has(repo) }),
            _ => None,
        }
    }
    pub fn is_app(&self) -> bool {
        matches!(self, Actor::App { .. })
    }
    /// Refuses a connected app. Called at the top of a service method that an app must never
    /// reach however much access it was given, so a tool bug cannot make one reachable.
    pub fn deny_app(&self, what: &str) -> crate::error::Result<()> {
        if self.is_app() { Err(crate::error::Error::forbidden(format!("A connected app cannot {what}."))) } else { Ok(()) }
    }
    /// Site admin power. Only a signed-in session with two-factor on counts, and `System`.
    /// Tokens never carry it (ARCHITECTURE.md section 7.1).
    pub fn is_site_admin(&self) -> bool {
        match self {
            Actor::User { is_admin, has_2fa, via, .. } => *is_admin && *has_2fa && *via == AuthVia::Session,
            Actor::System => true,
            _ => false,
        }
    }
    /// The same as `is_site_admin` but without requiring two-factor. Used only to say
    /// "Turn on two-factor first" instead of a flat refusal.
    pub fn is_admin_without_2fa(&self) -> bool {
        matches!(self, Actor::User { is_admin: true, has_2fa: false, via: AuthVia::Session, .. })
    }
    /// A label for the audit log.
    pub fn audit_label(&self) -> String {
        match self {
            Actor::Anonymous => "anonymous".into(),
            Actor::User { id, .. } => format!("user:{id}"),
            Actor::Token { user_id, token_id, .. } => format!("token:{token_id}/user:{user_id}"),
            Actor::Job { job_id, .. } => format!("job:{job_id}"),
            Actor::Runner { runner_id } => format!("runner:{runner_id}"),
            Actor::App { user_id, grant_id, .. } => format!("app:{grant_id}/user:{user_id}"),
            Actor::System => "System (cli)".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    #[default]
    None,
    Read,
    Triage,
    Write,
    Maintain,
    Admin,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::None => "none",
            Permission::Read => "read",
            Permission::Triage => "triage",
            Permission::Write => "write",
            Permission::Maintain => "maintain",
            Permission::Admin => "admin",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Permission::None,
            "read" | "pull" => Permission::Read,
            "triage" => Permission::Triage,
            "write" | "push" => Permission::Write,
            "maintain" => Permission::Maintain,
            "admin" => Permission::Admin,
            _ => return None,
        })
    }
    pub fn allows(self, needed: Permission) -> bool {
        self >= needed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(is_admin: bool, has_2fa: bool, via: AuthVia) -> Actor {
        Actor::User { id: UserId(1), is_admin, has_2fa, via }
    }

    #[test]
    fn permissions_are_ordered() {
        assert!(Permission::Admin > Permission::Maintain);
        assert!(Permission::Maintain > Permission::Write);
        assert!(Permission::Write > Permission::Triage);
        assert!(Permission::Triage > Permission::Read);
        assert!(Permission::Read > Permission::None);
        assert!(Permission::Write.allows(Permission::Read));
        assert!(!Permission::Read.allows(Permission::Write));
    }

    #[test]
    fn site_admin_needs_session_and_two_factor() {
        assert!(user(true, true, AuthVia::Session).is_site_admin());
        assert!(!user(true, false, AuthVia::Session).is_site_admin());
        assert!(user(true, false, AuthVia::Session).is_admin_without_2fa());
        assert!(!user(true, true, AuthVia::Basic).is_site_admin());
        assert!(!user(false, true, AuthVia::Session).is_site_admin());
        assert!(Actor::System.is_site_admin());
    }

    #[test]
    fn tokens_are_never_site_admin() {
        let t = Actor::Token { user_id: UserId(1), token_id: TokenId(2), scopes: Scopes::new(["repo", "admin:org"]) };
        assert!(!t.is_site_admin());
        assert_eq!(t.user_id(), Some(UserId(1)));
    }

    #[test]
    fn an_app_has_an_author_but_no_user() {
        let a = Actor::App {
            user_id: UserId(7),
            grant_id: GrantId(3),
            grant: AppGrant { read: RepoSet::All, write: RepoSet::none(), workflows: RepoSet::none() },
        };
        assert_eq!(a.user_id(), None, "every self-service check must fail closed");
        assert_eq!(a.author_id(), Some(UserId(7)));
        assert!(!a.is_site_admin());
        assert!(a.is_app());
        assert_eq!(a.audit_label(), "app:3/user:7");
        assert_eq!(user(false, false, AuthVia::Session).author_id(), Some(UserId(1)));
        assert_eq!(Actor::Anonymous.author_id(), None);
    }

    #[test]
    fn repo_sets() {
        let only = RepoSet::Only([RepoId(1), RepoId(2)].into_iter().collect());
        assert!(only.has(RepoId(2)) && !only.has(RepoId(3)));
        assert!(RepoSet::All.has(RepoId(99)));
        assert!(RepoSet::none().is_empty() && !only.is_empty() && !RepoSet::All.is_empty());
    }

    #[test]
    fn anonymous_has_no_user() {
        assert!(Actor::Anonymous.is_anonymous());
        assert_eq!(Actor::Anonymous.user_id(), None);
        assert!(!Actor::Anonymous.is_site_admin());
    }

    #[test]
    fn scopes_cap_repo_access() {
        assert_eq!(Scopes::new(["repo"]).repo_cap(), Permission::Admin);
        assert_eq!(Scopes::new(["repo:read"]).repo_cap(), Permission::Read);
        assert_eq!(Scopes::new(["user"]).repo_cap(), Permission::None);
    }

    #[test]
    fn permission_parse_round_trip() {
        for p in [Permission::None, Permission::Read, Permission::Triage, Permission::Write, Permission::Maintain, Permission::Admin] {
            assert_eq!(Permission::parse(p.as_str()), Some(p));
        }
        assert_eq!(Permission::parse("push"), Some(Permission::Write));
        assert_eq!(Permission::parse("root"), None);
    }
}
