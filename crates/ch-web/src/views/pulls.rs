//! Pull requests: list, create, the four tabs of one request, reviews and the merge box.

use super::code::{self, DiffMode, DiffOptions, InlineComment, InlineThread, ThreadForms, thread_html};
use super::issues::{self, Choice, CommentView, IssueRow, comment_views, label_map, label_views, md_for, qs, rows_for};
use super::repo::{CommitRow, status_from_runs};
use super::repo_common::{RepoCtx, branch_names};
use super::{FieldErrors, LabelView, Pager, Params, People, Query, UserRef};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::WebResult;
use crate::fmt::{self, StatusView, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::collab::MergeOptions;
use ch_core::collab::ThreadView;
use ch_core::model::{Compare, MergeMethod, NewPull, NewReviewComment, PullRequest, Review, ReviewState};
use ch_core::{CommentId, Error, Permission, UserId};
use std::collections::HashMap;

const PER_PAGE: u32 = 30;

// ------------------------------------------------------------------ state words

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    pub fn of(pr: &PullRequest) -> Self {
        if pr.merged_at.is_some() {
            PrState::Merged
        } else if !pr.issue.open {
            PrState::Closed
        } else if pr.draft {
            PrState::Draft
        } else {
            PrState::Open
        }
    }
    pub fn class(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            PrState::Open => "Open",
            PrState::Draft => "Draft",
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            PrState::Open => "git-pull-request",
            PrState::Draft => "git-pull-request-draft",
            PrState::Merged => "git-merge",
            PrState::Closed => "git-pull-request-closed",
        }
    }
}

pub fn review_words(s: ReviewState) -> (&'static str, &'static str) {
    match s {
        ReviewState::Approved => ("approved these changes", "ok"),
        ReviewState::ChangesRequested => ("requested changes", "fail"),
        ReviewState::Commented => ("reviewed", "info"),
        ReviewState::Dismissed => ("had a review dismissed", "skip"),
        ReviewState::Pending => ("started a review", "queued"),
    }
}

pub fn review_short(s: ReviewState) -> &'static str {
    match s {
        ReviewState::Approved => "Approved",
        ReviewState::ChangesRequested => "Changes requested",
        ReviewState::Commented => "Commented",
        ReviewState::Dismissed => "Dismissed",
        ReviewState::Pending => "Pending",
    }
}

pub fn parse_method(s: &str) -> MergeMethod {
    match s {
        "squash" => MergeMethod::Squash,
        "rebase" => MergeMethod::Rebase,
        _ => MergeMethod::Merge,
    }
}

pub fn parse_review_state(s: &str) -> Option<ReviewState> {
    match s {
        "approve" => Some(ReviewState::Approved),
        "changes" => Some(ReviewState::ChangesRequested),
        "comment" => Some(ReviewState::Commented),
        _ => None,
    }
}

// ------------------------------------------------------------------ list

pub struct PullRow {
    pub row: IssueRow,
    pub state_label: &'static str,
    pub state_class: &'static str,
    pub icon: &'static str,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
}

#[derive(Template)]
#[template(path = "pulls/list.html")]
struct ListPage {
    c: Chrome,
    base: String,
    show: String,
    open_count: String,
    closed_count: String,
    open_href: String,
    closed_href: String,
    merged_href: String,
    query: String,
    rows: Vec<PullRow>,
    pager: Pager,
    filtered: bool,
    clear_href: String,
    can_create: bool,
}

async fn pull_rows(ctx: &Ctx, rc: &RepoCtx, prs: &[PullRequest]) -> Vec<PullRow> {
    let issues: Vec<_> = prs.iter().map(|p| p.issue.clone()).collect();
    let rows = rows_for(ctx, rc, &issues, "pulls").await;
    rows.into_iter()
        .zip(prs)
        .map(|(mut row, pr)| {
            let st = PrState::of(pr);
            row.open = st == PrState::Open || st == PrState::Draft;
            PullRow {
                row,
                state_label: st.label(),
                state_class: st.class(),
                icon: st.icon(),
                head_ref: pr.head_ref.clone(),
                base_ref: pr.base_ref.clone(),
                draft: pr.draft,
            }
        })
        .collect()
}

pub async fn list(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let base = rc.base();
    let show = match q.get("state").map(String::as_str) {
        Some("closed") => "closed",
        Some("merged") => "merged",
        _ => "open",
    };
    let mut lq = issues::parse_filter(&ctx, &q, PER_PAGE + 1).await;
    lq.filter.open = Some(show == "open");
    let mut prs = ctx.state.pulls.list(&ctx.actor, rc.id(), lq.filter.clone()).await?;
    if show == "merged" {
        prs.retain(|p| p.merged_at.is_some());
    } else if show == "closed" {
        prs.retain(|p| p.merged_at.is_none());
    }
    let has_more = prs.len() as u32 > PER_PAGE;
    prs.truncate(PER_PAGE as usize);

    let count = |open: bool| {
        let mut f = lq.filter.clone();
        f.open = Some(open);
        f.page = 1;
        f.per_page = 100;
        let (ctx, id) = (&ctx, rc.id());
        async move { ctx.state.pulls.list(&ctx.actor, id, f).await.map(|v| v.len()).unwrap_or(0) }
    };
    let (oc, cc) = futures::join!(count(true), count(false));
    let rows = pull_rows(&ctx, &rc, &prs).await;
    let filtered = q.keys().any(|k| matches!(k.as_str(), "labels" | "assignee" | "author" | "milestone" | "q"));
    let c = rc.chrome(&ctx, "Pull requests", "pulls").await;
    ctx.page(&ListPage {
        c,
        show: show.into(),
        open_count: issues::count_label(oc, 100),
        closed_count: issues::count_label(cc, 100),
        open_href: format!("{base}/pulls{}", qs(&q, &[("state", "open")], &[])),
        closed_href: format!("{base}/pulls{}", qs(&q, &[("state", "closed")], &[])),
        merged_href: format!("{base}/pulls{}", qs(&q, &[("state", "merged")], &[])),
        query: q.get("q").cloned().unwrap_or_default(),
        rows,
        pager: Pager::new(&format!("{base}/pulls{}", qs(&q, &[], &[])), lq.page, has_more),
        filtered,
        clear_href: format!("{base}/pulls"),
        can_create: !rc.repo.archived,
        base,
    })
}

// ------------------------------------------------------------------ new

#[derive(Template)]
#[template(path = "pulls/new.html")]
struct NewPage {
    c: Chrome,
    owner: String,
    repo: String,
    branches: Vec<String>,
    /// Branches of the head repository: this one, or the chosen fork.
    head_branches: Vec<String>,
    base_ref: String,
    head_ref: String,
    has_result: bool,
    nothing: bool,
    title: String,
    body: String,
    rows: Vec<CommitRow>,
    files: usize,
    additions: u32,
    deletions: u32,
    diff_html: String,
    errors: FieldErrors,
    compare_href: String,
    /// Forks of this repository the viewer owns: `(owner, owner/name)`. A pull request can come
    /// from one of them (ch-8tj).
    forks: Vec<(String, String)>,
    /// The fork the head branch is in, by owner. Empty for this repository.
    head_owner: String,
}

/// The viewer's own forks of this repository: theirs, or an organization's they own.
async fn own_forks(ctx: &Ctx, rc: &RepoCtx) -> Vec<ch_core::model::Repo> {
    let Ok(uid) = ctx.user_id() else { return Vec::new() };
    let mine: Vec<i64> = super::people::owner_choices(ctx, uid, None).await.into_iter().map(|o| o.id).collect();
    ctx.state.repos.forks(&ctx.actor, rc.id()).await.unwrap_or_default().into_iter().filter(|f| mine.contains(&f.owner_id.0)).collect()
}

fn humanize_branch(b: &str) -> String {
    let t = b.rsplit('/').next().unwrap_or(b).replace(['-', '_'], " ");
    let mut cs = t.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn new_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    base_ref: &str,
    head_ref: &str,
    head_owner: &str,
    title: Option<String>,
    body: Option<String>,
    errors: FieldErrors,
    status: StatusCode,
) -> WebResult<Response> {
    let base = rc.base();
    let branches = branch_names(ctx, rc.id()).await?;
    let forks = own_forks(ctx, rc).await;
    let head_repo = forks.iter().find(|f| !head_owner.is_empty() && f.owner_name.eq_ignore_ascii_case(head_owner));
    let head_branches = match head_repo {
        Some(f) => branch_names(ctx, f.id).await.unwrap_or_default(),
        None => branches.clone(),
    };
    let has_result = !head_ref.is_empty() && (head_repo.is_some() || head_ref != base_ref);
    let (mut rows, mut files, mut additions, mut deletions, mut diff_html, mut nothing) = (Vec::new(), 0, 0, 0, String::new(), false);
    let (mut dt, mut db) = (String::new(), String::new());
    if has_result {
        // A fork keeps its own objects, so git reads both repositories (ch-8tj).
        let cmp = match head_repo {
            Some(f) => {
                let base_sha = ctx.state.git.resolve(rc.id(), &format!("refs/heads/{base_ref}")).await?;
                ctx.state.git.compare_across(rc.id(), &base_sha, f.id, head_ref).await?
            }
            None => ctx.state.git.compare(rc.id(), base_ref, head_ref).await?,
        };
        nothing = cmp.commits.is_empty() && cmp.diff.files.is_empty();
        let sum = code::summarize(&cmp.diff);
        (files, additions, deletions) = (sum.files, sum.additions, sum.deletions);
        diff_html = code::diff_html(&cmp.diff, DiffMode::Unified, &DiffOptions::default());
        if cmp.commits.len() == 1 {
            dt = fmt::title_of(&cmp.commits[0].message);
            db = fmt::body_of(&cmp.commits[0].message);
        } else {
            dt = humanize_branch(head_ref);
        }
        rows = super::repo::commit_rows_pub(ctx, rc, &cmp.commits).await;
    }
    let c = rc.chrome(ctx, "New pull request", "pulls").await;
    ctx.page_status(
        status,
        &NewPage {
            c,
            owner: rc.repo.owner_name.clone(),
            repo: rc.repo.name.clone(),
            compare_href: format!("{base}/pulls/new"),
            branches,
            head_branches,
            base_ref: base_ref.into(),
            head_ref: head_ref.into(),
            head_owner: head_repo.map(|f| f.owner_name.clone()).unwrap_or_default(),
            forks: forks.iter().map(|f| (f.owner_name.clone(), f.full_name())).collect(),
            has_result,
            nothing,
            title: title.unwrap_or(dt),
            body: body.unwrap_or(db),
            rows,
            files,
            additions,
            deletions,
            diff_html,
            errors,
        },
    )
}

pub async fn new_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let base_ref = q.get("base").cloned().unwrap_or_else(|| rc.repo.default_branch.clone());
    let head_ref = q.get("head").cloned().unwrap_or_default();
    let head_owner = q.get("head_owner").cloned().unwrap_or_default();
    new_page(&ctx, &rc, &base_ref, &head_ref, &head_owner, None, None, FieldErrors::default(), StatusCode::OK).await
}

pub async fn new_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let (base_ref, head_ref) = (form.get("base").to_string(), form.get("head").to_string());
    let head_owner = form.get("head_owner").trim().to_string();
    let title = form.get("title").trim().to_string();
    let again = |errors: FieldErrors| {
        let (ctx, rc, base_ref, head_ref, head_owner, title) = (&ctx, &rc, &base_ref, &head_ref, &head_owner, &title);
        let body = form.get("body").to_string();
        async move {
            new_page(ctx, rc, base_ref, head_ref, head_owner, Some(title.clone()), Some(body), errors, StatusCode::UNPROCESSABLE_ENTITY)
                .await
        }
    };
    if title.is_empty() {
        return again(FieldErrors::default().with("title", "Give the pull request a title.")).await;
    }
    let head_repo_id = if head_owner.is_empty() {
        rc.id()
    } else {
        match own_forks(&ctx, &rc).await.into_iter().find(|f| f.owner_name.eq_ignore_ascii_case(&head_owner)) {
            Some(f) => f.id,
            // Shown on the plain form, with no comparison: the branch may exist only in a fork
            // that is not the person's, and comparing it here would answer 404 (ch-ev5).
            None => {
                let errors = FieldErrors::general("Pick one of your own forks of this repository.");
                let body = form.get("body").to_string();
                return new_page(&ctx, &rc, &base_ref, "", "", Some(title), Some(body), errors, StatusCode::UNPROCESSABLE_ENTITY).await;
            }
        }
    };
    let new = NewPull {
        repo_id: rc.id(),
        title: title.clone(),
        body: form.get("body").to_string(),
        head_repo_id,
        head_ref: head_ref.clone(),
        base_ref: base_ref.clone(),
        draft: form.flag("draft"),
    };
    match ctx.state.pulls.create_publishing(&ctx.actor, new, form.flag("publish_head"), &ctx.client).await {
        Ok(pr) => Ok(ctx.redirect(&format!("{}/pulls/{}", rc.base(), pr.issue.number))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => again(errors).await,
            None => Err(e.into()),
        },
    }
}

// ------------------------------------------------------------------ one pull request

pub struct TimelineView {
    pub kind: &'static str,
    pub id: i64,
    pub author: UserRef,
    pub when: TimeView,
    pub html: String,
    pub verb: String,
    pub class: &'static str,
    pub inline: Vec<InlineLine>,
    pub is_pr_author: bool,
}

/// One inline thread as the conversation tab shows it: where it sits in the diff, and the same
/// markup the files tab draws under the line (comments, reply form, resolve button).
pub struct InlineLine {
    pub location: String,
    pub files_href: String,
    pub html: String,
}

pub struct GateView {
    pub mergeable: bool,
    pub reasons: Vec<String>,
}

pub struct CheckRow {
    pub name: String,
    pub status: StatusView,
    pub href: String,
}

/// What every tab of one pull request shows above its body.
pub struct PrHead {
    pub number: i64,
    pub title: String,
    pub raw_title: String,
    pub raw_body: String,
    pub state: &'static str,
    pub state_label: &'static str,
    pub icon: &'static str,
    pub author: UserRef,
    pub when: TimeView,
    pub base_ref: String,
    pub head_ref: String,
    pub commits: usize,
    pub files: usize,
    pub checks: usize,
    pub tab: &'static str,
    pub href: String,
    pub can_edit: bool,
    pub merged_by: Option<UserRef>,
    pub merged_when: Option<TimeView>,
    pub merge_sha: Option<String>,
}

struct Loaded {
    rc: RepoCtx,
    pr: PullRequest,
    cmp: Option<Compare>,
    people: People,
}

async fn load(ctx: &Ctx, owner: &str, name: &str, number: i64) -> WebResult<Loaded> {
    let rc = RepoCtx::load(ctx, owner, name).await?;
    let pr = ctx.state.pulls.get(&ctx.actor, rc.id(), number).await?;
    let base_rev = pr.merge_base.clone().unwrap_or_else(|| pr.base_sha.clone());
    // A head its author did not agree to publish here is not compared for people who cannot read
    // its repository (ch-4pn.14). Anything not known to be safe is hidden.
    let hidden = ctx.state.pulls.head_hidden(&ctx.actor, rc.id(), number).await.unwrap_or(true);
    let cmp = if hidden { None } else { ctx.state.git.compare(rc.id(), &base_rev, &pr.head_sha).await.ok() };
    Ok(Loaded { rc, pr, cmp, people: People::new(&ctx.state) })
}

/// The head branch is in this repository. The service stores no head repository for a branch
/// of the same repository, and a fork's id for a fork. Deleting a fork clears its id too, so
/// `from_fork` decides: a deleted fork's branch is never the branch of that name here.
fn head_is_here(pr: &PullRequest, rc: &RepoCtx) -> bool {
    !pr.from_fork && pr.head_repo_id.is_none_or(|h| h == rc.id())
}

/// The chrome of a pull request's pages. An open pull request from a branch of this same
/// repository opens the workspace on that branch. A fork's branch is not in this repository,
/// and a closed one's branch may be gone, so those keep the plain button.
async fn pr_chrome(ctx: &Ctx, l: &Loaded, title: &str) -> Chrome {
    let mut c = l.rc.chrome(ctx, title, "pulls").await;
    let st = PrState::of(&l.pr);
    if let Some(nav) = c.repo.as_mut()
        && !nav.workspace.is_empty()
        && head_is_here(&l.pr, &l.rc)
        && !l.pr.head_ref.is_empty()
        && (st == PrState::Open || st == PrState::Draft)
    {
        nav.workspace = format!("{}?branch={}", nav.workspace, fmt::enc(&l.pr.head_ref));
    }
    c
}

async fn head(ctx: &Ctx, l: &mut Loaded, tab: &'static str, check_count: usize) -> WebResult<PrHead> {
    let st = PrState::of(&l.pr);
    let me = ctx.user_id().ok();
    let can_edit = me.is_some() && !l.rc.repo.archived && (l.rc.perm.allows(Permission::Triage) || Some(l.pr.issue.author_id) == me);
    let (merged_by, merged_when) = match (l.pr.merged_by, l.pr.merged_at) {
        (Some(u), Some(t)) => (Some(l.people.get(u).await), Some(fmt::time(t))),
        (None, Some(t)) => (None, Some(fmt::time(t))),
        _ => (None, None),
    };
    Ok(PrHead {
        number: l.pr.issue.number,
        title: l.pr.issue.title.clone(),
        raw_title: l.pr.issue.title.clone(),
        raw_body: l.pr.issue.body.clone(),
        state: st.class(),
        state_label: st.label(),
        icon: st.icon(),
        author: l.people.get(l.pr.issue.author_id).await,
        when: fmt::time(l.pr.issue.created_at),
        base_ref: l.pr.base_ref.clone(),
        head_ref: l.pr.head_ref.clone(),
        commits: l.cmp.as_ref().map(|c| c.commits.len()).unwrap_or(0),
        files: l.cmp.as_ref().map(|c| c.diff.files.len()).unwrap_or(0),
        checks: check_count,
        tab,
        href: format!("{}/pulls/{}", l.rc.base(), l.pr.issue.number),
        can_edit,
        merged_by,
        merged_when,
        merge_sha: l.pr.merge_commit_sha.clone().map(|s| fmt::short_sha(&s)),
    })
}

async fn check_rows(ctx: &Ctx, rc: &RepoCtx, sha: &str) -> Vec<CheckRow> {
    // CI records keep the audience they were made for: each viewer sees what covers them (ch-4pn.12).
    let seen = ctx.state.checks.seen_for_sha(&ctx.actor, rc.id(), sha).await.ok();
    let (runs, statuses) = seen.map(|s| (s.runs, s.statuses)).unwrap_or_default();
    let mut out: Vec<CheckRow> = runs
        .iter()
        .map(|r| CheckRow {
            name: r.name.clone(),
            status: fmt::status(&r.status, r.conclusion.as_deref()),
            href: r.details_url.clone().unwrap_or_default(),
        })
        .collect();
    out.extend(statuses.iter().map(|s| CheckRow {
        name: s.context.clone(),
        status: fmt::status(&s.state, Some(&s.state)),
        href: s.target_url.clone().unwrap_or_default(),
    }));
    out
}

/// A service thread as the renderer draws it: comments rendered, the resolver named. The
/// thread sits at the anchor of its first comment.
fn inline_thread(rc: &RepoCtx, t: &ThreadView) -> InlineThread {
    let first = t.comments.first().map(|c| &c.comment);
    let named = |u: &ch_core::collab::UserRef| {
        if u.display_name.is_empty() { u.username.clone() } else { u.display_name.clone() }
    };
    InlineThread {
        id: t.id,
        path: first.map(|c| c.path.clone()).unwrap_or_else(|| t.path.clone()),
        side: first.map(|c| c.side.clone()).unwrap_or_else(|| "right".into()),
        line: first.and_then(|c| c.line.or(c.original_line)).unwrap_or(0),
        resolved: t.resolved,
        resolved_by: t.resolved_by.as_ref().map(named).unwrap_or_default(),
        comments: t
            .comments
            .iter()
            .map(|cv| InlineComment {
                id: cv.comment.id.0,
                author: named(&cv.author),
                ago: fmt::time(cv.comment.created_at).ago,
                body_html: md_for(rc, &cv.comment.body),
                is_reply: cv.comment.in_reply_to.is_some(),
                suggestion: cv.has_suggestion,
            })
            .collect(),
    }
}

/// Where the forms inside a thread post, and which of them this viewer gets. Resolving follows
/// the service rule: the pull request's author or someone with write access.
fn thread_forms(ctx: &Ctx, rc: &RepoCtx, pr: &PullRequest, me: Option<UserId>, number: i64, back: &str, can_reply: bool) -> ThreadForms {
    let base = rc.base();
    ThreadForms {
        csrf: ctx.csrf_token(),
        inline_action: format!("{base}/pulls/{number}/inline"),
        resolve_action: format!("{base}/pulls/{number}/resolve"),
        back: back.into(),
        can_reply,
        can_resolve: me.is_some() && !rc.repo.archived && (Some(pr.issue.author_id) == me || rc.perm.allows(Permission::Write)),
        // Write here, on an open pull request. A fork's branch also needs maintainer edits,
        // which the service checks.
        apply_action: if ctx.signed_in()
            && !rc.repo.archived
            && rc.perm.allows(Permission::Write)
            && pr.issue.open
            && pr.merged_at.is_none()
        {
            format!("{base}/pulls/{number}/suggestion")
        } else {
            String::new()
        },
    }
}

#[derive(Template)]
#[template(path = "pulls/show.html")]
struct ShowPage {
    c: Chrome,
    base: String,
    owner: String,
    repo: String,
    h: PrHead,
    body_html: String,
    timeline: Vec<TimelineView>,
    labels: Vec<LabelView>,
    reviewers: Vec<(UserRef, &'static str, &'static str)>,
    checks_summary: Option<StatusView>,
    gate: GateView,
    can_merge: bool,
    can_comment: bool,
    can_review: bool,
    is_open: bool,
    is_merged: bool,
    is_draft: bool,
    label_choices: Vec<Choice>,
    can_triage: bool,
    fork_approval: Option<ForkApproval>,
    /// The head commit the merger saw. The merge refuses if the branch moved since (D20).
    head_sha: String,
    /// The default squash title, shown as the title field's placeholder.
    squash_title: String,
    /// The head branch lives in this repository and is not its default branch.
    own_branch: bool,
    /// Merged or closed, and the head branch is still here for someone with write access.
    can_delete_branch: bool,
    /// No rule blocks the merge, but checks on the head commit are still queued or running.
    checks_running: bool,
    /// People and teams asked to review and not answered yet.
    requested: Vec<String>,
    /// Triage, or the author with a grant, on an open pull request (ch-4dv).
    can_request: bool,
    reviewer_choices: Vec<Choice>,
    assignees: Vec<UserRef>,
    assignee_choices: Vec<Choice>,
    milestone: Option<(String, String)>,
    milestone_choices: Vec<Choice>,
}

/// The side panel's pickers: who can be asked to review, assigned, and which milestone.
struct SideChoices {
    requested: Vec<String>,
    can_request: bool,
    reviewers: Vec<Choice>,
    assignees: Vec<UserRef>,
    assignee_choices: Vec<Choice>,
    milestone: Option<(String, String)>,
    milestone_choices: Vec<Choice>,
}

async fn side_choices(ctx: &Ctx, l: &mut Loaded, can_triage: bool, open: bool) -> SideChoices {
    let rc = &l.rc;
    let number = l.pr.issue.number;
    let me = ctx.user_id().ok();
    let requested = ctx.state.reviews.requested(&ctx.actor, rc.id(), number).await.unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for u in &requested.users {
        names.push(if u.display_name.is_empty() { u.username.clone() } else { u.display_name.clone() });
    }
    names.extend(requested.teams.iter().map(|t| format!("@{}/{}", t.org, t.name)));
    let is_author = me.is_some() && me == Some(l.pr.issue.author_id);
    // The author needs a grant: Read from the repository's tier alone is not one, the same rule
    // the service keeps (ch-4dv).
    let author_granted = is_author && ctx.state.authz.has_grant(&ctx.actor, rc.id()).await.unwrap_or(false);
    let can_request = open && ctx.signed_in() && !rc.repo.archived && (can_triage || author_granted);
    let mut reviewers: Vec<Choice> = Vec::new();
    if can_request {
        // People with access, when this viewer may list them. The author cannot review.
        if let Ok(collabs) = ctx.state.repos.collaborators(&ctx.actor, rc.id()).await {
            for cb in collabs.into_iter().filter(|c| c.user_id != l.pr.issue.author_id && c.permission >= Permission::Read) {
                reviewers.push(Choice {
                    value: format!("u:{}", cb.user_id.0),
                    on: requested.users.iter().any(|u| u.id == cb.user_id),
                    text: cb.username,
                    class: String::new(),
                });
            }
        }
        for u in &requested.users {
            if !reviewers.iter().any(|c| c.value == format!("u:{}", u.id.0)) {
                reviewers.push(Choice { value: format!("u:{}", u.id.0), text: u.username.clone(), on: true, class: String::new() });
            }
        }
        if let Ok(teams) = ctx.state.orgs.teams(&ctx.actor, ch_core::OrgId(rc.repo.owner_id.0)).await {
            for t in teams {
                reviewers.push(Choice {
                    value: format!("t:{}", t.id.0),
                    on: requested.teams.iter().any(|r| r.id == t.id),
                    text: format!("@{}/{}", rc.repo.owner_name, t.name),
                    class: String::new(),
                });
            }
        }
        for t in &requested.teams {
            if !reviewers.iter().any(|c| c.value == format!("t:{}", t.id.0)) {
                reviewers.push(Choice {
                    value: format!("t:{}", t.id.0),
                    text: format!("@{}/{}", t.org, t.name),
                    on: true,
                    class: String::new(),
                });
            }
        }
    }
    let mut assignees = Vec::new();
    for a in &l.pr.issue.assignees {
        assignees.push(l.people.get(*a).await);
    }
    let milestones = ctx.state.tracker.milestones(&ctx.actor, rc.id(), None).await.unwrap_or_default();
    let base = rc.base();
    let milestone =
        l.pr.issue
            .milestone_id
            .and_then(|id| milestones.iter().find(|m| m.id == id))
            .map(|m| (m.title.clone(), format!("{base}/pulls?milestone={}", m.id.0)));
    let (mut assignee_choices, mut milestone_choices) = (Vec::new(), Vec::new());
    if can_triage {
        if let Ok(collabs) = ctx.state.repos.collaborators(&ctx.actor, rc.id()).await {
            for cb in collabs {
                assignee_choices.push(Choice {
                    value: cb.user_id.0.to_string(),
                    on: l.pr.issue.assignees.contains(&cb.user_id),
                    text: cb.username,
                    class: String::new(),
                });
            }
        }
        milestone_choices = milestones
            .iter()
            .filter(|m| m.open || Some(m.id) == l.pr.issue.milestone_id)
            .map(|m| Choice {
                value: m.id.0.to_string(),
                text: m.title.clone(),
                on: Some(m.id) == l.pr.issue.milestone_id,
                class: String::new(),
            })
            .collect();
    }
    SideChoices { requested: names, can_request, reviewers, assignees, assignee_choices, milestone, milestone_choices }
}

/// A fork's commit that waits for approval before any workflow runs (ch-x12).
struct ForkApproval {
    sha: String,
    short: String,
    action: String,
    can_approve: bool,
}

async fn fork_approval(ctx: &Ctx, l: &Loaded) -> Option<ForkApproval> {
    if ctx.actor.is_anonymous() {
        return None;
    }
    let sha = ctx.state.ci.pending_fork_approval(&ctx.actor, l.rc.id(), l.pr.issue.number).await.ok().flatten()?;
    Some(ForkApproval {
        short: fmt::short_sha(&sha),
        action: format!("{}/pulls/{}/approve-runs", l.rc.base(), l.pr.issue.number),
        can_approve: l.rc.perm.allows(Permission::Write) && !l.rc.repo.archived && ctx.signed_in(),
        sha,
    })
}

pub async fn show(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>) -> WebResult<Response> {
    let mut l = load(&ctx, &owner, &name, number).await?;
    let base = l.rc.base();
    let checks = check_rows(&ctx, &l.rc, &l.pr.head_sha).await;
    let h = head(&ctx, &mut l, "conversation", checks.len()).await?;
    let st = PrState::of(&l.pr);
    // A signed-out visitor reads a public pull request and gets no forms.
    let me = ctx.user_id().ok();
    let can_comment = me.is_some() && !l.rc.repo.archived && !l.pr.issue.locked;
    let comments = ctx.state.issues.comments(&ctx.actor, l.rc.id(), number).await?;
    let reviews = ctx.state.reviews.list(&ctx.actor, l.rc.id(), number).await.unwrap_or_default();
    // Which reviewers count toward required approvals (write access). Unknown counts.
    let not_counted: Vec<i64> = ctx
        .state
        .reviews
        .views(&ctx.actor, l.rc.id(), number)
        .await
        .map(|v| v.into_iter().filter(|r| !r.counts).map(|r| r.review.id.0).collect())
        .unwrap_or_default();
    let threads = ctx.state.reviews.threads(&ctx.actor, l.rc.id(), number).await.unwrap_or_default();
    let forms = thread_forms(&ctx, &l.rc, &l.pr, me, number, "", can_comment && ctx.signed_in());

    let cviews = comment_views(&l.rc, &mut l.people, &l.pr.issue, &comments).await;
    let mut items: Vec<(chrono::DateTime<chrono::Utc>, TimelineView)> = Vec::new();
    for (c, v) in comments.iter().zip(cviews) {
        items.push((
            c.created_at,
            TimelineView {
                kind: "comment",
                id: v.id,
                author: v.author,
                when: v.when,
                html: v.html,
                verb: "commented".into(),
                class: "",
                inline: Vec::new(),
                is_pr_author: v.is_issue_author,
            },
        ));
    }
    // Each thread shows once: in the review that holds its first comment.
    let listed: Vec<i64> = reviews.iter().filter(|r| r.state != ReviewState::Pending).map(|r| r.id.0).collect();
    let mut by_review: HashMap<i64, Vec<InlineLine>> = HashMap::new();
    let thread_line = |t: &ThreadView| {
        let it = inline_thread(&l.rc, t);
        InlineLine {
            location: format!("{}:{}", it.path, it.line),
            files_href: format!("{base}/pulls/{number}/files#{}", code::file_anchor(&it.path)),
            html: thread_html(&it, &forms),
        }
    };
    for t in &threads {
        if let Some(r) = t.comments.first().and_then(|c| c.comment.review_id) {
            by_review.entry(r.0).or_default().push(thread_line(t));
        }
    }
    let mut reviewer_state: Vec<(UserRef, &'static str, &'static str)> = Vec::new();
    for r in reviews.iter().filter(|r| r.state != ReviewState::Pending) {
        let (verb, class) = review_words(r.state);
        let when = r.submitted_at.unwrap_or(l.pr.issue.created_at);
        let author = l.people.get(r.author_id).await;
        let lines = by_review.remove(&r.id.0).unwrap_or_default();
        reviewer_state.retain(|(u, _, _)| u.username != author.username);
        if r.state != ReviewState::Commented {
            let short = match r.state {
                ReviewState::Approved | ReviewState::ChangesRequested if not_counted.contains(&r.id.0) => {
                    if r.state == ReviewState::Approved { "Approved, does not count" } else { "Changes requested, does not count" }
                }
                _ => review_short(r.state),
            };
            reviewer_state.push((author.clone(), short, class));
        }
        // A reply posts a review of its own with no text, and its thread is shown under the
        // review that started it. An empty entry that says only "reviewed" adds nothing.
        if r.state == ReviewState::Commented && r.body.trim().is_empty() && lines.is_empty() {
            continue;
        }
        items.push((
            when,
            TimelineView {
                kind: "review",
                id: r.id.0,
                author,
                when: fmt::time(when),
                html: if r.body.trim().is_empty() { String::new() } else { md_for(&l.rc, &r.body) },
                verb: verb.into(),
                class,
                inline: lines,
                is_pr_author: false,
            },
        ));
    }
    // A thread whose first comment has no review on the page (a pending one, or none at all)
    // still has to show once, as its own entry.
    for t in &threads {
        let Some(first) = t.comments.first() else { continue };
        if first.comment.review_id.is_some_and(|r| listed.contains(&r.0)) {
            continue;
        }
        let named = &first.author;
        items.push((
            first.comment.created_at,
            TimelineView {
                kind: "review",
                id: t.id,
                author: UserRef {
                    username: named.username.clone(),
                    display: if named.display_name.is_empty() { named.username.clone() } else { named.display_name.clone() },
                },
                when: fmt::time(first.comment.created_at),
                html: String::new(),
                verb: "commented inline".into(),
                class: "info",
                inline: vec![thread_line(t)],
                is_pr_author: first.comment.author_id == l.pr.issue.author_id,
            },
        ));
    }
    items.sort_by_key(|(t, _)| *t);
    let timeline = items.into_iter().map(|(_, v)| v).collect();

    let lmap = label_map(&ctx, &l.rc).await;
    let can_triage = l.rc.perm.allows(Permission::Triage) && !l.rc.repo.archived;
    let label_choices = if can_triage {
        lmap.values()
            .map(|x| Choice {
                value: x.name.clone(),
                text: x.name.clone(),
                on: l.pr.issue.labels.contains(&x.name),
                class: fmt::label_class(&x.color),
            })
            .collect()
    } else {
        Vec::new()
    };
    let gate = if st == PrState::Open || st == PrState::Draft {
        let g = ctx.state.pulls.merge_gate(&ctx.actor, l.rc.id(), number).await.unwrap_or_default();
        GateView { mergeable: g.mergeable && st == PrState::Open, reasons: g.reasons }
    } else {
        GateView { mergeable: false, reasons: Vec::new() }
    };
    let checks_summary = match ctx.state.checks.seen_for_sha(&ctx.actor, l.rc.id(), &l.pr.head_sha).await {
        Ok(seen) => status_from_runs(&seen.runs),
        Err(_) => None,
    };
    // A run that has not created its checks yet still counts as running.
    let checks_running = gate.mergeable
        && !ctx.actor.is_anonymous()
        && match &checks_summary {
            Some(s) => matches!(s.class.as_str(), "run" | "queued"),
            None => ctx
                .state
                .ci
                .runs(&ctx.actor, l.rc.id(), 1, 20)
                .await
                .map(|runs| runs.iter().any(|r| r.head_sha == l.pr.head_sha && r.status != "completed"))
                .unwrap_or(false),
        };
    let own_branch = head_is_here(&l.pr, &l.rc) && !l.pr.head_ref.is_empty() && l.pr.head_ref != l.rc.repo.default_branch;
    let can_delete_branch = own_branch
        && (st == PrState::Merged || st == PrState::Closed)
        && l.rc.perm.allows(Permission::Write)
        && !l.rc.repo.archived
        && ctx.signed_in()
        && ctx.state.git.resolve(l.rc.id(), &format!("refs/heads/{}", l.pr.head_ref)).await.is_ok();
    let fork_approval = fork_approval(&ctx, &l).await;
    let side = side_choices(&ctx, &mut l, can_triage, st == PrState::Open || st == PrState::Draft).await;
    let c = pr_chrome(&ctx, &l, &format!("{} (#{number})", l.pr.issue.title)).await;
    ctx.page(&ShowPage {
        fork_approval,
        head_sha: l.pr.head_sha.clone(),
        squash_title: format!("{} (#{number})", l.pr.issue.title),
        own_branch,
        can_delete_branch,
        checks_running,
        requested: side.requested,
        can_request: side.can_request,
        reviewer_choices: side.reviewers,
        assignees: side.assignees,
        assignee_choices: side.assignee_choices,
        milestone: side.milestone,
        milestone_choices: side.milestone_choices,
        c,
        owner: l.rc.repo.owner_name.clone(),
        repo: l.rc.repo.name.clone(),
        body_html: if l.pr.issue.body.trim().is_empty() { String::new() } else { md_for(&l.rc, &l.pr.issue.body) },
        timeline,
        labels: label_views(&l.pr.issue.labels, &lmap, &format!("{base}/pulls")),
        reviewers: reviewer_state,
        checks_summary,
        can_merge: l.rc.perm.allows(Permission::Write) && !l.rc.repo.archived && st == PrState::Open,
        can_comment,
        can_review: me.is_some()
            && !l.rc.repo.archived
            && Some(l.pr.issue.author_id) != me
            && (st == PrState::Open || st == PrState::Draft),
        is_open: st == PrState::Open || st == PrState::Draft,
        is_merged: st == PrState::Merged,
        is_draft: st == PrState::Draft,
        gate,
        label_choices,
        can_triage,
        h,
        base,
    })
}

// ------------------------------------------------------------------ commits tab

#[derive(Template)]
#[template(path = "pulls/commits.html")]
struct CommitsPage {
    c: Chrome,
    h: PrHead,
    rows: Vec<CommitRow>,
}

pub async fn commits(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>) -> WebResult<Response> {
    let mut l = load(&ctx, &owner, &name, number).await?;
    let checks = check_rows(&ctx, &l.rc, &l.pr.head_sha).await.len();
    let h = head(&ctx, &mut l, "commits", checks).await?;
    let rows = match &l.cmp {
        Some(c) => super::repo::commit_rows_pub(&ctx, &l.rc, &c.commits).await,
        None => Vec::new(),
    };
    let c = pr_chrome(&ctx, &l, &format!("Commits - {} (#{number})", l.pr.issue.title)).await;
    ctx.page(&CommitsPage { c, h, rows })
}

// ------------------------------------------------------------------ checks tab

#[derive(Template)]
#[template(path = "pulls/checks.html")]
struct ChecksPage {
    c: Chrome,
    h: PrHead,
    rows: Vec<CheckRow>,
    summary: Option<StatusView>,
    sha: String,
    fork_approval: Option<ForkApproval>,
}

pub async fn checks(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>) -> WebResult<Response> {
    let mut l = load(&ctx, &owner, &name, number).await?;
    let rows = check_rows(&ctx, &l.rc, &l.pr.head_sha).await;
    let h = head(&ctx, &mut l, "checks", rows.len()).await?;
    let runs = ctx.state.checks.seen_for_sha(&ctx.actor, l.rc.id(), &l.pr.head_sha).await.map(|s| s.runs).unwrap_or_default();
    let fork_approval = fork_approval(&ctx, &l).await;
    let c = pr_chrome(&ctx, &l, &format!("Checks - {} (#{number})", l.pr.issue.title)).await;
    ctx.page(&ChecksPage { c, h, rows, summary: status_from_runs(&runs), sha: fmt::short_sha(&l.pr.head_sha), fork_approval })
}

/// A person with write access approves the latest commit of a pull request from a fork, which
/// starts its runs. The service refuses anything but a person, and the approval is audited.
pub async fn approve_runs_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let url = pr_url(&rc, number, "checks");
    match ctx.state.ci.approve_fork_commit(&ctx.actor, rc.id(), number, form.get("sha"), &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&url, "ok", "Approved. The workflows start when a runner is free.")),
        Err(Error::Conflict(m)) => Ok(ctx.redirect_flash(&url, "error", &m)),
        Err(Error::Validation(_)) => {
            Ok(ctx.redirect_flash(&url, "error", "The pull request has a newer commit since this page loaded. Approve that one."))
        }
        Err(e) => Err(e.into()),
    }
}

// ------------------------------------------------------------------ files tab

#[derive(Template)]
#[template(path = "pulls/files.html")]
struct FilesPage {
    c: Chrome,
    base: String,
    h: PrHead,
    files: usize,
    additions: u32,
    deletions: u32,
    diff_html: String,
    split: bool,
    mode_unified: String,
    mode_split: String,
    can_review: bool,
    file_index: Vec<(String, String, u32, u32)>,
    inline_action: String,
}

pub async fn files(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, Query(q): Query<Params>) -> WebResult<Response> {
    let mut l = load(&ctx, &owner, &name, number).await?;
    let base = l.rc.base();
    let checks = check_rows(&ctx, &l.rc, &l.pr.head_sha).await.len();
    let h = head(&ctx, &mut l, "files", checks).await?;
    let mode = DiffMode::from_query(q.get("diff").map(String::as_str));
    let st = PrState::of(&l.pr);
    let me = ctx.user_id().ok();
    let can_comment = me.is_some() && !l.rc.repo.archived && st != PrState::Merged;
    let threads = ctx.state.reviews.threads(&ctx.actor, l.rc.id(), number).await.unwrap_or_default();
    let mut opts = DiffOptions {
        can_comment,
        file_url: Some(format!("{base}/blob/{}/{{path}}", l.pr.head_sha)),
        forms: thread_forms(&ctx, &l.rc, &l.pr, me, number, "files", can_comment && ctx.signed_in()),
        ..Default::default()
    };
    for t in &threads {
        let it = inline_thread(&l.rc, t);
        // A thread without a placeable line cannot sit in the diff; the conversation still shows it.
        if it.line == 0 {
            continue;
        }
        opts.comments.entry((it.path.clone(), it.side.clone(), it.line)).or_default().push(it);
    }
    let (diff_html, sum, index) = match &l.cmp {
        Some(c) => {
            let s = code::summarize(&c.diff);
            let idx = c.diff.files.iter().map(|f| {
                let p = f.new_path.clone().or(f.old_path.clone()).unwrap_or_default();
                (code::file_anchor(&p), p, f.additions, f.deletions)
            });
            (code::diff_html(&c.diff, mode, &opts), s, idx.collect())
        }
        None => (String::new(), code::DiffSummary { files: 0, additions: 0, deletions: 0 }, Vec::new()),
    };
    let can_review =
        me.is_some() && !l.rc.repo.archived && Some(l.pr.issue.author_id) != me && (st == PrState::Open || st == PrState::Draft);
    let c = pr_chrome(&ctx, &l, &format!("Files changed - {} (#{number})", l.pr.issue.title)).await;
    ctx.page(&FilesPage {
        c,
        mode_unified: format!("{base}/pulls/{number}/files"),
        mode_split: format!("{base}/pulls/{number}/files?diff=split"),
        inline_action: format!("{base}/pulls/{number}/inline"),
        files: sum.files,
        additions: sum.additions,
        deletions: sum.deletions,
        diff_html,
        split: mode == DiffMode::Split,
        can_review,
        file_index: index,
        h,
        base,
    })
}

// ------------------------------------------------------------------ actions

fn pr_url(rc: &RepoCtx, number: i64, tab: &str) -> String {
    if tab.is_empty() { format!("{}/pulls/{number}", rc.base()) } else { format!("{}/pulls/{number}/{tab}", rc.base()) }
}

pub async fn comment_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    if form.get("action") == "close" {
        rc.require_writable(Permission::Read)?;
        let body = form.get("body").trim();
        if !body.is_empty() {
            ctx.state.issues.add_comment(&ctx.actor, rc.id(), number, body, &ctx.client).await?;
        }
        ctx.state.pulls.close(&ctx.actor, rc.id(), number, &ctx.client).await?;
        return Ok(ctx.redirect_flash(&pr_url(&rc, number, ""), "ok", "Pull request closed."));
    }
    issues::do_comment(&ctx, &rc, number, &form, "pulls").await
}

pub async fn edit_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    issues::do_edit(&ctx, &rc, number, &form, "pulls").await
}

pub async fn labels_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    issues::do_labels(&ctx, &rc, number, &form, "pulls").await
}

pub async fn merge_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let url = pr_url(&rc, number, "");
    let options = merge_options(&form);
    match ctx.state.pulls.merge_with(&ctx.actor, rc.id(), number, options, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&url, "ok", "Merged.")),
        Err(Error::MergeBlocked(reasons)) => {
            Ok(ctx.redirect_flash(&url, "error", &format!("This cannot be merged yet: {}", reasons.join(" "))))
        }
        Err(Error::Conflict(m)) => Ok(ctx.redirect_flash(&url, "error", &m)),
        Err(e) => Err(e.into()),
    }
}

/// The merge form as service options. An empty title or message keeps the default text. The
/// head sha is the one the page showed, so a push after the page loaded stops the merge. An
/// unticked delete box leaves the repository's own setting in charge.
fn merge_options(form: &FormData) -> MergeOptions {
    let title = form.get("title").trim();
    let message = form.get("message").replace("\r\n", "\n");
    let head = form.get("head_sha").trim();
    MergeOptions {
        method: Some(parse_method(form.get("method"))),
        title: (!title.is_empty()).then(|| title.to_string()),
        message: (!message.trim().is_empty()).then(|| message.trim_end().to_string()),
        expected_head_sha: (!head.is_empty()).then(|| head.to_string()),
        delete_branch: form.flag("delete_branch").then_some(true),
    }
}

pub async fn assignees_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    issues::do_assignees(&ctx, &rc, number, &form, "pulls").await
}

pub async fn milestone_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    issues::do_milestone(&ctx, &rc, number, &form, "pulls").await
}

/// Which reviewers a form asks for: `u:<id>` and `t:<id>` values, ignoring anything else.
fn reviewer_ids(values: &[&str]) -> (Vec<UserId>, Vec<ch_core::TeamId>) {
    let (mut users, mut teams) = (Vec::new(), Vec::new());
    for v in values {
        if let Some(id) = v.strip_prefix("u:").and_then(|n| n.parse::<i64>().ok()) {
            if !users.contains(&UserId(id)) {
                users.push(UserId(id));
            }
        } else if let Some(id) = v.strip_prefix("t:").and_then(|n| n.parse::<i64>().ok())
            && !teams.contains(&ch_core::TeamId(id))
        {
            teams.push(ch_core::TeamId(id));
        }
    }
    (users, teams)
}

/// Asks for reviews and withdraws requests in one form. Boxes the form offered and left
/// unticked are withdrawn; a username typed in is asked as well. The service checks who may
/// do this: triage, or the author with a grant.
pub async fn reviewers_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let url = pr_url(&rc, number, "");
    let (mut want_users, want_teams) = reviewer_ids(&form.all("reviewers"));
    let (offered_users, offered_teams) = reviewer_ids(&form.all("offered"));
    let typed = form.get("username").trim().trim_start_matches('@');
    if !typed.is_empty() {
        match ctx.state.accounts.get_by_username(&ctx.actor, typed).await {
            Ok(u) => {
                if !want_users.contains(&u.id) {
                    want_users.push(u.id);
                }
            }
            Err(_) => return Ok(ctx.redirect_flash(&url, "error", &format!("There is no account named {typed}."))),
        }
    }
    let current = ctx.state.reviews.requested(&ctx.actor, rc.id(), number).await?;
    let add_users: Vec<UserId> = want_users.iter().copied().filter(|u| !current.users.iter().any(|c| c.id == *u)).collect();
    let add_teams: Vec<ch_core::TeamId> = want_teams.iter().copied().filter(|t| !current.teams.iter().any(|c| c.id == *t)).collect();
    let drop_users: Vec<UserId> =
        offered_users.into_iter().filter(|u| !want_users.contains(u) && current.users.iter().any(|c| c.id == *u)).collect();
    let drop_teams: Vec<ch_core::TeamId> =
        offered_teams.into_iter().filter(|t| !want_teams.contains(t) && current.teams.iter().any(|c| c.id == *t)).collect();
    let mut result = Ok(());
    if !add_users.is_empty() || !add_teams.is_empty() {
        result = ctx.state.reviews.request_review(&ctx.actor, rc.id(), number, add_users, add_teams, &ctx.client).await;
    }
    if result.is_ok() && (!drop_users.is_empty() || !drop_teams.is_empty()) {
        result = ctx.state.reviews.remove_request(&ctx.actor, rc.id(), number, drop_users, drop_teams, &ctx.client).await;
    }
    match result {
        Ok(()) => Ok(ctx.redirect_flash(&url, "ok", "Reviewers updated.")),
        Err(e) => match refused_message(&e) {
            Some(m) => Ok(ctx.redirect_flash(&url, "error", &m)),
            None => Err(e.into()),
        },
    }
}

/// "Apply suggestion": commits a review comment's suggestion to the head branch.
pub async fn suggestion_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let back = form.opt("back").filter(|b| *b == "files").unwrap_or("");
    let url = pr_url(&rc, number, back);
    let Some(id) = form.int("comment") else {
        return Ok(ctx.redirect_flash(&url, "error", "Which suggestion?"));
    };
    match ctx.state.reviews.apply_suggestion(&ctx.actor, rc.id(), CommentId(id), &ctx.client).await {
        Ok(sha) => Ok(ctx.redirect_flash(&url, "ok", &format!("Applied the suggestion as commit {}.", fmt::short_sha(&sha)))),
        Err(e) => match refused_message(&e) {
            Some(m) => Ok(ctx.redirect_flash(&url, "error", &m)),
            None => Err(e.into()),
        },
    }
}

/// "Delete branch" after a merge or close: the head branch of this same repository only.
pub async fn delete_branch_post(
    ctx: Ctx,
    Path((owner, name, number)): Path<(String, String, i64)>,
    _form: FormData,
) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let pr = ctx.state.pulls.get(&ctx.actor, rc.id(), number).await?;
    let url = pr_url(&rc, number, "");
    let st = PrState::of(&pr);
    if !(st == PrState::Merged || st == PrState::Closed) {
        return Ok(ctx.redirect_flash(&url, "error", "Close or merge the pull request before deleting its branch."));
    }
    if !head_is_here(&pr, &rc) || pr.head_ref.is_empty() || pr.head_ref == rc.repo.default_branch {
        return Ok(ctx.redirect_flash(&url, "error", "That branch cannot be deleted from here."));
    }
    match ctx.state.git.delete_ref(&ctx.actor, rc.id(), &format!("refs/heads/{}", pr.head_ref), &ctx.client).await {
        Ok(()) => Ok(ctx.redirect_flash(&url, "ok", &format!("Deleted the branch {}.", pr.head_ref))),
        Err(Error::Forbidden(m)) | Err(Error::Conflict(m)) => Ok(ctx.redirect_flash(&url, "error", &m)),
        Err(e) => Err(e.into()),
    }
}

pub async fn review_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let back = form.opt("back").filter(|b| matches!(*b, "files" | "")).unwrap_or("");
    let url = pr_url(&rc, number, back);
    let Some(state) = parse_review_state(form.get("state")) else {
        return Ok(ctx.redirect_flash(&url, "error", "Choose comment, approve or request changes."));
    };
    let body = form.get("body").trim();
    if state != ReviewState::Approved && body.is_empty() {
        return Ok(ctx.redirect_flash(&url, "error", "Add a comment to go with that review."));
    }
    ctx.state.reviews.submit(&ctx.actor, rc.id(), number, state, body, Vec::new(), &ctx.client).await?;
    Ok(ctx.redirect_flash(&url, "ok", "Review submitted."))
}

/// The message to flash when the review service refuses an action, or None to let it become an
/// error page (an internal fault should still be loud).
fn refused_message(e: &Error) -> Option<String> {
    match e {
        Error::Forbidden(m) | Error::Conflict(m) => Some(m.clone()),
        Error::NotFound => Some("That conversation is not here.".into()),
        Error::Validation(list) => {
            let msgs: Vec<&str> = list.iter().filter_map(|f| f.message.as_deref()).collect();
            if msgs.is_empty() { None } else { Some(msgs.join(" ")) }
        }
        _ => None,
    }
}

/// One inline comment from the diff, posted on its own as a review comment. With `in_reply_to`
/// it lands in that thread, at the anchor of its first comment.
pub async fn inline_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let back = form.opt("back").filter(|b| *b == "files").unwrap_or("");
    let url = pr_url(&rc, number, back);
    let (path, side, body) = (form.get("path").to_string(), form.get("side"), form.get("body").trim().to_string());
    let line: u32 = form.get("line").trim().parse().unwrap_or(0);
    if path.is_empty() || line == 0 || body.is_empty() || !matches!(side, "left" | "right") {
        return Ok(ctx.redirect_flash(&url, "error", "That comment was not complete."));
    }
    let comment = NewReviewComment {
        path: path.clone(),
        side: side.to_string(),
        line,
        start_line: None,
        body,
        in_reply_to: form.int("in_reply_to").map(CommentId),
    };
    match ctx.state.reviews.submit(&ctx.actor, rc.id(), number, ReviewState::Commented, "", vec![comment], &ctx.client).await {
        Ok(_) => {
            let to = if back == "files" { format!("{url}#{}", code::file_anchor(&path)) } else { url };
            Ok(ctx.redirect_flash(&to, "ok", "Comment posted."))
        }
        Err(e) => match refused_message(&e) {
            Some(m) => Ok(ctx.redirect_flash(&url, "error", &m)),
            None => Err(e.into()),
        },
    }
}

pub async fn resolve_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let back = form.opt("back").filter(|b| *b == "files").unwrap_or("");
    let url = pr_url(&rc, number, back);
    let Some(id) = form.int("thread") else {
        return Ok(ctx.redirect_flash(&url, "error", "Which conversation?"));
    };
    let resolved = form.get("resolved") != "0";
    match ctx.state.reviews.resolve_thread(&ctx.actor, rc.id(), id, resolved, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&url, "ok", if resolved { "Conversation resolved." } else { "Conversation reopened." })),
        Err(e) => match refused_message(&e) {
            Some(m) => Ok(ctx.redirect_flash(&url, "error", &m)),
            None => Err(e.into()),
        },
    }
}

#[allow(dead_code)]
fn _keep(_: Review, _: CommentView) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_methods_default_to_a_merge_commit() {
        assert_eq!(parse_method("squash"), MergeMethod::Squash);
        assert_eq!(parse_method("rebase"), MergeMethod::Rebase);
        assert_eq!(parse_method("merge"), MergeMethod::Merge);
        assert_eq!(parse_method("whatever"), MergeMethod::Merge);
    }

    #[test]
    fn review_choices() {
        assert_eq!(parse_review_state("approve"), Some(ReviewState::Approved));
        assert_eq!(parse_review_state("changes"), Some(ReviewState::ChangesRequested));
        assert_eq!(parse_review_state("pending"), None);
        assert_eq!(review_words(ReviewState::Approved).0, "approved these changes");
    }

    #[test]
    fn the_merge_form_becomes_options() {
        let o = merge_options(&FormData::parse(
            b"method=squash&title=+Tidy+up+&message=Line+one%0D%0ALine+two%0D%0A&head_sha=abc123&delete_branch=1",
        ));
        assert_eq!(o.method, Some(MergeMethod::Squash));
        assert_eq!(o.title.as_deref(), Some("Tidy up"));
        assert_eq!(o.message.as_deref(), Some("Line one\nLine two"));
        assert_eq!(o.expected_head_sha.as_deref(), Some("abc123"));
        assert_eq!(o.delete_branch, Some(true));
        let o = merge_options(&FormData::parse(b"method=merge&title=&message=+&head_sha="));
        assert_eq!(
            (o.title, o.message, o.expected_head_sha, o.delete_branch),
            (None, None, None, None),
            "unticked follows the repository setting"
        );
    }

    #[test]
    fn reviewer_values_split_into_people_and_teams() {
        let (u, t) = reviewer_ids(&["u:3", "t:9", "u:3", "x:1", "u:abc", "t:", "u:4"]);
        assert_eq!(u, vec![UserId(3), UserId(4)]);
        assert_eq!(t, vec![ch_core::TeamId(9)]);
    }

    #[test]
    fn branch_names_become_titles() {
        assert_eq!(humanize_branch("feature/add-login_page"), "Add login page");
        assert_eq!(humanize_branch("fix"), "Fix");
        assert_eq!(humanize_branch(""), "");
    }
}
