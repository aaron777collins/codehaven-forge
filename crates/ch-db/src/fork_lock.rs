//! The fork lock (ch-4pn.16): a private fork (a person's, or an organization's) whose owner can no longer see one of the
//! private repositories it was forked from carries that repository's code, which its owner was
//! shut out of. The owner keeps the fork and can read and push it, but nothing may act on it for
//! anyone else: no workflow starts, no webhook is sent, no deploy key works.

use ch_core::services::Authz;
use ch_core::{Actor, AuthVia, Permission, RepoId, Result, UserId};
use sqlx::PgPool;

/// True when `repo` is a private fork and one of the people who control it cannot View a private
/// ancestor. A personal fork is controlled by its owner; an organization's fork by every owner of
/// the organization (an owner can add a webhook, a deploy key or a workflow, so one owner shut
/// out of the original is enough). Suspended organization owners are left out: they can do
/// nothing anywhere.
pub async fn is_locked(pool: &PgPool, authz: &dyn Authz, repo: RepoId) -> Result<bool> {
    let ancestors: Vec<i64> = sqlx::query_scalar(
        "WITH RECURSIVE chain (id, parent, depth) AS (
             SELECT r.id, r.fork_parent_id, 0 FROM repositories r
              WHERE r.id = $1 AND r.fork_parent_id IS NOT NULL AND r.access_tier = 'private'
             UNION ALL
             SELECT p.id, p.fork_parent_id, c.depth + 1 FROM repositories p JOIN chain c ON p.id = c.parent WHERE c.depth < 64)
         SELECT c.id FROM chain c JOIN repositories a ON a.id = c.id WHERE c.depth > 0 AND a.access_tier = 'private'",
    )
    .bind(repo.0)
    .fetch_all(pool)
    .await?;
    if ancestors.is_empty() {
        return Ok(false);
    }
    let owners: Vec<i64> = sqlx::query_scalar(
        "SELECT o.id FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = $1 AND o.kind = 'user'
         UNION
         SELECT m.user_id FROM repositories r JOIN users o ON o.id = r.owner_id
           JOIN org_members m ON m.org_id = o.id AND m.role = 'owner'
           JOIN users u ON u.id = m.user_id AND u.suspended_at IS NULL
          WHERE r.id = $1 AND o.kind = 'org'",
    )
    .bind(repo.0)
    .fetch_all(pool)
    .await?;
    for owner in owners {
        let who = Actor::User { id: UserId(owner), is_admin: false, has_2fa: false, via: AuthVia::Session };
        for ancestor in &ancestors {
            if authz.repo_permission(&who, RepoId(*ancestor)).await? < Permission::View {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
