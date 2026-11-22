//! Housekeeping: expired sessions, spent links, old lockout rows, old invites and the audit
//! log retention (one year, ARCHITECTURE.md section 7.7).

use ch_core::error::Result;
use ch_core::state::{AppState, Worker};
use sqlx::PgPool;
use std::time::Duration;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub sessions: u64,
    pub links: u64,
    pub lockouts: u64,
    pub invites: u64,
    pub audit: u64,
    /// Connected-app rows: old requests, codes, tokens and ended grants.
    pub apps: u64,
}

pub async fn sweep(pool: &PgPool) -> Result<Swept> {
    let sessions = sqlx::query("DELETE FROM sessions WHERE expires_at < now() OR created_at < now() - interval '30 days'")
        .execute(pool)
        .await?
        .rows_affected();
    let links =
        sqlx::query("DELETE FROM one_time_links WHERE expires_at < now() - interval '7 days' OR used_at < now() - interval '7 days'")
            .execute(pool)
            .await?
            .rows_affected();
    // Rows with no lock in force whose last failure is old: nothing left to remember.
    let lockouts = sqlx::query("DELETE FROM login_failures WHERE last_failure_at < now() - interval '2 days' AND (locked_until IS NULL OR locked_until < now() - interval '2 days')").execute(pool).await?.rows_affected();
    // Unused invites stay in the admin list for 90 days after they end.
    let invites =
        sqlx::query("DELETE FROM invites WHERE used_at IS NULL AND COALESCE(revoked_at, expires_at) < now() - interval '90 days'")
            .execute(pool)
            .await?
            .rows_affected();
    let audit = sqlx::query("DELETE FROM audit_events WHERE at < now() - interval '1 year'").execute(pool).await?.rows_affected();
    // Connected apps. Requests and codes a day after they end, a grant that never got its first
    // token after a day, access tokens a day after they expire, used refresh tokens once their
    // grant has ended, and ended grants after 90 days.
    let mut apps = 0;
    for q in [
        "DELETE FROM oauth_requests WHERE expires_at < now() - interval '1 day'",
        "DELETE FROM oauth_codes WHERE expires_at < now() - interval '1 day'",
        "DELETE FROM oauth_grants WHERE activated_at IS NULL AND created_at < now() - interval '1 day'",
        "DELETE FROM oauth_tokens WHERE kind = 'access' AND expires_at < now() - interval '1 day'",
        "DELETE FROM oauth_tokens t USING oauth_grants g WHERE t.grant_id = g.id AND t.kind = 'refresh' AND t.used_at IS NOT NULL AND (g.revoked_at IS NOT NULL OR g.expires_at < now())",
        "DELETE FROM oauth_grants WHERE COALESCE(revoked_at, expires_at) < now() - interval '90 days'",
    ] {
        apps += sqlx::query(q).execute(pool).await?.rows_affected();
    }
    Ok(Swept { sessions, links, lockouts, invites, audit, apps })
}

/// Runs the sweep every ten minutes until the token is cancelled.
pub fn worker(state: &AppState) -> Worker {
    let pool = state.db.clone();
    Worker::new("identity-sweep", move |stop| async move {
        loop {
            match sweep(&pool).await {
                Ok(s) if s != Swept::default() => tracing::info!(?s, "identity sweep"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "identity sweep failed"),
            }
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(600)) => {}
            }
        }
    })
}
