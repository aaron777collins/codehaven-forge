//! Changing who can read a repository (epic ch-4pn): `set_tier`, the store rebuild that comes
//! before any widening, and the write gate that keeps pushes out while the store is swapped.
//!
//! Git serves any object in a repository's store to anyone who may clone it, deleted history
//! included. So before a repository becomes readable to more people, its store is rebuilt from
//! its refs alone: a new bare repository fetches every ref, is checked, and replaces the old one.
//! Forks that borrowed this store's objects through `alternates` are made independent first.
//! Only after that does the tier change in the database.

use crate::core::GitCore;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::{ClientInfo, Repo, UserKind, Visibility};
use ch_core::services::OrgRole;
use ch_core::{Actor, Error, OrgId, Permission, RepoId, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a widening waits for running writes to finish, and a write waits for a rebuild.
const GATE_WAIT: Duration = Duration::from_secs(60);
/// How long the old store of a widened repository is kept, unreachable, before it is removed.
const PREPUBLISH_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

pub const MAKE_PUBLIC_IN_BROWSER: &str = "Make a repository public in the browser.";
pub const PUBLIC_OFF: &str = "Public repositories are off on this server.";
pub const FORK_CAP: &str = "A fork cannot be more visible than the repository it came from.";
pub const OWNER_ONLY: &str = "Only the owner can make this public.";

#[derive(Default)]
struct Slot {
    active: usize,
    rebuilding: bool,
}

/// Counts the writes running in each repository and lets one rebuild wait for them to finish.
/// Writes do not nest a lock, they only bump a counter, so a write inside a write never blocks.
#[derive(Default, Clone)]
pub struct WriteGate(Arc<Mutex<HashMap<i64, Slot>>>);

/// Held for the length of one write. Dropping it ends the write.
pub struct WriteTicket {
    gate: WriteGate,
    repo: i64,
}

impl Drop for WriteTicket {
    fn drop(&mut self) {
        if let Ok(mut m) = self.gate.0.lock()
            && let Some(s) = m.get_mut(&self.repo)
        {
            s.active = s.active.saturating_sub(1);
        }
    }
}

/// Held while a store is rebuilt. New writes wait until it is dropped.
pub struct RebuildTicket {
    gate: WriteGate,
    repo: i64,
}

impl Drop for RebuildTicket {
    fn drop(&mut self) {
        if let Ok(mut m) = self.gate.0.lock()
            && let Some(s) = m.get_mut(&self.repo)
        {
            s.rebuilding = false;
        }
    }
}

impl WriteGate {
    /// Starts a write, waiting while the store is being rebuilt.
    pub async fn write(&self, repo: RepoId) -> Result<WriteTicket> {
        let deadline = tokio::time::Instant::now() + GATE_WAIT;
        loop {
            {
                let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
                let s = m.entry(repo.0).or_default();
                if !s.rebuilding {
                    s.active += 1;
                    return Ok(WriteTicket { gate: self.clone(), repo: repo.0 });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::unavailable("This repository is being prepared for a visibility change. Try again in a minute."));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// One more ticket for a write that is already running (its caller holds a ticket), for the
    /// part that must finish even if the caller is dropped. It never waits: a rebuild that began
    /// meanwhile is still waiting for the caller's ticket, and now waits for this one too.
    pub(crate) fn also(&self, repo: RepoId) -> WriteTicket {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(repo.0).or_default().active += 1;
        WriteTicket { gate: self.clone(), repo: repo.0 }
    }

    /// Stops new writes and waits for running ones. Nothing has changed if this fails.
    pub async fn rebuild(&self, repo: RepoId) -> Result<RebuildTicket> {
        {
            let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let s = m.entry(repo.0).or_default();
            if s.rebuilding {
                return Err(Error::conflict("This repository is already being prepared for a visibility change."));
            }
            s.rebuilding = true;
        }
        let ticket = RebuildTicket { gate: self.clone(), repo: repo.0 };
        let deadline = tokio::time::Instant::now() + GATE_WAIT;
        loop {
            let active = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&repo.0).map(|s| s.active).unwrap_or(0);
            if active == 0 {
                return Ok(ticket);
            }
            if tokio::time::Instant::now() >= deadline {
                // Dropping the ticket lets writes in again.
                return Err(Error::unavailable("Pushes to this repository did not finish within a minute. Nothing changed. Try again."));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn io(e: std::io::Error) -> Error {
    Error::internal(anyhow::anyhow!(e))
}

/// The objects directory an `alternates` file points at, if the repository has one.
pub(crate) fn alternates_of(repo_path: &Path) -> Option<String> {
    let f = repo_path.join("objects/info/alternates");
    std::fs::read_to_string(f).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl GitCore {
    /// Where the stores of widened repositories wait out their retention.
    pub(crate) fn prepublish_dir(&self) -> PathBuf {
        self.store.root.join("_prepublish")
    }

    /// Makes a fork independent of the store it borrowed objects from: copies every object its
    /// refs need into its own pack and removes `alternates`.
    pub(crate) async fn detach_alternates(&self, id: RepoId) -> Result<()> {
        let path = self.store.repo_path(id);
        if alternates_of(&path).is_none() {
            return Ok(());
        }
        let _t = self.writes.rebuild(id).await?;
        // Without `-l`, `repack -a` includes the objects reachable from this repository's refs
        // that live in the alternate store.
        self.exec(self.git(id).args(["repack", "-a", "-d", "-q"]).timeout(Duration::from_secs(1800))).await?;
        let alt = path.join("objects/info/alternates");
        let parked = path.join("objects/info/alternates.detached");
        std::fs::rename(&alt, &parked).map_err(io)?;
        let ok =
            self.exec_any(self.git(id).args(["fsck", "--connectivity-only", "--no-progress"]).timeout(Duration::from_secs(1800))).await;
        match ok {
            Ok(o) if o.code == 0 => {
                let _ = std::fs::remove_file(&parked);
                Ok(())
            }
            other => {
                // Put it back so the fork keeps working, and say why.
                let _ = std::fs::rename(&parked, &alt);
                Err(Error::internal(anyhow::anyhow!("repository {id} is not complete without its alternates: {other:?}")))
            }
        }
    }

    /// Every fork whose `alternates` points into this repository's store.
    async fn borrowers_of(&self, id: RepoId) -> Result<Vec<RepoId>> {
        let objects = self.store.repo_path(id).join("objects");
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE (fork_root_id = $1 OR fork_parent_id = $1) AND id <> $1")
            .bind(id.0)
            .fetch_all(&self.db)
            .await?;
        Ok(ids
            .into_iter()
            .map(RepoId)
            .filter(|f| alternates_of(&self.store.repo_path(*f)).is_some_and(|a| Path::new(&a) == objects))
            .collect())
    }

    /// Rebuilds a repository's store from its refs, so it holds nothing a new reader may not
    /// have: no deleted branches, no force-pushed commits, nothing borrowed. The caller holds the
    /// rebuild ticket. On any failure the old store is still in place.
    pub(crate) async fn rebuild_store(&self, id: RepoId) -> Result<()> {
        let old = self.store.repo_path(id);
        if !self.store.exists(id) {
            return Err(Error::internal(anyhow::anyhow!("repository {id} has no files on disk")));
        }
        // A fork's store holds what it borrowed. Copy it in, so the new store can fetch it.
        if alternates_of(&old).is_some() {
            self.exec(self.git(id).args(["repack", "-a", "-d", "-q"]).timeout(Duration::from_secs(1800))).await?;
        }
        let stamp = chrono::Utc::now().timestamp_millis();
        let fresh = old.with_file_name(format!("{}.rebuild-{stamp}", id.0));
        let fresh_cmd = || self.store.bare_cmd().git_dir(&fresh);
        let head = self.exec_any(self.git(id).args(["symbolic-ref", "HEAD"])).await?.stdout_str().trim().to_string();
        let head = if head.starts_with("refs/heads/") { head } else { "refs/heads/main".to_string() };
        let result = async {
            self.exec(self.store.bare_cmd().args(["init", "--bare", "--quiet"]).arg(&fresh)).await?;
            for (k, v) in crate::store::repo_config(&self.store.hooks_dir) {
                self.exec(fresh_cmd().args(["config", &k, &v])).await?;
            }
            self.exec(fresh_cmd().args(["symbolic-ref", "HEAD", &head])).await?;
            // Every ref, and only what refs reach.
            self.exec(
                fresh_cmd()
                    .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--update-head-ok"])
                    .arg(&old)
                    .arg("+refs/*:refs/*")
                    .timeout(Duration::from_secs(1800)),
            )
            .await?;
            if fresh.join("objects/info/alternates").exists() {
                return Err(Error::internal(anyhow::anyhow!("the rebuilt store has alternates")));
            }
            self.exec(fresh_cmd().args(["fsck", "--connectivity-only", "--no-progress"]).timeout(Duration::from_secs(1800))).await?;
            let list = |dir: PathBuf| async move {
                self.exec(self.store.bare_cmd().git_dir(dir).args(["for-each-ref", "--format=%(objectname) %(refname)"]))
                    .await
                    .map(|o| o.stdout_str())
            };
            if list(old.clone()).await? != list(fresh.clone()).await? {
                return Err(Error::internal(anyhow::anyhow!("the rebuilt store does not have the same refs")));
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&fresh);
            return Err(e);
        }
        // The swap: old out of the way (never into the trash, which is emptied every few
        // minutes), new into place. Startup puts the old one back if only the first rename ran.
        std::fs::create_dir_all(self.prepublish_dir()).map_err(io)?;
        let parked = self.prepublish_dir().join(format!("{}-{stamp}.git", id.0));
        std::fs::rename(&old, &parked).map_err(io)?;
        if let Err(e) = std::fs::rename(&fresh, &old) {
            let _ = std::fs::rename(&parked, &old);
            let _ = std::fs::remove_dir_all(&fresh);
            return Err(io(e));
        }
        self.diff_cache.invalidate_all();
        Ok(())
    }

    /// Puts back a store whose swap was interrupted, and removes parked stores past retention.
    pub fn repair_and_sweep_prepublish(&self) {
        let dir = self.prepublish_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let now = std::time::SystemTime::now();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some((id, _)) = name.strip_suffix(".git").and_then(|n| n.split_once('-')) else { continue };
            let Ok(id) = id.parse::<i64>() else { continue };
            let live = self.store.repo_path(RepoId(id));
            if !live.exists() {
                if let Err(err) = std::fs::rename(e.path(), &live) {
                    tracing::error!(repo = id, error = %err, "could not restore a repository whose swap was interrupted");
                } else {
                    tracing::warn!(repo = id, "restored a repository whose visibility swap was interrupted");
                }
                continue;
            }
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|m| now.duration_since(m).ok());
            if old.is_some_and(|d| d > PREPUBLISH_KEEP) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        // A rebuild that never reached its swap leaves a `.rebuild-` directory beside the store.
        if let Ok(shards) = std::fs::read_dir(&self.store.root) {
            for s in shards.flatten() {
                let Ok(rd) = std::fs::read_dir(s.path()) else { continue };
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().contains(".rebuild-") {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
    }

    /// Makes every existing fork independent of the store it borrowed from. New forks never
    /// borrow. Runs once at start, one repository at a time.
    pub async fn detach_all_alternates(&self) {
        let ids: Vec<i64> =
            match sqlx::query_scalar("SELECT id FROM repositories WHERE fork_parent_id IS NOT NULL OR fork_root_id IS NOT NULL")
                .fetch_all(&self.db)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "could not list forks to detach");
                    return;
                }
            };
        for id in ids.into_iter().map(RepoId) {
            if alternates_of(&self.store.repo_path(id)).is_some()
                && let Err(e) = self.detach_alternates(id).await
            {
                tracing::warn!(repo = id.0, error = %e, "could not make a fork independent of its parent's store");
            }
        }
    }

    /// The instance switch for public repositories.
    pub(crate) async fn public_allowed(&self) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>("SELECT allow_public_repos FROM visibility_settings")
            .fetch_optional(&self.db)
            .await?
            .unwrap_or(false))
    }

    /// May this actor make the repository public? Only a browser session of the personal owner,
    /// an owner of the owning org, or a site admin with two-factor (D8).
    async fn may_publish(&self, actor: &Actor, repo: &Repo) -> Result<()> {
        let Actor::User { via: ch_core::actor::AuthVia::Session, id, .. } = actor else {
            return Err(Error::forbidden(MAKE_PUBLIC_IN_BROWSER));
        };
        if actor.is_site_admin() || *id == repo.owner_id {
            return Ok(());
        }
        let kind: Option<String> =
            sqlx::query_scalar("SELECT kind FROM users WHERE id = $1").bind(repo.owner_id.0).fetch_optional(&self.db).await?;
        if kind.as_deref().and_then(UserKind::parse) == Some(UserKind::Org)
            && self.authz.org_role(actor, OrgId(repo.owner_id.0)).await? == Some(OrgRole::Owner)
        {
            return Ok(());
        }
        Err(Error::forbidden(OWNER_ONLY))
    }

    /// The one way a repository's tier changes. Narrowing takes repo admin and any credential,
    /// archived or not, and takes effect at once. Widening rebuilds the store first. Public also
    /// needs the browser, the owner rule and the instance switch.
    pub(crate) async fn do_set_tier(&self, actor: &Actor, id: RepoId, to: Visibility, client: &ClientInfo) -> Result<Repo> {
        let repo = self.repo_row(id).await?;
        if !actor.is_system() {
            self.authz.require(actor, id, Permission::Admin).await?;
        }
        let from = repo.visibility;
        if to == from {
            return Ok(repo);
        }
        let audit = |outcome: Outcome, detail: serde_json::Value| {
            AuditEvent::new(actor, client, action::REPO_VISIBILITY, outcome).target("repository", id.0).details(detail)
        };
        if to < from {
            let mut tx = self.db.begin().await?;
            sqlx::query("UPDATE repositories SET access_tier = $2, updated_at = now() WHERE id = $1")
                .bind(id.0)
                .bind(to.as_str())
                .execute(&mut *tx)
                .await?;
            self.audit.record(&mut tx, audit(Outcome::Success, serde_json::json!({"from": from.as_str(), "to": to.as_str()}))).await?;
            tx.commit().await?;
            return self.repo_row(id).await;
        }
        // Widening.
        let refuse = |e: Error| async move {
            let msg = e.to_string();
            self.audit_standalone(
                actor,
                client,
                action::REPO_VISIBILITY,
                Outcome::Failure,
                id,
                serde_json::json!({"from": from.as_str(), "to": to.as_str(), "reason": msg}),
            )
            .await;
            Err::<Repo, Error>(e)
        };
        if repo.archived {
            return refuse(Error::forbidden("This repository is archived, so it is read only.")).await;
        }
        if to == Visibility::Public {
            if let Err(e) = self.may_publish(actor, &repo).await {
                return refuse(e).await;
            }
            if !self.public_allowed().await? {
                return refuse(Error::forbidden(PUBLIC_OFF)).await;
            }
        }
        if let Some(parent) = repo.fork_parent_id
            && let Ok(p) = self.repo_row(parent).await
            && p.visibility < to
        {
            return refuse(Error::invalid("Repository", "visibility", FORK_CAP)).await;
        }
        self.audit_standalone(
            actor,
            client,
            "repo.visibility_requested",
            Outcome::Success,
            id,
            serde_json::json!({"from": from.as_str(), "to": to.as_str()}),
        )
        .await;
        // Forks that borrow this store must not lose objects when it is replaced.
        for f in self.borrowers_of(id).await? {
            self.detach_alternates(f).await?;
        }
        let ticket = self.writes.rebuild(id).await?;
        if let Err(e) = self.rebuild_store(id).await {
            tracing::error!(repo = id.0, error = %e, "store rebuild failed; the repository keeps its tier");
            drop(ticket);
            return refuse(Error::unavailable("The repository could not be prepared. Nothing changed.")).await;
        }
        // Lock the settings row, then the parent, then the repository, in that order everywhere.
        let mut tx = self.db.begin().await?;
        let allowed: bool = sqlx::query_scalar("SELECT allow_public_repos FROM visibility_settings FOR SHARE").fetch_one(&mut *tx).await?;
        if let Some(parent) = repo.fork_parent_id {
            sqlx::query("SELECT id FROM repositories WHERE id = $1 FOR SHARE").bind(parent.0).execute(&mut *tx).await?;
        }
        let now_tier: Option<(String, bool)> = sqlx::query_as("SELECT access_tier, archived FROM repositories WHERE id = $1 FOR UPDATE")
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((now_tier, archived)) = now_tier else { return Err(Error::NotFound) };
        if now_tier != from.as_str() || archived || (to == Visibility::Public && !allowed) {
            drop(tx);
            return refuse(Error::conflict("The repository changed while it was being prepared. Nothing changed.")).await;
        }
        let res = sqlx::query(
            "UPDATE repositories SET access_tier = $2, updated_at = now(), \
             public_since = CASE WHEN $2 = 'public' THEN now() END, public_by = CASE WHEN $2 = 'public' THEN $3 END WHERE id = $1",
        )
        .bind(id.0)
        .bind(to.as_str())
        .bind(actor.user_id().map(|u| u.0))
        .execute(&mut *tx)
        .await;
        if let Err(e) = res {
            drop(tx);
            // The trigger caps a fork at its parent and refuses public while the switch is off.
            return refuse(Error::conflict(format!("The repository could not be widened: {e}"))).await;
        }
        let landed: String =
            sqlx::query_scalar("SELECT access_tier FROM repositories WHERE id = $1").bind(id.0).fetch_one(&mut *tx).await?;
        if landed != to.as_str() {
            drop(tx);
            return refuse(Error::invalid("Repository", "visibility", FORK_CAP)).await;
        }
        self.audit.record(&mut tx, audit(Outcome::Success, serde_json::json!({"from": from.as_str(), "to": to.as_str()}))).await?;
        tx.commit().await?;
        drop(ticket);
        self.repo_row(id).await
    }

    /// Who may open issues, comment and send pull requests without a grant. The account owner
    /// (or an org owner, or a site admin with two-factor) changes it, by any credential that
    /// carries repo admin.
    pub(crate) async fn do_set_contribution_policy(
        &self,
        actor: &Actor,
        id: RepoId,
        policy: ch_core::model::ContributionPolicy,
        client: &ClientInfo,
    ) -> Result<Repo> {
        let repo = self.repo_row(id).await?;
        if !actor.is_system() {
            self.authz.require(actor, id, Permission::Admin).await?;
            if actor.is_app() {
                return Err(Error::forbidden("A connected app cannot change who can contribute."));
            }
            self.must_be_account_owner(actor, &repo).await?;
        }
        if repo.contribution_policy == policy {
            return Ok(repo);
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE repositories SET contribution_policy = $2, updated_at = now() WHERE id = $1")
            .bind(id.0)
            .bind(policy.as_str())
            .execute(&mut *tx)
            .await?;
        self.audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, "repo.contribution_policy_changed", Outcome::Success)
                    .target("repository", id.0)
                    .details(serde_json::json!({"from": repo.contribution_policy.as_str(), "to": policy.as_str()})),
            )
            .await?;
        tx.commit().await?;
        self.repo_row(id).await
    }

    /// The instance switch. Off lowers every public repository to internal in one transaction
    /// (a trigger does it, so a racing publish cannot survive). On raises none.
    pub(crate) async fn do_set_public_switch(&self, actor: &Actor, on: bool, client: &ClientInfo) -> Result<u64> {
        if !actor.is_site_admin() {
            return Err(Error::NotFound);
        }
        let mut tx = self.db.begin().await?;
        let was: bool = sqlx::query_scalar("SELECT allow_public_repos FROM visibility_settings FOR UPDATE").fetch_one(&mut *tx).await?;
        if was == on {
            return Ok(0);
        }
        let lowered: Vec<i64> = if on {
            Vec::new()
        } else {
            sqlx::query_scalar("SELECT id FROM repositories WHERE access_tier = 'public' ORDER BY id").fetch_all(&mut *tx).await?
        };
        sqlx::query("UPDATE visibility_settings SET allow_public_repos = $1, updated_at = now()").bind(on).execute(&mut *tx).await?;
        self.audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, "instance.public_repos_changed", Outcome::Success)
                    .details(serde_json::json!({"on": on, "lowered": lowered})),
            )
            .await?;
        for r in &lowered {
            self.audit
                .record(
                    &mut tx,
                    AuditEvent::new(actor, client, action::REPO_VISIBILITY, Outcome::Success)
                        .target("repository", *r)
                        .details(serde_json::json!({"from": "public", "to": "internal", "reason": "public repositories turned off"})),
                )
                .await?;
        }
        tx.commit().await?;
        Ok(lowered.len() as u64)
    }

    pub(crate) async fn do_workspace_settings(&self) -> Result<ch_core::model::WorkspaceSettings> {
        let v: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = 'workspace'").fetch_optional(&self.db).await?;
        Ok(v.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    pub(crate) async fn do_set_workspace_settings(
        &self,
        actor: &Actor,
        settings: ch_core::model::WorkspaceSettings,
        client: &ClientInfo,
    ) -> Result<()> {
        if !actor.is_site_admin() {
            return Err(Error::NotFound);
        }
        let mut tx = self.db.begin().await?;
        let was: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = 'workspace' FOR UPDATE").fetch_optional(&mut *tx).await?;
        let was: ch_core::model::WorkspaceSettings = was.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let value = serde_json::to_value(settings).map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
        sqlx::query(
            "INSERT INTO instance_settings (key, value, updated_at) VALUES ('workspace', $1, now())
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(&value)
        .execute(&mut *tx)
        .await?;
        self.audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, "instance.workspace_changed", Outcome::Success)
                    .details(serde_json::json!({"from": {"editor": was.editor, "terminal": was.terminal}, "to": {"editor": settings.editor, "terminal": settings.terminal}})),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// What making a repository public would expose, and a hash of it.
    pub(crate) async fn do_publish_summary(&self, actor: &Actor, id: RepoId) -> Result<ch_core::model::PublishSummary> {
        let repo = self.repo_row(id).await?;
        if !actor.is_system() {
            self.authz.require(actor, id, Permission::Admin).await?;
        }
        let refs = if self.store.exists(id) { self.read_refs(id, None).await? } else { Vec::new() };
        let count = |p: &str| refs.iter().filter(|r| r.name.starts_with(p)).count() as i64;
        let n =
            |sql: &'static str| async move { sqlx::query_scalar::<_, i64>(sql).bind(id.0).fetch_one(&self.db).await.map_err(Error::from) };
        let issues = n("SELECT count(*) FROM issues WHERE repo_id = $1 AND NOT is_pull").await?;
        let pulls = n("SELECT count(*) FROM issues WHERE repo_id = $1 AND is_pull").await?;
        let comments = n("SELECT count(*) FROM issue_comments c JOIN issues i ON i.id = c.issue_id WHERE i.repo_id = $1").await?;
        let comments_by_others: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM issue_comments c JOIN issues i ON i.id = c.issue_id WHERE i.repo_id = $1 AND c.author_id <> $2",
        )
        .bind(id.0)
        .bind(repo.owner_id.0)
        .fetch_one(&self.db)
        .await?;
        let releases = n("SELECT count(*) FROM releases WHERE repo_id = $1 AND NOT draft").await?;
        let release_files =
            n("SELECT count(*) FROM release_assets a JOIN releases r ON r.id = a.release_id WHERE r.repo_id = $1 AND NOT r.draft").await?;
        let forks = n("SELECT count(*) FROM repositories WHERE fork_parent_id = $1").await?;
        let pulls_from_private_forks = n(
            "SELECT count(*) FROM pull_requests p JOIN repositories h ON h.id = p.head_repo_id WHERE p.repo_id = $1 AND h.access_tier = 'private'",
        )
        .await?;
        let size_kb = n("SELECT size_kb FROM repositories WHERE id = $1").await?;
        let linked_packages: Vec<String> =
            sqlx::query_scalar("SELECT name::text FROM packages WHERE repo_id = $1 ORDER BY name").bind(id.0).fetch_all(&self.db).await?;
        let mut s = ch_core::model::PublishSummary {
            branches: count("refs/heads/"),
            tags: count("refs/tags/"),
            pull_refs: count("refs/pull/"),
            size_kb,
            issues,
            pulls,
            comments,
            comments_by_others,
            releases,
            release_files,
            forks,
            pulls_from_private_forks,
            linked_packages,
            snapshot: String::new(),
        };
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        let mut lines: Vec<String> = refs.iter().map(|r| format!("{} {}", r.sha, r.name)).collect();
        lines.sort();
        for l in &lines {
            h.update(l.as_bytes());
            h.update(b"\n");
        }
        h.update(serde_json::to_vec(&s).unwrap_or_default());
        s.snapshot = hex::encode(h.finalize());
        Ok(s)
    }
}
