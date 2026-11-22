//! Housekeeping (ARCHITECTURE.md section 5.1): `git gc --auto` after pushes, a full `gc`
//! weekly per active repository, repository size, and emptying the trash.

use crate::core::GitCore;
use ch_core::RepoId;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Repositories pushed to since the last pass.
#[derive(Default)]
pub struct Pushed(Mutex<HashSet<i64>>);

impl Pushed {
    pub fn note(&self, repo: RepoId) {
        if let Ok(mut s) = self.0.lock() {
            s.insert(repo.0);
        }
    }
    pub fn take(&self) -> Vec<RepoId> {
        self.0.lock().map(|mut s| s.drain().map(RepoId).collect()).unwrap_or_default()
    }
}

impl GitCore {
    pub(crate) fn note_push(&self, repo: RepoId) {
        self.pushed.note(repo);
    }

    /// `gc --auto`: git decides whether there is enough loose data to bother. The repository
    /// config disables auto gc so that a push never pays for it, so the threshold is set here.
    pub async fn gc_auto(&self, repo: RepoId) {
        if !self.store.exists(repo) {
            return;
        }
        let cmd = self.git(repo).args(["-c", "gc.auto=6700", "gc", "--auto", "--quiet"]).timeout(Duration::from_secs(900));
        if let Err(e) = self.exec(cmd).await {
            tracing::warn!(repo = repo.0, error = %e, "git gc --auto failed");
        }
    }

    /// A full gc, then the repository size for the UI.
    pub async fn gc_full(&self, repo: RepoId) {
        if !self.store.exists(repo) {
            return;
        }
        let cmd = self.git(repo).args(["-c", "gc.auto=6700", "gc", "--quiet"]).timeout(Duration::from_secs(1800));
        if let Err(e) = self.exec(cmd).await {
            tracing::warn!(repo = repo.0, error = %e, "git gc failed");
            return;
        }
        self.refresh_size(repo).await;
        let _ = sqlx::query("UPDATE repositories SET flags = flags || jsonb_build_object('last_gc', now()) WHERE id = $1")
            .bind(repo.0)
            .execute(&self.db)
            .await;
    }

    /// Stores `size_kb` from `git count-objects -v`.
    pub async fn refresh_size(&self, repo: RepoId) {
        let Ok(out) = self.exec(self.git(repo).args(["count-objects", "-v"])).await else { return };
        let text = out.stdout_str();
        let field = |name: &str| -> i64 { text.lines().find_map(|l| l.strip_prefix(name)?.trim().parse::<i64>().ok()).unwrap_or(0) };
        let kb = field("size:") + field("size-pack:") + field("size-garbage:");
        let _ = sqlx::query("UPDATE repositories SET size_kb = $2 WHERE id = $1").bind(repo.0).bind(kb).execute(&self.db).await;
    }

    /// One pass: gc what was pushed to, and fully gc active repositories not gc'd for a week.
    pub async fn housekeeping_pass(&self) {
        for repo in self.pushed.take() {
            self.gc_auto(repo).await;
            self.refresh_size(repo).await;
        }
        let due: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM repositories WHERE pushed_at > now() - interval '7 days' \
             AND COALESCE((flags->>'last_gc')::timestamptz, 'epoch') < now() - interval '7 days' ORDER BY id LIMIT 20",
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for (id,) in due {
            self.gc_full(RepoId(id)).await;
        }
        let gone = self.store.empty_trash();
        if gone > 0 {
            tracing::info!(removed = gone, "emptied the repository trash");
        }
    }
}

/// Runs a pass every five minutes until cancelled, and once at start for the trash.
pub async fn run(core: Arc<GitCore>, cancel: CancellationToken) {
    let mut first = true;
    loop {
        let wait = if first { Duration::from_secs(20) } else { Duration::from_secs(300) };
        first = false;
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        core.housekeeping_pass().await;
    }
}
