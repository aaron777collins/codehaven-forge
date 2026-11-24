//! `Webhooks`: hook management, the queue that turns domain events into deliveries, ping and
//! redeliver. The delivery itself is in `deliver`.

use crate::payload::{self, Ctx};
use crate::{deliver, ssrf};
use async_trait::async_trait;
use ch_api_types::webhook::event as hook_event;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::error::{Error, FieldError, Result};
use ch_core::events::DomainEvent;
use ch_core::ids::*;
use ch_core::model::*;
use ch_core::services::{Audit, Authz, OrgRole, Webhooks};
use ch_core::{Actor, Config, Permission};
use ch_db::sealing::MasterKey;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;

pub const MAX_HOOKS_PER_SCOPE: i64 = 20;
const ZEN: &[&str] = &[
    "Keep it logically awesome.",
    "Design for failure.",
    "Half measures are as bad as nothing at all.",
    "Speak like a human.",
    "Favor focus over features.",
];

pub struct WebhooksService {
    pub(crate) pool: PgPool,
    pub(crate) key: MasterKey,
    pub(crate) authz: Arc<dyn Authz>,
    pub(crate) audit: Arc<dyn Audit>,
    pub(crate) config: Arc<Config>,
}

#[derive(Debug, Clone, Copy)]
enum Scope {
    Repo(RepoId),
    Org(OrgId),
}

#[derive(sqlx::FromRow)]
pub struct HookRow {
    pub id: i64,
    pub repo_id: Option<i64>,
    pub org_id: Option<i64>,
    pub url: String,
    pub secret_sealed: Option<Vec<u8>>,
    pub content_type: String,
    pub events: Vec<String>,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) const HOOK_COLS: &str = "id, scope, repo_id, org_id, url, secret_sealed, content_type, events, active, created_at";

impl From<HookRow> for Webhook {
    fn from(r: HookRow) -> Webhook {
        Webhook {
            id: WebhookId(r.id),
            repo_id: r.repo_id.map(RepoId),
            org_id: r.org_id.map(OrgId),
            url: r.url,
            events: r.events,
            active: r.active,
        }
    }
}

impl WebhooksService {
    pub fn new(pool: PgPool, key: MasterKey, authz: Arc<dyn Authz>, audit: Arc<dyn Audit>, config: Arc<Config>) -> Self {
        Self { pool, key, authz, audit, config }
    }

    async fn authorize(&self, actor: &Actor, scope: Scope) -> Result<()> {
        // A repository the caller cannot see is missing, whatever the token's scopes (ch-hcj).
        if let Scope::Repo(r) = scope
            && !actor.is_anonymous()
        {
            self.authz.require(actor, r, Permission::View).await?;
        }
        match actor {
            Actor::Anonymous => return Err(Error::unauthorized()),
            Actor::Token { scopes, .. } => {
                let ok = match scope {
                    Scope::Repo(_) => scopes.has("admin:repo_hook") || scopes.has("repo"),
                    Scope::Org(_) => scopes.has("admin:org"),
                };
                if !ok {
                    return Err(Error::forbidden("This token needs the admin:repo_hook scope"));
                }
            }
            Actor::User { .. } | Actor::System => {}
            _ => return Err(Error::forbidden("Not available to this kind of credential")),
        }
        match scope {
            Scope::Repo(r) => self.authz.require(actor, r, Permission::Maintain).await,
            Scope::Org(o) => match self.authz.org_role(actor, o).await? {
                Some(OrgRole::Owner) => Ok(()),
                Some(OrgRole::Member) => Err(Error::forbidden("Only organization owners can manage hooks.")),
                None => Err(Error::NotFound),
            },
        }
    }

    pub(crate) async fn hook_row(&self, id: WebhookId) -> Result<HookRow> {
        sqlx::query_as(&format!("SELECT {HOOK_COLS} FROM webhooks WHERE id = $1"))
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)
    }

    async fn authorize_hook(&self, actor: &Actor, id: WebhookId) -> Result<HookRow> {
        let row = self.hook_row(id).await?;
        let scope = match (row.repo_id, row.org_id) {
            (Some(r), _) => Scope::Repo(RepoId(r)),
            (_, Some(o)) => Scope::Org(OrgId(o)),
            _ => return Err(Error::NotFound),
        };
        self.authorize(actor, scope).await?;
        Ok(row)
    }

    fn validate_events(events: &[String]) -> Result<Vec<String>> {
        if events.is_empty() {
            return Err(Error::Validation(vec![FieldError::custom("Hook", "events", "Pick at least one event.")]));
        }
        if events.iter().any(|e| e == "*") {
            return Ok(hook_event::ALL.iter().map(|s| s.to_string()).collect());
        }
        if let Some(bad) = events.iter().find(|e| !deliver::is_known_event(e)) {
            return Err(Error::Validation(vec![FieldError::custom("Hook", "events", format!("Unknown event: {bad}"))]));
        }
        let mut v = events.to_vec();
        v.sort();
        v.dedup();
        Ok(v)
    }

    fn validate_secret(secret: &Option<String>) -> Result<()> {
        if secret.as_ref().is_some_and(|s| s.len() > 255) {
            return Err(Error::Validation(vec![FieldError::custom("Hook", "secret", "A secret can be 255 characters at most.")]));
        }
        Ok(())
    }

    async fn queue_ping(&self, hook: &HookRow, actor: &Actor) -> Result<()> {
        let base = self.config.server.external_url.trim_end_matches('/').to_string();
        let sender = match actor.user_id() {
            Some(u) => Some(payload::user_json(&self.pool, &base, u.0).await?),
            None => None,
        };
        let repository = match hook.repo_id {
            Some(r) => Some(payload::repo_json(&self.pool, &base, &self.config.ssh.external, r).await?),
            None => None,
        };
        let zen = ZEN[(hook.id as usize) % ZEN.len()];
        let dto = crate::rest::hook_dto(&self.pool, &base, hook).await?;
        let body = json!({"zen": zen, "hook_id": hook.id, "hook": dto, "repository": repository, "sender": sender});
        sqlx::query(
            "INSERT INTO webhook_deliveries (webhook_id, event, action, payload, next_attempt_at) VALUES ($1, 'ping', NULL, $2, now())",
        )
        .bind(hook.id)
        .bind(body)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait]
impl Webhooks for WebhooksService {
    async fn list(&self, actor: &Actor, repo: Option<RepoId>, org: Option<OrgId>) -> Result<Vec<Webhook>> {
        let (scope, col, id) = match (repo, org) {
            (Some(r), None) => (Scope::Repo(r), "repo_id", r.0),
            (None, Some(o)) => (Scope::Org(o), "org_id", o.0),
            _ => return Err(Error::invalid("Hook", "scope", "Name a repository or an organization.")),
        };
        self.authorize(actor, scope).await?;
        let rows: Vec<HookRow> = sqlx::query_as(&format!("SELECT {HOOK_COLS} FROM webhooks WHERE {col} = $1 ORDER BY id"))
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Webhook::from).collect())
    }

    async fn create(&self, actor: &Actor, new: NewWebhook, client: &ClientInfo) -> Result<Webhook> {
        let (scope, scope_s, repo_id, org_id) = match (new.repo_id, new.org_id) {
            (Some(r), None) => (Scope::Repo(r), "repo", Some(r.0), None),
            (None, Some(o)) => (Scope::Org(o), "org", None, Some(o.0)),
            _ => return Err(Error::invalid("Hook", "scope", "Name a repository or an organization, not both.")),
        };
        self.authorize(actor, scope).await?;
        let target = ssrf::parse_target(&new.url, self.config.webhooks.allow_private)?;
        ssrf::check_saved_target(&target, self.config.webhooks.allow_private).await?;
        let events = Self::validate_events(&new.events)?;
        Self::validate_secret(&new.secret)?;
        let have: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM webhooks WHERE (repo_id IS NOT DISTINCT FROM $1) AND (org_id IS NOT DISTINCT FROM $2)",
        )
        .bind(repo_id)
        .bind(org_id)
        .fetch_one(&self.pool)
        .await?;
        if have >= MAX_HOOKS_PER_SCOPE {
            return Err(Error::conflict("That is the most hooks it can have. Delete one first."));
        }
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar("INSERT INTO webhooks (scope, repo_id, org_id, url, events, active, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id")
            .bind(scope_s)
            .bind(repo_id)
            .bind(org_id)
            .bind(new.url.trim())
            .bind(&events)
            .bind(new.active)
            .bind(actor.user_id().map(|u| u.0))
            .fetch_one(&mut *tx)
            .await?;
        if let Some(secret) = new.secret.as_deref().filter(|s| !s.is_empty()) {
            // Bound to this row, so a sealed secret copied to another hook does not open.
            let sealed = self.key.seal(secret.as_bytes(), format!("webhook:{id}").as_bytes())?;
            sqlx::query("UPDATE webhooks SET secret_sealed = $2 WHERE id = $1").bind(id).bind(sealed).execute(&mut *tx).await?;
        }
        let ev = AuditEvent::new(actor, client, action::WEBHOOK_CREATED, Outcome::Success)
            .target("webhook", id)
            .details(json!({"url": new.url, "events": events, "scope": scope_s, "has_secret": new.secret.is_some()}));
        self.audit.record(&mut tx, ev).await?;
        tx.commit().await?;
        let row = self.hook_row(WebhookId(id)).await?;
        if row.active {
            self.queue_ping(&row, actor).await?;
        }
        Ok(row.into())
    }

    async fn delete(&self, actor: &Actor, id: WebhookId, client: &ClientInfo) -> Result<()> {
        let row = self.authorize_hook(actor, id).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM webhooks WHERE id = $1").bind(id.0).execute(&mut *tx).await?;
        let ev = AuditEvent::new(actor, client, action::WEBHOOK_DELETED, Outcome::Success)
            .target("webhook", id.0)
            .details(json!({"url": row.url}));
        self.audit.record(&mut tx, ev).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn redeliver(&self, actor: &Actor, delivery_id: i64) -> Result<()> {
        let hook_id: Option<i64> = sqlx::query_scalar("SELECT webhook_id FROM webhook_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_optional(&self.pool)
            .await?;
        let hook_id = hook_id.ok_or(Error::NotFound)?;
        self.authorize_hook(actor, WebhookId(hook_id)).await?;
        sqlx::query("INSERT INTO webhook_deliveries (webhook_id, event, action, payload, next_attempt_at) SELECT webhook_id, event, action, payload, now() FROM webhook_deliveries WHERE id = $1")
            .bind(delivery_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get(&self, actor: &Actor, id: WebhookId) -> Result<Webhook> {
        Ok(self.authorize_hook(actor, id).await?.into())
    }

    async fn update(&self, actor: &Actor, id: WebhookId, change: WebhookChange, client: &ClientInfo) -> Result<Webhook> {
        self.authorize_hook(actor, id).await?;
        if let Some(u) = &change.url {
            let target = ssrf::parse_target(u, self.config.webhooks.allow_private)?;
            ssrf::check_saved_target(&target, self.config.webhooks.allow_private).await?;
        }
        let events = match &change.events {
            Some(e) => Some(Self::validate_events(e)?),
            None => None,
        };
        Self::validate_secret(&change.secret)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE webhooks SET url = COALESCE($2, url), events = COALESCE($3, events), active = COALESCE($4, active) WHERE id = $1",
        )
        .bind(id.0)
        .bind(change.url.as_deref().map(str::trim))
        .bind(&events)
        .bind(change.active)
        .execute(&mut *tx)
        .await?;
        if let Some(secret) = &change.secret {
            // An empty secret removes it.
            let sealed =
                if secret.is_empty() { None } else { Some(self.key.seal(secret.as_bytes(), format!("webhook:{}", id.0).as_bytes())?) };
            sqlx::query("UPDATE webhooks SET secret_sealed = $2 WHERE id = $1").bind(id.0).bind(sealed).execute(&mut *tx).await?;
        }
        let ev = AuditEvent::new(actor, client, action::WEBHOOK_UPDATED, Outcome::Success)
            .target("webhook", id.0)
            .details(json!({"url_changed": change.url.is_some(), "events": events, "active": change.active, "secret_changed": change.secret.is_some()}));
        self.audit.record(&mut tx, ev).await?;
        tx.commit().await?;
        Ok(self.hook_row(id).await?.into())
    }

    async fn deliveries(&self, actor: &Actor, id: WebhookId, limit: u32) -> Result<Vec<WebhookDelivery>> {
        self.authorize_hook(actor, id).await?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, uuid::Uuid, String, Option<String>, i32, Option<bool>, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Option<serde_json::Value>, serde_json::Value, Option<i32>, Option<serde_json::Value>, Option<String>, Option<i32>, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as(
                "SELECT id, guid, event, action, attempt, success, delivered_at, next_attempt_at, request_headers, payload, response_status, response_headers, response_body, duration_ms, created_at
                 FROM webhook_deliveries WHERE webhook_id = $1 ORDER BY id DESC LIMIT $2",
            )
            .bind(id.0)
            .bind(limit.clamp(1, 100) as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    did,
                    guid,
                    event,
                    action,
                    attempt,
                    success,
                    delivered_at,
                    next_attempt_at,
                    request_headers,
                    payload,
                    response_status,
                    response_headers,
                    response_body,
                    duration_ms,
                    created_at,
                )| WebhookDelivery {
                    id: did,
                    webhook_id: id,
                    guid: guid.to_string(),
                    event,
                    action,
                    attempt,
                    success,
                    delivered_at,
                    next_attempt_at,
                    request_headers,
                    payload,
                    response_status,
                    response_headers,
                    response_body,
                    duration_ms,
                    created_at,
                },
            )
            .collect())
    }

    async fn ping(&self, actor: &Actor, id: WebhookId, _client: &ClientInfo) -> Result<()> {
        let row = self.authorize_hook(actor, id).await?;
        self.queue_ping(&row, actor).await
    }
}

/// Turns one domain event into deliveries for every hook that wants it. Returns how many were
/// queued. Builds the payload once, when the event is handled.
pub async fn enqueue_event(pool: &PgPool, config: &Config, ev: &DomainEvent) -> Result<usize> {
    let base = config.server.external_url.trim_end_matches('/').to_string();
    let cx = Ctx { pool, base: &base, ssh: &config.ssh.external };
    let Some(built) = payload::build(&cx, ev).await? else { return Ok(0) };
    let Some(repo) = built.repo_id else { return Ok(0) };
    let hooks: Vec<(i64, Vec<String>)> = sqlx::query_as(
        "SELECT id, events FROM webhooks WHERE active AND ((scope = 'repo' AND repo_id = $1) OR (scope = 'org' AND org_id = (SELECT owner_id FROM repositories WHERE id = $1)))",
    )
    .bind(repo.0)
    .fetch_all(pool)
    .await?;
    let mut n = 0;
    for (id, events) in hooks {
        if !events.iter().any(|e| e == built.event || e == "*") {
            continue;
        }
        sqlx::query("INSERT INTO webhook_deliveries (webhook_id, event, action, payload, next_attempt_at) VALUES ($1, $2, $3, $4, now())")
            .bind(id)
            .bind(built.event)
            .bind(&built.action)
            .bind(&built.body)
            .execute(pool)
            .await?;
        n += 1;
    }
    Ok(n)
}
