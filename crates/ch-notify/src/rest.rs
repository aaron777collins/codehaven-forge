//! REST: repository and organization hooks, notifications and watch levels.

use crate::webhooks::HookRow;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ch_api_types::hooks as dto;
use ch_core::Actor;
use ch_core::access::{Client, CurrentActor};
use ch_core::error::{Error, Result};
use ch_core::ids::*;
use ch_core::model::{NewWebhook, WebhookChange};
use ch_core::state::AppState;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;

type Res<T> = Result<T>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{repo}/hooks", get(list_repo_hooks).post(create_repo_hook))
        .route("/repos/{owner}/{repo}/hooks/{id}", get(get_hook).patch(patch_hook).delete(delete_hook))
        .route("/repos/{owner}/{repo}/hooks/{id}/pings", post(ping_hook))
        .route("/repos/{owner}/{repo}/hooks/{id}/deliveries", get(deliveries))
        .route("/repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery}", get(delivery_detail))
        .route("/repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery}/attempts", post(redeliver))
        .route("/orgs/{org}/hooks", get(list_org_hooks).post(create_org_hook))
        .route("/orgs/{org}/hooks/{id}", get(get_org_hook).patch(patch_org_hook).delete(delete_org_hook))
        .route("/notifications", get(inbox).put(mark_all_read))
        .route("/notifications/unread_count", get(unread_count))
        .route("/notifications/threads/{id}", get(get_thread).patch(mark_thread_read).delete(mark_thread_done))
        .route("/repos/{owner}/{repo}/subscription", get(get_watch).put(put_watch).delete(delete_watch))
}

fn base(s: &AppState) -> String {
    s.config.server.external_url.trim_end_matches('/').to_string()
}

async fn repo_id(pool: &PgPool, owner: &str, repo: &str) -> Res<RepoId> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT r.id FROM repositories r JOIN users u ON u.id = r.owner_id WHERE u.username = $1::citext AND r.name = $2::citext",
    )
    .bind(owner)
    .bind(repo)
    .fetch_optional(pool)
    .await?;
    id.map(RepoId).ok_or(Error::NotFound)
}

async fn org_id(pool: &PgPool, org: &str) -> Res<OrgId> {
    let id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE username = $1::citext AND kind = 'org'").bind(org).fetch_optional(pool).await?;
    id.map(OrgId).ok_or(Error::NotFound)
}

/// The hook as GitHub shapes it. The secret is never sent back, only a mask that says one is set.
pub async fn hook_dto(pool: &PgPool, base: &str, h: &HookRow) -> Res<dto::Hook> {
    let path = match (h.repo_id, h.org_id) {
        (Some(r), _) => {
            let full: String = sqlx::query_scalar(
                "SELECT o.username::text || '/' || r.name::text FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = $1",
            )
            .bind(r)
            .fetch_one(pool)
            .await?;
            format!("{base}/api/v3/repos/{full}/hooks/{}", h.id)
        }
        (_, Some(o)) => {
            let name: String = sqlx::query_scalar("SELECT username::text FROM users WHERE id = $1").bind(o).fetch_one(pool).await?;
            format!("{base}/api/v3/orgs/{name}/hooks/{}", h.id)
        }
        _ => format!("{base}/api/v3/hooks/{}", h.id),
    };
    let last: Option<(Option<i32>, Option<bool>)> = sqlx::query_as("SELECT response_status, success FROM webhook_deliveries WHERE webhook_id = $1 AND delivered_at IS NOT NULL OR (webhook_id = $1 AND attempt > 0) ORDER BY id DESC LIMIT 1").bind(h.id).fetch_optional(pool).await?;
    let last_response = Some(match last {
        Some((code, Some(true))) => dto::HookLastResponse { code, status: Some("active".into()), message: Some("OK".into()) },
        Some((code, Some(false))) => dto::HookLastResponse { code, status: Some("failing".into()), message: None },
        _ => dto::HookLastResponse { code: None, status: Some("unused".into()), message: None },
    });
    Ok(dto::Hook {
        id: h.id,
        name: "web".into(),
        active: h.active,
        events: h.events.clone(),
        config: dto::HookConfig {
            url: h.url.clone(),
            content_type: Some(h.content_type.clone()),
            secret: h.secret_sealed.as_ref().map(|_| "********".to_string()),
            insecure_ssl: Some("0".into()),
        },
        created_at: h.created_at,
        updated_at: h.created_at,
        ping_url: format!("{path}/pings"),
        deliveries_url: format!("{path}/deliveries"),
        url: path,
        last_response,
    })
}

async fn load(s: &AppState, actor: &Actor, id: i64) -> Res<HookRow> {
    // Authorization first (through the service), then the full row for the DTO.
    s.webhooks.get(actor, WebhookId(id)).await?;
    sqlx::query_as(&format!("SELECT {} FROM webhooks WHERE id = $1", crate::webhooks::HOOK_COLS))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or(Error::NotFound)
}

/// A hook id in a repository path must belong to that repository, so a hook of another
/// repository cannot be reached through a repository you can manage.
async fn load_in_repo(s: &AppState, actor: &Actor, owner: &str, repo: &str, id: i64) -> Res<HookRow> {
    let rid = repo_id(&s.db, owner, repo).await?;
    let row = load(s, actor, id).await?;
    if row.repo_id != Some(rid.0) {
        return Err(Error::NotFound);
    }
    Ok(row)
}

#[derive(Deserialize)]
struct HookBody {
    config: Option<dto::HookConfig>,
    events: Option<Vec<String>>,
    active: Option<bool>,
}

async fn create_hook(
    s: &AppState,
    actor: &Actor,
    client: &ch_core::model::ClientInfo,
    repo: Option<RepoId>,
    org: Option<OrgId>,
    body: HookBody,
) -> Res<(StatusCode, Json<dto::Hook>)> {
    let cfg = body.config.ok_or_else(|| Error::invalid("Hook", "config", "A hook needs a config with a url."))?;
    let wh = s
        .webhooks
        .create(
            actor,
            NewWebhook {
                repo_id: repo,
                org_id: org,
                url: cfg.url,
                secret: cfg.secret,
                events: body.events.unwrap_or_else(|| vec!["push".into()]),
                active: body.active.unwrap_or(true),
            },
            client,
        )
        .await?;
    if cfg.content_type.as_deref() == Some("form") {
        sqlx::query("UPDATE webhooks SET content_type = 'form' WHERE id = $1").bind(wh.id.0).execute(&s.db).await?;
    }
    let row = load(s, actor, wh.id.0).await?;
    Ok((StatusCode::CREATED, Json(hook_dto(&s.db, &base(s), &row).await?)))
}

async fn patch(s: &AppState, actor: &Actor, client: &ch_core::model::ClientInfo, id: i64, body: HookBody) -> Res<Json<dto::Hook>> {
    let (url, secret, content_type) = match body.config {
        Some(c) => (Some(c.url), c.secret, c.content_type),
        None => (None, None, None),
    };
    // A secret of "********" is the mask we sent, not a new secret.
    let secret = secret.filter(|v| v != "********");
    s.webhooks.update(actor, WebhookId(id), WebhookChange { url, secret, events: body.events, active: body.active }, client).await?;
    if let Some(ct) = content_type
        && (ct == "json" || ct == "form")
    {
        sqlx::query("UPDATE webhooks SET content_type = $2 WHERE id = $1").bind(id).bind(ct).execute(&s.db).await?;
    }
    let row = load(s, actor, id).await?;
    Ok(Json(hook_dto(&s.db, &base(s), &row).await?))
}

async fn list_repo_hooks(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo)): Path<(String, String)>,
) -> Res<Json<Vec<dto::Hook>>> {
    let rid = repo_id(&s.db, &owner, &repo).await?;
    let hooks = s.webhooks.list(&actor, Some(rid), None).await?;
    let mut out = Vec::new();
    for h in hooks {
        let row = load(&s, &actor, h.id.0).await?;
        out.push(hook_dto(&s.db, &base(&s), &row).await?);
    }
    Ok(Json(out))
}

async fn create_repo_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<HookBody>,
) -> Res<(StatusCode, Json<dto::Hook>)> {
    let rid = repo_id(&s.db, &owner, &repo).await?;
    create_hook(&s, &actor, &client, Some(rid), None, body).await
}

async fn get_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo, id)): Path<(String, String, i64)>,
) -> Res<Json<dto::Hook>> {
    let row = load_in_repo(&s, &actor, &owner, &repo, id).await?;
    Ok(Json(hook_dto(&s.db, &base(&s), &row).await?))
}

async fn patch_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, repo, id)): Path<(String, String, i64)>,
    Json(body): Json<HookBody>,
) -> Res<Json<dto::Hook>> {
    load_in_repo(&s, &actor, &owner, &repo, id).await?;
    patch(&s, &actor, &client, id, body).await
}

async fn delete_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, repo, id)): Path<(String, String, i64)>,
) -> Res<StatusCode> {
    load_in_repo(&s, &actor, &owner, &repo, id).await?;
    s.webhooks.delete(&actor, WebhookId(id), &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ping_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, repo, id)): Path<(String, String, i64)>,
) -> Res<StatusCode> {
    load_in_repo(&s, &actor, &owner, &repo, id).await?;
    s.webhooks.ping(&actor, WebhookId(id), &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Limit {
    per_page: Option<u32>,
}

fn delivery_summary(d: &ch_core::model::WebhookDelivery) -> dto::HookDelivery {
    dto::HookDelivery {
        id: d.id,
        guid: d.guid.clone(),
        delivered_at: d.delivered_at.unwrap_or(d.created_at),
        redelivery: false,
        duration: d.duration_ms.unwrap_or(0) as f64 / 1000.0,
        status: match d.success {
            Some(true) => "OK".into(),
            Some(false) => "failed".into(),
            None => "pending".into(),
        },
        status_code: d.response_status.unwrap_or(0),
        event: d.event.clone(),
        action: d.action.clone(),
    }
}

async fn deliveries(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo, id)): Path<(String, String, i64)>,
    Query(q): Query<Limit>,
) -> Res<Json<Vec<dto::HookDelivery>>> {
    load_in_repo(&s, &actor, &owner, &repo, id).await?;
    Ok(Json(s.webhooks.deliveries(&actor, WebhookId(id), q.per_page.unwrap_or(30)).await?.iter().map(delivery_summary).collect()))
}

async fn delivery_detail(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo, id, delivery)): Path<(String, String, i64, i64)>,
) -> Res<Json<dto::HookDeliveryDetail>> {
    load_in_repo(&s, &actor, &owner, &repo, id).await?;
    let all = s.webhooks.deliveries(&actor, WebhookId(id), 100).await?;
    let d = all.iter().find(|d| d.id == delivery).ok_or(Error::NotFound)?;
    let obj = |v: &Option<serde_json::Value>| v.as_ref().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    Ok(Json(dto::HookDeliveryDetail {
        delivery: delivery_summary(d),
        request: dto::DeliveryPayload { headers: obj(&d.request_headers), payload: d.payload.clone() },
        response: dto::DeliveryPayload {
            headers: obj(&d.response_headers),
            payload: serde_json::Value::String(d.response_body.clone().unwrap_or_default()),
        },
    }))
}

async fn redeliver(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo, id, delivery)): Path<(String, String, i64, i64)>,
) -> Res<StatusCode> {
    load_in_repo(&s, &actor, &owner, &repo, id).await?;
    // The delivery must belong to this hook.
    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM webhook_deliveries WHERE id = $1 AND webhook_id = $2)")
        .bind(delivery)
        .bind(id)
        .fetch_one(&s.db)
        .await?;
    if !ok {
        return Err(Error::NotFound);
    }
    s.webhooks.redeliver(&actor, delivery).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn list_org_hooks(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(org): Path<String>,
) -> Res<Json<Vec<dto::Hook>>> {
    let oid = org_id(&s.db, &org).await?;
    let hooks = s.webhooks.list(&actor, None, Some(oid)).await?;
    let mut out = Vec::new();
    for h in hooks {
        let row = load(&s, &actor, h.id.0).await?;
        out.push(hook_dto(&s.db, &base(&s), &row).await?);
    }
    Ok(Json(out))
}

async fn create_org_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path(org): Path<String>,
    Json(body): Json<HookBody>,
) -> Res<(StatusCode, Json<dto::Hook>)> {
    let oid = org_id(&s.db, &org).await?;
    create_hook(&s, &actor, &client, None, Some(oid), body).await
}

async fn load_in_org(s: &AppState, actor: &Actor, org: &str, id: i64) -> Res<HookRow> {
    let oid = org_id(&s.db, org).await?;
    let row = load(s, actor, id).await?;
    if row.org_id != Some(oid.0) {
        return Err(Error::NotFound);
    }
    Ok(row)
}

async fn get_org_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((org, id)): Path<(String, i64)>,
) -> Res<Json<dto::Hook>> {
    let row = load_in_org(&s, &actor, &org, id).await?;
    Ok(Json(hook_dto(&s.db, &base(&s), &row).await?))
}

async fn patch_org_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, id)): Path<(String, i64)>,
    Json(body): Json<HookBody>,
) -> Res<Json<dto::Hook>> {
    load_in_org(&s, &actor, &org, id).await?;
    patch(&s, &actor, &client, id, body).await
}

async fn delete_org_hook(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((org, id)): Path<(String, i64)>,
) -> Res<StatusCode> {
    load_in_org(&s, &actor, &org, id).await?;
    s.webhooks.delete(&actor, WebhookId(id), &client).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- notifications

#[derive(Deserialize)]
struct InboxQuery {
    all: Option<bool>,
    page: Option<u32>,
    per_page: Option<u32>,
}

async fn notification_dto(s: &AppState, n: &ch_core::model::Notification) -> Res<Option<dto::Notification>> {
    let Some(repo) = n.repo_id else { return Ok(None) };
    let b = base(s);
    let repository = crate::payload::repo_json(&s.db, &b, &s.config.ssh.external, repo.0).await?;
    let (kind, path) = match n.subject_type.as_str() {
        "pull" => ("PullRequest", "pulls"),
        "issue" => ("Issue", "issues"),
        "release" => ("Release", "releases"),
        "commit" => ("Commit", "commits"),
        _ => ("CheckSuite", "actions/runs"),
    };
    let number: Option<i64> = if n.subject_type == "issue" || n.subject_type == "pull" {
        sqlx::query_scalar("SELECT number FROM issues WHERE id = $1").bind(n.subject_id).fetch_optional(&s.db).await?
    } else {
        None
    };
    let tail = number.unwrap_or(n.subject_id);
    let id = n.id.0;
    Ok(Some(dto::Notification {
        id: id.to_string(),
        unread: n.unread,
        reason: n.reason.clone(),
        updated_at: n.updated_at,
        last_read_at: None,
        subject: dto::NotificationSubject {
            title: n.title.clone(),
            url: Some(format!("{b}/api/v3/repos/{}/{path}/{tail}", repository.full_name)),
            latest_comment_url: None,
            kind: kind.into(),
        },
        url: format!("{b}/api/v3/notifications/threads/{id}"),
        subscription_url: format!("{b}/api/v3/notifications/threads/{id}/subscription"),
        repository,
    }))
}

async fn inbox(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<InboxQuery>,
) -> Res<Json<Vec<dto::Notification>>> {
    let rows = s.notifications.inbox(&actor, !q.all.unwrap_or(false), q.page.unwrap_or(1), q.per_page.unwrap_or(50)).await?;
    let mut out = Vec::new();
    for n in &rows {
        if let Some(d) = notification_dto(&s, n).await? {
            out.push(d);
        }
    }
    Ok(Json(out))
}

async fn mark_all_read(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<StatusCode> {
    s.notifications.mark_read(&actor, None).await?;
    Ok(StatusCode::RESET_CONTENT)
}

async fn unread_count(State(s): State<AppState>, CurrentActor(actor): CurrentActor) -> Res<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({"unread": s.notifications.unread_count(&actor).await?})))
}

async fn get_thread(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(id): Path<i64>) -> Res<Json<dto::Notification>> {
    let rows = s.notifications.inbox(&actor, false, 1, 100).await?;
    let n = rows.iter().find(|n| n.id.0 == id).ok_or(Error::NotFound)?;
    notification_dto(&s, n).await?.map(Json).ok_or(Error::NotFound)
}

async fn mark_thread_read(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(id): Path<i64>) -> Res<StatusCode> {
    s.notifications.mark_read(&actor, Some(NotificationId(id))).await?;
    Ok(StatusCode::RESET_CONTENT)
}

async fn mark_thread_done(State(s): State<AppState>, CurrentActor(actor): CurrentActor, Path(id): Path<i64>) -> Res<StatusCode> {
    s.notifications.mark_done(&actor, NotificationId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn watch_dto(s: &AppState, owner: &str, repo: &str, level: &str, since: DateTime<Utc>) -> dto::RepoSubscription {
    let b = base(s);
    dto::RepoSubscription {
        subscribed: level == "all",
        ignored: level == "ignore",
        reason: None,
        created_at: since,
        url: format!("{b}/api/v3/repos/{owner}/{repo}/subscription"),
        repository_url: format!("{b}/api/v3/repos/{owner}/{repo}"),
    }
}

async fn get_watch(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo)): Path<(String, String)>,
) -> Res<Json<dto::RepoSubscription>> {
    let rid = repo_id(&s.db, &owner, &repo).await?;
    let level = s.notifications.watch_level(&actor, rid).await?;
    Ok(Json(watch_dto(&s, &owner, &repo, &level, Utc::now())))
}

async fn put_watch(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<dto::PutRepoSubscription>,
) -> Res<Json<dto::RepoSubscription>> {
    let rid = repo_id(&s.db, &owner, &repo).await?;
    let level = if body.ignored == Some(true) {
        "ignore"
    } else if body.subscribed == Some(true) {
        "all"
    } else {
        "participating"
    };
    s.notifications.subscribe(&actor, rid, level).await?;
    Ok(Json(watch_dto(&s, &owner, &repo, level, Utc::now())))
}

async fn delete_watch(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, repo)): Path<(String, String)>,
) -> Res<StatusCode> {
    let rid = repo_id(&s.db, &owner, &repo).await?;
    s.notifications.subscribe(&actor, rid, "participating").await?;
    Ok(StatusCode::NO_CONTENT)
}
