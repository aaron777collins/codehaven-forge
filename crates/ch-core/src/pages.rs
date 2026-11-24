//! Codehaven Pages: the contract between the pages service (`ch-pages`) and the settings pages
//! (`ch-web`). The design is `docs/design/pages.md`.

use crate::actor::Actor;
use crate::error::Result;
use crate::ids::*;
use crate::model::{ClientInfo, Visibility};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// The folder a site is published from: the repository root or `docs`, as on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagesFolder {
    Root,
    Docs,
}

impl PagesFolder {
    /// The stored spelling: `""` or `"docs"`.
    pub fn as_str(self) -> &'static str {
        match self {
            PagesFolder::Root => "",
            PagesFolder::Docs => "docs",
        }
    }
    /// How people read it: `/` or `/docs`.
    pub fn label(self) -> &'static str {
        match self {
            PagesFolder::Root => "/",
            PagesFolder::Docs => "/docs",
        }
    }
    /// The stored or form spelling. Anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "/" => Some(PagesFolder::Root),
            "docs" | "/docs" => Some(PagesFolder::Docs),
            _ => None,
        }
    }
}

/// What the repository admin chooses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagesConfigInput {
    /// A branch name, without `refs/heads/`.
    pub branch: String,
    pub folder: PagesFolder,
    /// The widest audience the site may have. Never above the repository's tier.
    pub audience: Visibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployState {
    Queued,
    Building,
    Ready,
    Failed,
}

impl DeployState {
    pub fn as_str(self) -> &'static str {
        match self {
            DeployState::Queued => "queued",
            DeployState::Building => "building",
            DeployState::Ready => "ready",
            DeployState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(DeployState::Queued),
            "building" => Some(DeployState::Building),
            "ready" => Some(DeployState::Ready),
            "failed" => Some(DeployState::Failed),
            _ => None,
        }
    }
}

/// One published copy, or an attempt at one.
#[derive(Debug, Clone)]
pub struct PagesDeployment {
    pub id: i64,
    pub commit_sha: String,
    pub branch: String,
    pub folder: PagesFolder,
    /// The repository's tier when the copy was made.
    pub tier: Visibility,
    pub state: DeployState,
    /// Why a failed copy failed, in plain words.
    pub error: Option<String>,
    pub file_count: i64,
    pub total_bytes: i64,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// This copy is the one being served.
    pub live: bool,
}

/// A repository's site, as its admin sees it.
#[derive(Debug, Clone)]
pub struct PagesSite {
    pub repo_id: RepoId,
    pub branch: String,
    pub folder: PagesFolder,
    pub audience: Visibility,
    /// Where people open it: the path form, or the subdomain form in full mode.
    pub url: String,
    /// The lowest of the audience, the live copy's tier and the repository's tier now. `None`
    /// until a copy is live.
    pub serving_tier: Option<Visibility>,
    /// Newest first, at most the copies kept plus a few failed ones.
    pub deployments: Vec<PagesDeployment>,
}

/// The instance-wide switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PagesSettings {
    /// Sites also serve at `<owner>.<pages host>/<project>/`, with no sandbox.
    pub full_mode: bool,
}

#[async_trait]
pub trait Pages: Send + Sync {
    /// Pages is configured on this instance (`pages.host` is set).
    fn enabled(&self) -> bool;
    /// The site of a repository, or `None` when Pages is off for it. Needs Admin.
    async fn site(&self, actor: &Actor, repo: RepoId) -> Result<Option<PagesSite>>;
    /// Turns Pages on, or changes the branch, folder or audience, and queues a copy of the
    /// branch head. Needs Admin.
    async fn configure(&self, actor: &Actor, repo: RepoId, input: PagesConfigInput, client: &ClientInfo) -> Result<PagesSite>;
    /// Queues a copy of the branch head. Needs Admin.
    async fn deploy(&self, actor: &Actor, repo: RepoId, client: &ClientInfo) -> Result<()>;
    /// Serves an older ready copy. Needs Admin.
    async fn rollback(&self, actor: &Actor, repo: RepoId, deployment: i64, client: &ClientInfo) -> Result<()>;
    /// Turns Pages off and deletes every copy. Needs Admin.
    async fn disable(&self, actor: &Actor, repo: RepoId, client: &ClientInfo) -> Result<()>;
    /// For a signed-in viewer who may see the site now: the address on the pages host, with a
    /// fresh per-site token, for `path` inside the site. `NotFound` for everything else.
    async fn open(&self, actor: &Actor, owner: &str, project: &str, path: &str) -> Result<String>;
    async fn settings(&self) -> Result<PagesSettings>;
    /// Site admin only. Audited.
    async fn set_settings(&self, actor: &Actor, settings: PagesSettings, client: &ClientInfo) -> Result<()>;
}

/// The lowest of the three tiers a site is served at (design section 5).
pub fn serving_tier(audience: Visibility, deployed: Visibility, repo_now: Visibility) -> Visibility {
    audience.min(deployed).min(repo_now)
}

/// True when `host` is a usable pages host for an app served at `app_host`: a lowercase DNS
/// name of at least two labels, different from the app's host and not a parent of it, so a
/// page can never be same-origin with the app and the app never sits under a site's host.
pub fn valid_pages_host(host: &str, app_host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    let app = app_host.to_ascii_lowercase();
    let labels: Vec<&str> = h.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    labels.len() >= 2 && h.len() <= 253 && labels.iter().all(label_ok) && h != app && !app.ends_with(&format!(".{h}"))
}

/// True when `label` can be the first label of a full-mode host: what account names already
/// are (letters, digits and single hyphens, 1 to 39, no hyphen at either end).
pub fn valid_owner_label(label: &str) -> bool {
    let b = label.as_bytes();
    !b.is_empty()
        && b.len() <= 39
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && !label.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;
    use Visibility::*;

    #[test]
    fn serving_tier_is_the_lowest() {
        assert_eq!(serving_tier(Public, Public, Public), Public);
        assert_eq!(serving_tier(Public, Private, Public), Private, "a copy made while private stays private");
        assert_eq!(serving_tier(Public, Public, Internal), Internal, "lowering the repository lowers the site");
        assert_eq!(serving_tier(Internal, Public, Public), Internal, "the admin narrowed it");
    }

    #[test]
    fn pages_host_never_shares_or_contains_the_app_host() {
        assert!(valid_pages_host("pages.example.com", "code.example.com"));
        assert!(!valid_pages_host("code.example.com", "code.example.com"), "the app's own host");
        assert!(!valid_pages_host("CODE.example.com", "code.example.com"));
        assert!(!valid_pages_host("example.com", "code.example.com"), "a parent of the app");
        assert!(!valid_pages_host("localhost", "code.example.com"), "one label");
        assert!(!valid_pages_host("pages.example.com:8443", "code.example.com"), "a port");
        assert!(!valid_pages_host("pa_ges.example.com", "code.example.com"));
        assert!(!valid_pages_host("-pages.example.com", "code.example.com"));
        assert!(!valid_pages_host("pages..example.com", "code.example.com"));
        assert!(valid_pages_host("pages.code.example.com", "code.example.com"), "a child of the app is its own origin");
    }

    #[test]
    fn owner_labels() {
        assert!(valid_owner_label("alice"));
        assert!(valid_owner_label("a-b-1"));
        assert!(!valid_owner_label(""));
        assert!(!valid_owner_label("-a"));
        assert!(!valid_owner_label("a-"));
        assert!(!valid_owner_label("xn--abc"));
        assert!(!valid_owner_label("a_b"));
        assert!(!valid_owner_label(&"a".repeat(40)));
    }

    #[test]
    fn folders() {
        assert_eq!(PagesFolder::parse("/"), Some(PagesFolder::Root));
        assert_eq!(PagesFolder::parse("/docs"), Some(PagesFolder::Docs));
        assert_eq!(PagesFolder::parse("docs/"), None);
        assert_eq!(PagesFolder::parse("../x"), None);
    }
}
