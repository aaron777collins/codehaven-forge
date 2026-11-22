//! Turns an `IssueFilter` plus the search language into one SQL query over `issues`.
//!
//! The query is built with bound parameters only. User text never reaches the SQL string.

use crate::core::{Core, ISSUE_COLS, IssueRow};
use crate::query::{self, Kind, ParsedQuery, SortDir, SortField, Who};
use crate::util;
use ch_core::collab::UserIssueScope;
use ch_core::model::IssueFilter;
use ch_core::{Actor, RepoId, Result, UserId};
use sqlx::{Postgres, QueryBuilder};

/// What to list. `repo` is `None` for lists that span repositories.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub repo: Option<RepoId>,
    pub filter: IssueFilter,
    pub scope: Option<(UserIssueScope, UserId)>,
    /// Restrict to one kind regardless of the query (the pull request list).
    pub force_kind: Option<Kind>,
    /// Pull requests only: `(owner, branch)`, with the owner optional.
    pub head: Option<(Option<String>, String)>,
    pub base: Option<String>,
    /// Search results: with free text and no explicit sort, best match first.
    pub rank: bool,
}

/// `Who` values turned into account ids. `None` means "no such account", which matches nothing.
type Resolved = Vec<Option<UserId>>;

struct Plan {
    q: ParsedQuery,
    authors: Resolved,
    assignees: Resolved,
    mentions: Vec<String>,
    involves: Resolved,
    review_requested: Resolved,
    repo_from_query: Option<RepoId>,
}

impl Core {
    async fn resolve_who(&self, who: &[Who], me: Option<UserId>) -> Result<Resolved> {
        let mut out = Vec::with_capacity(who.len());
        for w in who {
            out.push(match w {
                Who::Me => me,
                Who::Name(n) => self.user_id_by_name(n).await?,
            });
        }
        Ok(out)
    }

    async fn plan(&self, spec: &Spec, actor: &Actor) -> Result<Plan> {
        // Postgres text cannot hold a NUL, so a stray one would turn a search into a 500.
        let q = query::parse(&spec.filter.query.as_deref().unwrap_or("").replace('\0', ""));
        let me = actor.author_id();
        let mut mentions = Vec::new();
        for w in &q.mentions {
            match w {
                Who::Me => {
                    if let Some(m) = me
                        && let Ok(r) = self.user_ref(m).await
                    {
                        mentions.push(r.username);
                    }
                }
                Who::Name(n) => mentions.push(n.clone()),
            }
        }
        let repo_from_query = match &q.repo {
            Some((o, n)) => match self.repo_by_name(o, n).await? {
                Some(r) => Some(r.id),
                None => Some(RepoId(-1)),
            },
            None => None,
        };
        Ok(Plan {
            authors: self.resolve_who(&q.authors, me).await?,
            assignees: self.resolve_who(&q.assignees, me).await?,
            involves: self.resolve_who(&q.involves, me).await?,
            review_requested: self.resolve_who(&q.review_requested, me).await?,
            mentions,
            repo_from_query,
            q,
        })
    }

    /// One page of rows matching `spec`. No permission checks: callers check the repository,
    /// or filter cross-repository results with `readable`.
    pub async fn query_issue_rows(&self, actor: &Actor, spec: &Spec, offset: i64, limit: i64) -> Result<Vec<IssueRow>> {
        let plan = self.plan(spec, actor).await?;
        let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT {ISSUE_COLS} FROM issues i WHERE true"));
        build(&mut qb, spec, &plan);
        let (dir, field) = sort_of(spec, &plan.q);
        let text = plan.q.text.trim();
        let default_sort = (field, dir) == (SortField::Created, SortDir::Desc) && spec.filter.sort.is_none();
        if spec.rank && !text.is_empty() && default_sort {
            qb.push(" ORDER BY ts_rank(i.search, plainto_tsquery('simple', ")
                .push_bind(text.to_string())
                .push(")) DESC, i.updated_at DESC, i.id DESC");
        } else {
            qb.push(match (field, dir) {
                (SortField::Created, SortDir::Desc) => " ORDER BY i.created_at DESC, i.id DESC",
                (SortField::Created, SortDir::Asc) => " ORDER BY i.created_at ASC, i.id ASC",
                (SortField::Updated, SortDir::Desc) => " ORDER BY i.updated_at DESC, i.id DESC",
                (SortField::Updated, SortDir::Asc) => " ORDER BY i.updated_at ASC, i.id ASC",
                (SortField::Comments, SortDir::Desc) => " ORDER BY i.comments_count DESC, i.id DESC",
                (SortField::Comments, SortDir::Asc) => " ORDER BY i.comments_count ASC, i.id ASC",
            });
        }
        qb.push(" LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);
        Ok(qb.build_query_as::<IssueRow>().fetch_all(self.db()).await?)
    }
}

/// The explicit `sort` field wins over the one in the query string. It uses the same words:
/// `created`, `updated`, `comments`, optionally with `-asc` or `-desc`.
fn sort_of(spec: &Spec, q: &ParsedQuery) -> (SortDir, SortField) {
    if let Some(s) = spec.filter.sort.as_deref() {
        let parsed = query::parse(&format!("sort:{s}"));
        return (parsed.dir, parsed.sort);
    }
    (q.dir, q.sort)
}

fn build(qb: &mut QueryBuilder<'_, Postgres>, spec: &Spec, plan: &Plan) {
    let q = &plan.q;
    if let Some(r) = spec.repo.or(plan.repo_from_query) {
        qb.push(" AND i.repo_id = ").push_bind(r);
    }
    let f = &spec.filter;
    if let Some((owner, branch)) = &spec.head {
        qb.push(" AND EXISTS (SELECT 1 FROM pull_requests p WHERE p.issue_id = i.id AND p.head_ref = ").push_bind(branch.clone());
        if let Some(o) = owner {
            qb.push(" AND COALESCE(p.head_repo_id, p.repo_id) IN (SELECT r.id FROM repositories r JOIN users u ON u.id = r.owner_id WHERE u.username = ")
                .push_bind(o.clone())
                .push("::citext)");
        }
        qb.push(")");
    }
    if let Some(base) = &spec.base {
        qb.push(" AND EXISTS (SELECT 1 FROM pull_requests p WHERE p.issue_id = i.id AND p.base_ref = ").push_bind(base.clone()).push(")");
    }
    if let Some(open) = f.open.or(q.open) {
        qb.push(" AND i.open = ").push_bind(open);
    }
    match spec.force_kind.or(q.kind) {
        Some(Kind::Issue) => {
            qb.push(" AND NOT i.is_pull");
        }
        Some(Kind::Pull) => {
            qb.push(" AND i.is_pull");
        }
        None => {}
    }
    if let Some(m) = q.merged {
        qb.push(" AND i.is_pull AND EXISTS (SELECT 1 FROM pull_requests p WHERE p.issue_id = i.id AND (p.merged_at IS NOT NULL) = ")
            .push_bind(m)
            .push(")");
    }
    if let Some(d) = q.draft {
        qb.push(" AND i.is_pull AND EXISTS (SELECT 1 FROM pull_requests p WHERE p.issue_id = i.id AND p.draft = ").push_bind(d).push(")");
    }
    if let Some(l) = q.locked {
        qb.push(" AND i.locked = ").push_bind(l);
    }
    let mut labels: Vec<&String> = f.labels.iter().collect();
    labels.extend(q.labels.iter());
    for name in labels {
        qb.push(" AND EXISTS (SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id AND l.name = ")
            .push_bind(name.clone())
            .push("::citext)");
    }
    for name in &q.exclude_labels {
        qb.push(
            " AND NOT EXISTS (SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id AND l.name = ",
        )
        .push_bind(name.clone())
        .push("::citext)");
    }
    if q.no_label {
        qb.push(" AND NOT EXISTS (SELECT 1 FROM issue_labels il WHERE il.issue_id = i.id)");
    }
    if let Some(a) = f.author {
        qb.push(" AND i.author_id = ").push_bind(a);
    }
    for a in &plan.authors {
        match a {
            Some(id) => qb.push(" AND i.author_id = ").push_bind(*id),
            None => qb.push(" AND false"),
        };
    }
    if let Some(a) = f.assignee {
        qb.push(" AND EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ").push_bind(a).push(")");
    }
    for a in &plan.assignees {
        match a {
            Some(id) => {
                qb.push(" AND EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ").push_bind(*id).push(")")
            }
            None => qb.push(" AND false"),
        };
    }
    if q.no_assignee {
        qb.push(" AND NOT EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id)");
    }
    if let Some(m) = f.milestone {
        qb.push(" AND i.milestone_id = ").push_bind(m);
    }
    if let Some(title) = &q.milestone {
        qb.push(" AND EXISTS (SELECT 1 FROM milestones m WHERE m.id = i.milestone_id AND (lower(m.title) = lower(")
            .push_bind(title.clone())
            .push(")");
        if let Ok(n) = title.parse::<i64>() {
            qb.push(" OR m.number = ").push_bind(n);
        }
        qb.push("))");
    }
    if q.no_milestone {
        qb.push(" AND i.milestone_id IS NULL");
    }
    for name in &plan.mentions {
        qb.push(" AND i.body ILIKE ").push_bind(format!("%@{}%", util::like_escape(name)));
    }
    for who in &plan.involves {
        match who {
            Some(id) => {
                qb.push(" AND (i.author_id = ").push_bind(*id);
                qb.push(" OR EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ").push_bind(*id).push(")");
                qb.push(" OR EXISTS (SELECT 1 FROM issue_comments c WHERE c.issue_id = i.id AND c.author_id = ").push_bind(*id).push("))");
            }
            None => {
                qb.push(" AND false");
            }
        }
    }
    for who in &plan.review_requested {
        match who {
            Some(id) => {
                qb.push(" AND EXISTS (SELECT 1 FROM review_requests rr WHERE rr.pull_id = i.id AND rr.user_id = ").push_bind(*id).push(")")
            }
            None => qb.push(" AND false"),
        };
    }
    if let Some((scope, user)) = &spec.scope {
        match scope {
            UserIssueScope::Assigned => {
                qb.push(" AND EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ").push_bind(*user).push(")");
            }
            UserIssueScope::Created => {
                qb.push(" AND i.author_id = ").push_bind(*user);
            }
            UserIssueScope::Mentioned => {
                qb.push(" AND EXISTS (SELECT 1 FROM issue_subscriptions s WHERE s.issue_id = i.id AND s.user_id = ")
                    .push_bind(*user)
                    .push(" AND s.reason = 'mentioned')");
            }
            UserIssueScope::ReviewRequested => {
                qb.push(" AND EXISTS (SELECT 1 FROM review_requests rr WHERE rr.pull_id = i.id AND rr.user_id = ")
                    .push_bind(*user)
                    .push(")");
            }
        }
    }
    let text = q.text.trim();
    if !text.is_empty() {
        qb.push(" AND (i.search @@ plainto_tsquery('simple', ").push_bind(text.to_string()).push(") OR i.title ILIKE ");
        qb.push_bind(format!("%{}%", util::like_escape(text))).push(")");
    }
}
