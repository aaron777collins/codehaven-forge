//! Issues: creating, listing, changing state, labels, assignees, milestones, locking, pinning.
//!
//! A pull request is an issue row too, so everything here also serves pull requests. The pull
//! specific parts live in `pulls.rs`, which calls back into these helpers.

use crate::core::{Core, IssueRow, RepoRow};
use crate::filter::Spec;
use crate::util::{self, as_user, writing_user};
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::services::DbTx;
use ch_core::{Actor, Error, FieldError, IssueId, LabelId, MilestoneId, Permission, RepoId, Result, UserId};
use serde_json::json;

pub const MAX_ASSIGNEES: usize = 10;
pub const MAX_PINNED: i64 = 3;
const LOCK_REASONS: &[&str] = &["off-topic", "too heated", "resolved", "spam"];

impl Core {
    // ------------------------------------------------------------ shared checks

    /// Writes to an archived repository fail with one clear message.
    pub fn ensure_writable(repo: &RepoRow) -> Result<()> {
        if repo.archived {
            return Err(Error::forbidden("This repository is archived and read-only"));
        }
        Ok(())
    }

    /// Opening something needs read access and a writable repository with the feature on.
    async fn open_checks(&self, actor: &Actor, repo: RepoId, pull: bool) -> Result<(RepoRow, Permission)> {
        let perm = self.need_contribute(actor, repo).await?;
        let row = self.repo(repo).await?;
        Self::ensure_writable(&row)?;
        if pull && !row.has_pulls {
            return Err(Error::invalid("PullRequest", "base", "Pull requests are turned off for this repository"));
        }
        if !pull && !row.has_issues {
            return Err(Error::invalid("Issue", "repository", "Issues are turned off for this repository"));
        }
        Ok((row, perm))
    }

    /// May this caller edit or close `row`? The author may. Triage and above always may.
    pub fn can_edit(row: &IssueRow, user: UserId, perm: Permission) -> bool {
        row.author_id == user || perm >= Permission::Triage
    }

    /// Takes the next number from the repository and inserts the issue row.
    pub async fn alloc_issue(
        &self,
        tx: &mut DbTx<'_>,
        repo: RepoId,
        is_pull: bool,
        author: UserId,
        title: &str,
        body: &str,
    ) -> Result<IssueRow> {
        let number: i64 =
            sqlx::query_scalar("UPDATE repositories SET next_number = next_number + 1 WHERE id = $1 RETURNING next_number - 1")
                .bind(repo)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(Error::NotFound)?;
        let id: IssueId = sqlx::query_scalar(
            "INSERT INTO issues (repo_id, number, is_pull, author_id, title, body) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(repo)
        .bind(number)
        .bind(is_pull)
        .bind(author)
        .bind(title)
        .bind(body)
        .fetch_one(&mut **tx)
        .await?;
        self.issue_row_by_id_tx(tx, id).await
    }

    pub async fn issue_row_by_id_tx(&self, tx: &mut DbTx<'_>, id: IssueId) -> Result<IssueRow> {
        let sql = format!("SELECT {} FROM issues i WHERE i.id = $1", crate::core::ISSUE_COLS);
        Ok(sqlx::query_as::<_, IssueRow>(&sql).bind(id).fetch_one(&mut **tx).await?)
    }

    pub async fn issue_row_by_id(&self, id: IssueId) -> Result<IssueRow> {
        let sql = format!("SELECT {} FROM issues i WHERE i.id = $1", crate::core::ISSUE_COLS);
        sqlx::query_as::<_, IssueRow>(&sql).bind(id).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    // ------------------------------------------------------------ create

    pub async fn issue_create(&self, actor: &Actor, new: NewIssue, _client: &ClientInfo) -> Result<IssueView> {
        let user = writing_user(actor, new.repo_id)?;
        let (repo, perm) = self.open_checks(actor, new.repo_id, false).await?;
        let title = util::title("Issue", "title", &new.title)?;
        let body = util::body("Issue", "body", &new.body)?;
        if new.assignees.len() > MAX_ASSIGNEES {
            return Err(Error::invalid("Issue", "assignees", format!("at most {MAX_ASSIGNEES} people can be assigned")));
        }
        let triage = perm >= Permission::Triage;

        let mut tx = self.db().begin().await?;
        let row = self.alloc_issue(&mut tx, repo.id, false, user, &title, &body).await?;
        if triage {
            let labels = self.resolve_labels(&mut tx, repo.id, &new.labels).await?;
            for (lid, name, color) in &labels {
                sqlx::query("INSERT INTO issue_labels (issue_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                    .bind(row.id)
                    .bind(lid)
                    .execute(&mut *tx)
                    .await?;
                Self::add_event(&mut tx, row.id, Some(user), "labeled", json!({ "label": { "name": name, "color": color } })).await?;
                self.d
                    .events
                    .emit(
                        &mut tx,
                        DomainEvent::IssueLabeled {
                            repo_id: repo.id,
                            issue_id: row.id,
                            number: row.number,
                            label: name.clone(),
                            actor: user,
                        },
                    )
                    .await?;
            }
            for a in &new.assignees {
                self.assign_in_tx(&mut tx, &repo, &row, user, *a).await?;
            }
            if let Some(m) = new.milestone_id {
                let n = self.milestone_number_by_id(&mut tx, repo.id, m).await?;
                self.milestone_in_tx(&mut tx, &row, user, Some(n)).await?;
            }
        }
        Self::subscribe_default(&mut tx, row.id, user, "author").await?;
        let fresh = self.issue_row_by_id_tx(&mut tx, row.id).await?;
        self.record_references(&mut tx, actor, &fresh, &repo, &format!("{}\n{}", fresh.title, fresh.body)).await?;
        self.d
            .events
            .emit(&mut tx, DomainEvent::IssueOpened { repo_id: repo.id, issue_id: row.id, number: row.number, actor: user })
            .await?;
        tx.commit().await?;
        self.hydrate_one(self.issue_row_by_id(row.id).await?).await
    }

    // ------------------------------------------------------------ read

    pub async fn issue_view(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<IssueView> {
        self.need_view(actor, repo).await?;
        let row = self.issue_row(repo, number).await?;
        self.hydrate_one(row).await
    }

    pub async fn issue_views(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<IssueView>> {
        self.need_view(actor, repo).await?;
        let (offset, limit) = util::page(filter.page, filter.per_page);
        let spec = Spec { repo: Some(repo), filter, ..Default::default() };
        let rows = self.query_issue_rows(actor, &spec, offset, limit).await?;
        self.hydrate(rows).await
    }

    pub async fn issue_views_by_ids(&self, actor: &Actor, ids: &[IssueId]) -> Result<Vec<IssueView>> {
        if ids.is_empty() || actor.is_anonymous() {
            return Ok(Vec::new());
        }
        let raw: Vec<i64> = ids.iter().take(200).map(|i| i.0).collect();
        let sql = format!("SELECT {} FROM issues i WHERE i.id = ANY($1)", crate::core::ISSUE_COLS);
        let mut rows = sqlx::query_as::<_, IssueRow>(&sql).bind(&raw).fetch_all(self.db()).await?;
        let ok = self.readable(actor, rows.iter().map(|r| r.repo_id).collect()).await?;
        rows.retain(|r| ok.contains(&r.repo_id));
        // Keep the caller's order, which is the search rank.
        rows.sort_by_key(|r| raw.iter().position(|x| *x == r.id.0).unwrap_or(usize::MAX));
        self.hydrate(rows).await
    }

    /// Cross-repository lists. Scans in batches and keeps what the caller may read, so the
    /// permission model stays in `Authz` and is never copied into SQL.
    pub async fn issue_for_user(&self, actor: &Actor, scope: UserIssueScope, filter: IssueFilter) -> Result<Vec<IssueView>> {
        let user = util::author_user(actor)?;
        let (offset, limit) = util::page(filter.page, filter.per_page);
        let want = (offset + limit) as usize;
        let spec = Spec { filter, scope: Some((scope, user)), ..Default::default() };
        let batch = (limit * 3).clamp(30, 300);
        let mut kept: Vec<IssueRow> = Vec::new();
        let mut scanned = 0;
        for _ in 0..20 {
            let rows = self.query_issue_rows(actor, &spec, scanned, batch).await?;
            let n = rows.len() as i64;
            scanned += n;
            let ok = self.readable(actor, rows.iter().map(|r| r.repo_id).collect()).await?;
            kept.extend(rows.into_iter().filter(|r| ok.contains(&r.repo_id)));
            if kept.len() >= want || n < batch {
                break;
            }
        }
        let page: Vec<IssueRow> = kept.into_iter().skip(offset as usize).take(limit as usize).collect();
        self.hydrate(page).await
    }

    // ------------------------------------------------------------ state

    /// Opens or closes inside the caller's transaction. Pull requests get their own events.
    pub async fn set_open_in_tx(
        &self,
        tx: &mut DbTx<'_>,
        user: UserId,
        row: &IssueRow,
        open: bool,
        reason: Option<&str>,
        via: Option<serde_json::Value>,
    ) -> Result<bool> {
        if row.open == open {
            return Ok(false);
        }
        let merged: bool = if row.is_pull {
            sqlx::query_scalar("SELECT merged_at IS NOT NULL FROM pull_requests WHERE issue_id = $1")
                .bind(row.id)
                .fetch_one(&mut **tx)
                .await?
        } else {
            false
        };
        if merged {
            return Err(Error::invalid("PullRequest", "state", "A merged pull request cannot be closed or reopened"));
        }
        let reason = if open {
            Some("reopened".to_string())
        } else if row.is_pull {
            None
        } else {
            match reason {
                None | Some("completed") => Some("completed".to_string()),
                Some("not_planned") => Some("not_planned".to_string()),
                Some(_) => return Err(Error::invalid("Issue", "state_reason", "state_reason must be completed or not_planned")),
            }
        };
        sqlx::query(
            "UPDATE issues SET open = $2, state_reason = $3, closed_at = CASE WHEN $2 THEN NULL ELSE now() END,
                    closed_by = CASE WHEN $2 THEN NULL ELSE $4 END, updated_at = now() WHERE id = $1",
        )
        .bind(row.id)
        .bind(open)
        .bind(&reason)
        .bind(user)
        .execute(&mut **tx)
        .await?;
        let mut data = json!({});
        if let Some(r) = &reason {
            data["reason"] = json!(r);
        }
        if let Some(v) = via {
            data["via"] = v;
        }
        Self::add_event(tx, row.id, Some(user), if open { "reopened" } else { "closed" }, data).await?;
        let (repo_id, issue_id, number) = (row.repo_id, row.id, row.number);
        let ev = match (row.is_pull, open) {
            (false, true) => DomainEvent::IssueReopened { repo_id, issue_id, number, actor: user },
            (false, false) => DomainEvent::IssueClosed { repo_id, issue_id, number, actor: user },
            // Reopening is not pushing: whoever reopens did not put the head there, so CI
            // names nobody for it and a fork's commit waits for approval (ch-x12).
            (true, true) => DomainEvent::PrOpened {
                repo_id,
                pull_id: ch_core::PullId(issue_id.0),
                number,
                actor: user,
                app: None,
                head_sha: None,
                pusher: None,
            },
            (true, false) => {
                DomainEvent::PrClosed { repo_id, pull_id: ch_core::PullId(issue_id.0), number, merged: false, actor: Some(user) }
            }
        };
        self.d.events.emit(tx, ev).await?;
        Ok(true)
    }

    pub async fn issue_set_state(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        open: bool,
        reason: Option<&str>,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        // Reopening is contributing. Closing your own is withdrawing, which the contribution
        // setting never stops.
        let perm = if open { self.need_contribute(actor, repo).await? } else { self.need_read(actor, repo).await? };
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        if !Self::can_edit(&row, user, perm) {
            return Err(Error::forbidden("You can only close or reopen your own issues"));
        }
        if row.is_pull && open && !row.open {
            self.pull_reopen_check(&row).await?;
        }
        self.set_open_in_tx(&mut tx, user, &row, open, reason, None).await?;
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    // ------------------------------------------------------------ edit

    pub async fn issue_edit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        title: Option<String>,
        body: Option<String>,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        self.edit_in_tx(&mut tx, actor, user, perm, &rrow, &row, title, body).await?;
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn edit_in_tx(
        &self,
        tx: &mut DbTx<'_>,
        actor: &Actor,
        user: UserId,
        perm: Permission,
        repo: &RepoRow,
        row: &IssueRow,
        title: Option<String>,
        body: Option<String>,
    ) -> Result<()> {
        if title.is_none() && body.is_none() {
            return Ok(());
        }
        if !Self::can_edit(row, user, perm) {
            return Err(Error::forbidden("You can only edit your own issues"));
        }
        if row.locked && perm < Permission::Triage {
            return Err(Error::forbidden("This conversation is locked"));
        }
        let new_title = match &title {
            Some(t) => util::title("Issue", "title", t)?,
            None => row.title.clone(),
        };
        let new_body = match &body {
            Some(b) => util::body("Issue", "body", b)?,
            None => row.body.clone(),
        };
        if new_title == row.title && new_body == row.body {
            return Ok(());
        }
        sqlx::query("UPDATE issues SET title = $2, body = $3, updated_at = now() WHERE id = $1")
            .bind(row.id)
            .bind(&new_title)
            .bind(&new_body)
            .execute(&mut **tx)
            .await?;
        if new_title != row.title {
            Self::add_event(tx, row.id, Some(user), "renamed", json!({ "from": row.title, "to": new_title })).await?;
        }
        let mut fresh = row.clone();
        fresh.title = new_title;
        fresh.body = new_body.clone();
        // Only text that was not there before can add references.
        let added = added_text(&row.body, &new_body);
        self.record_references(tx, actor, &fresh, repo, &format!("{}\n{}", fresh.title, added)).await?;
        let ev = if row.is_pull {
            None
        } else {
            Some(DomainEvent::IssueEdited { repo_id: row.repo_id, issue_id: row.id, number: row.number, actor: user })
        };
        if let Some(ev) = ev {
            self.d.events.emit(tx, ev).await?;
        }
        Ok(())
    }

    /// `PATCH /issues/{n}` in one transaction.
    pub async fn issue_update(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        patch: IssuePatch,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let triage = perm >= Permission::Triage;
        if (patch.labels.is_some() || patch.assignees.is_some() || patch.milestone.is_some()) && !triage {
            return Err(Error::forbidden("You need triage access to change labels, assignees or milestone"));
        }
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        self.edit_in_tx(&mut tx, actor, user, perm, &rrow, &row, patch.title.clone(), patch.body.clone()).await?;
        if let Some(open) = patch.open {
            if !Self::can_edit(&row, user, perm) {
                return Err(Error::forbidden("You can only close or reopen your own issues"));
            }
            if row.is_pull && open && !row.open {
                self.pull_reopen_check(&row).await?;
            }
            self.set_open_in_tx(&mut tx, user, &row, open, patch.state_reason.as_deref(), None).await?;
        }
        if let Some(labels) = &patch.labels {
            let desired = self.resolve_labels(&mut tx, repo, labels).await?;
            self.labels_replace_in_tx(&mut tx, user, &row, &desired).await?;
        }
        if let Some(assignees) = &patch.assignees {
            self.assignees_replace_in_tx(&mut tx, &rrow, &row, user, assignees).await?;
        }
        if let Some(m) = patch.milestone {
            self.milestone_in_tx(&mut tx, &row, user, m).await?;
        }
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    // ------------------------------------------------------------ labels

    pub async fn labels_replace_in_tx(
        &self,
        tx: &mut DbTx<'_>,
        user: UserId,
        row: &IssueRow,
        desired: &[(LabelId, String, String)],
    ) -> Result<()> {
        let current: Vec<(LabelId, String, String)> = sqlx::query_as(
            "SELECT l.id, l.name::text, l.color FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = $1",
        )
        .bind(row.id)
        .fetch_all(&mut **tx)
        .await?;
        for (id, name, color) in &current {
            if !desired.iter().any(|(d, _, _)| d == id) {
                sqlx::query("DELETE FROM issue_labels WHERE issue_id = $1 AND label_id = $2")
                    .bind(row.id)
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
                Self::add_event(tx, row.id, Some(user), "unlabeled", json!({ "label": { "name": name, "color": color } })).await?;
            }
        }
        self.labels_add_in_tx(tx, user, row, desired).await?;
        Self::touch(tx, row.id).await
    }

    pub async fn labels_add_in_tx(
        &self,
        tx: &mut DbTx<'_>,
        user: UserId,
        row: &IssueRow,
        labels: &[(LabelId, String, String)],
    ) -> Result<()> {
        for (id, name, color) in labels {
            let r = sqlx::query("INSERT INTO issue_labels (issue_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                .bind(row.id)
                .bind(id)
                .execute(&mut **tx)
                .await?;
            if r.rows_affected() == 1 {
                Self::add_event(tx, row.id, Some(user), "labeled", json!({ "label": { "name": name, "color": color } })).await?;
                self.d
                    .events
                    .emit(
                        tx,
                        DomainEvent::IssueLabeled {
                            repo_id: row.repo_id,
                            issue_id: row.id,
                            number: row.number,
                            label: name.clone(),
                            actor: user,
                        },
                    )
                    .await?;
            }
        }
        Self::touch(tx, row.id).await
    }

    pub async fn issue_set_labels(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        labels: Vec<String>,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        let desired = self.resolve_labels(&mut tx, repo, &labels).await?;
        self.labels_replace_in_tx(&mut tx, user, &row, &desired).await?;
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    pub async fn issue_add_labels(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        labels: Vec<String>,
        _client: &ClientInfo,
    ) -> Result<Vec<LabelInfo>> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        let desired = self.resolve_labels(&mut tx, repo, &labels).await?;
        self.labels_add_in_tx(&mut tx, user, &row, &desired).await?;
        tx.commit().await?;
        Ok(self.labels_for(&[row.id]).await?.remove(&row.id).unwrap_or_default())
    }

    pub async fn issue_remove_label(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        label: &str,
        _client: &ClientInfo,
    ) -> Result<Vec<LabelInfo>> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        let lab: Option<(LabelId, String, String)> =
            sqlx::query_as("SELECT id, name::text, color FROM labels WHERE repo_id = $1 AND name = $2::citext")
                .bind(repo)
                .bind(label.trim())
                .fetch_optional(&mut *tx)
                .await?;
        let (lid, name, color) = lab.ok_or(Error::NotFound)?;
        let r =
            sqlx::query("DELETE FROM issue_labels WHERE issue_id = $1 AND label_id = $2").bind(row.id).bind(lid).execute(&mut *tx).await?;
        if r.rows_affected() == 1 {
            Self::add_event(&mut tx, row.id, Some(user), "unlabeled", json!({ "label": { "name": name, "color": color } })).await?;
            Self::touch(&mut tx, row.id).await?;
        }
        tx.commit().await?;
        Ok(self.labels_for(&[row.id]).await?.remove(&row.id).unwrap_or_default())
    }

    // ------------------------------------------------------------ assignees

    pub async fn assign_in_tx(&self, tx: &mut DbTx<'_>, repo: &RepoRow, row: &IssueRow, actor_user: UserId, who: UserId) -> Result<bool> {
        if self.permission(&as_user(who), repo.id).await? < Permission::Read {
            let name = self.user_ref(who).await.map(|u| u.username).unwrap_or_default();
            return Err(Error::Validation(vec![FieldError::custom(
                "Issue",
                "assignees",
                format!("{name} cannot be assigned: they cannot see this repository"),
            )]));
        }
        let r = sqlx::query("INSERT INTO issue_assignees (issue_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(row.id)
            .bind(who)
            .execute(&mut **tx)
            .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        let u = self.user_ref(who).await?;
        Self::add_event(tx, row.id, Some(actor_user), "assigned", json!({ "assignee": { "id": u.id.0, "username": u.username } })).await?;
        Self::subscribe_default(tx, row.id, who, "assigned").await?;
        self.d
            .events
            .emit(
                tx,
                DomainEvent::IssueAssigned { repo_id: row.repo_id, issue_id: row.id, number: row.number, assignee: who, actor: actor_user },
            )
            .await?;
        Self::touch(tx, row.id).await?;
        Ok(true)
    }

    pub async fn unassign_in_tx(&self, tx: &mut DbTx<'_>, row: &IssueRow, actor_user: UserId, who: UserId) -> Result<bool> {
        let r = sqlx::query("DELETE FROM issue_assignees WHERE issue_id = $1 AND user_id = $2")
            .bind(row.id)
            .bind(who)
            .execute(&mut **tx)
            .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        let u = self.user_ref(who).await?;
        Self::add_event(tx, row.id, Some(actor_user), "unassigned", json!({ "assignee": { "id": u.id.0, "username": u.username } }))
            .await?;
        Self::touch(tx, row.id).await?;
        Ok(true)
    }

    pub async fn assignees_replace_in_tx(
        &self,
        tx: &mut DbTx<'_>,
        repo: &RepoRow,
        row: &IssueRow,
        actor_user: UserId,
        desired: &[UserId],
    ) -> Result<()> {
        if desired.len() > MAX_ASSIGNEES {
            return Err(Error::invalid("Issue", "assignees", format!("at most {MAX_ASSIGNEES} people can be assigned")));
        }
        let current: Vec<UserId> =
            sqlx::query_scalar("SELECT user_id FROM issue_assignees WHERE issue_id = $1").bind(row.id).fetch_all(&mut **tx).await?;
        for c in &current {
            if !desired.contains(c) {
                self.unassign_in_tx(tx, row, actor_user, *c).await?;
            }
        }
        for d in desired {
            self.assign_in_tx(tx, repo, row, actor_user, *d).await?;
        }
        Ok(())
    }

    pub async fn issue_add_assignees(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        let current: i64 =
            sqlx::query_scalar("SELECT count(*) FROM issue_assignees WHERE issue_id = $1").bind(row.id).fetch_one(&mut *tx).await?;
        if current as usize + users.len() > MAX_ASSIGNEES {
            return Err(Error::invalid("Issue", "assignees", format!("at most {MAX_ASSIGNEES} people can be assigned")));
        }
        for u in users {
            self.assign_in_tx(&mut tx, &rrow, &row, user, u).await?;
        }
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    pub async fn issue_remove_assignees(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        for u in users {
            self.unassign_in_tx(&mut tx, &row, user, u).await?;
        }
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    /// The owner and everyone added to the repository directly. Who has access is for the people
    /// who assign work, Triage and up, not every reader (ch-uph).
    pub async fn assignable(&self, actor: &Actor, repo: RepoId) -> Result<Vec<UserRef>> {
        self.need(actor, repo, Permission::Triage).await?;
        let ids: Vec<UserId> = sqlx::query_scalar(
            "SELECT owner_id FROM repositories WHERE id = $1 UNION SELECT user_id FROM repo_collaborators WHERE repo_id = $1",
        )
        .bind(repo)
        .fetch_all(self.db())
        .await?;
        let refs = self.user_refs(&ids).await?;
        let mut v: Vec<UserRef> = refs.into_values().filter(|u| u.kind != UserKind::Org).collect();
        v.sort_by_key(|u| u.username.to_lowercase());
        Ok(v)
    }

    // ------------------------------------------------------------ milestone

    pub async fn milestone_number_by_id(&self, tx: &mut DbTx<'_>, repo: RepoId, id: MilestoneId) -> Result<i64> {
        sqlx::query_scalar("SELECT number FROM milestones WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| Error::Validation(vec![FieldError::new("Issue", "milestone", "invalid")]))
    }

    /// `None` clears the milestone.
    pub async fn milestone_in_tx(&self, tx: &mut DbTx<'_>, row: &IssueRow, user: UserId, number: Option<i64>) -> Result<()> {
        let target: Option<(MilestoneId, String)> = match number {
            None => None,
            Some(n) => Some(
                sqlx::query_as("SELECT id, title FROM milestones WHERE repo_id = $1 AND number = $2")
                    .bind(row.repo_id)
                    .bind(n)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or_else(|| Error::Validation(vec![FieldError::new("Issue", "milestone", "invalid")]))?,
            ),
        };
        let new_id = target.as_ref().map(|t| t.0);
        if new_id == row.milestone_id {
            return Ok(());
        }
        if let Some(old) = row.milestone_id {
            let title: Option<String> =
                sqlx::query_scalar("SELECT title FROM milestones WHERE id = $1").bind(old).fetch_optional(&mut **tx).await?;
            Self::add_event(tx, row.id, Some(user), "demilestoned", json!({ "title": title.unwrap_or_default() })).await?;
        }
        sqlx::query("UPDATE issues SET milestone_id = $2, updated_at = now() WHERE id = $1")
            .bind(row.id)
            .bind(new_id)
            .execute(&mut **tx)
            .await?;
        if let Some((_, title)) = target {
            Self::add_event(tx, row.id, Some(user), "milestoned", json!({ "title": title })).await?;
        }
        Ok(())
    }

    pub async fn issue_set_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        milestone: Option<i64>,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        self.milestone_in_tx(&mut tx, &row, user, milestone).await?;
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    // ------------------------------------------------------------ lock and pin

    pub async fn issue_set_locked(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        locked: bool,
        reason: Option<String>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let reason = match reason.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) if LOCK_REASONS.contains(&r) => Some(r.to_string()),
            Some(_) => return Err(Error::invalid("Issue", "lock_reason", "lock_reason must be off-topic, too heated, resolved or spam")),
            None => None,
        };
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        if row.locked != locked {
            sqlx::query("UPDATE issues SET locked = $2, lock_reason = $3, updated_at = now() WHERE id = $1")
                .bind(row.id)
                .bind(locked)
                .bind(if locked { reason.clone() } else { None })
                .execute(&mut *tx)
                .await?;
            Self::add_event(&mut tx, row.id, Some(user), if locked { "locked" } else { "unlocked" }, json!({ "reason": reason })).await?;
            self.d
                .audit
                .record(
                    &mut tx,
                    AuditEvent::new(actor, client, action::ISSUE_LOCKED, Outcome::Success)
                        .target("issue", row.id)
                        .details(json!({ "locked": locked, "number": number })),
                )
                .await?;
        }
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }

    pub async fn issue_set_pinned(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        pinned: bool,
        _client: &ClientInfo,
    ) -> Result<IssueView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Triage).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let row = self.issue_row_tx(&mut tx, repo, number).await?;
        if row.is_pull {
            return Err(Error::invalid("Issue", "pinned", "Only issues can be pinned"));
        }
        if row.pinned != pinned {
            if pinned {
                let n: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM issues WHERE repo_id = $1 AND pinned").bind(repo).fetch_one(&mut *tx).await?;
                if n >= MAX_PINNED {
                    return Err(Error::conflict(format!("A repository can pin at most {MAX_PINNED} issues")));
                }
            }
            sqlx::query("UPDATE issues SET pinned = $2 WHERE id = $1").bind(row.id).bind(pinned).execute(&mut *tx).await?;
            Self::add_event(&mut tx, row.id, Some(user), if pinned { "pinned" } else { "unpinned" }, json!({})).await?;
        }
        tx.commit().await?;
        self.hydrate_one(self.issue_row(repo, number).await?).await
    }
}

/// The text of `new` that was not in `old`, so editing an issue does not repeat old references.
/// Cheap and good enough: lines of `new` that are not lines of `old`.
fn added_text(old: &str, new: &str) -> String {
    let old_lines: std::collections::HashSet<&str> = old.lines().collect();
    new.lines().filter(|l| !old_lines.contains(l)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_text_keeps_only_new_lines() {
        assert_eq!(added_text("a\nb", "a\nb\nc #3"), "c #3");
        assert_eq!(added_text("a", "a"), "");
        assert_eq!(added_text("", "x\ny"), "x\ny");
    }
}
