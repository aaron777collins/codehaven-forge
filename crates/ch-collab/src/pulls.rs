//! Pull requests: opening, tracking head and base, mergeability, close and reopen, files and
//! commits. Merging and the gate are in `merge.rs`.

use crate::core::{Core, IssueRow, RepoRow, ghost};
use crate::filter::Spec;
use crate::query::Kind;
use crate::util::{self, writing_user};
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::services::DbTx;
use ch_core::{Actor, Error, FieldError, IssueId, Permission, PullId, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::FromRow;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, FromRow)]
pub struct PullRow {
    pub issue_id: IssueId,
    pub repo_id: RepoId,
    pub head_repo_id: Option<RepoId>,
    /// Set when the head came from another repository, and kept after that fork is deleted and
    /// `head_repo_id` goes NULL (migration 0122).
    pub from_fork: bool,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    pub merge_base: Option<String>,
    pub mergeable_state: String,
    pub merge_commit_sha: Option<String>,
    pub merge_method: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub merged_by: Option<UserId>,
    pub draft: bool,
    pub auto_merge_method: Option<String>,
    pub auto_merge_by: Option<UserId>,
    /// The head auto-merge was armed for (ch-m2y). It merges this commit and no other.
    pub auto_merge_head_sha: Option<String>,
    pub maintainer_can_modify: bool,
    pub commits_count: i32,
    pub additions: i32,
    pub deletions: i32,
    pub changed_files: i32,
}

pub const PULL_COLS: &str = "p.issue_id, p.repo_id, p.head_repo_id, p.from_fork, p.head_ref, p.head_sha, p.base_ref, p.base_sha, p.merge_base, p.mergeable_state, \
     p.merge_commit_sha, p.merge_method, p.merged_at, p.merged_by, p.draft, p.auto_merge_method, p.auto_merge_by, p.auto_merge_head_sha, p.maintainer_can_modify, \
     p.commits_count, p.additions, p.deletions, p.changed_files";

/// The repository that holds a pull request's head branch, in SQL. NULL once the fork it came
/// from is deleted, so it never matches the base repository's branch of the same name.
pub const HEAD_REPO_SQL: &str = "COALESCE(p.head_repo_id, CASE WHEN p.from_fork THEN NULL ELSE p.repo_id END)";

pub fn parse_mergeable(s: &str) -> MergeableState {
    match s {
        "clean" => MergeableState::Clean,
        "conflict" => MergeableState::Conflict,
        "behind" => MergeableState::Behind,
        "blocked" => MergeableState::Blocked,
        _ => MergeableState::Unknown,
    }
}

pub fn mergeable_str(m: MergeableState) -> &'static str {
    match m {
        MergeableState::Clean => "clean",
        MergeableState::Conflict => "conflict",
        MergeableState::Behind => "behind",
        MergeableState::Blocked => "blocked",
        MergeableState::Unknown => "unknown",
    }
}

pub fn parse_method(s: &str) -> Option<MergeMethod> {
    match s {
        "merge" => Some(MergeMethod::Merge),
        "squash" => Some(MergeMethod::Squash),
        "rebase" => Some(MergeMethod::Rebase),
        _ => None,
    }
}

pub fn method_str(m: MergeMethod) -> &'static str {
    match m {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    }
}

/// `refs/heads/x` and `x` are the same branch.
pub fn short_branch(r: &str) -> &str {
    r.strip_prefix("refs/heads/").unwrap_or(r)
}

pub fn full_branch(r: &str) -> String {
    format!("refs/heads/{}", short_branch(r))
}

impl PullRow {
    pub fn mergeable(&self) -> MergeableState {
        parse_mergeable(&self.mergeable_state)
    }

    /// The repository that holds the head branch, for comparing two head commits. A base
    /// commit is not always in a fork: compare against the base with `Core::compare_pull`.
    pub fn git_repo(&self) -> RepoId {
        self.head_repo_id.unwrap_or(self.repo_id)
    }

    /// The head came from another repository, including one that has since been deleted.
    pub fn is_fork(&self) -> bool {
        self.from_fork || self.head_repo_id.is_some_and(|h| h != self.repo_id)
    }

    /// The fork this pull request came from was deleted. Its `head_ref` names a branch that no
    /// longer exists anywhere, and the base repository's branch of the same name is not it.
    pub fn head_gone(&self) -> bool {
        self.from_fork && self.head_repo_id.is_none()
    }

    /// The repository that holds the head branch, or None once a fork it came from is deleted.
    /// Never the base repository in that case: the base may have a branch with the same name.
    pub fn head_repo(&self) -> Option<RepoId> {
        if self.head_gone() { None } else { Some(self.head_repo_id.unwrap_or(self.repo_id)) }
    }
}

impl Core {
    // ------------------------------------------------------------ loading

    pub async fn pull_row(&self, issue: IssueId) -> Result<PullRow> {
        let sql = format!("SELECT {PULL_COLS} FROM pull_requests p WHERE p.issue_id = $1");
        sqlx::query_as::<_, PullRow>(&sql).bind(issue).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    pub async fn pull_row_tx(&self, tx: &mut DbTx<'_>, issue: IssueId) -> Result<PullRow> {
        let sql = format!("SELECT {PULL_COLS} FROM pull_requests p WHERE p.issue_id = $1 FOR UPDATE");
        sqlx::query_as::<_, PullRow>(&sql).bind(issue).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
    }

    /// The issue row and pull row of `repo#number`. `NotFound` when it is a plain issue.
    pub async fn pull_pair(&self, repo: RepoId, number: i64) -> Result<(IssueRow, PullRow)> {
        let issue = self.issue_row(repo, number).await?;
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        let pull = self.pull_row(issue.id).await?;
        Ok((issue, pull))
    }

    async fn repo_refs(&self, ids: &[RepoId]) -> Result<HashMap<RepoId, RepoRef>> {
        let raw: Vec<i64> = ids.iter().map(|i| i.0).collect::<HashSet<_>>().into_iter().collect();
        if raw.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(RepoId, String, String)> = sqlx::query_as(
            "SELECT r.id, u.username::text, r.name::text FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = ANY($1)",
        )
        .bind(&raw)
        .fetch_all(self.db())
        .await?;
        Ok(rows.into_iter().map(|(id, owner, name)| (id, RepoRef { id, owner, name })).collect())
    }

    /// Turns pairs of rows into views in a few queries.
    pub async fn hydrate_pulls(&self, pairs: Vec<(IssueRow, PullRow)>) -> Result<Vec<PullView>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let issues = self.hydrate(pairs.iter().map(|(i, _)| i.clone()).collect()).await?;
        let mut repo_ids: Vec<RepoId> = Vec::new();
        let mut users: Vec<UserId> = Vec::new();
        for (_, p) in &pairs {
            repo_ids.push(p.repo_id);
            if let Some(h) = p.head_repo_id {
                repo_ids.push(h);
            }
            users.extend(p.merged_by);
            users.extend(p.auto_merge_by);
        }
        let repos = self.repo_refs(&repo_ids).await?;
        let pull_ids: Vec<i64> = pairs.iter().map(|(i, _)| i.id.0).collect();

        // A requested team carries its access to that pull request's repository (ch-4dv).
        #[allow(clippy::type_complexity)]
        let reqs: Vec<(IssueId, Option<UserId>, Option<ch_core::TeamId>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT rr.pull_id, rr.user_id, rr.team_id, o.username::text, t.name::text, tr.permission
             FROM review_requests rr
             JOIN issues i ON i.id = rr.pull_id
             LEFT JOIN teams t ON t.id = rr.team_id
             LEFT JOIN users o ON o.id = t.org_id
             LEFT JOIN team_repos tr ON tr.team_id = rr.team_id AND tr.repo_id = i.repo_id
             WHERE rr.pull_id = ANY($1) ORDER BY rr.id",
        )
        .bind(&pull_ids)
        .fetch_all(self.db())
        .await?;
        users.extend(reqs.iter().filter_map(|r| r.1));
        let counts: Vec<(IssueId, i64)> = sqlx::query_as(
            "SELECT c.pull_id, count(*) FROM review_comments c LEFT JOIN reviews r ON r.id = c.review_id
             WHERE c.pull_id = ANY($1) AND (r.id IS NULL OR r.state <> 'pending') GROUP BY c.pull_id",
        )
        .bind(&pull_ids)
        .fetch_all(self.db())
        .await?;
        let counts: HashMap<IssueId, i64> = counts.into_iter().collect();
        let user_refs = self.user_refs(&users).await?;

        let mut out = Vec::with_capacity(pairs.len());
        for ((_, p), issue) in pairs.into_iter().zip(issues) {
            let id = issue.issue.id;
            let requested = RequestedReviewers {
                users: reqs.iter().filter(|r| r.0 == id).filter_map(|r| r.1).filter_map(|u| user_refs.get(&u).cloned()).collect(),
                teams: reqs
                    .iter()
                    .filter(|r| r.0 == id)
                    .filter_map(|r| match (r.2, &r.3, &r.4) {
                        (Some(t), Some(org), Some(name)) => Some(TeamRef {
                            id: t,
                            org: org.clone(),
                            name: name.clone(),
                            permission: r.5.as_deref().map(github_permission).map(str::to_string),
                        }),
                        _ => None,
                    })
                    .collect(),
            };
            let pr = PullRequest {
                issue: issue.issue.clone(),
                head_repo_id: p.head_repo_id,
                from_fork: p.is_fork(),
                head_ref: p.head_ref.clone(),
                head_sha: p.head_sha.clone(),
                base_ref: p.base_ref.clone(),
                base_sha: p.base_sha.clone(),
                merge_base: p.merge_base.clone(),
                mergeable: p.mergeable(),
                merge_commit_sha: p.merge_commit_sha.clone(),
                merged_at: p.merged_at,
                merged_by: p.merged_by,
                draft: p.draft,
            };
            out.push(PullView {
                pull: pr,
                issue,
                head_repo: p.head_repo_id.and_then(|h| repos.get(&h).cloned()),
                base_repo: repos.get(&p.repo_id).cloned().unwrap_or(RepoRef { id: p.repo_id, owner: String::new(), name: String::new() }),
                merged_by: p.merged_by.map(|u| user_refs.get(&u).cloned().unwrap_or_else(|| ghost(u))),
                merge_method: p.merge_method.as_deref().and_then(parse_method),
                stats: PullStats {
                    commits: p.commits_count as u32,
                    additions: p.additions as u32,
                    deletions: p.deletions as u32,
                    changed_files: p.changed_files as u32,
                },
                requested,
                review_comments: counts.get(&id).copied().unwrap_or(0) as u32,
                auto_merge: match (p.auto_merge_method.as_deref().and_then(parse_method), p.auto_merge_by) {
                    (Some(method), Some(u)) => {
                        Some(AutoMergeInfo { method, enabled_by: user_refs.get(&u).cloned().unwrap_or_else(|| ghost(u)) })
                    }
                    _ => None,
                },
                maintainer_can_modify: p.maintainer_can_modify,
            });
        }
        Ok(out)
    }

    pub async fn hydrate_pull(&self, issue: IssueRow, pull: PullRow) -> Result<PullView> {
        self.hydrate_pulls(vec![(issue, pull)]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    pub async fn pull_view(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullView> {
        self.need_view(actor, repo).await?;
        let (issue, mut pull) = self.pull_pair(repo, number).await?;
        if pull.mergeable() == MergeableState::Unknown && issue.open {
            self.refresh_pull(&issue, &pull).await;
            pull = self.pull_row(issue.id).await?;
        }
        self.hydrate_pull(issue, pull).await
    }

    pub async fn pull_views(&self, actor: &Actor, repo: RepoId, filter: PullFilter) -> Result<Vec<PullView>> {
        self.need_view(actor, repo).await?;
        let (offset, limit) = util::page(filter.issue.page, filter.issue.per_page);
        let head = filter.head.as_deref().map(|h| match h.split_once(':') {
            Some((o, b)) => (Some(o.to_string()), short_branch(b).to_string()),
            None => (None, short_branch(h).to_string()),
        });
        let spec = Spec {
            repo: Some(repo),
            filter: filter.issue,
            force_kind: Some(Kind::Pull),
            head,
            base: filter.base.as_deref().map(|b| short_branch(b).to_string()),
            ..Default::default()
        };
        let rows = self.query_issue_rows(actor, &spec, offset, limit).await?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id.0).collect();
        let sql = format!("SELECT {PULL_COLS} FROM pull_requests p WHERE p.issue_id = ANY($1)");
        let mut pulls: HashMap<IssueId, PullRow> =
            sqlx::query_as::<_, PullRow>(&sql).bind(&ids).fetch_all(self.db()).await?.into_iter().map(|p| (p.issue_id, p)).collect();
        let pairs: Vec<(IssueRow, PullRow)> = rows.into_iter().filter_map(|r| pulls.remove(&r.id).map(|p| (r, p))).collect();
        self.hydrate_pulls(pairs).await
    }

    pub async fn pull_open_for_branch(
        &self,
        actor: &Actor,
        repo: RepoId,
        head_repo: RepoId,
        head_ref: &str,
        base_ref: &str,
    ) -> Result<Option<PullView>> {
        self.need_view(actor, repo).await?;
        let sql = format!(
            "SELECT {} FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             WHERE p.repo_id = $1 AND {HEAD_REPO_SQL} = $2 AND p.head_ref = $3 AND p.base_ref = $4 AND i.open ORDER BY i.number DESC LIMIT 1",
            crate::core::ISSUE_COLS
        );
        let Some(issue) = sqlx::query_as::<_, IssueRow>(&sql)
            .bind(repo)
            .bind(head_repo)
            .bind(short_branch(head_ref))
            .bind(short_branch(base_ref))
            .fetch_optional(self.db())
            .await?
        else {
            return Ok(None);
        };
        let pull = self.pull_row(issue.id).await?;
        Ok(Some(self.hydrate_pull(issue, pull).await?))
    }

    // ------------------------------------------------------------ create

    async fn network_of(&self, id: RepoId) -> Result<RepoId> {
        let r: Option<Option<RepoId>> =
            sqlx::query_scalar("SELECT fork_root_id FROM repositories WHERE id = $1").bind(id).fetch_optional(self.db()).await?;
        match r {
            None => Err(Error::NotFound),
            Some(root) => Ok(root.unwrap_or(id)),
        }
    }

    pub async fn pull_create(&self, actor: &Actor, new: NewPull, client: &ClientInfo) -> Result<PullView> {
        let user = writing_user(actor, new.repo_id)?;
        let base_repo = self.repo(new.repo_id).await.map_err(|_| Error::NotFound)?;
        self.need_contribute(actor, base_repo.id).await?;
        Self::ensure_writable(&base_repo)?;
        if !base_repo.has_pulls {
            return Err(Error::invalid("PullRequest", "base", "Pull requests are turned off for this repository"));
        }
        let title = util::title("PullRequest", "title", &new.title)?;
        let body = util::body("PullRequest", "body", &new.body)?;
        let base_ref = short_branch(new.base_ref.trim()).to_string();
        let head_ref = short_branch(new.head_ref.trim()).to_string();
        if base_ref.is_empty() || head_ref.is_empty() {
            return Err(Error::Validation(vec![FieldError::new(
                "PullRequest",
                if base_ref.is_empty() { "base" } else { "head" },
                "missing_field",
            )]));
        }
        let head_repo_id = new.head_repo_id;
        // Without a grant on the base, a pull request comes from your own fork, or a fork owned
        // by an organization you own: never from the base itself or someone else's fork (ch-8tj).
        let granted = self.d.authz.has_grant(actor, base_repo.id).await?;
        if head_repo_id != base_repo.id {
            self.need_read(actor, head_repo_id).await?;
            if self.network_of(head_repo_id).await? != self.network_of(base_repo.id).await? {
                return Err(Error::invalid(
                    "PullRequest",
                    "head",
                    "The head repository must be a fork of this repository or the repository itself",
                ));
            }
            if !granted && !self.owns_repo_account(actor, user, head_repo_id).await? {
                return Err(Error::invalid("PullRequest", "head", OWN_FORK_ONLY));
            }
            // Opening the pull request copies the head's commits into this repository, where
            // everyone who reads it sees them. Reading someone's private fork is not enough to
            // publish it (ch-g3r).
            if !self.may_copy(user, head_repo_id, base_repo.id).await? {
                return Err(Error::invalid("PullRequest", "head", NOT_YOURS_TO_SHARE));
            }
        } else if !granted {
            return Err(Error::invalid("PullRequest", "head", OWN_FORK_ONLY));
        } else if head_ref == base_ref {
            return Err(Error::invalid("PullRequest", "head", "The head and base branches must be different"));
        }

        let base_sha = self.d.git.resolve(base_repo.id, &full_branch(&base_ref)).await.map_err(|e| {
            if matches!(e, Error::NotFound) { Error::Validation(vec![FieldError::new("PullRequest", "base", "invalid")]) } else { e }
        })?;
        let head_sha = self.d.git.resolve(head_repo_id, &full_branch(&head_ref)).await.map_err(|e| {
            if matches!(e, Error::NotFound) { Error::Validation(vec![FieldError::new("PullRequest", "head", "invalid")]) } else { e }
        })?;

        let existing: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT i.number FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             WHERE p.repo_id = $1 AND {HEAD_REPO_SQL} = $2 AND p.head_ref = $3 AND p.base_ref = $4 AND i.open LIMIT 1",
        ))
        .bind(base_repo.id)
        .bind(head_repo_id)
        .bind(&head_ref)
        .bind(&base_ref)
        .fetch_optional(self.db())
        .await?;
        if let Some(n) = existing {
            return Err(Error::Validation(vec![FieldError::custom(
                "PullRequest",
                "head",
                format!("A pull request already exists for this branch (#{n})"),
            )]));
        }

        // A fork keeps its own objects, so the base commit may not be in it (ch-8tj).
        let cmp = self.d.git.compare_across(base_repo.id, &base_sha, head_repo_id, &head_sha).await?;
        if cmp.ahead_by == 0 {
            return Err(Error::Validation(vec![FieldError::custom(
                "PullRequest",
                "head",
                "There are no commits between the base and head branches",
            )]));
        }
        let stats = stats_of(&cmp);

        let mut tx = self.db().begin().await?;
        let row = self.alloc_issue(&mut tx, base_repo.id, true, user, &title, &body).await?;
        sqlx::query(
            "INSERT INTO pull_requests (issue_id, repo_id, head_repo_id, head_ref, head_sha, base_ref, base_sha, merge_base, draft, commits_count, additions, deletions, changed_files)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(row.id)
        .bind(base_repo.id)
        .bind(if head_repo_id == base_repo.id { None } else { Some(head_repo_id) })
        .bind(&head_ref)
        .bind(&head_sha)
        .bind(&base_ref)
        .bind(&base_sha)
        .bind(&cmp.merge_base)
        .bind(new.draft)
        .bind(stats.commits as i32)
        .bind(stats.additions as i32)
        .bind(stats.deletions as i32)
        .bind(stats.changed_files as i32)
        .execute(&mut *tx)
        .await?;
        Self::subscribe_default(&mut tx, row.id, user, "author").await?;
        self.record_references(&mut tx, actor, &row, &base_repo, &format!("{}\n{}", title, body)).await?;
        self.d
            .events
            .emit(
                &mut tx,
                DomainEvent::PrOpened {
                    repo_id: base_repo.id,
                    pull_id: PullId(row.id.0),
                    number: row.number,
                    actor: user,
                    app: actor.app_origin(base_repo.id),
                    // The person opening it chose this head. A push that lands before CI
                    // handles this event is judged by its own event instead.
                    head_sha: Some(head_sha.clone()),
                    pusher: Some(user),
                },
            )
            .await?;
        tx.commit().await?;

        // Everything below is best effort: the pull request exists either way and the
        // background sync fills in whatever failed here.
        let pull = self.pull_row(row.id).await?;
        let _ = client;
        self.publish_pull_ref(&row, &pull).await;
        self.refresh_pull(&row, &pull).await;
        self.request_code_owners(&row, &pull).await;
        let row = self.issue_row_by_id(row.id).await?;
        let pull = self.pull_row(row.id).await?;
        self.hydrate_pull(row, pull).await
    }

    /// `refs/pull/<n>/head` in the base repository, so CI and `git fetch origin pull/1/head` work.
    /// It follows the stored head, which only ever holds a commit the copy rule allowed (ch-cjp).
    pub async fn publish_pull_ref(&self, issue: &IssueRow, pull: &PullRow) {
        let name = format!("refs/pull/{}/head", issue.number);
        let git = &self.d.git;
        let r = match git.create_ref(&Actor::System, pull.repo_id, &name, &pull.head_sha, &ClientInfo::local()).await {
            Err(Error::Validation(v)) if v.iter().any(|f| f.code == "already_exists") => {
                git.update_ref(&Actor::System, pull.repo_id, &name, &pull.head_sha, true, &ClientInfo::local()).await
            }
            other => other,
        };
        if let Err(e) = r {
            tracing::debug!(repo = pull.repo_id.0, number = issue.number, error = %e, "could not write the pull ref");
        }
    }

    /// May commits of `source` be copied into `dest` because `user` asked (ch-g3r)? Yes with
    /// write access on the source, which an owner of a fork has, or when the source is already
    /// open to everyone who can read the destination. Reading a private fork is not enough.
    pub(crate) async fn may_copy(&self, user: UserId, source: RepoId, dest: RepoId) -> Result<bool> {
        if source == dest {
            return Ok(true);
        }
        let perm = self.permission(&util::as_user(user), source).await?;
        if perm >= Permission::Write {
            return Ok(true);
        }
        if perm < Permission::Read {
            return Ok(false);
        }
        // Private 0, internal 1, public 2 (1 while public repositories are switched off).
        let ranks: Vec<(RepoId, i32)> = sqlx::query_as(
            "SELECT r.id, CASE r.access_tier WHEN 'public' THEN CASE WHEN (SELECT allow_public_repos FROM visibility_settings) THEN 2 ELSE 1 END \
                    WHEN 'internal' THEN 1 ELSE 0 END \
             FROM repositories r WHERE r.id = ANY($1)",
        )
        .bind(vec![source.0, dest.0])
        .fetch_all(self.db())
        .await?;
        let rank = |id: RepoId| ranks.iter().find(|(r, _)| *r == id).map(|(_, n)| *n);
        Ok(match (rank(source), rank(dest)) {
            (Some(s), Some(d)) => s >= 1 && s >= d,
            _ => false,
        })
    }

    // ------------------------------------------------------------ recompute

    /// Reads the current head and base, updates counts, merge base and mergeability. Errors are
    /// logged and leave the stored values alone.
    pub async fn refresh_pull(&self, issue: &IssueRow, pull: &PullRow) {
        if let Err(e) = self.refresh_pull_inner(issue, pull).await {
            tracing::warn!(repo = pull.repo_id.0, number = issue.number, error = %e, "could not refresh a pull request");
        }
    }

    async fn refresh_pull_inner(&self, issue: &IssueRow, pull: &PullRow) -> Result<()> {
        let base_sha = self.d.git.resolve(pull.repo_id, &full_branch(&pull.base_ref)).await.unwrap_or_else(|_| pull.base_sha.clone());
        let (head_sha, state) = match pull.head_repo() {
            Some(head_repo) => {
                let live = self.d.git.resolve(head_repo, &full_branch(&pull.head_ref)).await.unwrap_or_else(|_| pull.head_sha.clone());
                // A later head is copied into the base only while the author may still share it
                // there (ch-g3r). Otherwise the pull request keeps the head it had.
                let sha = if live != pull.head_sha && !self.may_copy(issue.author_id, head_repo, pull.repo_id).await.unwrap_or(false) {
                    tracing::info!(
                        repo = pull.repo_id.0,
                        number = issue.number,
                        "a new head is held: its author may no longer share it here"
                    );
                    pull.head_sha.clone()
                } else {
                    live
                };
                // The test merge reads exactly that commit, never the branch as it is now.
                let state = self
                    .d
                    .git
                    .test_merge(pull.repo_id, &full_branch(&pull.base_ref), head_repo, &sha)
                    .await
                    .unwrap_or(MergeableState::Unknown);
                (sha, state)
            }
            // The fork is gone, so its head cannot move. The base's branch of the same name is
            // someone else's work and must not stand in for it.
            None => (pull.head_sha.clone(), pull.mergeable()),
        };
        let cmp = self.compare_pull(pull, &base_sha, &head_sha).await.ok();
        let (merge_base, stats) = match &cmp {
            Some(c) => (Some(c.merge_base.clone()), stats_of(c)),
            None => (
                pull.merge_base.clone(),
                PullStats {
                    commits: pull.commits_count as u32,
                    additions: pull.additions as u32,
                    deletions: pull.deletions as u32,
                    changed_files: pull.changed_files as u32,
                },
            ),
        };
        // `Blocked` is policy, not git. Textually the pull request merges cleanly.
        let state = if state == MergeableState::Blocked { MergeableState::Clean } else { state };
        sqlx::query(
            "UPDATE pull_requests SET head_sha = $2, base_sha = $3, merge_base = $4, mergeable_state = $5, commits_count = $6, additions = $7, deletions = $8, changed_files = $9
             WHERE issue_id = $1 AND merged_at IS NULL",
        )
        .bind(issue.id)
        .bind(&head_sha)
        .bind(&base_sha)
        .bind(&merge_base)
        .bind(mergeable_str(state))
        .bind(stats.commits as i32)
        .bind(stats.additions as i32)
        .bind(stats.deletions as i32)
        .bind(stats.changed_files as i32)
        .execute(self.db())
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------ update, close, reopen, draft

    pub async fn pull_update(&self, actor: &Actor, repo: RepoId, number: i64, patch: PullPatch, _client: &ClientInfo) -> Result<PullView> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let mut tx = self.db().begin().await?;
        let issue = self.issue_row_tx(&mut tx, repo, number).await?;
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        let pull = self.pull_row_tx(&mut tx, issue.id).await?;
        self.edit_in_tx(&mut tx, actor, user, perm, &rrow, &issue, patch.title.clone(), patch.body.clone()).await?;
        let mut base_changed = false;
        if let Some(base) = patch.base.as_deref() {
            let base = short_branch(base.trim()).to_string();
            if base != pull.base_ref {
                if !(issue.author_id == user || perm >= Permission::Write) {
                    return Err(Error::forbidden("You can only change the base of your own pull request"));
                }
                if !issue.open || pull.merged_at.is_some() {
                    return Err(Error::invalid("PullRequest", "base", "The base of a closed pull request cannot change"));
                }
                if pull.head_repo() == Some(repo) && pull.head_ref == base {
                    return Err(Error::invalid("PullRequest", "base", "The head and base branches must be different"));
                }
                let sha = self.d.git.resolve(repo, &full_branch(&base)).await.map_err(|e| {
                    if matches!(e, Error::NotFound) {
                        Error::Validation(vec![FieldError::new("PullRequest", "base", "invalid")])
                    } else {
                        e
                    }
                })?;
                sqlx::query("UPDATE pull_requests SET base_ref = $2, base_sha = $3, mergeable_state = 'unknown' WHERE issue_id = $1")
                    .bind(issue.id)
                    .bind(&base)
                    .bind(&sha)
                    .execute(&mut *tx)
                    .await?;
                Self::add_event(&mut tx, issue.id, Some(user), "base_ref_changed", json!({ "from": pull.base_ref, "to": base })).await?;
                // The person who armed auto-merge chose a base. Only someone with write access
                // may move it without turning auto-merge off (ch-m2y).
                if pull.auto_merge_method.is_some() && perm < Permission::Write {
                    sqlx::query("UPDATE pull_requests SET auto_merge_method = NULL, auto_merge_by = NULL, auto_merge_head_sha = NULL WHERE issue_id = $1")
                        .bind(issue.id)
                        .execute(&mut *tx)
                        .await?;
                    Self::add_event(&mut tx, issue.id, Some(user), "auto_merge_disabled", json!({ "reason": "the base branch changed" }))
                        .await?;
                }
                base_changed = true;
            }
        }
        tx.commit().await?;
        let issue = self.issue_row_by_id(issue.id).await?;
        let pull = self.pull_row(issue.id).await?;
        if base_changed {
            self.refresh_pull(&issue, &pull).await;
        }
        let pull = self.pull_row(issue.id).await?;
        self.hydrate_pull(issue, pull).await
    }

    /// A closed pull request can be reopened only while its head branch still exists.
    pub async fn pull_reopen_check(&self, issue: &IssueRow) -> Result<()> {
        let pull = self.pull_row(issue.id).await?;
        if pull.merged_at.is_some() {
            return Err(Error::invalid("PullRequest", "state", "A merged pull request cannot be reopened"));
        }
        let head_exists = match pull.head_repo() {
            Some(head_repo) => self.d.git.resolve(head_repo, &full_branch(&pull.head_ref)).await.is_ok(),
            None => false,
        };
        if !head_exists {
            return Err(Error::invalid(
                "PullRequest",
                "state",
                "The head branch no longer exists, so this pull request cannot be reopened",
            ));
        }
        Ok(())
    }

    pub async fn pull_close(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullRequest> {
        self.pull_set_open(actor, repo, number, false, client).await.map(|v| v.pull)
    }

    pub async fn pull_set_open(&self, actor: &Actor, repo: RepoId, number: i64, open: bool, client: &ClientInfo) -> Result<PullView> {
        let (issue, _) = self.pull_pair(repo, number).await.map_err(|_| Error::NotFound)?;
        let _ = issue;
        self.issue_set_state(actor, repo, number, open, None, client).await?;
        let (issue, mut pull) = self.pull_pair(repo, number).await?;
        if open {
            self.refresh_pull(&issue, &pull).await;
            pull = self.pull_row(issue.id).await?;
        }
        self.hydrate_pull(issue, pull).await
    }

    pub async fn pull_set_draft(&self, actor: &Actor, repo: RepoId, number: i64, draft: bool, _client: &ClientInfo) -> Result<PullView> {
        let user = writing_user(actor, repo)?;
        let perm = self.need_contribute(actor, repo).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let mut tx = self.db().begin().await?;
        let issue = self.issue_row_tx(&mut tx, repo, number).await?;
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        if !(issue.author_id == user || perm >= Permission::Write) {
            return Err(Error::forbidden("Only the author or someone with write access can change the draft state"));
        }
        let pull = self.pull_row_tx(&mut tx, issue.id).await?;
        if pull.merged_at.is_some() || !issue.open {
            return Err(Error::invalid("PullRequest", "draft", "Only an open pull request can change its draft state"));
        }
        if pull.draft != draft {
            sqlx::query("UPDATE pull_requests SET draft = $2 WHERE issue_id = $1").bind(issue.id).bind(draft).execute(&mut *tx).await?;
            Self::add_event(&mut tx, issue.id, Some(user), if draft { "convert_to_draft" } else { "ready_for_review" }, json!({})).await?;
            Self::touch(&mut tx, issue.id).await?;
        }
        tx.commit().await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        self.hydrate_pull(issue, pull).await
    }

    // ------------------------------------------------------------ files, commits

    pub(crate) async fn pull_compare(&self, issue: &IssueRow, pull: &PullRow) -> Result<Compare> {
        // Open pull requests follow the base branch. Closed and merged ones keep the numbers
        // they were closed with, because the base has moved on.
        let base = if issue.open && pull.merged_at.is_none() {
            self.d.git.resolve(pull.repo_id, &full_branch(&pull.base_ref)).await.unwrap_or_else(|_| pull.base_sha.clone())
        } else {
            pull.merge_base.clone().unwrap_or_else(|| pull.base_sha.clone())
        };
        self.compare_pull(pull, &base, &pull.head_sha).await
    }

    /// A base commit against a head commit of this pull request. A fork keeps its own objects,
    /// so neither store alone may hold both (ch-8tj): git reads the base repository with the
    /// head repository's objects beside it. Once the head repository or its branch is gone, the
    /// copy under `refs/pull/<n>/head` in the base still answers.
    pub(crate) async fn compare_pull(&self, pull: &PullRow, base: &str, head: &str) -> Result<Compare> {
        let Some(head_repo) = pull.head_repo() else {
            return self.d.git.compare(pull.repo_id, base, head).await;
        };
        match self.d.git.compare_across(pull.repo_id, base, head_repo, head).await {
            Ok(c) => Ok(c),
            Err(e) if pull.is_fork() => self.d.git.compare(pull.repo_id, base, head).await.map_err(|_| e),
            Err(e) => Err(e),
        }
    }

    pub async fn pull_files(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Diff> {
        self.need_view(actor, repo).await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        Ok(self.pull_compare(&issue, &pull).await?.diff)
    }

    pub async fn pull_commits(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<CommitInfo>> {
        self.need_view(actor, repo).await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        Ok(self.pull_compare(&issue, &pull).await?.commits)
    }

    pub async fn pull_diff_paths(&self, issue: &IssueRow, pull: &PullRow) -> Result<Vec<String>> {
        let cmp = self.pull_compare(issue, pull).await?;
        Ok(cmp.diff.files.iter().filter_map(|f| f.new_path.clone().or_else(|| f.old_path.clone())).collect())
    }

    // ------------------------------------------------------------ plain trait shapes

    pub async fn pull_get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullRequest> {
        Ok(self.pull_view(actor, repo, number).await?.pull)
    }

    pub async fn pull_list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<PullRequest>> {
        Ok(self.pull_views(actor, repo, PullFilter { issue: filter, head: None, base: None }).await?.into_iter().map(|v| v.pull).collect())
    }

    /// Open pull requests whose head is `head_ref` in `head_repo`.
    pub async fn open_pulls_by_head(&self, head_repo: RepoId, head_ref: &str) -> Result<Vec<(IssueRow, PullRow)>> {
        self.open_pulls_where(&format!("{HEAD_REPO_SQL} = $1 AND p.head_ref = $2"), head_repo.0, short_branch(head_ref)).await
    }

    /// Open pull requests whose base is `base_ref` in `repo`.
    pub async fn open_pulls_by_base(&self, repo: RepoId, base_ref: &str) -> Result<Vec<(IssueRow, PullRow)>> {
        self.open_pulls_where("p.repo_id = $1 AND p.base_ref = $2", repo.0, short_branch(base_ref)).await
    }

    async fn open_pulls_where(&self, cond: &str, repo: i64, branch: &str) -> Result<Vec<(IssueRow, PullRow)>> {
        let sql = format!(
            "SELECT {} FROM issues i JOIN pull_requests p ON p.issue_id = i.id WHERE i.open AND p.merged_at IS NULL AND {cond} ORDER BY i.number",
            crate::core::ISSUE_COLS
        );
        let issues = sqlx::query_as::<_, IssueRow>(&sql).bind(repo).bind(branch).fetch_all(self.db()).await?;
        let mut out = Vec::new();
        for i in issues {
            let p = self.pull_row(i.id).await?;
            out.push((i, p));
        }
        Ok(out)
    }

    pub async fn pull_repo_row(&self, pull: &PullRow) -> Result<RepoRow> {
        self.repo(pull.repo_id).await
    }
}

pub fn stats_of(c: &Compare) -> PullStats {
    PullStats {
        commits: c.commits.len() as u32,
        additions: c.diff.files.iter().map(|f| f.additions).sum(),
        deletions: c.diff.files.iter().map(|f| f.deletions).sum(),
        changed_files: c.diff.files.len() as u32,
    }
}

/// Why someone without access to a repository cannot open a pull request from a branch there.
pub const OWN_FORK_ONLY: &str = "Without access to this repository, send the pull request from a fork you own";
/// A pull request copies its head into the base, where everyone who reads the base sees it.
pub const NOT_YOURS_TO_SHARE: &str = "You need write access to the head repository to share its commits here, or it must already be readable by everyone who can read this repository";

impl Core {
    /// The repository belongs to `user`, or to an organization `user` owns.
    async fn owns_repo_account(&self, actor: &Actor, user: UserId, repo: RepoId) -> Result<bool> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT r.owner_id, u.kind FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1")
                .bind(repo)
                .fetch_optional(self.db())
                .await?;
        let Some((owner, kind)) = row else { return Ok(false) };
        if owner == user.0 {
            return Ok(true);
        }
        Ok(kind == "org" && self.d.authz.org_role(actor, ch_core::OrgId(owner)).await? == Some(ch_core::services::OrgRole::Owner))
    }
}

/// A stored grant level in GitHub's words for a team's `permission`.
pub fn github_permission(p: &str) -> &'static str {
    match p {
        "admin" => "admin",
        "maintain" => "maintain",
        "write" => "push",
        "triage" => "triage",
        _ => "pull",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        assert_eq!(short_branch("refs/heads/main"), "main");
        assert_eq!(short_branch("main"), "main");
        assert_eq!(full_branch("main"), "refs/heads/main");
        assert_eq!(full_branch("refs/heads/a/b"), "refs/heads/a/b");
    }

    #[test]
    fn mergeable_round_trips() {
        for m in [MergeableState::Clean, MergeableState::Conflict, MergeableState::Behind, MergeableState::Blocked, MergeableState::Unknown]
        {
            assert_eq!(parse_mergeable(mergeable_str(m)), m);
        }
        assert_eq!(parse_mergeable("???"), MergeableState::Unknown);
    }

    #[test]
    fn methods_round_trip() {
        for m in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
            assert_eq!(parse_method(method_str(m)), Some(m));
        }
        assert_eq!(parse_method("x"), None);
    }

    fn row(head: Option<i64>, from_fork: bool) -> PullRow {
        PullRow {
            issue_id: IssueId(1),
            repo_id: RepoId(10),
            head_repo_id: head.map(RepoId),
            from_fork,
            head_ref: "staging".into(),
            head_sha: "a".into(),
            base_ref: "main".into(),
            base_sha: "b".into(),
            merge_base: None,
            mergeable_state: "clean".into(),
            merge_commit_sha: None,
            merge_method: None,
            merged_at: None,
            merged_by: None,
            draft: false,
            auto_merge_method: None,
            auto_merge_by: None,
            auto_merge_head_sha: None,
            maintainer_can_modify: false,
            commits_count: 0,
            additions: 0,
            deletions: 0,
            changed_files: 0,
        }
    }

    #[test]
    fn a_deleted_fork_is_still_a_fork_and_has_no_head_repository() {
        // A branch of the same repository.
        let same = row(None, false);
        assert!(!same.is_fork() && !same.head_gone());
        assert_eq!(same.head_repo(), Some(RepoId(10)));
        // A live fork.
        let fork = row(Some(20), true);
        assert!(fork.is_fork() && !fork.head_gone());
        assert_eq!(fork.head_repo(), Some(RepoId(20)));
        // The fork was deleted: never the base repository.
        let gone = row(None, true);
        assert!(gone.is_fork() && gone.head_gone());
        assert_eq!(gone.head_repo(), None);
    }
}
