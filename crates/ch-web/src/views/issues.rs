//! Issues, labels and milestones.

use super::repo_common::RepoCtx;
use super::{FieldErrors, LabelView, Pager, Params, People, Query, UserRef};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use ch_core::issue_form::{FormField, compose};
use ch_core::model::{Comment, Issue, IssueFilter, NewIssue};
use ch_core::web_contract::{Label, Milestone};
use ch_core::{Error, FieldError, MilestoneId, Permission, UserId};
use ch_render::markdown::{self, MdContext};
use std::collections::HashMap;

pub const PER_PAGE: u32 = 30;

// ------------------------------------------------------------------ shared views

pub struct IssueRow {
    pub number: i64,
    pub title: String,
    pub href: String,
    pub open: bool,
    pub icon: &'static str,
    pub state_class: &'static str,
    pub labels: Vec<LabelView>,
    pub author: UserRef,
    pub when: TimeView,
    pub comments: i64,
    pub assignees: Vec<UserRef>,
}

pub struct CommentView {
    pub id: i64,
    pub author: UserRef,
    pub when: TimeView,
    pub html: String,
    pub is_issue_author: bool,
}

/// Renders markdown the way comments and bodies are shown: references linked, no heading anchors.
pub fn md_for(rc: &RepoCtx, src: &str) -> String {
    markdown::render(
        src,
        &MdContext {
            owner: rc.repo.owner_name.clone(),
            repo: rc.repo.name.clone(),
            git_ref: rc.repo.default_branch.clone(),
            dir: String::new(),
            link_refs: true,
            heading_ids: false,
        },
    )
}

pub async fn comment_views(rc: &RepoCtx, people: &mut People, issue: &Issue, comments: &[Comment]) -> Vec<CommentView> {
    let mut out = Vec::with_capacity(comments.len());
    for c in comments {
        out.push(CommentView {
            id: c.id.0,
            author: people.get(c.author_id).await,
            when: fmt::time(c.created_at),
            html: md_for(rc, &c.body),
            is_issue_author: c.author_id == issue.author_id,
        });
    }
    out
}

pub async fn label_map(ctx: &Ctx, rc: &RepoCtx) -> HashMap<String, Label> {
    ctx.state.tracker.labels(&ctx.actor, rc.id()).await.unwrap_or_default().into_iter().map(|l| (l.name.clone(), l)).collect()
}

pub fn label_views(names: &[String], map: &HashMap<String, Label>, base: &str) -> Vec<LabelView> {
    names
        .iter()
        .map(|n| match map.get(n) {
            Some(l) => LabelView::from_label(l, base),
            None => LabelView::new(n, "", "", base),
        })
        .collect()
}

/// Builds a query string from the current parameters with some replaced or removed.
pub fn qs(q: &Params, set: &[(&str, &str)], drop: &[&str]) -> String {
    let mut pairs: Vec<(String, String)> =
        q.iter().filter(|(k, _)| k.as_str() != "page" && !drop.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    pairs.sort();
    for (k, v) in set {
        pairs.retain(|(pk, _)| pk != k);
        if !v.is_empty() {
            pairs.push((k.to_string(), v.to_string()));
        }
    }
    if pairs.is_empty() {
        return String::new();
    }
    format!("?{}", pairs.iter().map(|(k, v)| format!("{}={}", fmt::enc(k), fmt::enc(v))).collect::<Vec<_>>().join("&"))
}

pub fn count_label(n: usize, cap: usize) -> String {
    if n >= cap { format!("{cap}+") } else { n.to_string() }
}

// ------------------------------------------------------------------ list

pub struct LabelFilterItem {
    pub name: String,
    pub class: String,
    pub on: bool,
    pub href: String,
}

#[derive(Template)]
#[template(path = "issues/list.html")]
struct ListPage {
    c: Chrome,
    base: String,
    open: bool,
    open_count: String,
    closed_count: String,
    open_href: String,
    closed_href: String,
    query: String,
    sort: String,
    active_labels: Vec<String>,
    label_items: Vec<LabelFilterItem>,
    rows: Vec<IssueRow>,
    /// The pinned issues, first on the first page of an unfiltered open list (ch-u6j).
    pinned_rows: Vec<IssueRow>,
    can_pin: bool,
    pager: Pager,
    can_create: bool,
    filtered: bool,
    clear_href: String,
    labels_href: String,
    milestones_href: String,
    /// Filter menus: milestones as links, and the people filters as typed usernames.
    milestone_items: Vec<LabelFilterItem>,
    milestone_id: String,
    author: String,
    assignee: String,
    /// The milestone this list is narrowed to, shown as its page header (ch-d9t).
    milestone: Option<MilestoneHead>,
}

/// Closed issues as a whole percent of all of them. None at all reads as 0.
pub fn milestone_percent(open: i64, closed: i64) -> u8 {
    let total = open.max(0) + closed.max(0);
    if total == 0 { 0 } else { u8::try_from(closed.max(0) * 100 / total).unwrap_or(100) }
}

/// A milestone as the header of the issue list filtered to it.
pub struct MilestoneHead {
    pub title: String,
    pub description: String,
    pub due: Option<String>,
    pub open: bool,
    pub open_issues: i64,
    pub closed_issues: i64,
    pub percent: u8,
}

pub struct ListQuery {
    pub filter: IssueFilter,
    pub open: bool,
    pub page: u32,
}

async fn lookup_user(ctx: &Ctx, name: Option<&str>) -> Option<UserId> {
    match name {
        Some(n) if !n.is_empty() => ctx.state.accounts.get_by_username(&ctx.actor, n).await.ok().map(|u| u.id),
        _ => None,
    }
}

pub async fn parse_filter(ctx: &Ctx, q: &Params, per_page: u32) -> ListQuery {
    let open = q.get("state").map(String::as_str) != Some("closed");
    let page: u32 = q.get("page").and_then(|p| p.parse().ok()).filter(|p| *p >= 1).unwrap_or(1);
    let labels: Vec<String> =
        q.get("labels").map(|l| l.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let assignee = lookup_user(ctx, q.get("assignee").map(String::as_str)).await;
    let author = lookup_user(ctx, q.get("author").map(String::as_str)).await;
    let milestone = q.get("milestone").and_then(|m| m.parse::<i64>().ok()).map(MilestoneId);
    let query = q.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let sort = q.get("sort").cloned().filter(|s| matches!(s.as_str(), "newest" | "oldest" | "updated" | "comments"));
    ListQuery { filter: IssueFilter { open: Some(open), labels, assignee, author, milestone, query, sort, page, per_page }, open, page }
}

pub async fn rows_for(ctx: &Ctx, rc: &RepoCtx, issues: &[Issue], kind: &str) -> Vec<IssueRow> {
    let mut people = People::new(&ctx.state);
    let labels = label_map(ctx, rc).await;
    let base = rc.base();
    let mut rows = Vec::with_capacity(issues.len());
    for i in issues {
        let mut assignees = Vec::new();
        for a in &i.assignees {
            assignees.push(people.get(*a).await);
        }
        rows.push(IssueRow {
            number: i.number,
            title: i.title.clone(),
            href: format!("{base}/{kind}/{}", i.number),
            open: i.open,
            icon: if i.open { "circle-dot" } else { "circle-check" },
            state_class: if i.open { "open" } else { "closed" },
            labels: label_views(&i.labels, &labels, &format!("{base}/{kind}")),
            author: people.get(i.author_id).await,
            when: fmt::time(i.created_at),
            comments: i.comments,
            assignees,
        });
    }
    rows
}

pub async fn list(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let base = rc.base();
    let lq = parse_filter(&ctx, &q, PER_PAGE + 1).await;
    let mut issues: Vec<Issue> =
        ctx.state.issues.list(&ctx.actor, rc.id(), lq.filter.clone()).await?.into_iter().filter(|i| !i.is_pull).collect();
    let has_more = issues.len() as u32 > PER_PAGE;
    issues.truncate(PER_PAGE as usize);

    let count = |open: bool| {
        let mut f = lq.filter.clone();
        f.open = Some(open);
        f.page = 1;
        f.per_page = 100;
        let ctx = &ctx;
        let id = rc.id();
        async move { ctx.state.issues.list(&ctx.actor, id, f).await.map(|v| v.iter().filter(|i| !i.is_pull).count()).unwrap_or(0) }
    };
    let (oc, cc) = futures::join!(count(true), count(false));

    let all_labels = ctx.state.tracker.labels(&ctx.actor, rc.id()).await.unwrap_or_default();
    let active = lq.filter.labels.clone();
    let label_items = all_labels
        .iter()
        .map(|l| {
            let on = active.contains(&l.name);
            let next: Vec<String> = if on {
                active.iter().filter(|a| **a != l.name).cloned().collect()
            } else {
                active.iter().cloned().chain(std::iter::once(l.name.clone())).collect()
            };
            LabelFilterItem {
                name: l.name.clone(),
                class: fmt::label_class(&l.color),
                on,
                href: format!("{base}/issues{}", qs(&q, &[("labels", &next.join(","))], &[])),
            }
        })
        .collect();
    let rows = rows_for(&ctx, &rc, &issues, "issues").await;
    let filtered = q.keys().any(|k| matches!(k.as_str(), "labels" | "assignee" | "author" | "milestone" | "q"));
    let all_milestones = ctx.state.tracker.milestones(&ctx.actor, rc.id(), None).await.unwrap_or_default();
    let picked = lq.filter.milestone;
    let milestone_items = all_milestones
        .iter()
        .filter(|m| m.open || Some(m.id) == picked)
        .map(|m| LabelFilterItem {
            name: m.title.clone(),
            class: String::new(),
            on: Some(m.id) == picked,
            href: format!(
                "{base}/issues{}",
                if Some(m.id) == picked { qs(&q, &[], &["milestone"]) } else { qs(&q, &[("milestone", &m.id.0.to_string())], &[]) }
            ),
        })
        .collect();
    let milestone = picked.and_then(|id| all_milestones.iter().find(|m| m.id == id)).map(|m| MilestoneHead {
        title: m.title.clone(),
        description: m.description.clone(),
        due: m.due_on.map(|d| d.format("%-d %B %Y").to_string()),
        open: m.open,
        open_issues: m.open_issues,
        closed_issues: m.closed_issues,
        percent: milestone_percent(m.open_issues, m.closed_issues),
    });
    let pinned_issues: Vec<Issue> = if lq.open && !filtered && lq.page <= 1 {
        ctx.state.issues.pinned(&ctx.actor, rc.id()).await.unwrap_or_default().into_iter().map(|v| v.issue).filter(|i| i.open).collect()
    } else {
        Vec::new()
    };
    let pinned_rows = rows_for(&ctx, &rc, &pinned_issues, "issues").await;
    // A pinned issue sits in the Pinned block and is not listed again under Other issues (ch-4ls).
    let rows: Vec<IssueRow> = rows.into_iter().filter(|r| !pinned_rows.iter().any(|p| p.number == r.number)).collect();
    let can_pin = rc.perm.allows(Permission::Triage) && !rc.repo.archived;
    let c = rc.chrome(&ctx, "Issues", "issues").await;
    ctx.page(&ListPage {
        c,
        open: lq.open,
        open_count: count_label(oc, 100),
        closed_count: count_label(cc, 100),
        open_href: format!("{base}/issues{}", qs(&q, &[("state", "open")], &[])),
        closed_href: format!("{base}/issues{}", qs(&q, &[("state", "closed")], &[])),
        query: q.get("q").cloned().unwrap_or_default(),
        sort: q.get("sort").cloned().unwrap_or_else(|| "newest".into()),
        active_labels: active,
        label_items,
        rows,
        pinned_rows,
        can_pin,
        pager: Pager::new(&format!("{base}/issues{}", qs(&q, &[], &[])), lq.page, has_more),
        can_create: true,
        filtered,
        clear_href: format!("{base}/issues"),
        labels_href: format!("{base}/labels"),
        milestones_href: format!("{base}/milestones"),
        milestone_items,
        milestone_id: picked.map(|m| m.0.to_string()).unwrap_or_default(),
        author: q.get("author").cloned().unwrap_or_default(),
        assignee: q.get("assignee").cloned().unwrap_or_default(),
        milestone,
        base,
    })
}

// ------------------------------------------------------------------ new issue

pub struct Choice {
    pub value: String,
    pub text: String,
    pub on: bool,
    pub class: String,
}

pub struct TemplatePick {
    pub name: String,
    pub about: String,
    pub href: String,
}

pub struct FormOptionView {
    /// What the control sends: a dropdown option's label, or a checkbox's position.
    pub value: String,
    pub label: String,
    pub on: bool,
}

/// One field of a YAML issue form, ready to draw: what was submitted is already folded in.
pub struct FormFieldView {
    pub field: FormField,
    /// A markdown field's rendered html; empty for the others.
    pub html: String,
    /// What an input or textarea shows: the answer given, else the form's own prefill.
    pub value: String,
    pub options: Vec<FormOptionView>,
    pub error: String,
}

#[derive(Template)]
#[template(path = "issues/new.html")]
struct NewPage {
    c: Chrome,
    base: String,
    owner: String,
    repo: String,
    title: String,
    body: String,
    labels: Vec<Choice>,
    milestones: Vec<Choice>,
    can_triage: bool,
    errors: FieldErrors,
    template_choices: Vec<TemplatePick>,
    template_path: String,
    is_form: bool,
    form_fields: Vec<FormFieldView>,
}

#[allow(clippy::too_many_arguments)]
async fn new_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    title: &str,
    body: &str,
    picked: &[String],
    milestone: Option<i64>,
    errors: FieldErrors,
    status: StatusCode,
    template: &str,
    submitted: &HashMap<String, Vec<String>>,
) -> WebResult<Response> {
    let can_triage = rc.perm.allows(Permission::Triage) && !rc.repo.archived;
    let templates = ctx.state.issues.templates(&ctx.actor, rc.id()).await.unwrap_or_default();
    let chosen = templates.iter().find(|t| t.path == template);
    // A fresh view takes the template's title, body and labels; a re-render keeps what was sent.
    let fresh = title.is_empty() && body.is_empty() && picked.is_empty() && submitted.is_empty();
    let (title, body): (String, String) = match chosen {
        Some(t) if fresh => (t.title.clone(), if t.form.is_none() { t.body.clone() } else { body.into() }),
        _ => (title.into(), body.into()),
    };
    let picked: Vec<String> = if fresh && can_triage { chosen.map(|t| t.labels.clone()).unwrap_or_default() } else { picked.to_vec() };
    let labels = if can_triage {
        ctx.state
            .tracker
            .labels(&ctx.actor, rc.id())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|l| Choice { on: picked.contains(&l.name), class: fmt::label_class(&l.color), value: l.name.clone(), text: l.name })
            .collect()
    } else {
        Vec::new()
    };
    let milestones = if can_triage {
        ctx.state
            .tracker
            .milestones(&ctx.actor, rc.id(), Some(true))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| Choice { on: Some(m.id.0) == milestone, class: String::new(), value: m.id.0.to_string(), text: m.title })
            .collect()
    } else {
        Vec::new()
    };
    let (is_form, form_fields): (bool, Vec<FormFieldView>) = match chosen.and_then(|t| t.form.as_ref()) {
        Some(form) => (
            true,
            form.fields
                .iter()
                .map(|f| {
                    let control = f.control();
                    let sent = submitted.get(&control);
                    FormFieldView {
                        field: f.clone(),
                        html: if f.is_markdown() { md_for(rc, &f.value) } else { String::new() },
                        value: sent.and_then(|v| v.first().cloned()).unwrap_or_else(|| f.value.clone()),
                        options: if f.is_dropdown() {
                            f.options
                                .iter()
                                .map(|o| FormOptionView {
                                    value: o.label.clone(),
                                    label: o.label.clone(),
                                    on: sent.is_some_and(|vs| vs.contains(&o.label)),
                                })
                                .collect()
                        } else if f.is_checkboxes() {
                            f.options
                                .iter()
                                .enumerate()
                                .map(|(i, o)| FormOptionView {
                                    value: i.to_string(),
                                    label: o.label.clone(),
                                    on: sent.is_some_and(|vs| vs.iter().any(|s| *s == i.to_string())),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        },
                        error: errors.get(&control),
                    }
                })
                .collect(),
        ),
        None => (false, Vec::new()),
    };
    let template_choices: Vec<TemplatePick> = if chosen.is_none() {
        templates
            .iter()
            .map(|t| TemplatePick {
                name: t.name.clone(),
                about: t.about.clone(),
                href: format!("{}/issues/new?template={}", rc.base(), fmt::enc(&t.path)),
            })
            .collect()
    } else {
        Vec::new()
    };
    let template_path = chosen.map(|t| t.path.clone()).unwrap_or_default();
    let c = rc.chrome(ctx, "New issue", "issues").await;
    ctx.page_status(
        status,
        &NewPage {
            c,
            base: rc.base(),
            owner: rc.repo.owner_name.clone(),
            repo: rc.repo.name.clone(),
            title,
            body,
            labels,
            milestones,
            can_triage,
            errors,
            template_choices,
            template_path,
            is_form,
            form_fields,
        },
    )
}

pub async fn new_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let template = q.get("template").cloned().unwrap_or_default();
    new_page(&ctx, &rc, "", "", &[], None, FieldErrors::default(), StatusCode::OK, &template, &HashMap::new()).await
}

pub async fn new_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Read)?;
    let title = form.get("title").trim().to_string();
    let body = form.get("body").to_string();
    let template = form.get("template").trim().to_string();
    let can_triage = rc.perm.allows(Permission::Triage);
    let labels: Vec<String> = if can_triage { form.all("labels").into_iter().map(str::to_string).collect() } else { Vec::new() };
    let milestone = if can_triage { form.int("milestone") } else { None };
    let mut submitted: HashMap<String, Vec<String>> = HashMap::new();
    let mut errors = FieldErrors::default();
    let mut composed: Option<String> = None;
    if !template.is_empty() {
        // Field definitions come from the repository again; the request only carries answers.
        let templates = ctx.state.issues.templates(&ctx.actor, rc.id()).await.unwrap_or_default();
        if let Some(issue_form) = templates.iter().find(|t| t.path == template).and_then(|t| t.form.as_ref()) {
            for field in &issue_form.fields {
                if !field.is_markdown() {
                    submitted.insert(field.control(), form.all(&field.control()).into_iter().map(str::to_string).collect());
                }
            }
            match compose(issue_form, &submitted) {
                Ok(text) => composed = Some(text),
                Err(list) => {
                    for (control, message) in list {
                        errors = errors.with(&control, &message);
                    }
                }
            }
        }
    }
    if title.is_empty() {
        errors = errors.with("title", "Give the issue a title.");
    }
    if errors.any() {
        return new_page(&ctx, &rc, &title, &body, &labels, milestone, errors, StatusCode::UNPROCESSABLE_ENTITY, &template, &submitted)
            .await;
    }
    let body = composed.unwrap_or(body);
    let new = NewIssue {
        repo_id: rc.id(),
        title: title.clone(),
        body: body.clone(),
        labels: labels.clone(),
        assignees: Vec::new(),
        milestone_id: milestone.map(MilestoneId),
    };
    match ctx.state.issues.create(&ctx.actor, new, &ctx.client).await {
        Ok(issue) => Ok(ctx.redirect(&format!("{}/issues/{}", rc.base(), issue.number))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => {
                new_page(&ctx, &rc, &title, &body, &labels, milestone, errors, StatusCode::UNPROCESSABLE_ENTITY, &template, &submitted)
                    .await
            }
            None => Err(e.into()),
        },
    }
}

// ------------------------------------------------------------------ one issue

#[derive(Template)]
#[template(path = "issues/show.html")]
struct ShowPage {
    c: Chrome,
    base: String,
    owner: String,
    repo: String,
    number: i64,
    title: String,
    open: bool,
    state_label: String,
    author: UserRef,
    when: TimeView,
    body_html: String,
    comments: Vec<CommentView>,
    labels: Vec<LabelView>,
    assignees: Vec<UserRef>,
    milestone: Option<(String, String)>,
    closed_note: Option<(String, TimeView)>,
    can_comment: bool,
    can_triage: bool,
    can_edit: bool,
    pinned: bool,
    can_write: bool,
    can_admin: bool,
    label_choices: Vec<Choice>,
    milestone_choices: Vec<Choice>,
    assignee_choices: Vec<Choice>,
    raw_title: String,
    raw_body: String,
}

pub async fn show(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let base = rc.base();
    let issue = ctx.state.issues.get(&ctx.actor, rc.id(), number).await?;
    if issue.is_pull {
        return Ok(ctx.redirect(&format!("{base}/pulls/{number}")));
    }
    let comments = ctx.state.issues.comments(&ctx.actor, rc.id(), number).await?;
    let mut people = People::new(&ctx.state);
    let comment_views = comment_views(&rc, &mut people, &issue, &comments).await;
    let lmap = label_map(&ctx, &rc).await;
    let mut assignees = Vec::new();
    for a in &issue.assignees {
        assignees.push(people.get(*a).await);
    }
    let can_triage = rc.perm.allows(Permission::Triage) && !rc.repo.archived;
    // A signed-out visitor reads a public issue and gets no forms.
    let me = ctx.user_id().ok();
    let can_edit = can_triage || (Some(issue.author_id) == me && !rc.repo.archived);

    let milestones = ctx.state.tracker.milestones(&ctx.actor, rc.id(), None).await.unwrap_or_default();
    let milestone = issue
        .milestone_id
        .and_then(|id| milestones.iter().find(|m| m.id == id))
        .map(|m| (m.title.clone(), format!("{base}/issues?milestone={}", m.id.0)));
    let (label_choices, milestone_choices, assignee_choices) = if can_triage {
        let lc = lmap
            .values()
            .map(|l| Choice {
                value: l.name.clone(),
                text: l.name.clone(),
                on: issue.labels.contains(&l.name),
                class: fmt::label_class(&l.color),
            })
            .collect::<Vec<_>>();
        let mc = milestones
            .iter()
            .filter(|m| m.open || Some(m.id) == issue.milestone_id)
            .map(|m| Choice {
                value: m.id.0.to_string(),
                text: m.title.clone(),
                on: Some(m.id) == issue.milestone_id,
                class: String::new(),
            })
            .collect();
        let mut ac = Vec::new();
        if let Ok(collabs) = ctx.state.repos.collaborators(&ctx.actor, rc.id()).await {
            for cb in collabs {
                ac.push(Choice {
                    value: cb.user_id.0.to_string(),
                    text: cb.username,
                    on: issue.assignees.contains(&cb.user_id),
                    class: String::new(),
                });
            }
        }
        (lc, mc, ac)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    let mut label_choices = label_choices;
    label_choices.sort_by_key(|a| a.text.to_lowercase());
    let closed_note = if !issue.open {
        issue
            .closed_at
            .map(|t| (issue.state_reason.clone().map(|r| r.replace('_', " ")).unwrap_or_else(|| "completed".into()), fmt::time(t)))
    } else {
        None
    };
    let c = rc.chrome(&ctx, &format!("{} (#{number})", issue.title), "issues").await;
    ctx.page(&ShowPage {
        c,
        owner: rc.repo.owner_name.clone(),
        repo: rc.repo.name.clone(),
        number,
        title: issue.title.clone(),
        open: issue.open,
        state_label: if issue.open { "Open".into() } else { "Closed".into() },
        author: people.get(issue.author_id).await,
        when: fmt::time(issue.created_at),
        body_html: if issue.body.trim().is_empty() { String::new() } else { md_for(&rc, &issue.body) },
        comments: comment_views,
        labels: label_views(&issue.labels, &lmap, &format!("{base}/issues")),
        assignees,
        milestone,
        closed_note,
        can_comment: me.is_some() && !rc.repo.archived && !issue.locked,
        can_triage,
        can_edit,
        pinned: ctx.state.issues.pinned(&ctx.actor, rc.id()).await.map(|p| p.iter().any(|v| v.issue.number == number)).unwrap_or(false),
        can_write: rc.perm.allows(Permission::Write) && !rc.repo.archived,
        can_admin: rc.perm.allows(Permission::Admin) && !rc.repo.archived,
        label_choices,
        milestone_choices,
        assignee_choices,
        raw_title: issue.title,
        raw_body: issue.body,
        base,
    })
}

// ------------------------------------------------------------------ changing one issue

fn back(rc: &RepoCtx, kind: &str, number: i64) -> String {
    format!("{}/{kind}/{number}", rc.base())
}

/// Adds a comment and, when asked, closes or reopens, in one form post. Shared with pull requests.
pub async fn do_comment(ctx: &Ctx, rc: &RepoCtx, number: i64, form: &FormData, kind: &str) -> WebResult<Response> {
    rc.require_writable(Permission::Read)?;
    let body = form.get("body").trim();
    let action = form.get("action");
    if body.is_empty() && action.is_empty() {
        return Ok(ctx.redirect_flash(&back(rc, kind, number), "error", "Write something first."));
    }
    let mut anchor = String::new();
    if !body.is_empty() {
        let cm = ctx.state.issues.add_comment(&ctx.actor, rc.id(), number, body, &ctx.client).await?;
        anchor = format!("#comment-{}", cm.id.0);
    }
    match action {
        "close" => {
            let reason = if form.get("reason") == "not_planned" { "not_planned" } else { "completed" };
            ctx.state.issues.set_state(&ctx.actor, rc.id(), number, false, Some(reason), &ctx.client).await?;
        }
        "reopen" => {
            ctx.state.issues.set_state(&ctx.actor, rc.id(), number, true, None, &ctx.client).await?;
        }
        _ => {}
    }
    Ok(ctx.redirect(&format!("{}{anchor}", back(rc, kind, number))))
}

pub async fn comment_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    do_comment(&ctx, &rc, number, &form, "issues").await
}

pub async fn do_edit(ctx: &Ctx, rc: &RepoCtx, number: i64, form: &FormData, kind: &str) -> WebResult<Response> {
    rc.require_writable(Permission::Read)?;
    let title = form.opt("title").map(str::to_string);
    if title.is_none() {
        return Ok(ctx.redirect_flash(&back(rc, kind, number), "error", "A title cannot be empty."));
    }
    ctx.state.issues.edit(&ctx.actor, rc.id(), number, title, Some(form.get("body").to_string()), &ctx.client).await?;
    Ok(ctx.redirect_flash(&back(rc, kind, number), "ok", "Saved."))
}

pub async fn edit_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    do_edit(&ctx, &rc, number, &form, "issues").await
}

pub async fn do_labels(ctx: &Ctx, rc: &RepoCtx, number: i64, form: &FormData, kind: &str) -> WebResult<Response> {
    rc.require_writable(Permission::Triage)?;
    let labels = form.all("labels").into_iter().map(str::to_string).collect();
    ctx.state.issues.set_labels(&ctx.actor, rc.id(), number, labels, &ctx.client).await?;
    Ok(ctx.redirect(&back(rc, kind, number)))
}

pub async fn labels_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    do_labels(&ctx, &rc, number, &form, "issues").await
}

pub async fn do_milestone(ctx: &Ctx, rc: &RepoCtx, number: i64, form: &FormData, kind: &str) -> WebResult<Response> {
    rc.require_writable(Permission::Triage)?;
    ctx.state.tracker.set_milestone(&ctx.actor, rc.id(), number, form.int("milestone").map(MilestoneId), &ctx.client).await?;
    Ok(ctx.redirect(&back(rc, kind, number)))
}

pub async fn milestone_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    do_milestone(&ctx, &rc, number, &form, "issues").await
}

pub async fn do_assignees(ctx: &Ctx, rc: &RepoCtx, number: i64, form: &FormData, kind: &str) -> WebResult<Response> {
    rc.require_writable(Permission::Triage)?;
    let users: Vec<UserId> = form.all("assignees").into_iter().filter_map(|v| v.parse::<i64>().ok()).map(UserId).collect();
    ctx.state.tracker.set_assignees(&ctx.actor, rc.id(), number, users, &ctx.client).await?;
    Ok(ctx.redirect(&back(rc, kind, number)))
}

/// Pins or unpins an issue (`pin` is `1` or `0`).
pub async fn pin_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Triage)?;
    match ctx.state.issues.set_pinned(&ctx.actor, rc.id(), number, form.get("pin") == "1", &ctx.client).await {
        Ok(_) => Ok(ctx.redirect(&back(&rc, "issues", number))),
        Err(e) => Err(e.into()),
    }
}

/// Moves one pinned issue up or down: `move` is `<number>:up` or `<number>:down`.
pub async fn pinned_order_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Triage)?;
    let mut order: Vec<i64> = ctx.state.issues.pinned(&ctx.actor, rc.id()).await?.iter().map(|v| v.issue.number).collect();
    if let Some((n, dir)) = form.get("move").split_once(':')
        && let Ok(n) = n.parse::<i64>()
        && let Some(at) = order.iter().position(|x| *x == n)
    {
        let to = if dir == "up" { at.checked_sub(1) } else { Some(at + 1).filter(|t| *t < order.len()) };
        if let Some(to) = to {
            order.swap(at, to);
            ctx.state.issues.reorder_pinned(&ctx.actor, rc.id(), order, &ctx.client).await?;
        }
    }
    Ok(ctx.redirect(&format!("{}/issues", rc.base())))
}

/// Deletes an issue for good. Repository admins only.
pub async fn delete_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, _form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Admin)?;
    ctx.state.issues.delete(&ctx.actor, rc.id(), number, &ctx.client).await?;
    Ok(ctx.redirect(&format!("{}/issues", rc.base())))
}

/// Moves an issue to another repository, named `owner/name` in the `target` field.
pub async fn transfer_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let wanted = form.get("target").trim().to_string();
    let target = match wanted.split_once('/') {
        Some((o, n)) => ctx.state.repos.get_by_name(&ctx.actor, o.trim(), n.trim()).await.ok(),
        None => None,
    };
    let Some(target) = target else {
        return Err(Error::Validation(vec![FieldError::custom("Issue", "target", "Name a repository you can see, as owner/name")]).into());
    };
    let moved = ctx.state.issues.transfer(&ctx.actor, rc.id(), number, target.id, &ctx.client).await?;
    Ok(ctx.redirect(&format!("/{}/{}/issues/{}", target.owner_name, target.name, moved.issue.number)))
}

pub async fn assignees_post(ctx: Ctx, Path((owner, name, number)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    do_assignees(&ctx, &rc, number, &form, "issues").await
}

// ------------------------------------------------------------------ labels

pub struct LabelRow {
    pub name: String,
    pub description: String,
    pub class: String,
    pub color: String,
    pub href: String,
}

#[derive(Template)]
#[template(path = "issues/labels.html")]
struct LabelsPage {
    c: Chrome,
    base: String,
    rows: Vec<LabelRow>,
    can_manage: bool,
    errors: FieldErrors,
    name: String,
    color: String,
    description: String,
    palette: Vec<&'static str>,
}

/// Starting colours offered in the new label form. They map onto the stylesheet's palette.
pub const LABEL_COLORS: &[&str] =
    &["b42318", "c2410c", "a16207", "4d7c0f", "15803d", "0f766e", "0e7490", "1d4ed8", "4338ca", "7e22ce", "be185d", "475569"];

pub fn valid_color(s: &str) -> Option<String> {
    let h = s.trim().trim_start_matches('#').to_lowercase();
    if h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()) { Some(h) } else { None }
}

async fn labels_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    errors: FieldErrors,
    name: &str,
    color: &str,
    description: &str,
    status: StatusCode,
) -> WebResult<Response> {
    let labels = ctx.state.tracker.labels(&ctx.actor, rc.id()).await?;
    let base = rc.base();
    let rows = labels
        .iter()
        .map(|l| LabelRow {
            name: l.name.clone(),
            description: l.description.clone(),
            class: fmt::label_class(&l.color),
            color: l.color.clone(),
            href: format!("{base}/issues?labels={}", fmt::enc(&l.name)),
        })
        .collect();
    let c = rc.chrome(ctx, "Labels", "issues").await;
    ctx.page_status(
        status,
        &LabelsPage {
            c,
            base,
            rows,
            can_manage: rc.perm.allows(Permission::Triage) && !rc.repo.archived,
            errors,
            name: name.into(),
            color: color.into(),
            description: description.into(),
            palette: LABEL_COLORS.to_vec(),
        },
    )
}

pub async fn labels_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    labels_page(&ctx, &rc, FieldErrors::default(), "", "0f766e", "", StatusCode::OK).await
}

pub async fn label_create(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Triage)?;
    let n = form.get("name").trim();
    let Some(color) = valid_color(form.get("color")) else {
        return labels_page(
            &ctx,
            &rc,
            FieldErrors::default().with("color", "Use six hex digits, like 0f766e."),
            n,
            form.get("color"),
            form.get("description"),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    };
    if n.is_empty() || n.len() > 50 || n.contains(',') {
        return labels_page(
            &ctx,
            &rc,
            FieldErrors::default().with("name", "Give it a name of up to 50 characters, without commas."),
            n,
            &color,
            form.get("description"),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    match ctx.state.tracker.create_label(&ctx.actor, rc.id(), n, &color, form.get("description").trim(), &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&format!("{}/labels", rc.base()), "ok", &format!("Created the label {n}."))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => labels_page(&ctx, &rc, errors, n, &color, form.get("description"), StatusCode::UNPROCESSABLE_ENTITY).await,
            None => Err(e.into()),
        },
    }
}

pub async fn label_delete(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Triage)?;
    let n = form.get("name").trim();
    ctx.state.tracker.delete_label(&ctx.actor, rc.id(), n, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/labels", rc.base()), "ok", &format!("Deleted the label {n}.")))
}

// ------------------------------------------------------------------ milestones

pub struct MilestoneRow {
    pub id: i64,
    pub title: String,
    pub description_html: String,
    pub due: Option<TimeView>,
    pub overdue: bool,
    pub open: bool,
    pub open_issues: i64,
    pub closed_issues: i64,
    pub percent: i64,
    pub href: String,
}

pub fn percent(closed: i64, open: i64) -> i64 {
    let total = closed + open;
    if total <= 0 { 0 } else { (closed * 100 + total / 2) / total }
}

#[derive(Template)]
#[template(path = "issues/milestones.html")]
struct MilestonesPage {
    c: Chrome,
    base: String,
    show_open: bool,
    open_href: String,
    closed_href: String,
    rows: Vec<MilestoneRow>,
    can_manage: bool,
    errors: FieldErrors,
    title: String,
    description: String,
}

fn milestone_row(rc: &RepoCtx, m: &Milestone) -> MilestoneRow {
    MilestoneRow {
        id: m.id.0,
        title: m.title.clone(),
        description_html: if m.description.trim().is_empty() { String::new() } else { md_for(rc, &m.description) },
        due: fmt::time_opt(m.due_on),
        overdue: m.open && m.due_on.is_some_and(|d| d < chrono::Utc::now()),
        open: m.open,
        open_issues: m.open_issues,
        closed_issues: m.closed_issues,
        percent: percent(m.closed_issues, m.open_issues),
        href: format!("{}/issues?milestone={}", rc.base(), m.id.0),
    }
}

async fn milestones_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    show_open: bool,
    errors: FieldErrors,
    title: &str,
    description: &str,
    status: StatusCode,
) -> WebResult<Response> {
    let ms = ctx.state.tracker.milestones(&ctx.actor, rc.id(), Some(show_open)).await?;
    let base = rc.base();
    let c = rc.chrome(ctx, "Milestones", "issues").await;
    ctx.page_status(
        status,
        &MilestonesPage {
            c,
            show_open,
            open_href: format!("{base}/milestones"),
            closed_href: format!("{base}/milestones?state=closed"),
            rows: ms.iter().map(|m| milestone_row(rc, m)).collect(),
            can_manage: rc.perm.allows(Permission::Triage) && !rc.repo.archived,
            errors,
            title: title.into(),
            description: description.into(),
            base,
        },
    )
}

pub async fn milestones_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    milestones_page(&ctx, &rc, q.get("state").map(String::as_str) != Some("closed"), FieldErrors::default(), "", "", StatusCode::OK).await
}

pub async fn milestone_create(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Triage)?;
    let title = form.get("title").trim().to_string();
    if title.is_empty() {
        return milestones_page(
            &ctx,
            &rc,
            true,
            FieldErrors::default().with("title", "Give the milestone a title."),
            &title,
            form.get("description"),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let due = match form.opt("due") {
        Some(d) => match chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d") {
            Ok(date) => date.and_hms_opt(23, 59, 59).map(|t| t.and_utc()),
            Err(_) => {
                return milestones_page(
                    &ctx,
                    &rc,
                    true,
                    FieldErrors::default().with("due", "Use a date like 2026-12-31."),
                    &title,
                    form.get("description"),
                    StatusCode::UNPROCESSABLE_ENTITY,
                )
                .await;
            }
        },
        None => None,
    };
    match ctx.state.tracker.create_milestone(&ctx.actor, rc.id(), &title, form.get("description").trim(), due, &ctx.client).await {
        Ok(_) => Ok(ctx.redirect_flash(&format!("{}/milestones", rc.base()), "ok", &format!("Created the milestone {title}."))),
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errors) => {
                milestones_page(&ctx, &rc, true, errors, &title, form.get("description"), StatusCode::UNPROCESSABLE_ENTITY).await
            }
            None => Err(e.into()),
        },
    }
}

pub async fn milestone_state(ctx: Ctx, Path((owner, name, id)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Triage)?;
    let open = form.get("state") == "open";
    ctx.state.tracker.set_milestone_state(&ctx.actor, rc.id(), MilestoneId(id), open, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/milestones", rc.base()), "ok", if open { "Reopened." } else { "Closed." }))
}

#[allow(dead_code)]
fn _unused(_: WebError, _: Error) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milestone_progress_is_a_whole_percent() {
        assert_eq!(milestone_percent(3, 5), 62);
        assert_eq!(milestone_percent(0, 0), 0);
        assert_eq!(milestone_percent(0, 4), 100);
        assert_eq!(milestone_percent(-1, 1), 100);
    }

    fn q(pairs: &[(&str, &str)]) -> Params {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn query_strings_replace_drop_and_reset_the_page() {
        let p = q(&[("state", "open"), ("labels", "bug"), ("page", "3")]);
        assert_eq!(qs(&p, &[], &[]), "?labels=bug&state=open");
        assert_eq!(qs(&p, &[("state", "closed")], &[]), "?labels=bug&state=closed");
        assert_eq!(qs(&p, &[("labels", "")], &[]), "?state=open");
        assert_eq!(qs(&p, &[], &["labels", "state"]), "");
        assert_eq!(qs(&q(&[("q", "a b&c")]), &[], &[]), "?q=a%20b%26c");
    }

    #[test]
    fn counts_and_percentages() {
        assert_eq!(count_label(5, 100), "5");
        assert_eq!(count_label(100, 100), "100+");
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(1, 3), 25);
        assert_eq!(percent(3, 0), 100);
    }

    #[test]
    fn colours_must_be_six_hex_digits() {
        assert_eq!(valid_color("#0F766E").as_deref(), Some("0f766e"));
        assert_eq!(valid_color("0f766e").as_deref(), Some("0f766e"));
        assert_eq!(valid_color("fff"), None);
        assert_eq!(valid_color("zzzzzz"), None);
    }
}
