//! Webhook delivery (ARCHITECTURE.md section 8.5).
//!
//! Each delivery is signed with HMAC-SHA256 in `X-Hub-Signature-256` (the header name tools
//! already verify), carries `X-Codehaven-Event` plus `X-GitHub-Event` and a delivery GUID, and
//! is retried at 1 minute, 5 minutes, 30 minutes, 2 hours and 6 hours. The request goes to
//! an address that `ssrf::resolve_checked` approved, redirects are never followed, and the
//! response is capped, so a hook cannot be used to probe the network or exhaust memory.

use crate::ssrf;
use ch_api_types::webhook::event as hook_event;
use ch_core::error::Result;
use ch_db::sealing::MasterKey;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use std::time::Instant;

/// Delay before attempt 2, 3, 4, 5 and 6.
pub const RETRY_DELAYS_SECS: [i64; 5] = [60, 300, 1800, 7200, 21600];
pub const MAX_ATTEMPTS: i32 = (RETRY_DELAYS_SECS.len() + 1) as i32;
const REQUEST_TIMEOUT_SECS: u64 = 10;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Stored response bodies are cut to this many characters.
const STORED_BODY_CHARS: usize = 16 * 1024;

pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The delay before the next attempt after `attempts_made` failed attempts, or `None` when
/// the delivery is out of attempts.
pub fn retry_delay(attempts_made: i32) -> Option<Duration> {
    let i = attempts_made.checked_sub(1)? as usize;
    RETRY_DELAYS_SECS.get(i).map(|s| Duration::seconds(*s))
}

#[derive(Clone)]
pub struct Deliverer {
    pool: PgPool,
    key: MasterKey,
    allow_private: bool,
}

#[derive(sqlx::FromRow)]
struct Due {
    id: i64,
    webhook_id: i64,
    guid: uuid::Uuid,
    event: String,
    payload: Value,
    attempt: i32,
}

#[derive(sqlx::FromRow)]
struct HookRow {
    url: String,
    secret_sealed: Option<Vec<u8>>,
    content_type: String,
    active: bool,
}

#[derive(Debug)]
struct Outcome {
    request_headers: Value,
    status: Option<i32>,
    response_headers: Option<Value>,
    response_body: Option<String>,
    duration_ms: i32,
    ok: bool,
}

impl Deliverer {
    pub fn new(pool: PgPool, key: MasterKey, allow_private: bool) -> Self {
        Self { pool, key, allow_private }
    }

    /// Claims up to `limit` due deliveries (so a second worker or a crash never doubles them
    /// up) and attempts each. Returns how many were attempted.
    pub async fn run_due(&self, limit: i64) -> Result<usize> {
        let due: Vec<Due> = sqlx::query_as(
            "UPDATE webhook_deliveries SET next_attempt_at = now() + interval '5 minutes'
             WHERE id IN (SELECT id FROM webhook_deliveries WHERE delivered_at IS NULL AND next_attempt_at <= now() ORDER BY next_attempt_at LIMIT $1 FOR UPDATE SKIP LOCKED)
             RETURNING id, webhook_id, guid, event, payload, attempt",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let n = due.len();
        for d in due {
            if let Err(e) = self.deliver(d).await {
                tracing::warn!(error = %e, "webhook delivery bookkeeping failed");
            }
        }
        Ok(n)
    }

    async fn deliver(&self, d: Due) -> Result<()> {
        let hook: Option<HookRow> = sqlx::query_as("SELECT url, secret_sealed, content_type, active FROM webhooks WHERE id = $1")
            .bind(d.webhook_id)
            .fetch_optional(&self.pool)
            .await?;
        let attempt = d.attempt + 1;
        let Some(hook) = hook.filter(|h| h.active) else {
            // The hook was deleted or switched off after this was queued: stop quietly.
            sqlx::query("UPDATE webhook_deliveries SET delivered_at = now(), next_attempt_at = NULL, success = false, response_body = 'The hook is off or gone.' WHERE id = $1").bind(d.id).execute(&self.pool).await?;
            return Ok(());
        };
        let secret = match &hook.secret_sealed {
            Some(s) => Some(self.key.open(s, format!("webhook:{}", d.webhook_id).as_bytes())?),
            None => None,
        };
        let out = self.attempt(&hook, secret.as_deref(), &d).await;
        let retry = if out.ok { None } else { retry_delay(attempt) };
        let finished = out.ok || retry.is_none();
        let next: Option<DateTime<Utc>> = retry.map(|r| Utc::now() + r);
        sqlx::query(
            "UPDATE webhook_deliveries SET attempt = $2, success = $3, request_headers = $4, response_status = $5, response_headers = $6, response_body = $7, duration_ms = $8,
                    next_attempt_at = $9, delivered_at = CASE WHEN $10 THEN now() END WHERE id = $1",
        )
        .bind(d.id)
        .bind(attempt)
        .bind(out.ok)
        .bind(&out.request_headers)
        .bind(out.status)
        .bind(&out.response_headers)
        .bind(&out.response_body)
        .bind(out.duration_ms)
        .bind(next)
        .bind(finished)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn attempt(&self, hook: &HookRow, secret: Option<&[u8]>, d: &Due) -> Outcome {
        let started = Instant::now();
        let body_json = d.payload.to_string();
        let (content_type, body) = if hook.content_type == "form" {
            ("application/x-www-form-urlencoded", format!("payload={}", form_encode(&body_json)))
        } else {
            ("application/json", body_json)
        };
        let mut headers = json!({
            "Content-Type": content_type,
            "User-Agent": format!("Codehaven-Hookshot/{}", env!("CARGO_PKG_VERSION")),
            "X-Codehaven-Event": d.event,
            "X-GitHub-Event": d.event,
            "X-Codehaven-Delivery": d.guid.to_string(),
            "X-GitHub-Delivery": d.guid.to_string(),
            "X-Codehaven-Hook-ID": d.webhook_id.to_string(),
        });
        if let Some(s) = secret {
            headers["X-Hub-Signature-256"] = json!(sign(s, body.as_bytes()));
        }
        let fail = |msg: String, started: Instant, headers: Value| Outcome {
            request_headers: headers,
            status: None,
            response_headers: None,
            response_body: Some(msg),
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
            ok: false,
        };
        let target = match ssrf::parse_target(&hook.url, self.allow_private) {
            Ok(t) => t,
            Err(e) => return fail(describe(&e), started, headers),
        };
        let addrs = match ssrf::resolve_checked(&target, self.allow_private).await {
            Ok(a) => a,
            Err(e) => return fail(describe(&e), started, headers),
        };
        // Connect to the address that was checked, not to whatever the name says next.
        let mut cb = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .no_proxy()
            .user_agent(format!("Codehaven-Hookshot/{}", env!("CARGO_PKG_VERSION")));
        if target.url.host_str().is_some_and(|h| h.parse::<std::net::IpAddr>().is_err()) {
            cb = cb.resolve(&target.host, addrs[0]);
        }
        let client = match cb.build() {
            Ok(c) => c,
            Err(e) => return fail(format!("could not build the HTTP client: {e}"), started, headers),
        };
        let mut req = client.post(target.url.clone()).body(body);
        if let Some(map) = headers.as_object() {
            for (k, v) in map {
                if let Some(v) = v.as_str() {
                    req = req.header(k.as_str(), v);
                }
            }
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return fail(format!("request failed: {}", e.without_url()), started, headers),
        };
        let status = resp.status();
        let mut rh = serde_json::Map::new();
        for (k, v) in resp.headers() {
            if let Ok(v) = v.to_str() {
                rh.insert(k.as_str().to_string(), json!(v));
            }
        }
        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    buf.extend_from_slice(&c);
                    if buf.len() >= MAX_RESPONSE_BYTES {
                        buf.truncate(MAX_RESPONSE_BYTES);
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        let text: String = String::from_utf8_lossy(&buf).chars().take(STORED_BODY_CHARS).collect();
        Outcome {
            request_headers: headers,
            status: Some(status.as_u16() as i32),
            response_headers: Some(Value::Object(rh)),
            response_body: Some(text),
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
            ok: status.is_success(),
        }
    }
}

/// What went wrong, in words the hook's owner can act on. Validation errors keep their messages.
fn describe(e: &ch_core::Error) -> String {
    match e {
        ch_core::Error::Validation(errs) => errs.iter().filter_map(|f| f.message.clone()).collect::<Vec<_>>().join(" "),
        other => other.to_string(),
    }
}

fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// True when `name` is an event a hook may subscribe to.
pub fn is_known_event(name: &str) -> bool {
    hook_event::ALL.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_the_github_documented_example() {
        // From GitHub's "Validating webhook deliveries" page.
        let sig = sign(b"It's a Secret to Everybody", b"Hello, World!");
        assert_eq!(sig, "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17");
    }

    #[test]
    fn retry_schedule_is_1m_5m_30m_2h_6h() {
        let secs: Vec<i64> = (1..=5).map(|n| retry_delay(n).unwrap().num_seconds()).collect();
        assert_eq!(secs, vec![60, 300, 1800, 7200, 21600]);
        assert!(retry_delay(6).is_none(), "six attempts in all");
        assert!(retry_delay(0).is_none());
        assert_eq!(MAX_ATTEMPTS, 6);
    }

    #[test]
    fn form_encoding() {
        assert_eq!(form_encode("{\"a\": 1}"), "%7B%22a%22%3A+1%7D");
        assert_eq!(form_encode("é"), "%C3%A9");
    }

    #[test]
    fn known_events() {
        assert!(is_known_event("push") && is_known_event("ping") && is_known_event("workflow_run"));
        assert!(!is_known_event("everything") && !is_known_event(""));
    }
}
