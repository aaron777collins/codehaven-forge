//! The fork lock (ch-4pn.16): a personal private fork whose owner can no longer see one of the
//! private repositories it was forked from carries that repository's code, which its owner was
//! shut out of. The owner keeps the fork and can read and push it, but nothing may act on it for
//! anyone else: no workflow starts, no webhook is sent, no deploy key works.

use ch_core::services::Authz;
use ch_core::{Actor, AuthVia, Permission, RepoId, Result, UserId};
use sqlx::PgPool;

/// True when `repo` is a personal private fork and its owner cannot View a private ancestor.
pub async fn is_locked(pool: &PgPool, authz: &dyn Authz, repo: RepoId) -> Result<bool> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "WITH RECURSIVE chain (id, parent, depth) AS (
             SELECT r.id, r.fork_parent_id, 0 FROM repositories r JOIN users o ON o.id = r.owner_id
              WHERE r.id = $1 AND r.fork_parent_id IS NOT NULL AND r.access_tier = 'private' AND o.kind = 'user'
             UNION ALL
             SELECT p.id, p.fork_parent_id, c.depth + 1 FROM repositories p JOIN chain c ON p.id = c.parent WHERE c.depth < 64)
         SELECT c.id, (SELECT owner_id FROM repositories WHERE id = $1) FROM chain c JOIN repositories a ON a.id = c.id
          WHERE c.depth > 0 AND a.access_tier = 'private'",
    )
    .bind(repo.0)
    .fetch_all(pool)
    .await?;
    for (ancestor, owner) in rows {
        let who = Actor::User { id: UserId(owner), is_admin: false, has_2fa: false, via: AuthVia::Session };
        if authz.repo_permission(&who, RepoId(ancestor)).await? < Permission::View {
            return Ok(true);
        }
    }
    Ok(false)
}
