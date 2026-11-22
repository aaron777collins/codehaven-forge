//! The engine behind every collab trait: shared lookups, hydration of rows into views, the
//! timeline and cross references. `issues.rs`, `pulls.rs` and friends add methods to `Core`.

use crate::deps::Deps;
use crate::util;
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::text::{self, IssueRef};
use ch_core::{Actor, CommentId, Error, IssueId, LabelId, MilestoneId, Permission, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub struct Core {
    pub d: Deps,
}

/// What collab needs to know about a repository.
#[derive(Debug, Clone, FromRow)]
pub struct RepoRow {
    pub id: RepoId,
    pub owner_id: UserId,
    pub owner_name: String,
    pub name: String,
    pub default_branch: String,
    pub archived: bool,
    pub has_issues: bool,
    pub has_pulls: bool,
    pub allow_merge_commit: bool,
    pub allow_squash_merge: bool,
    pub allow_rebase_merge: bool,
    pub delete_branch_on_merge: bool,
    pub fork_parent_id: Option<RepoId>,
}

impl RepoRow {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner_name, self.name)
    }
    pub fn as_ref(&self) -> RepoRef {
        RepoRef { id: self.id, owner: self.owner_name.clone(), name: self.name.clone() }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct IssueRow {
    pub id: IssueId,
    pub repo_id: RepoId,
    pub number: i64,
    pub is_pull: bool,
    pub author_id: UserId,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub state_reason: Option<String>,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub pinned: bool,
    pub milestone_id: Option<MilestoneId>,
    pub comments_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_by: Option<UserId>,
}

pub const ISSUE_COLS: &str = "i.id, i.repo_id, i.number, i.is_pull, i.author_id, i.title, i.body, i.open, i.state_reason, i.locked, \
     i.lock_reason, i.pinned, i.milestone_id, i.comments_count, i.created_at, i.updated_at, i.closed_at, i.closed_by";

pub const REPO_COLS: &str = "r.id, r.owner_id, u.username::text AS owner_name, r.name::text AS name, r.default_branch, r.archived, \
     r.has_issues, r.has_pulls, r.allow_merge_commit, r.allow_squash_merge, r.allow_rebase_merge, r.delete_branch_on_merge, r.fork_parent_id";

impl Core {
    pub fn new(d: Deps) -> Self {
        Self { d }
    }

    pub fn db(&self) -> &PgPool {
        &self.d.db
    }

    // ------------------------------------------------------------ permissions

    pub async fn permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        if actor.is_anonymous() {
            return Ok(Permission::None);
        }
        self.d.authz.repo_permission(actor, repo).await
    }

    /// `NotFound` when the caller cannot see the repository at all.
    pub async fn need_read(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        let p = self.permission(actor, repo).await?;
        if p < Permission::Read {
            return Err(Error::NotFound);
        }
        Ok(p)
    }

    /// `NotFound` without read access, `Forbidden` with read but not `needed`.
    pub async fn need(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<Permission> {
        let p = self.permission(actor, repo).await?;
        if p < Permission::Read {
            return Err(Error::NotFound);
        }
        if p < needed {
            return Err(Error::forbidden(match needed {
                Permission::Triage => "You need triage access to do this",
                Permission::Write => "You need write access to do this",
                Permission::Maintain => "You need maintain access to do this",
                Permission::Admin => "You need admin access to do this",
                _ => "You do not have permission to do this",
            }));
        }
        Ok(p)
    }

    // ------------------------------------------------------------ lookups

    pub async fn repo(&self, id: RepoId) -> Result<RepoRow> {
        let sql = format!("SELECT {REPO_COLS} FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1");
        sqlx::query_as::<_, RepoRow>(&sql).bind(id).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    pub async fn repo_by_name(&self, owner: &str, name: &str) -> Result<Option<RepoRow>> {
        let sql = format!(
            "SELECT {REPO_COLS} FROM repositories r JOIN users u ON u.id = r.owner_id WHERE u.username = $1::citext AND r.name = $2::citext"
        );
        Ok(sqlx::query_as::<_, RepoRow>(&sql).bind(owner).bind(name).fetch_optional(self.db()).await?)
    }

    pub async fn issue_row(&self, repo: RepoId, number: i64) -> Result<IssueRow> {
        let sql = format!("SELECT {ISSUE_COLS} FROM issues i WHERE i.repo_id = $1 AND i.number = $2");
        sqlx::query_as::<_, IssueRow>(&sql).bind(repo).bind(number).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    pub async fn issue_row_tx(&self, tx: &mut ch_core::services::DbTx<'_>, repo: RepoId, number: i64) -> Result<IssueRow> {
        let sql = format!("SELECT {ISSUE_COLS} FROM issues i WHERE i.repo_id = $1 AND i.number = $2 FOR UPDATE");
        sqlx::query_as::<_, IssueRow>(&sql).bind(repo).bind(number).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
    }

    pub async fn user_refs(&self, ids: &[UserId]) -> Result<HashMap<UserId, UserRef>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let raw: Vec<i64> = ids.iter().map(|i| i.0).collect();
        let rows: Vec<(UserId, String, String, String)> =
            sqlx::query_as("SELECT id, username::text, display_name, kind FROM users WHERE id = ANY($1)")
                .bind(&raw)
                .fetch_all(self.db())
                .await?;
        Ok(rows
            .into_iter()
            .map(|(id, username, display_name, kind)| {
                (id, UserRef { id, username, display_name, kind: UserKind::parse(&kind).unwrap_or(UserKind::User) })
            })
            .collect())
    }

    pub async fn user_ref(&self, id: UserId) -> Result<UserRef> {
        self.user_refs(&[id]).await?.remove(&id).ok_or(Error::NotFound)
    }

    pub async fn user_id_by_name(&self, name: &str) -> Result<Option<UserId>> {
        let n = name.trim().trim_start_matches('@');
        Ok(sqlx::query_scalar("SELECT id FROM users WHERE username = $1::citext AND kind <> 'org'")
            .bind(n)
            .fetch_optional(self.db())
            .await?)
    }

    /// Name, primary email and a fallback, for a commit signature.
    pub async fn identity(&self, id: UserId) -> Result<(String, String)> {
        let row: Option<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT u.username::text, u.display_name, (SELECT e.email::text FROM user_emails e WHERE e.user_id = u.id ORDER BY e.is_primary DESC, e.id LIMIT 1)
             FROM users u WHERE u.id = $1",
        )
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        let (username, display, email) = row.ok_or(Error::NotFound)?;
        let name = if display.trim().is_empty() { username.clone() } else { display };
        let host = self.d.config.server.external_url.trim_start_matches("https://").trim_start_matches("http://");
        let host = host.split('/').next().unwrap_or("localhost").split(':').next().unwrap_or("localhost");
        Ok((name, email.unwrap_or_else(|| format!("{username}@users.noreply.{host}"))))
    }

    // ------------------------------------------------------------ hydration

    pub async fn labels_for(&self, issue_ids: &[IssueId]) -> Result<HashMap<IssueId, Vec<LabelInfo>>> {
        let mut out: HashMap<IssueId, Vec<LabelInfo>> = HashMap::new();
        if issue_ids.is_empty() {
            return Ok(out);
        }
        let raw: Vec<i64> = issue_ids.iter().map(|i| i.0).collect();
        let rows: Vec<(IssueId, LabelId, String, String, String)> = sqlx::query_as(
            "SELECT il.issue_id, l.id, l.name::text, l.color, l.description
             FROM issue_labels il JOIN labels l ON l.id = il.label_id
             WHERE il.issue_id = ANY($1) ORDER BY lower(l.name::text)",
        )
        .bind(&raw)
        .fetch_all(self.db())
        .await?;
        for (issue, id, name, color, description) in rows {
            let is_default = crate::labels::is_default_label(&name, &color);
            out.entry(issue).or_default().push(LabelInfo { id, name, color, description, is_default });
        }
        Ok(out)
    }

    pub async fn assignees_for(&self, issue_ids: &[IssueId]) -> Result<HashMap<IssueId, Vec<UserId>>> {
        let mut out: HashMap<IssueId, Vec<UserId>> = HashMap::new();
        if issue_ids.is_empty() {
            return Ok(out);
        }
        let raw: Vec<i64> = issue_ids.iter().map(|i| i.0).collect();
        let rows: Vec<(IssueId, UserId)> = sqlx::query_as(
            "SELECT a.issue_id, a.user_id FROM issue_assignees a JOIN users u ON u.id = a.user_id WHERE a.issue_id = ANY($1) ORDER BY u.username",
        )
        .bind(&raw)
        .fetch_all(self.db())
        .await?;
        for (i, u) in rows {
            out.entry(i).or_default().push(u);
        }
        Ok(out)
    }

    pub async fn milestones_by_id(&self, ids: &[MilestoneId]) -> Result<HashMap<MilestoneId, Milestone>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let raw: Vec<i64> = ids.iter().map(|i| i.0).collect();
        let rows = sqlx::query_as::<_, MilestoneRow>(&format!("{MILESTONE_SELECT} WHERE m.id = ANY($1) GROUP BY m.id"))
            .bind(&raw)
            .fetch_all(self.db())
            .await?;
        Ok(rows.into_iter().map(|r| (r.id, r.into())).collect())
    }

    pub async fn reaction_counts(&self, subject_type: &str, ids: &[i64]) -> Result<HashMap<i64, ReactionCounts>> {
        let mut out: HashMap<i64, ReactionCounts> = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(
            "SELECT subject_id, content, count(*) FROM reactions WHERE subject_type = $1 AND subject_id = ANY($2) GROUP BY subject_id, content",
        )
        .bind(subject_type)
        .bind(ids)
        .fetch_all(self.db())
        .await?;
        for (id, content, n) in rows {
            out.entry(id).or_default().add(&content, n);
        }
        Ok(out)
    }

    /// Turns issue rows into views with a handful of queries, however many rows there are.
    pub async fn hydrate(&self, rows: Vec<IssueRow>) -> Result<Vec<IssueView>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<IssueId> = rows.iter().map(|r| r.id).collect();
        let raw_ids: Vec<i64> = ids.iter().map(|i| i.0).collect();
        let labels = self.labels_for(&ids).await?;
        let assignees = self.assignees_for(&ids).await?;
        let reactions = self.reaction_counts("issue", &raw_ids).await?;

        let mut user_ids: HashSet<UserId> = HashSet::new();
        for r in &rows {
            user_ids.insert(r.author_id);
            if let Some(c) = r.closed_by {
                user_ids.insert(c);
            }
        }
        for v in assignees.values() {
            user_ids.extend(v.iter().copied());
        }
        let user_ids: Vec<UserId> = user_ids.into_iter().collect();
        let users = self.user_refs(&user_ids).await?;

        let ms_ids: Vec<MilestoneId> = rows.iter().filter_map(|r| r.milestone_id).collect::<HashSet<_>>().into_iter().collect();
        let milestones = self.milestones_by_id(&ms_ids).await?;

        let repo_ids: Vec<i64> = rows.iter().map(|r| r.repo_id.0).collect::<HashSet<_>>().into_iter().collect();
        let names: Vec<(RepoId, String, String)> = sqlx::query_as(
            "SELECT r.id, u.username::text, r.name::text FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = ANY($1)",
        )
        .bind(&repo_ids)
        .fetch_all(self.db())
        .await?;
        let names: HashMap<RepoId, String> = names.into_iter().map(|(id, o, n)| (id, format!("{o}/{n}"))).collect();

        let pull_ids: Vec<i64> = rows.iter().filter(|r| r.is_pull).map(|r| r.id.0).collect();
        let merged: HashMap<IssueId, Option<DateTime<Utc>>> = if pull_ids.is_empty() {
            HashMap::new()
        } else {
            sqlx::query_as::<_, (IssueId, Option<DateTime<Utc>>)>("SELECT issue_id, merged_at FROM pull_requests WHERE issue_id = ANY($1)")
                .bind(&pull_ids)
                .fetch_all(self.db())
                .await?
                .into_iter()
                .collect()
        };

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let author = users.get(&r.author_id).cloned().unwrap_or_else(|| ghost(r.author_id));
            let labs = labels.get(&r.id).cloned().unwrap_or_default();
            let assignee_ids = assignees.get(&r.id).cloned().unwrap_or_default();
            let assignee_refs: Vec<UserRef> = assignee_ids.iter().filter_map(|u| users.get(u).cloned()).collect();
            let issue = Issue {
                id: r.id,
                repo_id: r.repo_id,
                number: r.number,
                is_pull: r.is_pull,
                author_id: r.author_id,
                title: r.title.clone(),
                body: r.body.clone(),
                open: r.open,
                state_reason: r.state_reason.clone(),
                locked: r.locked,
                labels: labs.iter().map(|l| l.name.clone()).collect(),
                assignees: assignee_ids,
                milestone_id: r.milestone_id,
                comments: r.comments_count as i64,
                created_at: r.created_at,
                updated_at: r.updated_at,
                closed_at: r.closed_at,
            };
            out.push(IssueView {
                issue,
                author,
                assignees: assignee_refs,
                labels: labs,
                milestone: r.milestone_id.and_then(|m| milestones.get(&m).cloned()),
                closed_by: r.closed_by.and_then(|u| users.get(&u).cloned()),
                reactions: reactions.get(&r.id.0).copied().unwrap_or_default(),
                lock_reason: r.lock_reason.clone(),
                pinned: r.pinned,
                merged_at: merged.get(&r.id).copied().flatten(),
                repo_full_name: names.get(&r.repo_id).cloned().unwrap_or_default(),
            });
        }
        Ok(out)
    }

    pub async fn hydrate_one(&self, row: IssueRow) -> Result<IssueView> {
        self.hydrate(vec![row]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    // ------------------------------------------------------------ timeline and references

    pub async fn add_event(
        tx: &mut ch_core::services::DbTx<'_>,
        issue: IssueId,
        actor: Option<UserId>,
        event: &str,
        data: Value,
    ) -> Result<i64> {
        let id: i64 =
            sqlx::query_scalar("INSERT INTO timeline_events (issue_id, actor_id, event, data) VALUES ($1, $2, $3, $4) RETURNING id")
                .bind(issue)
                .bind(actor)
                .bind(event)
                .bind(data)
                .fetch_one(&mut **tx)
                .await?;
        Ok(id)
    }

    /// Bumps `updated_at`, so lists sorted by recent activity stay honest.
    pub async fn touch(tx: &mut ch_core::services::DbTx<'_>, issue: IssueId) -> Result<()> {
        sqlx::query("UPDATE issues SET updated_at = now() WHERE id = $1").bind(issue).execute(&mut **tx).await?;
        Ok(())
    }

    /// Subscribes people who took part, unless they chose otherwise already.
    pub async fn subscribe_default(tx: &mut ch_core::services::DbTx<'_>, issue: IssueId, user: UserId, reason: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO issue_subscriptions (issue_id, user_id, subscribed, reason) VALUES ($1, $2, true, $3) ON CONFLICT DO NOTHING",
        )
        .bind(issue)
        .bind(user)
        .bind(reason)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Reads `#12` and `owner/repo#12` in `text` and leaves a `cross-referenced` event on each
    /// target the actor may read, once per source. Also subscribes people who were mentioned
    /// and can read the repository. Pure side effects inside the caller's transaction.
    pub async fn record_references(
        &self,
        tx: &mut ch_core::services::DbTx<'_>,
        actor: &Actor,
        source: &IssueRow,
        source_repo: &RepoRow,
        text_in: &str,
    ) -> Result<()> {
        let refs = text::refs(text_in);
        let actor_user = actor.author_id();
        for r in refs.issues.iter().take(25) {
            let Some(target) = self.resolve_ref(source_repo, r).await? else { continue };
            if target.id == source.id {
                continue;
            }
            if target.repo_id != source.repo_id && self.permission(actor, target.repo_id).await? < Permission::Read {
                continue;
            }
            // A connected app leaves a cross-reference only where it was given write access.
            if let Actor::App { grant, .. } = actor
                && target.repo_id != source.repo_id
                && !grant.write.has(target.repo_id)
            {
                continue;
            }
            let already: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM timeline_events WHERE issue_id = $1 AND event = 'cross-referenced' AND (data->>'source_id')::bigint = $2",
            )
            .bind(target.id)
            .bind(source.id)
            .fetch_optional(&mut **tx)
            .await?;
            if already.is_some() {
                continue;
            }
            let data = json!({
                "source_id": source.id.0,
                "source_repo_id": source_repo.id.0,
                "source": { "repo": source_repo.full_name(), "number": source.number, "title": source.title, "is_pull": source.is_pull }
            });
            Self::add_event(tx, target.id, actor_user, "cross-referenced", data).await?;
        }
        for name in refs.mentions.iter().take(25) {
            if let Some(uid) = self.user_id_by_name(name).await?
                && self.permission(&util::as_user(uid), source.repo_id).await? >= Permission::Read
            {
                Self::subscribe_default(tx, source.id, uid, "mentioned").await?;
            }
        }
        Ok(())
    }

    async fn resolve_ref(&self, source_repo: &RepoRow, r: &IssueRef) -> Result<Option<IssueRow>> {
        let repo_id = match &r.repo {
            None => source_repo.id,
            Some((o, n)) => match self.repo_by_name(o, n).await? {
                Some(x) => x.id,
                None => return Ok(None),
            },
        };
        let sql = format!("SELECT {ISSUE_COLS} FROM issues i WHERE i.repo_id = $1 AND i.number = $2");
        Ok(sqlx::query_as::<_, IssueRow>(&sql).bind(repo_id).bind(r.number).fetch_optional(self.db()).await?)
    }

    /// The repositories among `ids` that the actor may read.
    pub async fn readable(&self, actor: &Actor, ids: Vec<RepoId>) -> Result<HashSet<RepoId>> {
        let mut seen: HashMap<RepoId, bool> = HashMap::new();
        for id in ids {
            if let std::collections::hash_map::Entry::Vacant(v) = seen.entry(id) {
                v.insert(self.permission(actor, id).await? >= Permission::Read);
            }
        }
        Ok(seen.into_iter().filter(|(_, ok)| *ok).map(|(id, _)| id).collect())
    }
}

/// A user who no longer exists: the row an author column points at can vanish only through
/// manual database work, so show something rather than fail a whole page.
pub fn ghost(id: UserId) -> UserRef {
    UserRef { id, username: "ghost".into(), display_name: "Deleted user".into(), kind: UserKind::User }
}

// -------------------------------------------------------------------- milestones rows

#[derive(Debug, Clone, FromRow)]
pub struct MilestoneRow {
    pub id: MilestoneId,
    pub repo_id: RepoId,
    pub number: i64,
    pub title: String,
    pub description: String,
    pub open: bool,
    pub due_on: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub open_issues: i64,
    pub closed_issues: i64,
}

impl From<MilestoneRow> for Milestone {
    fn from(r: MilestoneRow) -> Self {
        Milestone {
            id: r.id,
            repo_id: r.repo_id,
            number: r.number,
            title: r.title,
            description: r.description,
            open: r.open,
            due_on: r.due_on,
            closed_at: r.closed_at,
            created_at: r.created_at,
            open_issues: r.open_issues,
            closed_issues: r.closed_issues,
        }
    }
}

pub const MILESTONE_SELECT: &str = "SELECT m.id, m.repo_id, m.number, m.title, m.description, m.open, m.due_on, m.closed_at, m.created_at,
        count(i.id) FILTER (WHERE i.open) AS open_issues, count(i.id) FILTER (WHERE NOT i.open) AS closed_issues
     FROM milestones m LEFT JOIN issues i ON i.milestone_id = m.id";

/// Convenience used all over: a comment id from a raw column.
pub fn comment_id(v: i64) -> CommentId {
    CommentId(v)
}
