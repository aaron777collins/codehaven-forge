//! Who may read which Actions history (epic ch-4pn, item ch-4pn.12).
//!
//! Runs, jobs, logs, artifacts, check runs and commit statuses are read with View. Below Read,
//! only rows recorded at a tier that covers the viewer show (`ch_core::access::history_tiers`),
//! and a hidden row looks exactly like a missing one.

use crate::ctx::Ctx;
use ch_core::{Actor, Error, Permission, RepoId, Result};

/// `None`: every row. `Some(list)`: only rows whose `recorded_tier` is in the list.
pub type Tiers = Option<&'static [&'static str]>;

/// Requires View on the repository and says which recorded tiers the actor may see.
pub async fn tiers(ctx: &Ctx, actor: &Actor, repo: RepoId) -> Result<Tiers> {
    ctx.authz.require(actor, repo, Permission::View).await?;
    let p = ctx.authz.repo_permission(actor, repo).await?;
    Ok(ch_core::access::history_tiers(actor, p))
}

/// The list as a SQL bind for `($n::text[] IS NULL OR recorded_tier = ANY($n))`.
pub fn bind(t: Tiers) -> Option<Vec<String>> {
    t.map(|l| l.iter().map(|s| (*s).to_string()).collect())
}

/// `NotFound` unless the run is recorded at a tier the viewer may see. A run with no recorded
/// tier is hidden below Read.
pub async fn run_visible(ctx: &Ctx, run: i64, t: Tiers) -> Result<()> {
    if t.is_none() {
        return Ok(());
    }
    let ok: Option<bool> = sqlx::query_scalar("SELECT COALESCE(recorded_tier = ANY($2), false) FROM workflow_runs WHERE id = $1")
        .bind(run)
        .bind(bind(t))
        .fetch_optional(&ctx.db)
        .await?;
    if ok == Some(true) { Ok(()) } else { Err(Error::NotFound) }
}
