//! The packages pages' view of the registry (ch-2fc): `ch_core::packages::Packages`.
//!
//! Reading uses the rule a pull uses (`access::can_pull`), so a list never shows a package its
//! reader could not pull, and a package they may not read looks like one that is not there.
//! Changing a package (link, unlink, delete) needs package admin: the owner, or an owner of the
//! owning organization. Linking also needs write on the repository, which must belong to the same
//! owner, and an explicit confirmation of who gains access. Unlinking leaves the package to its
//! owner alone, so it never widens.

use crate::access::{Package, PullMemo, can_pull};
use async_trait::async_trait;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::ClientInfo;
use ch_core::packages::{LinkPreview, PackageDetail, PackageSummary, PackageVersion, Packages, PackagesHandle};
use ch_core::services::OrgRole;
use ch_core::state::AppState;
use ch_core::{Actor, Error, OrgId, Permission, RepoId, Result};
use chrono::{DateTime, Utc};
use std::sync::Arc;

const MAX_SCAN: i64 = 1000;
const MAX_LISTED: usize = 200;
const MAX_VERSIONS: i64 = 200;

pub struct RegistryPackages {
    state: AppState,
}

/// Puts the service where the web layer finds it. Call it once the state is built.
pub fn install(state: &AppState) {
    state.ext.insert(Arc::new(PackagesHandle(Arc::new(RegistryPackages { state: state.clone() }))));
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    owner_id: i64,
    repo_id: Option<i64>,
    owner_name: String,
    owner_kind: String,
    kind: String,
    name: String,
    orphaned: bool,
    repo_name: Option<String>,
    latest: Option<String>,
    versions: i64,
    size: i64,
    updated_at: DateTime<Utc>,
}

const SELECT: &str = "SELECT p.id, p.owner_id, p.repo_id, u.username::text AS owner_name, u.kind AS owner_kind, p.kind, p.name::text AS name, p.orphaned,
        (SELECT ru.username::text || '/' || r.name::text FROM repositories r JOIN users ru ON ru.id = r.owner_id WHERE r.id = p.repo_id) AS repo_name,
        CASE WHEN p.kind = 'container'
             THEN (SELECT t.name FROM oci_tags t WHERE t.package_id = p.id ORDER BY t.updated_at DESC, t.name LIMIT 1)
             ELSE (SELECT v.version FROM nuget_versions v WHERE v.package_id = p.id AND v.listed ORDER BY v.created_at DESC LIMIT 1) END AS latest,
        CASE WHEN p.kind = 'container'
             THEN (SELECT count(*) FROM oci_tags t WHERE t.package_id = p.id)
             ELSE (SELECT count(*) FROM nuget_versions v WHERE v.package_id = p.id) END AS versions,
        CASE WHEN p.kind = 'container'
             THEN COALESCE((SELECT sum(b.size) FROM oci_blobs b WHERE b.package_id = p.id), 0)
             ELSE COALESCE((SELECT sum(v.size) FROM nuget_versions v WHERE v.package_id = p.id), 0) END::bigint AS size,
        GREATEST(p.created_at,
                 COALESCE((SELECT max(t.updated_at) FROM oci_tags t WHERE t.package_id = p.id), p.created_at),
                 COALESCE((SELECT max(v.created_at) FROM nuget_versions v WHERE v.package_id = p.id), p.created_at)) AS updated_at
   FROM packages p JOIN users u ON u.id = p.owner_id";

impl Row {
    fn package(&self) -> Package {
        Package {
            id: self.id,
            owner_id: self.owner_id,
            repo_id: self.repo_id,
            owner_name: self.owner_name.clone(),
            name: self.name.clone(),
            orphaned: self.orphaned,
        }
    }
    fn summary(&self) -> PackageSummary {
        PackageSummary {
            owner: self.owner_name.clone(),
            kind: self.kind.clone(),
            name: self.name.clone(),
            repo: self.repo_name.clone(),
            orphaned: self.orphaned,
            latest: self.latest.clone(),
            versions: self.versions,
            size: self.size,
            updated_at: self.updated_at,
        }
    }
}

fn valid_kind(kind: &str) -> bool {
    matches!(kind, "container" | "nuget")
}

impl RegistryPackages {
    async fn row(&self, owner: &str, kind: &str, name: &str) -> Result<Row> {
        if !valid_kind(kind) {
            return Err(Error::NotFound);
        }
        sqlx::query_as::<_, Row>(&format!("{SELECT} WHERE u.username = $1::citext AND p.kind = $2 AND p.name = $3::citext"))
            .bind(owner)
            .bind(kind)
            .bind(name)
            .fetch_optional(&self.state.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// The row, for a caller who may pull it. Anyone else gets `NotFound`, as if it did not exist.
    async fn readable(&self, actor: &Actor, owner: &str, kind: &str, name: &str) -> Result<Row> {
        let row = self.row(owner, kind, name).await?;
        let mut memo = PullMemo::default();
        let ok = can_pull(&self.state, actor, &row.package(), row.owner_kind == "org", &mut memo).await.map_err(reg)?;
        if ok { Ok(row) } else { Err(Error::NotFound) }
    }

    async fn is_admin(&self, actor: &Actor, row: &Row) -> Result<bool> {
        if actor.user_id().map(|u| u.0) == Some(row.owner_id) {
            return Ok(true);
        }
        if row.owner_kind == "org" {
            return Ok(self.state.authz.org_role(actor, OrgId(row.owner_id)).await? == Some(OrgRole::Owner));
        }
        Ok(false)
    }

    /// A package the caller administers. Someone who may only read it is told they may not; anyone
    /// else is told it is not there.
    async fn admin_of(&self, actor: &Actor, owner: &str, kind: &str, name: &str) -> Result<Row> {
        actor.deny_app("change a package")?;
        if actor.user_id().is_none() {
            return Err(Error::forbidden("Only a person can change a package."));
        }
        let row = self.readable(actor, owner, kind, name).await?;
        if self.is_admin(actor, &row).await? { Ok(row) } else { Err(Error::forbidden("Only the owner of this package can change it.")) }
    }

    async fn record(&self, actor: &Actor, client: &ClientInfo, act: &str, row: &Row, detail: serde_json::Value) {
        let ev = AuditEvent::new(actor, client, act, Outcome::Success).target("package", row.id).details(serde_json::json!({
            "package": format!("{}/{}", row.owner_name, row.name), "kind": row.kind, "detail": detail,
        }));
        if let Err(e) = self.state.audit.record_standalone(ev).await {
            tracing::warn!(error = %e, "could not record a package change");
        }
    }

    /// The repository `owner/name` a package may follow: the same owner, and the caller writes it.
    async fn link_target(&self, actor: &Actor, row: &Row, repo: &str) -> Result<(i64, String)> {
        let bad = || Error::invalid("Package", "repository", "Choose one of your repositories under the same owner, where you can write.");
        let (o, n) = repo.split_once('/').ok_or_else(bad)?;
        if !o.eq_ignore_ascii_case(&row.owner_name) {
            return Err(bad());
        }
        let found: Option<(i64, String)> =
            sqlx::query_as("SELECT r.id, r.access_tier FROM repositories r WHERE r.owner_id = $1 AND r.name = $2::citext")
                .bind(row.owner_id)
                .bind(n)
                .fetch_optional(&self.state.db)
                .await?;
        let Some((id, tier)) = found else { return Err(bad()) };
        if self.state.authz.repo_permission(actor, RepoId(id)).await? < Permission::Write {
            return Err(bad());
        }
        Ok((id, tier))
    }
}

fn reg(e: crate::error::RegError) -> Error {
    Error::internal(std::io::Error::other(format!("registry access check failed: {e:?}")))
}

fn platforms_of(media_type: &str, content: &[u8]) -> Vec<String> {
    if !media_type.contains("index") && !media_type.contains("manifest.list") {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(content) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for m in v.get("manifests").and_then(|m| m.as_array()).map(Vec::as_slice).unwrap_or_default() {
        let p = m.get("platform");
        let os = p.and_then(|p| p.get("os")).and_then(|x| x.as_str()).unwrap_or("");
        let arch = p.and_then(|p| p.get("architecture")).and_then(|x| x.as_str()).unwrap_or("");
        // Attestations ride along as `unknown/unknown`.
        if os.is_empty() || arch.is_empty() || os == "unknown" {
            continue;
        }
        let variant = p.and_then(|p| p.get("variant")).and_then(|x| x.as_str()).unwrap_or("");
        let label = if variant.is_empty() { format!("{os}/{arch}") } else { format!("{os}/{arch}/{variant}") };
        if !out.contains(&label) {
            out.push(label);
        }
        if out.len() >= 12 {
            break;
        }
    }
    out
}

#[async_trait]
impl Packages for RegistryPackages {
    async fn list(&self, actor: &Actor, owner: Option<&str>, repo: Option<RepoId>) -> Result<Vec<PackageSummary>> {
        if !crate::access::may_list(actor) {
            return Ok(Vec::new());
        }
        if let Some(r) = repo {
            self.state.authz.require(actor, r, Permission::View).await?;
        }
        let rows = sqlx::query_as::<_, Row>(&format!(
            "{SELECT} WHERE ($1::text IS NULL OR u.username = $1::citext) AND ($2::bigint IS NULL OR p.repo_id = $2)
              ORDER BY updated_at DESC, p.id DESC LIMIT $3"
        ))
        .bind(owner)
        .bind(repo.map(|r| r.0))
        .bind(MAX_SCAN)
        .fetch_all(&self.state.db)
        .await?;
        let mut memo = PullMemo::default();
        let mut out = Vec::new();
        for r in rows {
            if can_pull(&self.state, actor, &r.package(), r.owner_kind == "org", &mut memo).await.map_err(reg)? {
                out.push(r.summary());
                if out.len() >= MAX_LISTED {
                    break;
                }
            }
        }
        Ok(out)
    }

    async fn detail(&self, actor: &Actor, owner: &str, kind: &str, name: &str) -> Result<PackageDetail> {
        let row = self.readable(actor, owner, kind, name).await?;
        let versions = if kind == "container" {
            let tags: Vec<(String, String, i64, DateTime<Utc>, String, Vec<u8>)> = sqlx::query_as(
                "SELECT t.name, m.digest, m.size, t.updated_at, m.media_type, m.content FROM oci_tags t JOIN oci_manifests m ON m.id = t.manifest_id
                  WHERE t.package_id = $1 ORDER BY t.updated_at DESC, t.name LIMIT $2",
            )
            .bind(row.id)
            .bind(MAX_VERSIONS)
            .fetch_all(&self.state.db)
            .await?;
            tags.into_iter()
                .map(|(label, digest, size, pushed_at, media, content)| PackageVersion {
                    label,
                    digest,
                    size,
                    pushed_at,
                    platforms: platforms_of(&media, &content),
                })
                .collect()
        } else {
            let vs: Vec<(String, i64, DateTime<Utc>)> = sqlx::query_as(
                "SELECT version, size, created_at FROM nuget_versions WHERE package_id = $1 ORDER BY created_at DESC LIMIT $2",
            )
            .bind(row.id)
            .bind(MAX_VERSIONS)
            .fetch_all(&self.state.db)
            .await?;
            vs.into_iter()
                .map(|(label, size, pushed_at)| PackageVersion { label, digest: String::new(), size, pushed_at, platforms: Vec::new() })
                .collect()
        };
        let can_admin = self.is_admin(actor, &row).await?;
        let mut linkable = Vec::new();
        if can_admin {
            let names: Vec<(i64, String)> =
                sqlx::query_as("SELECT id, name::text FROM repositories WHERE owner_id = $1 ORDER BY name LIMIT 300")
                    .bind(row.owner_id)
                    .fetch_all(&self.state.db)
                    .await?;
            for (id, n) in names {
                if self.state.authz.repo_permission(actor, RepoId(id)).await? >= Permission::Write {
                    linkable.push(format!("{}/{n}", row.owner_name));
                }
            }
        }
        Ok(PackageDetail { summary: row.summary(), versions, can_admin, linkable })
    }

    async fn preview_link(&self, actor: &Actor, owner: &str, kind: &str, name: &str, repo: &str) -> Result<LinkPreview> {
        let row = self.admin_of(actor, owner, kind, name).await?;
        let (_, tier) = self.link_target(actor, &row, repo).await?;
        let (audience, widens) = match tier.as_str() {
            "public" => (
                format!(
                    "Anyone, signed out too, while the site allows public repositories. {repo} is public, so its packages can be pulled without an account."
                ),
                true,
            ),
            "internal" => {
                (format!("Everyone with an account on this site. {repo} is internal, so every signed-in person can read it."), true)
            }
            _ => (
                format!(
                    "People who can read {repo}: its collaborators and the members of its teams and organization. People who can write {repo} can push."
                ),
                true,
            ),
        };
        Ok(LinkPreview { repo: repo.to_string(), tier, audience, widens })
    }

    async fn link(
        &self,
        actor: &Actor,
        owner: &str,
        kind: &str,
        name: &str,
        repo: &str,
        confirmed: bool,
        client: &ClientInfo,
    ) -> Result<()> {
        let row = self.admin_of(actor, owner, kind, name).await?;
        if !confirmed {
            return Err(Error::invalid("Package", "confirm", "Confirm who gains access before linking."));
        }
        let (id, tier) = self.link_target(actor, &row, repo).await?;
        sqlx::query("UPDATE packages SET repo_id = $2, orphaned = false WHERE id = $1")
            .bind(row.id)
            .bind(id)
            .execute(&self.state.db)
            .await?;
        self.record(actor, client, action::PACKAGE_LINKED, &row, serde_json::json!({"repository": repo, "tier": tier})).await;
        Ok(())
    }

    async fn unlink(&self, actor: &Actor, owner: &str, kind: &str, name: &str, client: &ClientInfo) -> Result<()> {
        let row = self.admin_of(actor, owner, kind, name).await?;
        if row.repo_id.is_none() {
            return Ok(());
        }
        // Orphaned means the owner alone (an organization's owners), never more than the
        // repository gave: an unlinked organization package would otherwise open to every member.
        sqlx::query("UPDATE packages SET repo_id = NULL, orphaned = true WHERE id = $1").bind(row.id).execute(&self.state.db).await?;
        self.record(actor, client, action::PACKAGE_UNLINKED, &row, serde_json::json!({"was": row.repo_name})).await;
        Ok(())
    }

    async fn delete_version(
        &self,
        actor: &Actor,
        owner: &str,
        kind: &str,
        name: &str,
        label: &str,
        confirm_name: &str,
        client: &ClientInfo,
    ) -> Result<()> {
        let row = self.admin_of(actor, owner, kind, name).await?;
        if !confirm_name.trim().eq_ignore_ascii_case(&row.name) {
            return Err(Error::invalid("Package", "confirm", "Type the package name to confirm."));
        }
        let mut tx = self.state.db.begin().await?;
        let gone = if kind == "container" {
            let manifest: Option<i64> =
                sqlx::query_scalar("DELETE FROM oci_tags WHERE package_id = $1 AND name = $2 RETURNING manifest_id")
                    .bind(row.id)
                    .bind(label)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some(m) = manifest {
                // Only a manifest with no tag left and that no index of the package names. A
                // child of a multi-arch index stays pullable by digest, the same rule the sweep
                // uses (gc.rs). Locking the package's manifests keeps a push of an index that
                // names it from racing this delete.
                let rows: Vec<(i64, String, Vec<u8>)> =
                    sqlx::query_as("SELECT id, digest, content FROM oci_manifests WHERE package_id = $1 FOR UPDATE")
                        .bind(row.id)
                        .fetch_all(&mut *tx)
                        .await?;
                let named = rows
                    .iter()
                    .find(|(id, ..)| *id == m)
                    .is_some_and(|(_, digest, _)| rows.iter().any(|(id, _, c)| *id != m && crate::gc::references(c).contains(digest)));
                if !named {
                    sqlx::query(
                        "DELETE FROM oci_manifests m WHERE m.id = $1 AND NOT EXISTS (SELECT 1 FROM oci_tags t WHERE t.manifest_id = m.id)",
                    )
                    .bind(m)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            manifest.is_some()
        } else {
            sqlx::query("DELETE FROM nuget_versions WHERE package_id = $1 AND version = $2")
                .bind(row.id)
                .bind(label)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0
        };
        if !gone {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        self.record(actor, client, action::PACKAGE_VERSION_DELETED, &row, serde_json::json!({"version": label})).await;
        Ok(())
    }

    async fn delete_package(
        &self,
        actor: &Actor,
        owner: &str,
        kind: &str,
        name: &str,
        confirm_name: &str,
        client: &ClientInfo,
    ) -> Result<()> {
        let row = self.admin_of(actor, owner, kind, name).await?;
        if !confirm_name.trim().eq_ignore_ascii_case(&row.name) {
            return Err(Error::invalid("Package", "confirm", "Type the package name to confirm."));
        }
        // Record first: the row is about to go.
        self.record(actor, client, action::PACKAGE_DELETED, &row, serde_json::json!({"versions": row.versions})).await;
        sqlx::query("DELETE FROM packages WHERE id = $1").bind(row.id).execute(&self.state.db).await?;
        Ok(())
    }
}
