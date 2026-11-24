//! Notifications, webhooks and the event consumers behind them.
//!
//! Three background loops read the domain event outbox: `notifications` fans events out to
//! inboxes, `webhooks` queues deliveries for hooks that want them, and `webhook-deliver`
//! sends the queued deliveries and retries the failures. See ARCHITECTURE.md sections 8.4
//! and 8.5.

pub mod deliver;
pub mod fanout;
pub mod mail;
pub mod mail_queue;
pub mod notifications;
pub mod payload;
pub mod rest;
pub mod ssrf;
pub mod webhooks;

use axum::Router;
use ch_core::Result;
use ch_core::services::{Notifications, Webhooks};
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use ch_db::sealing::MasterKey;
use std::sync::Arc;
use std::time::Duration;

/// Mounted under `/api/v3`: `/notifications`, `/repos/{o}/{r}/hooks`, `/orgs/{o}/hooks`.
pub fn api_routes() -> Router<AppState> {
    rest::routes()
}

/// How the consumers poll. The binary uses the defaults.
const IDLE_POLL: Duration = Duration::from_secs(2);
const BATCH: i64 = 100;

/// Event consumers `notifications` and `webhooks`, and the webhook delivery loop.
pub fn workers(state: &AppState) -> Vec<Worker> {
    let mut v = Vec::new();

    let fan = fanout::Fanout::new(state.db.clone(), state.authz.clone()).with_mail(state.mail.enabled());
    let s = state.clone();
    v.push(Worker::new("notify-fanout", move |stop| async move {
        consume(&s, "notifications", stop, |ev| {
            let fan = fan.clone();
            async move { fan.process(&ev).await.map(|_| ()) }
        })
        .await
    }));

    // Notification emails (ch-0km). Without mail settings there is nothing to send and nothing
    // is queued.
    if state.mail.enabled() {
        let sender = mail_queue::Sender::new(state.db.clone(), state.authz.clone(), state.mail.clone(), &state.config.server.external_url);
        v.push(Worker::new("notify-mail", move |stop| async move {
            loop {
                let did = sender.run_due(20).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "notification email sweep failed");
                    0
                });
                if did == 0 {
                    tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(Duration::from_secs(10)) => {} }
                }
            }
        }));
    }

    let s = state.clone();
    v.push(Worker::new("webhook-queue", move |stop| async move {
        let pool = s.db.clone();
        let cfg = s.config.clone();
        consume(&s, "webhooks", stop, |ev| {
            let (pool, cfg) = (pool.clone(), cfg.clone());
            async move { webhooks::enqueue_event(&pool, &cfg, &ev).await.map(|_| ()) }
        })
        .await
    }));

    let s = state.clone();
    v.push(Worker::new("webhook-deliver", move |stop| async move {
        let key = match MasterKey::load_or_create(&s.config.master_key_path()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "webhook delivery cannot start without the master key");
                return;
            }
        };
        let d = deliver::Deliverer::new(s.db.clone(), key, s.config.webhooks.allow_private);
        loop {
            let did = d.run_due(20).await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "webhook delivery sweep failed");
                0
            });
            if did == 0 {
                tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(IDLE_POLL) => {} }
            } else if stop.is_cancelled() {
                return;
            }
        }
    }));
    v
}

/// Reads the outbox from this consumer's cursor, hands each event to `handle`, and moves the
/// cursor past it. A handler error is logged and the event is skipped, so one bad event never
/// stops the stream.
async fn consume<F, Fut>(state: &AppState, name: &'static str, stop: tokio_util::sync::CancellationToken, handle: F)
where
    F: Fn(ch_core::DomainEvent) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    loop {
        let events = match state.events.poll(name, BATCH).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(consumer = name, error = %e, "could not read events");
                Vec::new()
            }
        };
        let idle = events.is_empty();
        for ev in events {
            if let Err(e) = handle(ev.event).await {
                tracing::warn!(consumer = name, event = ev.id, error = %e, "could not handle an event");
            }
            if let Err(e) = state.events.ack(name, ev.id).await {
                tracing::warn!(consumer = name, error = %e, "could not move the cursor");
                break;
            }
        }
        if idle {
            tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(IDLE_POLL) => {} }
        } else if stop.is_cancelled() {
            return;
        }
    }
}

pub struct NotifyServices {
    pub notifications: Arc<dyn Notifications>,
    pub webhooks: Arc<dyn Webhooks>,
    /// `None` keeps mail off: nothing is configured to send with.
    pub mail: Option<Arc<dyn ch_core::services::Mail>>,
}

impl Install for NotifyServices {
    fn install(self, b: &mut AppStateBuilder) {
        b.notifications = self.notifications;
        b.webhooks = self.webhooks;
        if let Some(m) = self.mail {
            b.mail = m;
        }
    }
}

#[derive(Default)]
pub struct NotifyOptions {
    /// `None` loads or creates `data/secrets/master.key`, the same file the identity crate uses.
    pub master_key: Option<MasterKey>,
}

/// Built after identity: both services check access through `b.authz`.
pub fn build(b: &AppStateBuilder) -> Result<NotifyServices> {
    build_with(b, NotifyOptions::default())
}

pub fn build_with(b: &AppStateBuilder, opts: NotifyOptions) -> Result<NotifyServices> {
    let key = match opts.master_key {
        Some(k) => k,
        None => MasterKey::load_or_create(&b.config.master_key_path())?,
    };
    // Saving a hook and delivering to it both judge addresses with this list.
    ssrf::set_blocked_ips(&b.config.webhooks.blocked_ips);
    let mail = mail::SmtpMail::build(b.config.mail.smtp.as_ref())?.map(|m| Arc::new(m) as Arc<dyn ch_core::services::Mail>);
    Ok(NotifyServices {
        notifications: Arc::new(notifications::NotificationsService::new(b.db.clone(), b.authz.clone())),
        webhooks: Arc::new(webhooks::WebhooksService::new(b.db.clone(), key, b.authz.clone(), b.audit.clone(), b.config.clone())),
        mail,
    })
}
