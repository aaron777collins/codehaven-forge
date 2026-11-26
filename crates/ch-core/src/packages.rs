//! Packages pages (ch-2fc): the contract between the registry (`ch-registry`) and the pages that
//! list container images and NuGet packages (`ch-web`).
//!
//! Every method decides access itself, with the rule a pull uses, and answers `NotFound` for what
//! the caller may not read. A list never holds a package its reader could not pull, and never
//! says how many it left out.

use crate::actor::Actor;
use crate::error::Result;
use crate::ids::RepoId;
use crate::model::ClientInfo;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// Held in `AppState::ext` so the web layer can find the service without a new field.
pub struct PackagesHandle(pub Arc<dyn Packages>);

#[derive(Debug, Clone)]
pub struct PackageSummary {
    pub owner: String,
    /// `container` or `nuget`.
    pub kind: String,
    pub name: String,
    /// `owner/repo` it follows, when linked.
    pub repo: Option<String>,
    pub orphaned: bool,
    /// Why it has no repository: `unlinked`, `repo_deleted` or `repo_moved`. None for a package that
    /// was never linked, and for one orphaned before the reason was kept.
    pub orphan_reason: Option<String>,
    pub latest: Option<String>,
    pub versions: i64,
    pub size: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct PackageVersion {
    /// A tag, or a NuGet version.
    pub label: String,
    /// `sha256:...` for an image; empty for NuGet.
    pub digest: String,
    pub size: i64,
    pub pushed_at: DateTime<Utc>,
    /// `linux/amd64` and so on, from an image index.
    pub platforms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PackageDetail {
    pub summary: PackageSummary,
    pub versions: Vec<PackageVersion>,
    /// The caller may link, unlink and delete: the owner, or an owner of the owning organization.
    pub can_admin: bool,
    /// Repositories of the same owner the caller can write, which the package may be linked to.
    pub linkable: Vec<String>,
}

/// What linking would change, shown before it is done.
#[derive(Debug, Clone)]
pub struct LinkPreview {
    pub repo: String,
    /// `private`, `internal` or `public`.
    pub tier: String,
    /// Who may pull the package after the link, in words.
    pub audience: String,
    /// True when more people could pull it than can now.
    pub widens: bool,
}

#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait Packages: Send + Sync {
    /// Packages the caller could pull, for one owner or one repository, or all of them.
    async fn list(&self, actor: &Actor, owner: Option<&str>, repo: Option<RepoId>) -> Result<Vec<PackageSummary>>;
    async fn detail(&self, actor: &Actor, owner: &str, kind: &str, name: &str) -> Result<PackageDetail>;
    /// Who would gain access if the package followed `repo` (`owner/name`). Changes nothing.
    async fn preview_link(&self, actor: &Actor, owner: &str, kind: &str, name: &str, repo: &str) -> Result<LinkPreview>;
    /// Links the package to the repository. `confirmed` must be true: the caller has seen the preview.
    async fn link(
        &self,
        actor: &Actor,
        owner: &str,
        kind: &str,
        name: &str,
        repo: &str,
        confirmed: bool,
        client: &ClientInfo,
    ) -> Result<()>;
    /// Takes the link away. The package is then the owner's alone (an organization's owners): unlinking never widens it.
    async fn unlink(&self, actor: &Actor, owner: &str, kind: &str, name: &str, client: &ClientInfo) -> Result<()>;
    /// Deletes one tag or version. `confirm_name` must equal the package name.
    async fn delete_version(
        &self,
        actor: &Actor,
        owner: &str,
        kind: &str,
        name: &str,
        label: &str,
        confirm_name: &str,
        client: &ClientInfo,
    ) -> Result<()>;
    /// Deletes the package and every version. `confirm_name` must equal the package name.
    async fn delete_package(
        &self,
        actor: &Actor,
        owner: &str,
        kind: &str,
        name: &str,
        confirm_name: &str,
        client: &ClientInfo,
    ) -> Result<()>;
}
