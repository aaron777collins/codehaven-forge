//! Issue comments, reactions, the timeline, subscriptions and issue templates.

use crate::core::{Core, IssueRow, ghost};
use crate::util::{self, writing_user};
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::{Actor, CommentId, Error, IssueId, Permission, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::FromRow;
use std::collections::HashSet;

#[derive(Debug, Clone, FromRow)]
struct CommentRow {
    id: CommentId,
    issue_id: IssueId,
    author_id: UserId,
    body: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<CommentRow> for Comment {
    fn from(r: CommentRow) -> Self {
        Comment { id: r.id, issue_id: r.issue_id, author_id: r.author_id, body: r.body, created_at: r.created_at, updated_at: r.updated_at }
    }
}

type EventRow = (i64, Option<UserId>, String, Value, DateTime<Utc>);

const COMMENT_COLS: &str = "c.id, c.issue_id, c.author_id, c.body, c.created_at, c.updated_at";

impl Core {
    async fn comment_views_from(&self, rows: Vec<CommentRow>) -> Result<Vec<CommentView>> {
        let users: Vec<UserId> = rows.iter().map(|r| r.author_id).collect::<HashSet<_>>().into_iter().collect();
        let refs = self.user_refs(&users).await?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id.0).collect();
        let reactions = self.reaction_counts("issue_comment", &ids).await?;
        Ok(rows
            .into_iter()
            .map(|r| CommentView {
                author: refs.get(&r.author_id).cloned().unwrap_or_else(|| ghost(r.author_id)),
                reactions: reactions.get(&r.id.0).copied().unwrap_or_default(),
                comment: r.into(),
            })
            .collect())
    }

    pub async fn comment_views(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<CommentView>> {
        self.need_view(actor, repo).await?;
        let row = self.issue_row(repo, number).await?;
        let sql = format!("SELECT {COMMENT_COLS} FROM issue_comments c WHERE c.issue_id = $1 ORDER BY c.created_at, c.id");
        let rows = sqlx::query_as::<_, CommentRow>(&sql).bind(row.id).fetch_all(self.db()).await?;
        self.comment_views_from(rows).await
    }

    /// The comment, checked to belong to `repo`, with the issue it sits on.
    async fn comment_in_repo(&self, repo: RepoId, comment: CommentId) -> Result<(CommentRow, IssueRow)> {
        let sql = format!("SELECT {COMMENT_COLS} FROM issue_comments c WHERE c.id = $1");
        let c = sqlx::query_as::<_, CommentRow>(&sql).bind(comment).fetch_optional(self.db()).await?.ok_or(Error::NotFound)?;
        let issue = self.issue_row_by_id(c.issue_id).await?;
        if issue.repo_id != repo {
            return Err(Error::NotFound);
        }
        Ok((c, issue))
    }

    pub async fn comment_view(&self, actor: &Actor, repo: RepoId, comment: CommentId) -> Result<CommentView> {
        self.need_view(actor, repo).await?;
        let (c, _) = self.comment_in_repo(repo, comment).await?;
        self.comment_views_from(vec![c]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    pub async fn issue_comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Comment>> {
        Ok(self.comment_views(actor, repo, number).await?.into_iter().map(|c| c.comment).collect())
    }

    pub async fn issue_add_comment(&self, actor: &Actor, repo: RepoId, number: i64, body: &str, _client: &ClientInfo) -> Result<Comment> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let body = util::required_body("IssueComment", "body", body)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        if row.locked && perm < Permission::Triage {
            return Err(Error::forbidden("This conversation is locked. Only people with triage access can comment"));
        }
        let sql = format!(
            "WITH c AS (INSERT INTO issue_comments (issue_id, author_id, body) VALUES ($1, $2, $3) RETURNING *) SELECT {COMMENT_COLS} FROM c AS c"
        );
        let c = sqlx::query_as::<_, CommentRow>(&sql).bind(row.id).bind(user).bind(&body).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE issues SET comments_count = comments_count + 1, updated_at = now() WHERE id = $1")
            .bind(row.id)
            .execute(&mut *tx)
            .await?;
        Self::subscribe_default(&mut tx, row.id, user, "commented").await?;
        self.record_references(&mut tx, actor, &row, &rrow, &body).await?;
        self.d
            .events
            .emit(
                &mut tx,
                DomainEvent::CommentCreated { repo_id: repo, issue_id: row.id, number: row.number, comment_id: c.id, actor: user },
            )
            .await?;
        tx.commit().await?;
        Ok(c.into())
    }

    pub async fn issue_edit_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        comment: CommentId,
        body: &str,
        _client: &ClientInfo,
    ) -> Result<CommentView> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (c, issue) = self.comment_in_repo(repo, comment).await?;
        if c.author_id != user && perm < Permission::Triage {
            return Err(Error::forbidden("You can only edit your own comments"));
        }
        if issue.locked && perm < Permission::Triage {
            return Err(Error::forbidden("This conversation is locked"));
        }
        let body = util::required_body("IssueComment", "body", body)?;
        sqlx::query("UPDATE issue_comments SET body = $2, updated_at = now() WHERE id = $1")
            .bind(comment)
            .bind(&body)
            .execute(self.db())
            .await?;
        self.comment_view(actor, repo, comment).await
    }

    pub async fn issue_delete_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<()> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_read(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (c, issue) = self.comment_in_repo(repo, comment).await?;
        if c.author_id != user && perm < Permission::Triage {
            return Err(Error::forbidden("You can only delete your own comments"));
        }
        let mut tx = self.db().begin().await?;
        sqlx::query("DELETE FROM issue_comments WHERE id = $1").bind(comment).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM reactions WHERE subject_type = 'issue_comment' AND subject_id = $1")
            .bind(comment)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE issues SET comments_count = GREATEST(comments_count - 1, 0) WHERE id = $1")
            .bind(issue.id)
            .execute(&mut *tx)
            .await?;
        self.d
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::COMMENT_DELETED, Outcome::Success)
                    .target("issue_comment", comment)
                    .details(json!({ "repo_id": repo.0, "issue": issue.number, "author_id": c.author_id.0 })),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ------------------------------------------------------------ reactions

    /// `(subject_type, subject_id)` for a target, checking it belongs to `repo`.
    async fn reaction_subject(&self, repo: RepoId, target: ReactionTarget) -> Result<(&'static str, i64)> {
        match target {
            ReactionTarget::Issue(number) => Ok(("issue", self.issue_row(repo, number).await?.id.0)),
            ReactionTarget::IssueComment(id) => {
                self.comment_in_repo(repo, id).await?;
                Ok(("issue_comment", id.0))
            }
            ReactionTarget::ReviewComment(id) => {
                let r: Option<RepoId> =
                    sqlx::query_scalar("SELECT i.repo_id FROM review_comments c JOIN issues i ON i.id = c.pull_id WHERE c.id = $1")
                        .bind(id)
                        .fetch_optional(self.db())
                        .await?;
                if r != Some(repo) {
                    return Err(Error::NotFound);
                }
                Ok(("review_comment", id.0))
            }
        }
    }

    pub async fn react(
        &self,
        actor: &Actor,
        repo: RepoId,
        target: ReactionTarget,
        content: &str,
        _client: &ClientInfo,
    ) -> Result<ReactionRow> {
        let user = writing_user(actor, repo)?;
        self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        if !ReactionCounts::is_valid(content) {
            return Err(Error::invalid(
                "Reaction",
                "content",
                "content must be one of +1, -1, laugh, confused, heart, hooray, rocket, eyes",
            ));
        }
        let (kind, subject) = self.reaction_subject(repo, target).await?;
        let row: (i64, DateTime<Utc>) = sqlx::query_as(
            "INSERT INTO reactions (user_id, subject_type, subject_id, content) VALUES ($1, $2, $3, $4)
             ON CONFLICT (user_id, subject_type, subject_id, content) DO UPDATE SET content = EXCLUDED.content RETURNING id, created_at",
        )
        .bind(user)
        .bind(kind)
        .bind(subject)
        .bind(content)
        .fetch_one(self.db())
        .await?;
        Ok(ReactionRow { id: row.0, user: self.user_ref(user).await?, content: content.to_string(), created_at: row.1 })
    }

    pub async fn reactions(&self, actor: &Actor, repo: RepoId, target: ReactionTarget) -> Result<Vec<ReactionRow>> {
        self.need_view(actor, repo).await?;
        let (kind, subject) = self.reaction_subject(repo, target).await?;
        let rows: Vec<(i64, UserId, String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT id, user_id, content, created_at FROM reactions WHERE subject_type = $1 AND subject_id = $2 ORDER BY created_at, id",
        )
        .bind(kind)
        .bind(subject)
        .fetch_all(self.db())
        .await?;
        let ids: Vec<UserId> = rows.iter().map(|r| r.1).collect::<HashSet<_>>().into_iter().collect();
        let users = self.user_refs(&ids).await?;
        Ok(rows
            .into_iter()
            .map(|(id, u, content, created_at)| ReactionRow {
                id,
                user: users.get(&u).cloned().unwrap_or_else(|| ghost(u)),
                content,
                created_at,
            })
            .collect())
    }

    pub async fn unreact(&self, actor: &Actor, repo: RepoId, target: ReactionTarget, reaction_id: i64, _client: &ClientInfo) -> Result<()> {
        let user = writing_user(actor, repo)?;
        self.need_read(actor, repo).await?;
        let (kind, subject) = self.reaction_subject(repo, target).await?;
        let r = sqlx::query("DELETE FROM reactions WHERE id = $1 AND user_id = $2 AND subject_type = $3 AND subject_id = $4")
            .bind(reaction_id)
            .bind(user)
            .bind(kind)
            .bind(subject)
            .execute(self.db())
            .await?;
        if r.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    // ------------------------------------------------------------ timeline

    /// Comments and events in time order. Cross references from repositories the viewer cannot
    /// read are left out, so a private repository's issue titles never leak through a link.
    pub async fn timeline(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<TimelineItem>> {
        self.need_view(actor, repo).await?;
        let row = self.issue_row(repo, number).await?;
        let sql = format!("SELECT {COMMENT_COLS} FROM issue_comments c WHERE c.issue_id = $1 ORDER BY c.created_at, c.id");
        let comments = sqlx::query_as::<_, CommentRow>(&sql).bind(row.id).fetch_all(self.db()).await?;
        let comment_views = self.comment_views_from(comments).await?;
        let events: Vec<EventRow> =
            sqlx::query_as("SELECT id, actor_id, event, data, created_at FROM timeline_events WHERE issue_id = $1 ORDER BY created_at, id")
                .bind(row.id)
                .fetch_all(self.db())
                .await?;
        let actors: Vec<UserId> = events.iter().filter_map(|e| e.1).collect::<HashSet<_>>().into_iter().collect();
        let users = self.user_refs(&actors).await?;
        let source_repos: Vec<RepoId> =
            events.iter().filter_map(|e| e.3.get("source_repo_id").and_then(Value::as_i64)).map(RepoId).collect();
        let visible = self.readable(actor, source_repos).await?;

        let mut items: Vec<TimelineItem> = Vec::with_capacity(events.len() + comment_views.len());
        for (id, who, event, data, at) in events {
            if event == "cross-referenced"
                && let Some(r) = data.get("source_repo_id").and_then(Value::as_i64)
                && !visible.contains(&RepoId(r))
            {
                continue;
            }
            items.push(TimelineItem { id, at, actor: who.and_then(|u| users.get(&u).cloned()), kind: event, data, comment: None });
        }
        for c in comment_views {
            items.push(TimelineItem {
                id: c.comment.id.0,
                at: c.comment.created_at,
                actor: Some(c.author.clone()),
                kind: "comment".into(),
                data: Value::Null,
                comment: Some(c),
            });
        }
        items.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| (a.kind == "comment").cmp(&(b.kind == "comment"))).then(a.id.cmp(&b.id)));
        Ok(items)
    }

    // ------------------------------------------------------------ subscriptions

    pub async fn subscription(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Option<bool>> {
        let user = util::acting_user(actor)?;
        self.need_read(actor, repo).await?;
        let row = self.issue_row(repo, number).await?;
        Ok(sqlx::query_scalar("SELECT subscribed FROM issue_subscriptions WHERE issue_id = $1 AND user_id = $2")
            .bind(row.id)
            .bind(user)
            .fetch_optional(self.db())
            .await?)
    }

    pub async fn set_subscription(&self, actor: &Actor, repo: RepoId, number: i64, subscribed: bool) -> Result<()> {
        let user = writing_user(actor, repo)?;
        self.need_read(actor, repo).await?;
        let row = self.issue_row(repo, number).await?;
        sqlx::query(
            "INSERT INTO issue_subscriptions (issue_id, user_id, subscribed, reason) VALUES ($1, $2, $3, 'manual')
             ON CONFLICT (issue_id, user_id) DO UPDATE SET subscribed = EXCLUDED.subscribed, reason = 'manual'",
        )
        .bind(row.id)
        .bind(user)
        .bind(subscribed)
        .execute(self.db())
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------ templates

    pub async fn templates(&self, actor: &Actor, repo: RepoId) -> Result<Vec<IssueTemplate>> {
        self.need_view(actor, repo).await?;
        let r = self.repo(repo).await?;
        let mut out = Vec::new();
        for dir in [".codehaven/ISSUE_TEMPLATE", ".github/ISSUE_TEMPLATE"] {
            let Ok(entries) = self.d.git.tree(repo, &r.default_branch, dir).await else { continue };
            let wanted = |n: &str| {
                let n = n.to_lowercase();
                n.ends_with(".md") || n.ends_with(".yml") || n.ends_with(".yaml")
            };
            for e in entries.into_iter().filter(|e| e.kind == EntryKind::Blob && wanted(&e.name) && e.name != "config.yml").take(20) {
                let limit = if e.name.to_lowercase().ends_with(".md") { 16 * 1024 } else { ch_core::issue_form::MAX_FORM_BYTES + 1 };
                let Ok(blob) = self.d.git.blob(repo, &r.default_branch, &e.path, limit as u64).await else { continue };
                if blob.is_binary || blob.truncated {
                    continue;
                }
                let text = String::from_utf8_lossy(&blob.content).into_owned();
                if e.name.to_lowercase().ends_with(".md") {
                    out.push(parse_template(&e.name, &e.path, &text));
                } else {
                    // A form that cannot be read is left out rather than shown half made.
                    match ch_core::issue_form::parse(&e.name, &text) {
                        Ok(p) => out.push(IssueTemplate {
                            name: p.name,
                            about: p.about,
                            title: p.title,
                            labels: p.labels,
                            body: String::new(),
                            path: e.path.clone(),
                            form: Some(p.form),
                        }),
                        Err(why) => tracing::debug!(path = %e.path, %why, "an issue form could not be read"),
                    }
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        out.sort_by_key(|t| t.name.to_lowercase());
        Ok(out)
    }
}

/// A template is markdown with an optional front matter block:
/// `---`, lines like `name: Bug report`, `about: ...`, `title: [Bug] `, `labels: bug, triage`, `---`.
pub fn parse_template(file_name: &str, path: &str, text: &str) -> IssueTemplate {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (front, body) = match text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) {
        Some((f, b)) => (f.to_string(), b.trim_start_matches('\n').to_string()),
        None => (String::new(), text.clone()),
    };
    let mut name = file_name.trim_end_matches(".md").replace(['-', '_'], " ");
    let (mut about, mut title, mut labels) = (String::new(), String::new(), Vec::new());
    for line in front.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match k.trim() {
            "name" if !v.is_empty() => name = v,
            "about" | "description" => about = v,
            "title" => title = v,
            "labels" => {
                labels = v
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            _ => {}
        }
    }
    IssueTemplate { name, about, title, labels, body, path: path.to_string(), form: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_front_matter() {
        let t = parse_template(
            "bug_report.md",
            ".github/ISSUE_TEMPLATE/bug_report.md",
            "---\nname: Bug report\nabout: Something broke\ntitle: \"[Bug] \"\nlabels: bug, triage\n---\n\n## What happened\n",
        );
        assert_eq!(t.name, "Bug report");
        assert_eq!(t.about, "Something broke");
        assert_eq!(t.title, "[Bug] ");
        assert_eq!(t.labels, vec!["bug", "triage"]);
        assert!(t.body.starts_with("## What happened"));
    }

    #[test]
    fn template_without_front_matter_uses_file_name() {
        let t = parse_template("feature-request.md", "p", "Describe it\n");
        assert_eq!(t.name, "feature request");
        assert_eq!(t.body, "Describe it\n");
        assert!(t.labels.is_empty());
    }

    #[test]
    fn template_labels_in_brackets_and_crlf() {
        let t = parse_template("x.md", "p", "---\r\nname: X\r\nlabels: [a, \"b c\"]\r\n---\r\nbody\r\n");
        assert_eq!(t.labels, vec!["a", "b c"]);
        assert_eq!(t.body, "body\n");
    }
}
