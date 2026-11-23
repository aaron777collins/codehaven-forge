//! Reviews: pending and submitted reviews, inline comments on lines and ranges, threads and
//! resolution, suggestions, review requests and CODEOWNERS.

use crate::codeowners::{self, Owner};
use crate::core::{Core, IssueRow, ghost};
use crate::pulls::{PullRow, full_branch};
use crate::suggest;
use crate::util::{self, as_user, writing_user};
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::services::DbTx;
use ch_core::{Actor, CommentId, Error, FieldError, IssueId, Permission, PullId, RepoId, Result, ReviewId, TeamId, UserId};
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::FromRow;
use std::collections::{HashMap, HashSet};

pub fn parse_state(s: &str) -> ReviewState {
    match s {
        "approved" => ReviewState::Approved,
        "changes_requested" => ReviewState::ChangesRequested,
        "dismissed" => ReviewState::Dismissed,
        "pending" => ReviewState::Pending,
        _ => ReviewState::Commented,
    }
}

pub fn state_str(s: ReviewState) -> &'static str {
    match s {
        ReviewState::Pending => "pending",
        ReviewState::Commented => "commented",
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Dismissed => "dismissed",
    }
}

#[derive(Debug, Clone, FromRow)]
struct ReviewRow {
    id: ReviewId,
    pull_id: IssueId,
    author_id: UserId,
    state: String,
    body: String,
    commit_sha: String,
    submitted_at: Option<DateTime<Utc>>,
}

impl From<ReviewRow> for Review {
    fn from(r: ReviewRow) -> Self {
        Review {
            id: r.id,
            pull_id: PullId(r.pull_id.0),
            author_id: r.author_id,
            state: parse_state(&r.state),
            body: r.body,
            commit_sha: r.commit_sha,
            submitted_at: r.submitted_at,
        }
    }
}

const REVIEW_COLS: &str = "r.id, r.pull_id, r.author_id, r.state, r.body, r.commit_sha, r.submitted_at";

#[derive(Debug, Clone, FromRow)]
struct CommentRow {
    id: CommentId,
    pull_id: IssueId,
    review_id: Option<ReviewId>,
    thread_id: Option<i64>,
    author_id: UserId,
    in_reply_to: Option<CommentId>,
    path: String,
    side: String,
    line: Option<i32>,
    start_line: Option<i32>,
    original_line: Option<i32>,
    commit_sha: String,
    original_commit_sha: String,
    diff_hunk: String,
    body: String,
    outdated: bool,
    pending: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<CommentRow> for ReviewComment {
    fn from(r: CommentRow) -> Self {
        ReviewComment {
            id: r.id,
            pull_id: PullId(r.pull_id.0),
            review_id: r.review_id,
            thread_id: r.thread_id,
            author_id: r.author_id,
            in_reply_to: r.in_reply_to,
            path: r.path,
            side: r.side,
            line: r.line.map(|v| v as u32),
            start_line: r.start_line.map(|v| v as u32),
            original_line: r.original_line.map(|v| v as u32),
            commit_sha: r.commit_sha,
            original_commit_sha: r.original_commit_sha,
            diff_hunk: r.diff_hunk,
            body: r.body,
            outdated: r.outdated,
            pending: r.pending,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const COMMENT_SELECT: &str = "SELECT c.id, c.pull_id, c.review_id, c.thread_id, c.author_id, c.in_reply_to, c.path, c.side, c.line, c.start_line, c.original_line,
        c.commit_sha, c.original_commit_sha, c.diff_hunk, c.body, c.outdated, COALESCE(r.state = 'pending', false) AS pending, c.created_at, c.updated_at
     FROM review_comments c LEFT JOIN reviews r ON r.id = c.review_id";

// ---------------------------------------------------------------- pure diff helpers

/// Where a line of the old file ends up in the new file, or `None` when the line itself was
/// changed or removed. Hunks come from `diff old new` for one file.
pub fn map_line(hunks: &[Hunk], line: u32) -> Option<u32> {
    let mut offset: i64 = 0;
    for h in hunks {
        let old_nos: Vec<u32> = h.lines.iter().filter_map(|l| l.old_no).collect();
        let (Some(first), Some(last)) = (old_nos.iter().min().copied(), old_nos.iter().max().copied()) else {
            // A hunk that only adds lines still shifts what comes after it.
            let added = h.lines.iter().filter(|l| l.kind == LineKind::Added).count() as i64;
            let new_first = h.lines.iter().filter_map(|l| l.new_no).min();
            if let Some(nf) = new_first
                && (nf as i64) <= line as i64 + offset
            {
                offset += added;
            }
            continue;
        };
        if line < first {
            break;
        }
        if line <= last {
            return h.lines.iter().find(|l| l.old_no == Some(line) && l.kind == LineKind::Context).and_then(|l| l.new_no);
        }
        let added = h.lines.iter().filter(|l| l.kind == LineKind::Added).count() as i64;
        let removed = h.lines.iter().filter(|l| l.kind == LineKind::Removed).count() as i64;
        offset += added - removed;
    }
    let v = line as i64 + offset;
    (v >= 1).then_some(v as u32)
}

/// The diff hunk text a review comment keeps, ending at the commented line.
pub fn hunk_text(file: &FileDiff, left: bool, line: u32) -> Option<String> {
    for h in &file.hunks {
        let hit = h.lines.iter().position(|l| {
            if left { l.old_no == Some(line) && l.kind != LineKind::Added } else { l.new_no == Some(line) && l.kind != LineKind::Removed }
        });
        if let Some(i) = hit {
            let from = i.saturating_sub(7);
            let mut out = String::new();
            out.push_str(&h.header);
            for l in &h.lines[from..=i] {
                out.push('\n');
                out.push(match l.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                });
                out.push_str(&l.text);
            }
            return Some(out);
        }
    }
    None
}

fn find_file<'a>(diff: &'a Diff, path: &str, left: bool) -> Option<&'a FileDiff> {
    diff.files.iter().find(|f| if left { f.old_path.as_deref() == Some(path) } else { f.new_path.as_deref() == Some(path) })
}

fn side_of(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "right" | "" => Ok(false),
        "left" => Ok(true),
        _ => Err(Error::invalid("PullRequestReviewComment", "side", "side must be left or right")),
    }
}

/// A comment checked against the diff, ready to insert.
struct Placed {
    path: String,
    side: &'static str,
    line: u32,
    start_line: Option<u32>,
    hunk: String,
    body: String,
    reply_to: Option<CommentId>,
    thread: Option<i64>,
}

impl Core {
    // ------------------------------------------------------------ views

    async fn review_views_from(&self, rows: Vec<ReviewRow>, base_repo: RepoId, viewer: Option<UserId>) -> Result<Vec<ReviewView>> {
        let rows: Vec<ReviewRow> = rows.into_iter().filter(|r| r.state != "pending" || Some(r.author_id) == viewer).collect();
        let users: Vec<UserId> = rows.iter().map(|r| r.author_id).collect::<HashSet<_>>().into_iter().collect();
        let refs = self.user_refs(&users).await?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id.0).collect();
        let counts: HashMap<i64, i64> = if ids.is_empty() {
            HashMap::new()
        } else {
            sqlx::query_as::<_, (i64, i64)>("SELECT review_id, count(*) FROM review_comments WHERE review_id = ANY($1) GROUP BY review_id")
                .bind(&ids)
                .fetch_all(self.db())
                .await?
                .into_iter()
                .collect()
        };
        let mut perms: HashMap<UserId, (bool, String)> = HashMap::new();
        let mut out = Vec::new();
        for r in rows {
            let (counts_toward, association) = match perms.get(&r.author_id) {
                Some(c) => c.clone(),
                None => {
                    let c = self.permission(&as_user(r.author_id), base_repo).await? >= Permission::Write;
                    let a = self.association(base_repo, r.author_id, c).await?;
                    perms.insert(r.author_id, (c, a.clone()));
                    (c, a)
                }
            };
            let n = counts.get(&r.id.0).copied().unwrap_or(0) as u32;
            let author = refs.get(&r.author_id).cloned().unwrap_or_else(|| ghost(r.author_id));
            out.push(ReviewView { review: r.into(), author, counts: counts_toward, comment_count: n, association });
        }
        Ok(out)
    }

    /// GitHub's `author_association` of `user` on `repo` (ch-yci): `OWNER` for the owner (or an
    /// owner of the owning organization), `MEMBER` for a member of the owning organization,
    /// `COLLABORATOR` for an explicit collaborator or anyone else who can write, else `NONE`.
    pub async fn association(&self, repo: RepoId, user: UserId, can_write: bool) -> Result<String> {
        let row: Option<(bool, Option<String>, bool)> = sqlx::query_as(
            "SELECT r.owner_id = $2,
                    (SELECT m.role FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = $2),
                    EXISTS (SELECT 1 FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $2)
             FROM repositories r WHERE r.id = $1",
        )
        .bind(repo.0)
        .bind(user.0)
        .fetch_optional(self.db())
        .await?;
        let Some((owner, org_role, collaborator)) = row else { return Ok("NONE".into()) };
        Ok(match (owner, org_role.as_deref()) {
            (true, _) | (false, Some("owner")) => "OWNER",
            (false, Some(_)) => "MEMBER",
            _ if collaborator || can_write => "COLLABORATOR",
            _ => "NONE",
        }
        .into())
    }

    pub async fn review_views(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ReviewView>> {
        self.need_view(actor, repo).await?;
        let (issue, _) = self.pull_pair(repo, number).await?;
        let sql = format!("SELECT {REVIEW_COLS} FROM reviews r WHERE r.pull_id = $1 ORDER BY COALESCE(r.submitted_at, r.created_at), r.id");
        let rows = sqlx::query_as::<_, ReviewRow>(&sql).bind(issue.id).fetch_all(self.db()).await?;
        self.review_views_from(rows, repo, actor.author_id()).await
    }

    pub async fn review_list(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Review>> {
        Ok(self.review_views(actor, repo, number).await?.into_iter().map(|v| v.review).collect())
    }

    async fn comment_views_for(&self, rows: Vec<CommentRow>) -> Result<Vec<ReviewCommentView>> {
        let users: Vec<UserId> = rows.iter().map(|r| r.author_id).collect::<HashSet<_>>().into_iter().collect();
        let refs = self.user_refs(&users).await?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id.0).collect();
        let reactions = self.reaction_counts("review_comment", &ids).await?;
        Ok(rows
            .into_iter()
            .map(|r| ReviewCommentView {
                author: refs.get(&r.author_id).cloned().unwrap_or_else(|| ghost(r.author_id)),
                reactions: reactions.get(&r.id.0).copied().unwrap_or_default(),
                has_suggestion: suggest::extract(&r.body).is_some() && r.side == "right",
                comment: r.into(),
            })
            .collect())
    }

    async fn comment_row(&self, repo: RepoId, comment: CommentId) -> Result<(CommentRow, IssueRow)> {
        let sql = format!("{COMMENT_SELECT} WHERE c.id = $1");
        let c = sqlx::query_as::<_, CommentRow>(&sql).bind(comment).fetch_optional(self.db()).await?.ok_or(Error::NotFound)?;
        let issue = self.issue_row_by_id(c.pull_id).await?;
        if issue.repo_id != repo {
            return Err(Error::NotFound);
        }
        Ok((c, issue))
    }

    pub async fn review_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId) -> Result<ReviewCommentView> {
        self.need_view(actor, repo).await?;
        let (c, _) = self.comment_row(repo, comment).await?;
        // Someone else's pending comment does not exist as far as this caller is concerned.
        if c.pending && Some(c.author_id) != actor.author_id() {
            return Err(Error::NotFound);
        }
        self.comment_views_for(vec![c]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    pub async fn review_threads(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ThreadView>> {
        self.need_view(actor, repo).await?;
        let (issue, _) = self.pull_pair(repo, number).await?;
        let viewer = actor.author_id();
        let sql = format!(
            "{COMMENT_SELECT} WHERE c.pull_id = $1 AND (r.id IS NULL OR r.state <> 'pending' OR r.author_id = $2) ORDER BY c.created_at, c.id"
        );
        let rows = sqlx::query_as::<_, CommentRow>(&sql).bind(issue.id).bind(viewer).fetch_all(self.db()).await?;
        let views = self.comment_views_for(rows).await?;
        let threads: Vec<(i64, String, bool, Option<UserId>)> =
            sqlx::query_as("SELECT id, path, resolved, resolved_by FROM review_threads WHERE pull_id = $1 ORDER BY created_at, id")
                .bind(issue.id)
                .fetch_all(self.db())
                .await?;
        let resolvers: Vec<UserId> = threads.iter().filter_map(|t| t.3).collect();
        let refs = self.user_refs(&resolvers).await?;
        let mut out = Vec::new();
        for (id, path, resolved, by) in threads {
            let comments: Vec<ReviewCommentView> = views.iter().filter(|c| c.comment.thread_id == Some(id)).cloned().collect();
            if comments.is_empty() {
                continue;
            }
            let outdated = comments.first().is_some_and(|c| c.comment.outdated);
            out.push(ThreadView { id, path, resolved, resolved_by: by.and_then(|u| refs.get(&u).cloned()), outdated, comments });
        }
        Ok(out)
    }

    // ------------------------------------------------------------ placing comments

    /// Checks each new comment against the diff of the pull request and builds the hunk.
    async fn place_comments(&self, issue: &IssueRow, pull: &PullRow, comments: &[NewReviewComment]) -> Result<Vec<Placed>> {
        if comments.is_empty() {
            return Ok(Vec::new());
        }
        if comments.len() > 100 {
            return Err(Error::invalid("PullRequestReview", "comments", "A review can hold at most 100 inline comments at once"));
        }
        let mut diff: Option<Diff> = None;
        let mut out = Vec::new();
        for c in comments {
            let body = util::required_body("PullRequestReviewComment", "body", &c.body)?;
            if let Some(parent) = c.in_reply_to {
                let sql = format!("{COMMENT_SELECT} WHERE c.id = $1 AND c.pull_id = $2");
                let p = sqlx::query_as::<_, CommentRow>(&sql)
                    .bind(parent)
                    .bind(issue.id)
                    .fetch_optional(self.db())
                    .await?
                    .ok_or_else(|| Error::Validation(vec![FieldError::new("PullRequestReviewComment", "in_reply_to", "invalid")]))?;
                let root_row = match p.in_reply_to {
                    Some(root) => {
                        sqlx::query_as::<_, CommentRow>(&format!("{COMMENT_SELECT} WHERE c.id = $1"))
                            .bind(root)
                            .fetch_one(self.db())
                            .await?
                    }
                    None => p,
                };
                out.push(Placed {
                    path: root_row.path.clone(),
                    side: if root_row.side == "left" { "left" } else { "right" },
                    line: root_row.line.or(root_row.original_line).unwrap_or(0) as u32,
                    start_line: root_row.start_line.map(|v| v as u32),
                    hunk: root_row.diff_hunk.clone(),
                    body,
                    reply_to: Some(root_row.id),
                    thread: root_row.thread_id,
                });
                continue;
            }
            let left = side_of(&c.side)?;
            let path = c.path.trim().to_string();
            if path.is_empty() {
                return Err(Error::Validation(vec![FieldError::new("PullRequestReviewComment", "path", "missing_field")]));
            }
            if c.line == 0 || c.start_line.is_some_and(|s| s == 0 || s > c.line) {
                return Err(Error::invalid("PullRequestReviewComment", "line", "line numbers start at 1 and a range must run forward"));
            }
            if diff.is_none() {
                diff = Some(self.pull_compare(issue, pull).await?.diff);
            }
            let d = diff.as_ref().ok_or(Error::NotFound)?;
            let file = find_file(d, &path, left).ok_or_else(|| {
                Error::Validation(vec![FieldError::custom("PullRequestReviewComment", "path", "This file is not part of the pull request")])
            })?;
            if file.collapsed || file.binary {
                return Err(Error::invalid("PullRequestReviewComment", "path", "This file's diff is too large or binary to comment on"));
            }
            let hunk = hunk_text(file, left, c.line).ok_or_else(|| {
                Error::Validation(vec![FieldError::custom("PullRequestReviewComment", "line", "The line is not part of the diff")])
            })?;
            if let Some(s) = c.start_line
                && hunk_text(file, left, s).is_none()
            {
                return Err(Error::Validation(vec![FieldError::custom(
                    "PullRequestReviewComment",
                    "start_line",
                    "The start line is not part of the diff",
                )]));
            }
            out.push(Placed {
                path,
                side: if left { "left" } else { "right" },
                line: c.line,
                start_line: c.start_line,
                hunk,
                body,
                reply_to: None,
                thread: None,
            });
        }
        Ok(out)
    }

    async fn insert_comment(
        &self,
        tx: &mut DbTx<'_>,
        issue: &IssueRow,
        pull: &PullRow,
        user: UserId,
        review: Option<ReviewId>,
        p: &Placed,
    ) -> Result<CommentId> {
        let thread = match p.thread {
            Some(t) => t,
            None => {
                sqlx::query_scalar("INSERT INTO review_threads (pull_id, path) VALUES ($1, $2) RETURNING id")
                    .bind(issue.id)
                    .bind(&p.path)
                    .fetch_one(&mut **tx)
                    .await?
            }
        };
        let id: CommentId = sqlx::query_scalar(
            "INSERT INTO review_comments (pull_id, review_id, thread_id, author_id, in_reply_to, path, side, line, start_line, original_line, commit_sha, original_commit_sha, diff_hunk, body)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $8, $10, $10, $11, $12) RETURNING id",
        )
        .bind(issue.id)
        .bind(review)
        .bind(thread)
        .bind(user)
        .bind(p.reply_to)
        .bind(&p.path)
        .bind(p.side)
        .bind(p.line as i32)
        .bind(p.start_line.map(|v| v as i32))
        .bind(&pull.head_sha)
        .bind(&p.hunk)
        .bind(&p.body)
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    // ------------------------------------------------------------ reviews

    async fn pending_review_of(&self, tx: &mut DbTx<'_>, pull: IssueId, user: UserId) -> Result<Option<ReviewRow>> {
        let sql =
            format!("SELECT {REVIEW_COLS} FROM reviews r WHERE r.pull_id = $1 AND r.author_id = $2 AND r.state = 'pending' FOR UPDATE");
        Ok(sqlx::query_as::<_, ReviewRow>(&sql).bind(pull).bind(user).fetch_optional(&mut **tx).await?)
    }

    fn check_decision(issue: &IssueRow, pull: &PullRow, user: UserId, state: ReviewState, body: &str, has_comments: bool) -> Result<()> {
        match state {
            ReviewState::Approved | ReviewState::ChangesRequested => {
                if issue.author_id == user {
                    return Err(Error::invalid(
                        "PullRequestReview",
                        "event",
                        "You cannot approve or request changes on your own pull request",
                    ));
                }
                if !issue.open || pull.merged_at.is_some() {
                    return Err(Error::invalid("PullRequestReview", "event", "Only an open pull request can be approved or sent back"));
                }
                if state == ReviewState::ChangesRequested && body.trim().is_empty() && !has_comments {
                    return Err(Error::Validation(vec![FieldError::new("PullRequestReview", "body", "missing_field")]));
                }
            }
            ReviewState::Commented => {
                if body.trim().is_empty() && !has_comments {
                    return Err(Error::Validation(vec![FieldError::new("PullRequestReview", "body", "missing_field")]));
                }
            }
            ReviewState::Pending | ReviewState::Dismissed => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn review_submit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        state: ReviewState,
        body: &str,
        comments: Vec<NewReviewComment>,
        _client: &ClientInfo,
    ) -> Result<Review> {
        let user = writing_user(actor, repo)?;
        self.need_contribute(actor, repo).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        if state == ReviewState::Dismissed {
            return Err(Error::invalid("PullRequestReview", "event", "A review cannot be submitted as dismissed"));
        }
        let (issue, pull) = self.pull_pair(repo, number).await?;
        let body = util::body("PullRequestReview", "body", body)?;
        let placed = self.place_comments(&issue, &pull, &comments).await?;
        if state != ReviewState::Pending {
            // A pending review of the caller's own is finished by this call, so its comments count.
            let existing: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM review_comments c JOIN reviews r ON r.id = c.review_id WHERE r.pull_id = $1 AND r.author_id = $2 AND r.state = 'pending'",
            )
            .bind(issue.id)
            .bind(user)
            .fetch_one(self.db())
            .await?;
            Self::check_decision(&issue, &pull, user, state, &body, !placed.is_empty() || existing > 0)?;
        }
        let mut tx = self.db().begin().await?;
        let review = match self.pending_review_of(&mut tx, issue.id, user).await? {
            Some(r) => r,
            None => {
                let sql = format!(
                    "WITH r AS (INSERT INTO reviews (pull_id, author_id, state, body, commit_sha) VALUES ($1, $2, 'pending', '', $3) RETURNING *) SELECT {REVIEW_COLS} FROM r AS r"
                );
                sqlx::query_as::<_, ReviewRow>(&sql).bind(issue.id).bind(user).bind(&pull.head_sha).fetch_one(&mut *tx).await?
            }
        };
        for p in &placed {
            self.insert_comment(&mut tx, &issue, &pull, user, Some(review.id), p).await?;
        }
        if state == ReviewState::Pending {
            sqlx::query("UPDATE reviews SET body = $2 WHERE id = $1").bind(review.id).bind(&body).execute(&mut *tx).await?;
            tx.commit().await?;
            return self.review_by_id(review.id).await;
        }
        self.finish_review(&mut tx, actor, &issue, &pull, user, review.id, state, &body).await?;
        tx.commit().await?;
        self.review_by_id(review.id).await
    }

    async fn review_by_id(&self, id: ReviewId) -> Result<Review> {
        let sql = format!("SELECT {REVIEW_COLS} FROM reviews r WHERE r.id = $1");
        Ok(sqlx::query_as::<_, ReviewRow>(&sql).bind(id).fetch_optional(self.db()).await?.ok_or(Error::NotFound)?.into())
    }

    /// Turns a pending review into a submitted one and announces it, in the caller's transaction.
    #[allow(clippy::too_many_arguments)]
    async fn finish_review(
        &self,
        tx: &mut DbTx<'_>,
        actor: &Actor,
        issue: &IssueRow,
        pull: &PullRow,
        user: UserId,
        review: ReviewId,
        state: ReviewState,
        body: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE reviews SET state = $2, body = $3, commit_sha = $4, submitted_at = now() WHERE id = $1")
            .bind(review)
            .bind(state_str(state))
            .bind(body)
            .bind(&pull.head_sha)
            .execute(&mut **tx)
            .await?;
        // A review answers the request for one.
        sqlx::query("DELETE FROM review_requests WHERE pull_id = $1 AND user_id = $2").bind(issue.id).bind(user).execute(&mut **tx).await?;
        Self::subscribe_default(tx, issue.id, user, "reviewing").await?;
        Self::touch(tx, issue.id).await?;
        if !body.trim().is_empty() {
            let rrow = self.repo(issue.repo_id).await?;
            self.record_references(tx, actor, issue, &rrow, body).await?;
        }
        let pid = PullId(issue.id.0);
        self.d
            .events
            .emit(
                tx,
                DomainEvent::ReviewSubmitted {
                    repo_id: issue.repo_id,
                    pull_id: pid,
                    number: issue.number,
                    review_id: review,
                    state: state_str(state).into(),
                    actor: user,
                },
            )
            .await?;
        let ids: Vec<CommentId> =
            sqlx::query_scalar("SELECT id FROM review_comments WHERE review_id = $1 ORDER BY id").bind(review).fetch_all(&mut **tx).await?;
        for c in ids {
            self.d
                .events
                .emit(
                    tx,
                    DomainEvent::ReviewCommentCreated {
                        repo_id: issue.repo_id,
                        pull_id: pid,
                        number: issue.number,
                        comment_id: c,
                        actor: user,
                    },
                )
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn review_submit_pending(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        state: ReviewState,
        body: &str,
        _client: &ClientInfo,
    ) -> Result<Review> {
        let user = writing_user(actor, repo)?;
        self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        if matches!(state, ReviewState::Pending | ReviewState::Dismissed) {
            return Err(Error::invalid("PullRequestReview", "event", "event must be COMMENT, APPROVE or REQUEST_CHANGES"));
        }
        let (issue, pull) = self.pull_pair(repo, number).await?;
        let body = util::body("PullRequestReview", "body", body)?;
        let mut tx = self.db().begin().await?;
        let r = self.pending_review_of(&mut tx, issue.id, user).await?.filter(|r| r.id == review).ok_or(Error::NotFound)?;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM review_comments WHERE review_id = $1").bind(r.id).fetch_one(&mut *tx).await?;
        Self::check_decision(&issue, &pull, user, state, &body, n > 0)?;
        self.finish_review(&mut tx, actor, &issue, &pull, user, r.id, state, &body).await?;
        tx.commit().await?;
        self.review_by_id(review).await
    }

    pub async fn review_delete_pending(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        _client: &ClientInfo,
    ) -> Result<()> {
        let user = writing_user(actor, repo)?;
        self.need_read(actor, repo).await?;
        let (issue, _) = self.pull_pair(repo, number).await?;
        let mut tx = self.db().begin().await?;
        let r = self.pending_review_of(&mut tx, issue.id, user).await?.filter(|r| r.id == review).ok_or(Error::NotFound)?;
        sqlx::query("DELETE FROM review_comments WHERE review_id = $1").bind(r.id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM reviews WHERE id = $1").bind(r.id).execute(&mut *tx).await?;
        Self::delete_empty_threads(&mut tx, issue.id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_empty_threads(tx: &mut DbTx<'_>, pull: IssueId) -> Result<()> {
        sqlx::query(
            "DELETE FROM review_threads t WHERE t.pull_id = $1 AND NOT EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id)",
        )
        .bind(pull)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn review_dismiss(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        message: &str,
        client: &ClientInfo,
    ) -> Result<Review> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (issue, _) = self.pull_pair(repo, number).await?;
        let message = util::required_body("PullRequestReview", "message", message)?;
        let mut tx = self.db().begin().await?;
        let sql = format!("SELECT {REVIEW_COLS} FROM reviews r WHERE r.id = $1 AND r.pull_id = $2 FOR UPDATE");
        let r = sqlx::query_as::<_, ReviewRow>(&sql).bind(review).bind(issue.id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
        if !matches!(parse_state(&r.state), ReviewState::Approved | ReviewState::ChangesRequested) {
            return Err(Error::invalid("PullRequestReview", "state", "Only an approval or a change request can be dismissed"));
        }
        sqlx::query("UPDATE reviews SET state = 'dismissed' WHERE id = $1").bind(review).execute(&mut *tx).await?;
        Self::add_event(
            &mut tx,
            issue.id,
            Some(user),
            "review_dismissed",
            json!({ "review_id": review.0, "reviewer_id": r.author_id.0, "message": message }),
        )
        .await?;
        self.d
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::REVIEW_DISMISSED, Outcome::Success)
                    .target("review", review)
                    .details(json!({ "repo_id": repo.0, "number": number, "reviewer_id": r.author_id.0 })),
            )
            .await?;
        Self::touch(&mut tx, issue.id).await?;
        tx.commit().await?;
        self.review_by_id(review).await
    }

    // ------------------------------------------------------------ inline comments

    #[allow(clippy::too_many_arguments)]
    pub async fn review_add_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        comment: NewReviewComment,
        review: Option<ReviewId>,
        _client: &ClientInfo,
    ) -> Result<ReviewCommentView> {
        let user = writing_user(actor, repo)?;
        self.need_contribute(actor, repo).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        let perm = self.permission(actor, repo).await?;
        if issue.locked && perm < Permission::Triage {
            return Err(Error::forbidden("This conversation is locked"));
        }
        let placed = self.place_comments(&issue, &pull, std::slice::from_ref(&comment)).await?;
        let p = placed.first().ok_or(Error::NotFound)?;
        let mut tx = self.db().begin().await?;
        if let Some(rid) = review {
            self.pending_review_of(&mut tx, issue.id, user).await?.filter(|r| r.id == rid).ok_or(Error::NotFound)?;
        }
        let id = self.insert_comment(&mut tx, &issue, &pull, user, review, p).await?;
        if review.is_none() {
            Self::subscribe_default(&mut tx, issue.id, user, "commented").await?;
            Self::touch(&mut tx, issue.id).await?;
            self.record_references(&mut tx, actor, &issue, &rrow, &p.body).await?;
            self.d
                .events
                .emit(
                    &mut tx,
                    DomainEvent::ReviewCommentCreated { repo_id: repo, pull_id: PullId(issue.id.0), number, comment_id: id, actor: user },
                )
                .await?;
        }
        tx.commit().await?;
        self.review_comment(actor, repo, id).await
    }

    pub async fn review_edit_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        comment: CommentId,
        body: &str,
        _client: &ClientInfo,
    ) -> Result<ReviewCommentView> {
        let user = writing_user(actor, repo)?;
        self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (c, _) = self.comment_row(repo, comment).await?;
        if c.author_id != user {
            return Err(Error::forbidden("You can only edit your own comments"));
        }
        let body = util::required_body("PullRequestReviewComment", "body", body)?;
        sqlx::query("UPDATE review_comments SET body = $2, updated_at = now() WHERE id = $1")
            .bind(comment)
            .bind(&body)
            .execute(self.db())
            .await?;
        self.review_comment(actor, repo, comment).await
    }

    pub async fn review_delete_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<()> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_read(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (c, issue) = self.comment_row(repo, comment).await?;
        if c.author_id != user && perm < Permission::Write {
            return Err(Error::forbidden("You can only delete your own comments"));
        }
        let mut tx = self.db().begin().await?;
        sqlx::query("DELETE FROM reactions WHERE subject_type = 'review_comment' AND subject_id = $1")
            .bind(comment)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM review_comments WHERE id = $1").bind(comment).execute(&mut *tx).await?;
        Self::delete_empty_threads(&mut tx, issue.id).await?;
        if c.author_id != user {
            self.d
                .audit
                .record(
                    &mut tx,
                    AuditEvent::new(actor, client, action::COMMENT_DELETED, Outcome::Success)
                        .target("review_comment", comment)
                        .details(json!({ "repo_id": repo.0, "author_id": c.author_id.0 })),
                )
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn review_resolve_thread(
        &self,
        actor: &Actor,
        repo: RepoId,
        thread: i64,
        resolved: bool,
        _client: &ClientInfo,
    ) -> Result<()> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let row: Option<(IssueId,)> =
            sqlx::query_as("SELECT pull_id FROM review_threads WHERE id = $1").bind(thread).fetch_optional(self.db()).await?;
        let issue = self.issue_row_by_id(row.ok_or(Error::NotFound)?.0).await?;
        if issue.repo_id != repo {
            return Err(Error::NotFound);
        }
        if issue.author_id != user && perm < Permission::Write {
            return Err(Error::forbidden("Only the author of the pull request or someone with write access can resolve a conversation"));
        }
        sqlx::query("UPDATE review_threads SET resolved = $2, resolved_by = CASE WHEN $2 THEN $3::bigint ELSE NULL END WHERE id = $1")
            .bind(thread)
            .bind(resolved)
            .bind(user)
            .execute(self.db())
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------ suggestions

    pub async fn review_apply_suggestion(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<String> {
        let user = writing_user(actor, repo)?;
        self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (c, issue) = self.comment_row(repo, comment).await?;
        if c.pending {
            return Err(Error::invalid("PullRequestReviewComment", "state", "Submit the review before applying its suggestions"));
        }
        let pull = self.pull_row(issue.id).await?;
        if !issue.open || pull.merged_at.is_some() {
            return Err(Error::invalid("PullRequestReviewComment", "state", "Suggestions can only be applied to an open pull request"));
        }
        let head_repo = pull.head_repo();
        let may = self.permission(actor, head_repo).await? >= Permission::Write
            || (pull.is_fork() && pull.maintainer_can_modify && self.permission(actor, repo).await? >= Permission::Write);
        if !may {
            return Err(Error::forbidden("You need write access to the head branch to apply a suggestion"));
        }
        let Some(lines) = suggest::extract(&c.body).filter(|_| c.side == "right") else {
            return Err(Error::invalid("PullRequestReviewComment", "body", "This comment has no suggestion to apply"));
        };
        if c.outdated || c.commit_sha != pull.head_sha {
            return Err(Error::conflict("This suggestion is out of date because the code changed. Ask for a new one"));
        }
        let end = c.line.unwrap_or(0).max(0) as u32;
        let start = c.start_line.map(|v| v as u32).unwrap_or(end);
        let blob = self.d.git.blob(head_repo, &pull.head_sha, &c.path, 4 * 1024 * 1024).await?;
        if blob.is_binary || blob.truncated {
            return Err(Error::invalid("PullRequestReviewComment", "path", "This file cannot be edited from a suggestion"));
        }
        let text = String::from_utf8(blob.content)
            .map_err(|_| Error::invalid("PullRequestReviewComment", "path", "This file is not valid UTF-8"))?;
        let new_text = suggest::apply(&text, start, end, &lines)
            .map_err(|e| Error::conflict(format!("The suggestion no longer fits the file: {e}")))?;
        let (name, email) = self.identity(user).await?;
        let reviewer = self.user_ref(c.author_id).await?;
        let (rname, remail) = self.identity(c.author_id).await?;
        let sig = Signature { name, email, at: Utc::now() };
        let req = CommitFileChange {
            repo_id: head_repo,
            branch: full_branch(&pull.head_ref),
            new_branch: None,
            expected_sha: Some(pull.head_sha.clone()),
            changes: vec![FileChange { path: c.path.clone(), content: Some(new_text.into_bytes()), renamed_from: None }],
            message: format!("Apply suggestion from @{}\n\nCo-authored-by: {} <{}>", reviewer.username, rname, remail),
            author: sig,
        };
        let new_sha = self.d.git.commit_file_change(actor, req, client).await?;
        let mut tx = self.db().begin().await?;
        sqlx::query("UPDATE pull_requests SET head_sha = $2, mergeable_state = 'unknown' WHERE issue_id = $1")
            .bind(issue.id)
            .bind(&new_sha)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE review_comments SET outdated = true WHERE id = $1").bind(comment).execute(&mut *tx).await?;
        if let Some(t) = c.thread_id {
            sqlx::query("UPDATE review_threads SET resolved = true, resolved_by = $2 WHERE id = $1")
                .bind(t)
                .bind(user)
                .execute(&mut *tx)
                .await?;
        }
        Self::add_event(
            &mut tx,
            issue.id,
            Some(user),
            "head_ref_updated",
            json!({ "before": pull.head_sha, "after": new_sha, "reason": "suggestion" }),
        )
        .await?;
        self.d
            .events
            .emit(
                &mut tx,
                DomainEvent::PrSynchronized {
                    repo_id: repo,
                    pull_id: PullId(issue.id.0),
                    number: issue.number,
                    head_sha: new_sha.clone(),
                    // A connected app applying a suggestion is still an app's commit.
                    app: actor.app_origin(repo),
                    pusher: Some(user),
                    // The server made this exact commit for this person, on top of a head they
                    // did not necessarily write. CI counts them only if that head was itself
                    // let run or approved (ch-x12).
                    origin_known: true,
                    built_on: Some(pull.head_sha.clone()),
                },
            )
            .await?;
        tx.commit().await?;
        Ok(new_sha)
    }

    // ------------------------------------------------------------ review requests

    pub async fn review_requested(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<RequestedReviewers> {
        self.need_view(actor, repo).await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        Ok(self.hydrate_pull(issue, pull).await?.requested)
    }

    pub async fn review_request(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        _client: &ClientInfo,
    ) -> Result<()> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_read(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        if issue.author_id != user && perm < Permission::Triage {
            return Err(Error::forbidden("Only the author or someone with triage access can request reviews"));
        }
        if !issue.open || pull.merged_at.is_some() {
            return Err(Error::invalid("PullRequest", "reviewers", "Only an open pull request can have review requests"));
        }
        let mut tx = self.db().begin().await?;
        for u in users {
            if u == issue.author_id {
                return Err(Error::invalid("PullRequest", "reviewers", "The author cannot be asked to review their own pull request"));
            }
            if self.permission(&as_user(u), repo).await? < Permission::Read {
                return Err(Error::invalid("PullRequest", "reviewers", "That person cannot see this repository"));
            }
            self.request_user_in_tx(&mut tx, &issue, Some(user), u).await?;
        }
        for t in teams {
            let name: Option<(String, String)> =
                sqlx::query_as("SELECT o.username::text, t.name::text FROM teams t JOIN users o ON o.id = t.org_id WHERE t.id = $1")
                    .bind(t)
                    .fetch_optional(&mut *tx)
                    .await?;
            let (org, team) = name.ok_or_else(|| Error::Validation(vec![FieldError::new("PullRequest", "team_reviewers", "invalid")]))?;
            self.request_team_in_tx(&mut tx, &issue, Some(user), t, &org, &team).await?;
        }
        Self::touch(&mut tx, issue.id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn request_user_in_tx(&self, tx: &mut DbTx<'_>, issue: &IssueRow, by: Option<UserId>, who: UserId) -> Result<bool> {
        let r = sqlx::query("INSERT INTO review_requests (pull_id, user_id, requested_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(issue.id)
            .bind(who)
            .bind(by)
            .execute(&mut **tx)
            .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        let u = self.user_ref(who).await?;
        Self::add_event(tx, issue.id, by, "review_requested", json!({ "reviewer": { "id": u.id.0, "username": u.username } })).await?;
        Self::subscribe_default(tx, issue.id, who, "review_requested").await?;
        self.d
            .events
            .emit(
                tx,
                DomainEvent::ReviewRequested {
                    repo_id: issue.repo_id,
                    pull_id: PullId(issue.id.0),
                    number: issue.number,
                    reviewer: Some(who),
                    team: None,
                    actor: by,
                },
            )
            .await?;
        Ok(true)
    }

    async fn request_team_in_tx(
        &self,
        tx: &mut DbTx<'_>,
        issue: &IssueRow,
        by: Option<UserId>,
        team: TeamId,
        org: &str,
        name: &str,
    ) -> Result<bool> {
        let r = sqlx::query("INSERT INTO review_requests (pull_id, team_id, requested_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(issue.id)
            .bind(team)
            .bind(by)
            .execute(&mut **tx)
            .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        Self::add_event(tx, issue.id, by, "review_requested", json!({ "team": { "id": team.0, "org": org, "name": name } })).await?;
        self.d
            .events
            .emit(
                tx,
                DomainEvent::ReviewRequested {
                    repo_id: issue.repo_id,
                    pull_id: PullId(issue.id.0),
                    number: issue.number,
                    reviewer: None,
                    team: Some(team),
                    actor: by,
                },
            )
            .await?;
        Ok(true)
    }

    pub async fn review_remove_request(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        _client: &ClientInfo,
    ) -> Result<()> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_read(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (issue, _) = self.pull_pair(repo, number).await?;
        if issue.author_id != user && perm < Permission::Triage {
            return Err(Error::forbidden("Only the author or someone with triage access can remove review requests"));
        }
        let mut tx = self.db().begin().await?;
        for u in users {
            let r = sqlx::query("DELETE FROM review_requests WHERE pull_id = $1 AND user_id = $2")
                .bind(issue.id)
                .bind(u)
                .execute(&mut *tx)
                .await?;
            if r.rows_affected() > 0 {
                let ur = self.user_ref(u).await?;
                Self::add_event(
                    &mut tx,
                    issue.id,
                    Some(user),
                    "review_request_removed",
                    json!({ "reviewer": { "id": ur.id.0, "username": ur.username } }),
                )
                .await?;
            }
        }
        for t in teams {
            let r = sqlx::query("DELETE FROM review_requests WHERE pull_id = $1 AND team_id = $2")
                .bind(issue.id)
                .bind(t)
                .execute(&mut *tx)
                .await?;
            if r.rows_affected() > 0 {
                Self::add_event(&mut tx, issue.id, Some(user), "review_request_removed", json!({ "team": { "id": t.0 } })).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Asks the owners named in CODEOWNERS to review. Best effort, safe to repeat.
    pub async fn request_code_owners(&self, issue: &IssueRow, pull: &PullRow) {
        if let Err(e) = self.request_code_owners_inner(issue, pull).await {
            tracing::warn!(repo = issue.repo_id.0, number = issue.number, error = %e, "could not request code owner reviews");
        }
    }

    async fn request_code_owners_inner(&self, issue: &IssueRow, pull: &PullRow) -> Result<()> {
        let rules = self.read_codeowners(pull.repo_id, &pull.base_ref).await;
        if rules.is_empty() || !issue.open {
            return Ok(());
        }
        let paths = self.pull_diff_paths(issue, pull).await?;
        let owners = codeowners::owners_of_paths(&rules, &paths);
        if owners.is_empty() {
            return Ok(());
        }
        let mut tx = self.db().begin().await?;
        for o in owners {
            match o {
                Owner::User(name) => {
                    let Some(uid) = self.user_id_by_name(&name).await? else { continue };
                    if uid == issue.author_id || self.permission(&as_user(uid), issue.repo_id).await? < Permission::Read {
                        continue;
                    }
                    // Someone who already reviewed does not need asking again.
                    let reviewed: Option<i32> =
                        sqlx::query_scalar("SELECT 1 FROM reviews WHERE pull_id = $1 AND author_id = $2 AND state <> 'pending' LIMIT 1")
                            .bind(issue.id)
                            .bind(uid)
                            .fetch_optional(&mut *tx)
                            .await?;
                    if reviewed.is_none() {
                        self.request_user_in_tx(&mut tx, issue, None, uid).await?;
                    }
                }
                Owner::Team { org, team } => {
                    let t: Option<TeamId> = sqlx::query_scalar(
                        "SELECT t.id FROM teams t JOIN users o ON o.id = t.org_id WHERE o.username = $1::citext AND t.name = $2::citext",
                    )
                    .bind(&org)
                    .bind(&team)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if let Some(t) = t {
                        self.request_team_in_tx(&mut tx, issue, None, t, &org, &team).await?;
                    }
                }
                Owner::Email(_) => {}
            }
        }
        tx.commit().await?;
        Ok(())
    }

    // ------------------------------------------------------------ outdated comments

    /// After the head moved from `old_head` to the pull request's current head: shifts the
    /// line numbers of comments whose lines are still there, and marks the rest outdated.
    pub async fn adjust_comments_for_new_head(&self, issue: &IssueRow, pull: &PullRow, old_head: &str) -> Result<()> {
        if old_head == pull.head_sha {
            return Ok(());
        }
        let rows = sqlx::query_as::<_, CommentRow>(&format!(
            "{COMMENT_SELECT} WHERE c.pull_id = $1 AND NOT c.outdated AND c.side = 'right' AND c.commit_sha = $2"
        ))
        .bind(issue.id)
        .bind(old_head)
        .fetch_all(self.db())
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let cmp = self.d.git.compare(pull.git_repo(), old_head, &pull.head_sha).await?;
        for c in rows {
            let file = cmp.diff.files.iter().find(|f| f.old_path.as_deref() == Some(c.path.as_str()));
            let (Some(line), start) = (c.line, c.start_line) else { continue };
            let mapped = match file {
                None => Some((line as u32, start.map(|s| s as u32))),
                Some(f) if f.status == FileStatus::Removed || f.collapsed || f.binary => None,
                Some(f) => {
                    let l = map_line(&f.hunks, line as u32);
                    let s = start.map(|s| map_line(&f.hunks, s as u32));
                    match (l, s) {
                        (Some(l), None) => Some((l, None)),
                        (Some(l), Some(Some(s))) => Some((l, Some(s))),
                        _ => None,
                    }
                }
            };
            match mapped {
                Some((l, s)) => {
                    let new_path = file.and_then(|f| f.new_path.clone()).unwrap_or_else(|| c.path.clone());
                    sqlx::query("UPDATE review_comments SET line = $2, start_line = $3, commit_sha = $4, path = $5 WHERE id = $1")
                        .bind(c.id)
                        .bind(l as i32)
                        .bind(s.map(|v| v as i32))
                        .bind(&pull.head_sha)
                        .bind(&new_path)
                        .execute(self.db())
                        .await?;
                }
                None => {
                    sqlx::query("UPDATE review_comments SET outdated = true WHERE id = $1").bind(c.id).execute(self.db()).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine { kind, old_no: old, new_no: new, text: text.into() }
    }

    fn hunk(lines: Vec<DiffLine>) -> Hunk {
        Hunk { header: "@@ -1,5 +1,6 @@".into(), lines }
    }

    #[test]
    fn states_round_trip() {
        for s in
            [ReviewState::Pending, ReviewState::Commented, ReviewState::Approved, ReviewState::ChangesRequested, ReviewState::Dismissed]
        {
            assert_eq!(parse_state(state_str(s)), s);
        }
    }

    #[test]
    fn map_line_without_hunks_is_identity() {
        assert_eq!(map_line(&[], 7), Some(7));
    }

    #[test]
    fn lines_before_a_hunk_do_not_move() {
        let h = hunk(vec![dl(LineKind::Context, Some(10), Some(10), "a"), dl(LineKind::Added, None, Some(11), "x")]);
        assert_eq!(map_line(&[h], 3), Some(3));
    }

    #[test]
    fn lines_after_an_insertion_shift_down() {
        let h = hunk(vec![
            dl(LineKind::Context, Some(2), Some(2), "a"),
            dl(LineKind::Added, None, Some(3), "x"),
            dl(LineKind::Added, None, Some(4), "y"),
            dl(LineKind::Context, Some(3), Some(5), "b"),
        ]);
        assert_eq!(map_line(&[h], 20), Some(22));
    }

    #[test]
    fn lines_after_a_deletion_shift_up() {
        let h = hunk(vec![
            dl(LineKind::Context, Some(2), Some(2), "a"),
            dl(LineKind::Removed, Some(3), None, "x"),
            dl(LineKind::Context, Some(4), Some(3), "b"),
        ]);
        assert_eq!(map_line(&[h], 10), Some(9));
    }

    #[test]
    fn a_removed_line_is_gone_and_a_context_line_maps() {
        let h = hunk(vec![
            dl(LineKind::Context, Some(2), Some(2), "a"),
            dl(LineKind::Removed, Some(3), None, "x"),
            dl(LineKind::Added, None, Some(3), "y"),
            dl(LineKind::Context, Some(4), Some(4), "b"),
        ]);
        assert_eq!(map_line(std::slice::from_ref(&h), 3), None);
        assert_eq!(map_line(std::slice::from_ref(&h), 2), Some(2));
        assert_eq!(map_line(std::slice::from_ref(&h), 4), Some(4));
    }

    #[test]
    fn two_hunks_accumulate() {
        let h1 = hunk(vec![
            dl(LineKind::Context, Some(1), Some(1), "a"),
            dl(LineKind::Added, None, Some(2), "x"),
            dl(LineKind::Context, Some(2), Some(3), "b"),
        ]);
        let h2 = hunk(vec![
            dl(LineKind::Context, Some(10), Some(11), "c"),
            dl(LineKind::Removed, Some(11), None, "z"),
            dl(LineKind::Removed, Some(12), None, "w"),
            dl(LineKind::Context, Some(13), Some(12), "d"),
        ]);
        assert_eq!(map_line(&[h1.clone(), h2.clone()], 5), Some(6));
        assert_eq!(map_line(&[h1.clone(), h2.clone()], 20), Some(19));
        assert_eq!(map_line(&[h1, h2], 13), Some(12));
    }

    #[test]
    fn hunk_text_ends_at_the_line() {
        let h = Hunk {
            header: "@@ -1,3 +1,3 @@".into(),
            lines: vec![
                dl(LineKind::Context, Some(1), Some(1), "a"),
                dl(LineKind::Removed, Some(2), None, "b"),
                dl(LineKind::Added, None, Some(2), "B"),
                dl(LineKind::Context, Some(3), Some(3), "c"),
            ],
        };
        let f = FileDiff {
            status: FileStatus::Modified,
            old_path: Some("f".into()),
            new_path: Some("f".into()),
            binary: false,
            collapsed: false,
            additions: 1,
            deletions: 1,
            hunks: vec![h],
        };
        assert_eq!(hunk_text(&f, false, 2).unwrap(), "@@ -1,3 +1,3 @@\n a\n-b\n+B");
        assert_eq!(hunk_text(&f, true, 2).unwrap(), "@@ -1,3 +1,3 @@\n a\n-b");
        assert!(hunk_text(&f, false, 99).is_none());
        assert!(hunk_text(&f, true, 3).is_some());
    }

    #[test]
    fn sides() {
        assert!(!side_of("RIGHT").unwrap());
        assert!(side_of("left").unwrap());
        assert!(side_of("up").is_err());
    }
}
