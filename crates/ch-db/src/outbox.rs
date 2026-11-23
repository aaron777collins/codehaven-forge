//! The domain event outbox (ARCHITECTURE.md section 4.4).
//!
//! Producers `emit` inside the same transaction as the change. Consumers keep a cursor in
//! `event_cursors` and process in id order, so a crash never loses an event.
//!
//! Ordering: ids come from an identity column, and two transactions can commit in the
//! opposite order to their ids. A consumer that moved its cursor past the higher id would
//! then skip the lower one for good. `emit` therefore takes a transaction-scoped advisory
//! lock before it inserts, which serialises emitters until they commit. Ids then become
//! visible in order. Only transactions that emit pay for it.

use async_trait::async_trait;
use ch_core::events::{DomainEvent, StoredEvent};
use ch_core::services::{DbTx, EventBus};
use ch_core::{Error, Result};
use sqlx::PgPool;

/// Arbitrary constant naming the emit lock.
const EMIT_LOCK: i64 = 0x434F_4445_4841_5645; // "CODEHAVE"

#[derive(Clone)]
pub struct PgEventBus {
    pool: PgPool,
}

impl PgEventBus {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventBus for PgEventBus {
    async fn emit(&self, tx: &mut DbTx<'_>, event: DomainEvent) -> Result<i64> {
        sqlx::query!("SELECT pg_advisory_xact_lock($1)", EMIT_LOCK).execute(&mut **tx).await?;
        let payload = serde_json::to_value(&event).map_err(Error::internal)?;
        let id = sqlx::query_scalar!("INSERT INTO outbox_events (kind, payload) VALUES ($1, $2) RETURNING id", event.kind(), payload)
            .fetch_one(&mut **tx)
            .await?;
        Ok(id)
    }

    async fn poll(&self, consumer: &str, limit: i64) -> Result<Vec<StoredEvent>> {
        sqlx::query!("INSERT INTO event_cursors (consumer) VALUES ($1) ON CONFLICT DO NOTHING", consumer).execute(&self.pool).await?;
        let rows = sqlx::query!(
            r#"SELECT e.id, e.created_at, e.payload
               FROM outbox_events e
               WHERE e.id > (SELECT last_id FROM event_cursors WHERE consumer = $1)
               ORDER BY e.id
               LIMIT $2"#,
            consumer,
            limit.clamp(1, 1000),
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let event: DomainEvent = serde_json::from_value(r.payload).map_err(Error::internal)?;
            out.push(StoredEvent { id: r.id, at: r.created_at, event });
        }
        Ok(out)
    }

    async fn ack(&self, consumer: &str, id: i64) -> Result<()> {
        sqlx::query!(
            "INSERT INTO event_cursors (consumer, last_id) VALUES ($1, $2)
             ON CONFLICT (consumer) DO UPDATE SET last_id = GREATEST(event_cursors.last_id, EXCLUDED.last_id), updated_at = now()",
            consumer,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::{PullId, RepoId, UserId};

    fn ev(n: i64) -> DomainEvent {
        DomainEvent::PrOpened {
            repo_id: RepoId(1),
            pull_id: PullId(n),
            number: n,
            actor: UserId(1),
            app: None,
            head_sha: None,
            pusher: Some(UserId(1)),
        }
    }

    async fn emit_one(bus: &PgEventBus, pool: &PgPool, n: i64) -> i64 {
        let mut tx = pool.begin().await.unwrap();
        let id = bus.emit(&mut tx, ev(n)).await.unwrap();
        tx.commit().await.unwrap();
        id
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn consumer_sees_events_in_order_and_resumes_from_cursor(pool: PgPool) {
        let bus = PgEventBus::new(pool.clone());
        for n in 1..=5 {
            emit_one(&bus, &pool, n).await;
        }
        let first = bus.poll("c1", 3).await.unwrap();
        assert_eq!(first.len(), 3);
        assert!(first.windows(2).all(|w| w[0].id < w[1].id));
        // Not acked: polling again returns the same events.
        assert_eq!(bus.poll("c1", 3).await.unwrap()[0].id, first[0].id);
        bus.ack("c1", first[2].id).await.unwrap();
        let rest = bus.poll("c1", 10).await.unwrap();
        assert_eq!(rest.len(), 2);
        bus.ack("c1", rest[1].id).await.unwrap();
        assert!(bus.poll("c1", 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn consumers_have_independent_cursors(pool: PgPool) {
        let bus = PgEventBus::new(pool.clone());
        emit_one(&bus, &pool, 1).await;
        emit_one(&bus, &pool, 2).await;
        let a = bus.poll("a", 10).await.unwrap();
        bus.ack("a", a[1].id).await.unwrap();
        assert_eq!(bus.poll("b", 10).await.unwrap().len(), 2);
        assert!(bus.poll("a", 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn ack_never_moves_backwards(pool: PgPool) {
        let bus = PgEventBus::new(pool.clone());
        for n in 1..=3 {
            emit_one(&bus, &pool, n).await;
        }
        let all = bus.poll("c", 10).await.unwrap();
        bus.ack("c", all[2].id).await.unwrap();
        bus.ack("c", all[0].id).await.unwrap();
        assert!(bus.poll("c", 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rolled_back_emit_is_never_seen(pool: PgPool) {
        let bus = PgEventBus::new(pool.clone());
        let mut tx = pool.begin().await.unwrap();
        bus.emit(&mut tx, ev(1)).await.unwrap();
        tx.rollback().await.unwrap();
        emit_one(&bus, &pool, 2).await;
        let got = bus.poll("c", 10).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event, ev(2));
    }

    // Two open transactions emit concurrently. The second must wait for the first to
    // commit, so a consumer can never observe id 2 without id 1.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn concurrent_emitters_commit_in_id_order(pool: PgPool) {
        let bus = PgEventBus::new(pool.clone());
        let mut tx1 = pool.begin().await.unwrap();
        let id1 = bus.emit(&mut tx1, ev(1)).await.unwrap();

        let bus2 = bus.clone();
        let pool2 = pool.clone();
        let second = tokio::spawn(async move {
            let mut tx2 = pool2.begin().await.unwrap();
            let id2 = bus2.emit(&mut tx2, ev(2)).await.unwrap();
            tx2.commit().await.unwrap();
            id2
        });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!second.is_finished(), "second emitter must wait for the first to commit");
        assert!(bus.poll("c", 10).await.unwrap().is_empty());

        tx1.commit().await.unwrap();
        let id2 = second.await.unwrap();
        assert!(id2 > id1);
        let got = bus.poll("c", 10).await.unwrap();
        assert_eq!(got.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id1, id2]);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn many_parallel_emitters_lose_nothing(pool: PgPool) {
        let bus = PgEventBus::new(pool.clone());
        let mut handles = vec![];
        for n in 0..30 {
            let bus = bus.clone();
            let pool = pool.clone();
            handles.push(tokio::spawn(async move { emit_one(&bus, &pool, n).await }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let got = bus.poll("c", 100).await.unwrap();
        assert_eq!(got.len(), 30);
        assert!(got.windows(2).all(|w| w[0].id < w[1].id));
    }
}
