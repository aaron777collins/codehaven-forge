//! Notification emails (ch-0km).
//!
//! A notification for someone who turned email on also queues a message. The queue is a table,
//! so a restart loses nothing. At send time every condition is checked again: the instance still
//! has mail, the account is active and still wants email, it has a primary address, and it may
//! still see the repository (spec section 4, "Notifications": the same View check as the inbox).
//! A message that fails to send is tried again later, five times at most.

use ch_core::services::{Authz, Mail};
use ch_core::{Actor, AuthVia, Permission, RepoId, Result, UserId};
use sqlx::PgPool;
use std::sync::Arc;

/// How many times one message is tried before it is given up.
pub const MAX_ATTEMPTS: i32 = 5;

/// Queues an email for a notification just written, if the person wants one and mail is on.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue(
    pool: &PgPool,
    mail_on: bool,
    user: i64,
    repo: i64,
    reason: &str,
    subject_type: &str,
    subject_id: i64,
    title: &str,
) -> Result<()> {
    if !mail_on {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO notification_mail_queue (user_id, repo_id, reason, subject_type, subject_id, title)
         SELECT $1, $2, $3, $4, $5, $6 FROM users WHERE id = $1 AND notify_email",
    )
    .bind(user)
    .bind(repo)
    .bind(reason)
    .bind(subject_type)
    .bind(subject_id)
    .bind(title)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Due {
    id: i64,
    user_id: i64,
    repo_id: Option<i64>,
    reason: String,
    subject_type: String,
    subject_id: i64,
    title: String,
    attempts: i32,
}

pub struct Sender {
    pool: PgPool,
    authz: Arc<dyn Authz>,
    mail: Arc<dyn Mail>,
    base: String,
    instance: String,
}

impl Sender {
    pub fn new(pool: PgPool, authz: Arc<dyn Authz>, mail: Arc<dyn Mail>, external_url: &str) -> Self {
        let base = external_url.trim_end_matches('/').to_string();
        let instance = url::Url::parse(&base).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| base.clone());
        Self { pool, authz, mail, base, instance }
    }

    /// Sends what is due, up to `limit`. Returns how many rows it handled.
    pub async fn run_due(&self, limit: i64) -> Result<usize> {
        if !self.mail.enabled() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let due: Vec<Due> = sqlx::query_as(
            "SELECT id, user_id, repo_id, reason, subject_type, subject_id, title, attempts FROM notification_mail_queue
             WHERE done_at IS NULL AND next_at <= now() ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let mut handled = 0;
        for d in due {
            let outcome = self.send_one(&d).await;
            match outcome {
                Ok(what) => {
                    sqlx::query("UPDATE notification_mail_queue SET done_at = now(), outcome = $2, attempts = attempts + 1 WHERE id = $1")
                        .bind(d.id)
                        .bind(what)
                        .execute(&mut *tx)
                        .await?;
                }
                Err(e) => {
                    let last = d.attempts + 1 >= MAX_ATTEMPTS;
                    tracing::warn!(id = d.id, attempt = d.attempts + 1, error = %e, "a notification email did not send");
                    sqlx::query(
                        "UPDATE notification_mail_queue SET attempts = attempts + 1, next_at = now() + make_interval(mins => 2 * (attempts + 1)),
                           done_at = CASE WHEN $2 THEN now() END, outcome = CASE WHEN $2 THEN 'gave up' END WHERE id = $1",
                    )
                    .bind(d.id)
                    .bind(last)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            handled += 1;
        }
        tx.commit().await?;
        Ok(handled)
    }

    /// `Ok("sent")`, or `Ok(<why not>)` when it should not go at all; `Err` to try again.
    async fn send_one(&self, d: &Due) -> Result<&'static str> {
        let row: Option<(String, bool, bool, Option<String>)> = sqlx::query_as(
            "SELECT u.username::text, u.notify_email, u.suspended_at IS NULL AND u.kind = 'user',
                    (SELECT e.email::text FROM user_emails e WHERE e.user_id = u.id AND e.is_primary)
             FROM users u WHERE u.id = $1",
        )
        .bind(d.user_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((username, wants, active, email)) = row else { return Ok("no account") };
        if !wants {
            return Ok("turned off");
        }
        if !active {
            return Ok("account not active");
        }
        let Some(email) = email else { return Ok("no address") };
        let Some(repo) = d.repo_id else { return Ok("no repository") };
        let as_user = Actor::User { id: UserId(d.user_id), is_admin: false, has_2fa: false, via: AuthVia::Session };
        if self.authz.repo_permission(&as_user, RepoId(repo)).await? < Permission::View {
            return Ok("no longer visible");
        }
        let full: Option<String> = sqlx::query_scalar(
            "SELECT u.username::text || '/' || r.name::text FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        let Some(full) = full else { return Ok("no repository") };
        // Issues and pull requests are stored by id; the link needs the number.
        let number: Option<i64> = if matches!(d.subject_type.as_str(), "issue" | "pull") {
            sqlx::query_scalar("SELECT number FROM issues WHERE id = $1 AND repo_id = $2")
                .bind(d.subject_id)
                .bind(repo)
                .fetch_optional(&self.pool)
                .await?
        } else {
            None
        };
        let (subject, body) = message(&self.base, &self.instance, &username, &full, &d.subject_type, number, &d.reason, &d.title);
        self.mail.send(&email, &subject, &body).await?;
        Ok("sent")
    }
}

/// The subject line and plain-text body. Only the title and where to read it: no comment text,
/// so a message holds nothing the link does not already show the person.
#[allow(clippy::too_many_arguments)]
pub fn message(
    base: &str,
    instance: &str,
    username: &str,
    full: &str,
    kind: &str,
    number: Option<i64>,
    reason: &str,
    title: &str,
) -> (String, String) {
    let path = match (kind, number) {
        ("pull", Some(n)) => format!("/{full}/pulls/{n}"),
        ("issue", Some(n)) => format!("/{full}/issues/{n}"),
        _ => format!("/{full}"),
    };
    let why = match reason {
        "mention" => "You were mentioned.",
        "team_mention" => "Your team was mentioned.",
        "assign" => "You were assigned.",
        "review_requested" => "Your review was requested.",
        "author" => "You opened it.",
        "comment" => "You commented on it.",
        _ => "You are watching it.",
    };
    let title: String = title.chars().take(150).collect::<String>().replace(['\r', '\n'], " ");
    let subject = format!("[{full}] {title}");
    let body = format!(
        "Hello {username},\n\nThere is something new on {full}:\n\n  {title}\n\n{why}\n\nRead it: {base}{path}\n\n\
         You get this because email notifications are on for your account on {instance}.\n\
         Turn them off: {base}/settings/notifications\n"
    );
    (subject, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_holds_the_title_and_the_way_out_and_nothing_else() {
        let (s, b) =
            message("https://code.example.com", "code.example.com", "bob", "alice/demo", "issue", Some(7), "mention", "Crash\r\nBcc: x");
        assert_eq!(s, "[alice/demo] Crash  Bcc: x", "no line breaks reach the subject");
        assert!(b.contains("https://code.example.com/alice/demo/issues/7"));
        assert!(b.contains("You were mentioned."));
        assert!(b.contains("Turn them off: https://code.example.com/settings/notifications"));
    }
}
