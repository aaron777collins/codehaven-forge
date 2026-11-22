//! The audit log. `record` writes in the caller's transaction so the record exists exactly
//! when the change does. Only names and ids go into `details`, never a secret.

use async_trait::async_trait;
use ch_core::audit::{AuditEvent, AuditFilter, Outcome, StoredAudit};
use ch_core::services::{Audit, DbTx};
use ch_core::{Actor, AuditId, Error, Result};
use ipnetwork::IpNetwork;
use sqlx::PgPool;

#[derive(Clone)]
pub struct PgAudit {
    pool: PgPool,
}

impl PgAudit {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn outcome_str(o: Outcome) -> &'static str {
    o.as_str()
}

#[async_trait]
impl Audit for PgAudit {
    async fn record(&self, tx: &mut DbTx<'_>, event: AuditEvent) -> Result<()> {
        let ip = event.client_ip.map(IpNetwork::from);
        sqlx::query!(
            "INSERT INTO audit_events (actor_kind, actor_id, actor_label, client_ip, user_agent, action, target_type, target_id, outcome, details)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            event.actor_kind,
            event.actor_id,
            event.actor_label,
            ip,
            event.user_agent,
            event.action,
            event.target_type,
            event.target_id,
            outcome_str(event.outcome),
            event.details,
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn record_standalone(&self, event: AuditEvent) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        self.record(&mut tx, event).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn query(&self, actor: &Actor, filter: AuditFilter) -> Result<Vec<StoredAudit>> {
        if !actor.is_site_admin() {
            return Err(if actor.is_anonymous() { Error::NotFound } else { Error::forbidden("Site admin with two-factor turned on only") });
        }
        let limit = filter.limit.unwrap_or(100).clamp(1, 1000);
        let ip = filter.client_ip.map(IpNetwork::from);
        let rows = sqlx::query!(
            r#"SELECT id, at, actor_kind, actor_id, actor_label, client_ip, user_agent, action, target_type, target_id, outcome, details
               FROM audit_events
               WHERE ($1::bigint IS NULL OR actor_id = $1)
                 AND ($2::text IS NULL OR action = $2)
                 AND ($3::inet IS NULL OR client_ip = $3)
                 AND ($4::timestamptz IS NULL OR at >= $4)
                 AND ($5::bigint IS NULL OR id < $5)
               ORDER BY id DESC
               LIMIT $6"#,
            filter.actor_id.map(|u| u.0),
            filter.action,
            ip,
            filter.since,
            filter.before_id,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| StoredAudit {
                id: AuditId(r.id),
                at: r.at,
                event: AuditEvent {
                    actor_kind: r.actor_kind,
                    actor_id: r.actor_id,
                    actor_label: r.actor_label,
                    client_ip: r.client_ip.map(|n| n.ip()),
                    user_agent: r.user_agent,
                    action: r.action,
                    target_type: r.target_type,
                    target_id: r.target_id,
                    outcome: if r.outcome == "success" { Outcome::Success } else { Outcome::Failure },
                    details: r.details,
                },
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::UserId;
    use ch_core::actor::AuthVia;
    use ch_core::audit::action;
    use ch_core::model::ClientInfo;
    use serde_json::json;

    fn admin() -> Actor {
        Actor::User { id: UserId(1), is_admin: true, has_2fa: true, via: AuthVia::Session }
    }
    fn client(ip: &str) -> ClientInfo {
        ClientInfo { ip: ip.parse().unwrap(), user_agent: Some("test".into()) }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn record_in_transaction_and_query(pool: PgPool) {
        let audit = PgAudit::new(pool.clone());
        let mut tx = pool.begin().await.unwrap();
        audit
            .record(
                &mut tx,
                AuditEvent::new(&Actor::System, &client("10.0.0.1"), action::INVITE_CREATED, Outcome::Success)
                    .target("invite", 7)
                    .details(json!({"grants_admin": false})),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = audit.query(&admin(), AuditFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.action, "invite.created");
        assert_eq!(rows[0].event.actor_label, "System (cli)");
        assert_eq!(rows[0].event.client_ip, Some("10.0.0.1".parse().unwrap()));
        assert_eq!(rows[0].event.target_id.as_deref(), Some("7"));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rolled_back_change_leaves_no_audit_row(pool: PgPool) {
        let audit = PgAudit::new(pool.clone());
        let mut tx = pool.begin().await.unwrap();
        audit.record(&mut tx, AuditEvent::new(&Actor::System, &client("10.0.0.1"), action::USER_CREATED, Outcome::Success)).await.unwrap();
        tx.rollback().await.unwrap();
        assert!(audit.query(&admin(), AuditFilter::default()).await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn filters_by_action_and_ip(pool: PgPool) {
        let audit = PgAudit::new(pool.clone());
        for (a, ip) in [(action::SIGN_IN, "10.0.0.1"), (action::SIGN_IN_FAILED, "10.0.0.2"), (action::SIGN_IN_FAILED, "10.0.0.1")] {
            audit.record_standalone(AuditEvent::new(&Actor::Anonymous, &client(ip), a, Outcome::Failure)).await.unwrap();
        }
        let f = AuditFilter { action: Some(action::SIGN_IN_FAILED.into()), ..Default::default() };
        assert_eq!(audit.query(&admin(), f).await.unwrap().len(), 2);
        let f = AuditFilter { client_ip: Some("10.0.0.1".parse().unwrap()), ..Default::default() };
        assert_eq!(audit.query(&admin(), f).await.unwrap().len(), 2);
        // Newest first.
        let all = audit.query(&admin(), AuditFilter::default()).await.unwrap();
        assert!(all[0].id.0 > all[2].id.0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn only_site_admins_can_read(pool: PgPool) {
        let audit = PgAudit::new(pool);
        let plain = Actor::User { id: UserId(2), is_admin: false, has_2fa: true, via: AuthVia::Session };
        assert!(audit.query(&plain, AuditFilter::default()).await.is_err());
        assert!(audit.query(&Actor::Anonymous, AuditFilter::default()).await.is_err());
        let no_2fa = Actor::User { id: UserId(1), is_admin: true, has_2fa: false, via: AuthVia::Session };
        assert!(audit.query(&no_2fa, AuditFilter::default()).await.is_err());
        let token = Actor::Token { user_id: UserId(1), token_id: ch_core::TokenId(1), scopes: ch_core::Scopes::new(["repo"]) };
        assert!(audit.query(&token, AuditFilter::default()).await.is_err());
    }
}
