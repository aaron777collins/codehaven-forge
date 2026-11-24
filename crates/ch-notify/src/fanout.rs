//! Turns domain events into inbox rows (ARCHITECTURE.md section 8.4).
//!
//! One row per thread per person. Who hears about an event:
//! - people on the thread: the author, assignees, anyone subscribed, earlier commenters, and
//!   for a pull request the reviewers and requested reviewers
//! - people watching the repository at "all activity"
//! - people mentioned by `@name` or `@org/team` in the text that came with the event
//!
//! Then the filters, in this order: never the person who did it, never someone who ignores
//! the repository, never a muted thread (a mention still gets through), never a suspended
//! account, and never anyone who cannot read the repository. That last one is the privacy
//! rule: a title must not reach a person who could not open the page it links to.

use ch_core::error::Result;
use ch_core::events::DomainEvent;
use ch_core::ids::*;
use ch_core::services::Authz;
use ch_core::text::refs;
use ch_core::{Actor, AuthVia, Permission};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Why a person is told, strongest first. A person who qualifies for several gets the
/// strongest.
const REASON_ORDER: &[&str] =
    &["mention", "team_mention", "review_requested", "assigned", "author", "comment", "subscribed", "state_change", "ci_activity"];

fn rank(reason: &str) -> usize {
    REASON_ORDER.iter().position(|r| *r == reason).unwrap_or(REASON_ORDER.len())
}

#[derive(Clone)]
pub struct Fanout {
    pool: PgPool,
    authz: Arc<dyn Authz>,
    /// Queue an email beside each notification for people who want one (ch-0km).
    mail_on: bool,
}

#[derive(Debug, Default)]
struct Who {
    /// user id -> strongest reason
    people: BTreeMap<i64, &'static str>,
}

impl Who {
    fn add(&mut self, user: i64, reason: &'static str) {
        let e = self.people.entry(user).or_insert(reason);
        if rank(reason) < rank(e) {
            *e = reason;
        }
    }
    fn add_all(&mut self, users: impl IntoIterator<Item = i64>, reason: &'static str) {
        for u in users {
            self.add(u, reason);
        }
    }
}

struct Subject<'a> {
    kind: &'static str,
    id: i64,
    repo: i64,
    title: &'a str,
}

impl Fanout {
    pub fn new(pool: PgPool, authz: Arc<dyn Authz>) -> Self {
        Self { pool, authz, mail_on: false }
    }

    /// The same, also queueing notification emails when the instance has mail.
    pub fn with_mail(mut self, on: bool) -> Self {
        self.mail_on = on;
        self
    }

    /// Handles one event. Returns how many inbox rows were written.
    pub async fn process(&self, ev: &DomainEvent) -> Result<usize> {
        self.process_inner(ev).await
    }

    async fn process_inner(&self, ev: &DomainEvent) -> Result<usize> {
        use DomainEvent::*;
        match ev {
            IssueOpened { repo_id, issue_id, actor, .. } => {
                self.thread_event(*repo_id, issue_id.0, Some(actor.0), None, true, true, TextFrom::Issue).await
            }
            PrOpened { repo_id, pull_id, actor, .. } => {
                self.thread_event(*repo_id, pull_id.0, Some(actor.0), None, true, true, TextFrom::Issue).await
            }
            IssueAssigned { repo_id, issue_id, assignee, actor, .. } => {
                let mut who = Who::default();
                who.add(assignee.0, "assigned");
                let (title, kind) = self.issue_title(issue_id.0).await?;
                self.write(&Subject { kind, id: issue_id.0, repo: repo_id.0, title: &title }, who, Some(actor.0), &BTreeSet::new()).await
            }
            CommentCreated { repo_id, issue_id, comment_id, actor, .. } => {
                self.thread_event(*repo_id, issue_id.0, Some(actor.0), None, false, true, TextFrom::IssueComment(comment_id.0)).await
            }
            IssueClosed { repo_id, issue_id, actor, .. } | IssueReopened { repo_id, issue_id, actor, .. } => {
                self.thread_event(*repo_id, issue_id.0, Some(actor.0), Some("state_change"), true, true, TextFrom::None).await
            }
            PrClosed { repo_id, pull_id, actor, .. } => {
                self.thread_event(*repo_id, pull_id.0, actor.map(|a| a.0), Some("state_change"), true, true, TextFrom::None).await
            }
            PrMerged { repo_id, pull_id, actor, .. } => {
                self.thread_event(*repo_id, pull_id.0, Some(actor.0), Some("state_change"), true, true, TextFrom::None).await
            }
            ReviewSubmitted { repo_id, pull_id, review_id, actor, .. } => {
                self.thread_event(*repo_id, pull_id.0, Some(actor.0), None, false, true, TextFrom::Review(review_id.0)).await
            }
            ReviewCommentCreated { repo_id, pull_id, comment_id, actor, .. } => {
                self.thread_event(*repo_id, pull_id.0, Some(actor.0), None, false, true, TextFrom::ReviewComment(comment_id.0)).await
            }
            ReleasePublished { repo_id, release_id, tag, actor } => {
                let mut who = Who::default();
                who.add_all(self.watchers(repo_id.0).await?, "subscribed");
                let body: Option<String> =
                    sqlx::query_scalar("SELECT body FROM releases WHERE id = $1").bind(release_id.0).fetch_optional(&self.pool).await?;
                let mentions = self.mention_ids(&body.unwrap_or_default(), &mut who).await?;
                self.write(&Subject { kind: "release", id: release_id.0, repo: repo_id.0, title: tag }, who, Some(actor.0), &mentions).await
            }
            WorkflowRunUpdated { repo_id, run_id, status, conclusion }
                if status == "completed" && matches!(conclusion.as_deref(), Some("failure" | "timed_out" | "startup_failure")) =>
            {
                // Only the person who triggered the run hears about it, and only when it failed
                // (ch-ed1): a note for every passing run buries the ones that matter.
                let row: Option<(Option<i64>, String)> = sqlx::query_as("SELECT actor_id, title FROM workflow_runs WHERE id = $1")
                    .bind(run_id.0)
                    .fetch_optional(&self.pool)
                    .await?;
                let Some((Some(triggerer), title)) = row else { return Ok(0) };
                let title =
                    format!("{}: {}", if title.is_empty() { "Workflow run" } else { &title }, conclusion.as_deref().unwrap_or("finished"));
                let mut who = Who::default();
                who.add(triggerer, "ci_activity");
                // The triggerer is not "the actor" here: nobody is excluded.
                self.write(&Subject { kind: "workflow_run", id: run_id.0, repo: repo_id.0, title: &title }, who, None, &BTreeSet::new())
                    .await
            }
            _ => Ok(0),
        }
    }

    async fn issue_title(&self, issue: i64) -> Result<(String, &'static str)> {
        let row: Option<(String, bool)> =
            sqlx::query_as("SELECT title, is_pull FROM issues WHERE id = $1").bind(issue).fetch_optional(&self.pool).await?;
        Ok(match row {
            Some((t, true)) => (t, "pull"),
            Some((t, false)) => (t, "issue"),
            None => (String::new(), "issue"),
        })
    }

    async fn watchers(&self, repo: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar("SELECT user_id FROM watches WHERE repo_id = $1 AND level = 'all'").bind(repo).fetch_all(&self.pool).await?)
    }

    /// Everyone on an issue or pull request thread, each with the reason that fits.
    async fn thread_people(&self, issue: i64, who: &mut Who) -> Result<()> {
        let author: Option<i64> =
            sqlx::query_scalar("SELECT author_id FROM issues WHERE id = $1").bind(issue).fetch_optional(&self.pool).await?;
        who.add_all(author, "author");
        let assignees: Vec<i64> =
            sqlx::query_scalar("SELECT user_id FROM issue_assignees WHERE issue_id = $1").bind(issue).fetch_all(&self.pool).await?;
        who.add_all(assignees, "assigned");
        let subscribed: Vec<i64> = sqlx::query_scalar("SELECT user_id FROM issue_subscriptions WHERE issue_id = $1 AND subscribed")
            .bind(issue)
            .fetch_all(&self.pool)
            .await?;
        who.add_all(subscribed, "subscribed");
        let commenters: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT author_id FROM issue_comments WHERE issue_id = $1")
            .bind(issue)
            .fetch_all(&self.pool)
            .await?;
        who.add_all(commenters, "comment");
        // Pull request extras. For a plain issue these tables have no rows.
        let requested: Vec<i64> = sqlx::query_scalar("SELECT user_id FROM review_requests WHERE pull_id = $1 AND user_id IS NOT NULL")
            .bind(issue)
            .fetch_all(&self.pool)
            .await?;
        who.add_all(requested, "review_requested");
        let reviewers: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT author_id FROM reviews WHERE pull_id = $1 AND state <> 'pending'")
            .bind(issue)
            .fetch_all(&self.pool)
            .await?;
        who.add_all(reviewers, "comment");
        let review_commenters: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT author_id FROM review_comments WHERE pull_id = $1")
            .bind(issue)
            .fetch_all(&self.pool)
            .await?;
        who.add_all(review_commenters, "comment");
        Ok(())
    }

    /// Resolves `@user` and `@org/team` in `text` to account ids, adds them to `who` with the
    /// mention reasons, and returns the set of mentioned ids.
    async fn mention_ids(&self, text: &str, who: &mut Who) -> Result<BTreeSet<i64>> {
        let r = refs(text);
        let mut out = BTreeSet::new();
        if !r.mentions.is_empty() {
            let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM users WHERE username = ANY($1::citext[]) AND kind = 'user'")
                .bind(&r.mentions)
                .fetch_all(&self.pool)
                .await?;
            for id in ids {
                who.add(id, "mention");
                out.insert(id);
            }
        }
        for (org, team) in &r.team_mentions {
            let ids: Vec<i64> = sqlx::query_scalar(
                "SELECT tm.user_id FROM team_members tm JOIN teams t ON t.id = tm.team_id JOIN users o ON o.id = t.org_id WHERE o.username = $1::citext AND t.name = $2::citext",
            )
            .bind(org)
            .bind(team)
            .fetch_all(&self.pool)
            .await?;
            for id in ids {
                who.add(id, "team_mention");
                out.insert(id);
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    async fn thread_event(
        &self,
        repo: RepoId,
        issue: i64,
        actor: Option<i64>,
        reason_override: Option<&'static str>,
        watchers: bool,
        people: bool,
        text: TextFrom,
    ) -> Result<usize> {
        let (title, kind) = self.issue_title(issue).await?;
        if title.is_empty() {
            return Ok(0);
        }
        let mut who = Who::default();
        if people {
            self.thread_people(issue, &mut who).await?;
        }
        if watchers {
            who.add_all(self.watchers(repo.0).await?, "subscribed");
        }
        // A state change reads as "state change" for everyone except those who were singled out.
        if let Some(r) = reason_override {
            for v in who.people.values_mut() {
                if rank(v) > rank("author") {
                    *v = r;
                }
            }
        }
        let body = match text {
            TextFrom::None => None,
            TextFrom::Issue => {
                sqlx::query_scalar::<_, String>("SELECT title || E'\\n' || body FROM issues WHERE id = $1")
                    .bind(issue)
                    .fetch_optional(&self.pool)
                    .await?
            }
            TextFrom::IssueComment(id) => {
                sqlx::query_scalar("SELECT body FROM issue_comments WHERE id = $1").bind(id).fetch_optional(&self.pool).await?
            }
            TextFrom::Review(id) => {
                sqlx::query_scalar("SELECT body FROM reviews WHERE id = $1").bind(id).fetch_optional(&self.pool).await?
            }
            TextFrom::ReviewComment(id) => {
                sqlx::query_scalar("SELECT body FROM review_comments WHERE id = $1").bind(id).fetch_optional(&self.pool).await?
            }
        };
        let mentions = match body {
            Some(b) => self.mention_ids(&b, &mut who).await?,
            None => BTreeSet::new(),
        };
        self.write(&Subject { kind, id: issue, repo: repo.0, title: &title }, who, actor, &mentions).await
    }

    /// The filters, then the upserts.
    async fn write(&self, s: &Subject<'_>, who: Who, actor: Option<i64>, mentions: &BTreeSet<i64>) -> Result<usize> {
        let mut people = who.people;
        if let Some(a) = actor {
            people.remove(&a);
        }
        if people.is_empty() {
            return Ok(0);
        }
        let ids: Vec<i64> = people.keys().copied().collect();
        // Ignoring a repository silences it completely.
        let ignored: Vec<i64> =
            sqlx::query_scalar("SELECT user_id FROM watches WHERE repo_id = $1 AND level = 'ignore' AND user_id = ANY($2)")
                .bind(s.repo)
                .bind(&ids)
                .fetch_all(&self.pool)
                .await?;
        // A muted thread is quiet unless someone is mentioned in it.
        let muted: Vec<i64> = if s.kind == "issue" || s.kind == "pull" {
            sqlx::query_scalar("SELECT user_id FROM issue_subscriptions WHERE issue_id = $1 AND NOT subscribed AND user_id = ANY($2)")
                .bind(s.id)
                .bind(&ids)
                .fetch_all(&self.pool)
                .await?
        } else {
            Vec::new()
        };
        let live: BTreeSet<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1) AND kind = 'user' AND suspended_at IS NULL")
            .bind(&ids)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .collect();
        let mut written = 0;
        for (user, reason) in people {
            if ignored.contains(&user) || !live.contains(&user) || (muted.contains(&user) && !mentions.contains(&user)) {
                continue;
            }
            let as_user = Actor::User { id: UserId(user), is_admin: false, has_2fa: false, via: AuthVia::Session };
            // Anyone who can see the repository with an account (ch-4pn: fanout needs View).
            if self.authz.repo_permission(&as_user, RepoId(s.repo)).await? < Permission::View {
                continue;
            }
            sqlx::query(
                "INSERT INTO notifications (user_id, repo_id, reason, subject_type, subject_id, title, unread, done, updated_at) VALUES ($1, $2, $3, $4, $5, $6, true, false, now())
                 ON CONFLICT (user_id, subject_type, subject_id) DO UPDATE SET
                   -- While unread, a mention stays a mention: being assigned right after it
                   -- still reads as the mention. Everything else says what happened last.
                   reason = CASE WHEN notifications.unread AND NOT notifications.done
                                  AND notifications.reason IN ('mention', 'team_mention')
                                  AND EXCLUDED.reason NOT IN ('mention', 'team_mention')
                                 THEN notifications.reason ELSE EXCLUDED.reason END,
                   title = EXCLUDED.title, repo_id = EXCLUDED.repo_id, unread = true, done = false, updated_at = now()",
            )
            .bind(user)
            .bind(s.repo)
            .bind(reason)
            .bind(s.kind)
            .bind(s.id)
            .bind(s.title)
            .execute(&self.pool)
            .await?;
            crate::mail_queue::enqueue(&self.pool, self.mail_on, user, s.repo, reason, s.kind, s.id, s.title).await?;
            written += 1;
        }
        Ok(written)
    }
}

enum TextFrom {
    None,
    Issue,
    IssueComment(i64),
    Review(i64),
    ReviewComment(i64),
}
