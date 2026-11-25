//! Issues, comments, labels, milestones, reactions, assignees and timelines.

use super::dto::Ctx;
use super::{ApiJson, Client, CurrentActor, OptionalJson, Paging, created, double_option, no_content, ok, page_of, quote, repo_of};
use axum::Router;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post, put};
use ch_api_types::issues as api;
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, CommentId, Error, FieldError, Result, UserId};
use chrono::{DateTime, Utc};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/issues", get(my_issues))
        .route("/user/issues", get(my_issues))
        .route("/repos/{owner}/{repo}/issues", get(list_issues).post(create_issue))
        .route("/repos/{owner}/{repo}/issues/{number}", get(get_issue).patch(patch_issue))
        .route("/repos/{owner}/{repo}/issues/{number}/lock", put(lock_issue).delete(unlock_issue))
        .route("/repos/{owner}/{repo}/issues/{number}/pin", put(pin_issue).delete(unpin_issue))
        // Codehaven's own, under their own prefix because GitHub has no REST for them (ch-u6j).
        .route("/repos/{owner}/{repo}/codehaven/issues/pinned", get(list_pinned))
        .route("/repos/{owner}/{repo}/codehaven/issues/pinned/order", put(order_pinned))
        .route("/repos/{owner}/{repo}/codehaven/issues/{number}", axum::routing::delete(delete_issue))
        .route("/repos/{owner}/{repo}/codehaven/issues/{number}/transfer", post(transfer_issue))
        .route("/repos/{owner}/{repo}/issues/{number}/comments", get(list_comments).post(add_comment))
        .route("/repos/{owner}/{repo}/issues/comments", get(list_repo_comments))
        .route("/repos/{owner}/{repo}/issues/comments/{id}", get(get_comment).patch(edit_comment).delete(delete_comment))
        .route("/repos/{owner}/{repo}/issues/{number}/labels", get(issue_labels).post(add_labels).put(set_labels).delete(clear_labels))
        .route("/repos/{owner}/{repo}/issues/{number}/labels/{name}", axum::routing::delete(remove_label))
        .route("/repos/{owner}/{repo}/issues/{number}/assignees", post(add_assignees).delete(remove_assignees))
        .route("/repos/{owner}/{repo}/issues/{number}/events", get(issue_events))
        .route("/repos/{owner}/{repo}/issues/{number}/timeline", get(issue_timeline))
        .route("/repos/{owner}/{repo}/issues/{number}/reactions", get(issue_reactions).post(add_issue_reaction))
        .route("/repos/{owner}/{repo}/issues/{number}/reactions/{id}", axum::routing::delete(del_issue_reaction))
        .route("/repos/{owner}/{repo}/issues/comments/{id}/reactions", get(comment_reactions).post(add_comment_reaction))
        .route("/repos/{owner}/{repo}/issues/comments/{id}/reactions/{rid}", axum::routing::delete(del_comment_reaction))
        .route(
            "/repos/{owner}/{repo}/issues/{number}/subscription",
            get(get_subscription).put(put_subscription).delete(delete_subscription),
        )
        .route("/repos/{owner}/{repo}/labels", get(list_labels).post(create_label))
        .route("/repos/{owner}/{repo}/labels/{name}", get(get_label).patch(update_label).delete(delete_label))
        .route("/repos/{owner}/{repo}/milestones", get(list_milestones).post(create_milestone))
        .route("/repos/{owner}/{repo}/milestones/{number}", get(get_milestone).patch(update_milestone).delete(delete_milestone))
        .route("/repos/{owner}/{repo}/assignees", get(list_assignees))
        .route("/repos/{owner}/{repo}/assignees/{username}", get(check_assignee))
        .route("/repos/{owner}/{repo}/issue-templates", get(list_templates))
}

fn base(s: &AppState) -> &str {
    &s.config.server.external_url
}

/// The login of an account, or a 422 that names the field.
async fn user_id(s: &AppState, actor: &Actor, login: &str, field: &str) -> Result<UserId> {
    match s.accounts.get_by_username(actor, login.trim_start_matches('@')).await {
        Ok(u) => Ok(u.id),
        Err(Error::NotFound) => Err(Error::Validation(vec![FieldError::custom("Issue", field, format!("{login} is not a known user"))])),
        Err(e) => Err(e),
    }
}

async fn user_ids(s: &AppState, actor: &Actor, logins: &[String], field: &str) -> Result<Vec<UserId>> {
    let mut out = Vec::new();
    for l in logins {
        out.push(user_id(s, actor, l, field).await?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- lists

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    state: Option<String>,
    labels: Option<String>,
    assignee: Option<String>,
    creator: Option<String>,
    mentioned: Option<String>,
    milestone: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    /// Codehaven extension: the search language, `is:open label:bug`.
    q: Option<String>,
    filter: Option<String>,
}

fn filter_of(q: &ListQuery, paging: Paging) -> IssueFilter {
    let mut terms: Vec<String> = Vec::new();
    match q.assignee.as_deref() {
        Some("none") => terms.push("no:assignee".into()),
        Some("*") | None | Some("") => {}
        Some(a) => terms.push(format!("assignee:{}", quote(a))),
    }
    if let Some(c) = q.creator.as_deref().filter(|c| !c.is_empty()) {
        terms.push(format!("author:{}", quote(c)));
    }
    if let Some(m) = q.mentioned.as_deref().filter(|c| !c.is_empty()) {
        terms.push(format!("mentions:{}", quote(m)));
    }
    match q.milestone.as_deref() {
        Some("none") => terms.push("no:milestone".into()),
        Some("*") | None | Some("") => {}
        Some(m) => terms.push(format!("milestone:{}", quote(m))),
    }
    if let Some(extra) = q.q.as_deref().filter(|x| !x.trim().is_empty()) {
        terms.push(extra.to_string());
    }
    let sort = q.sort.as_deref().filter(|s| matches!(*s, "created" | "updated" | "comments")).map(|s| match q.direction.as_deref() {
        Some("asc") => format!("{s}-asc"),
        _ => format!("{s}-desc"),
    });
    IssueFilter {
        open: match q.state.as_deref() {
            Some("closed") => Some(false),
            Some("all") => None,
            _ => Some(true),
        },
        labels: q.labels.as_deref().unwrap_or("").split(',').map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect(),
        query: if terms.is_empty() { None } else { Some(terms.join(" ")) },
        sort,
        page: paging.page(),
        per_page: paging.per_page(),
        ..Default::default()
    }
}

async fn list_issues(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let views = s.issues.views(&actor, repo.id, filter_of(&q, paging)).await?;
    Ok(page_of(views.iter().map(|v| ctx.issue(v)).collect(), &uri, base(&s), paging))
}

/// The caller's own issues across repositories: `filter=assigned|created|mentioned|review_requested`.
async fn my_issues(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Query(q): Query<ListQuery>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let scope = match q.filter.as_deref().unwrap_or("assigned") {
        "assigned" => UserIssueScope::Assigned,
        "created" => UserIssueScope::Created,
        "mentioned" => UserIssueScope::Mentioned,
        "review_requested" => UserIssueScope::ReviewRequested,
        _ => return Err(Error::invalid("Issue", "filter", "filter must be assigned, created, mentioned or review_requested")),
    };
    let views = s.issues.for_user(&actor, scope, filter_of(&q, paging)).await?;
    let b = base(&s).to_string();
    Ok(page_of(views.iter().map(|v| Ctx::from_full_name(&b, &v.repo_full_name).issue(v)).collect(), &uri, &b, paging))
}

// ---------------------------------------------------------------- one issue

async fn create_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<api::CreateIssue>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let assignees = user_ids(&s, &actor, &body.assignees, "assignees").await?;
    let milestone_id = match body.milestone {
        Some(n) => Some(
            s.issues
                .milestone(&actor, repo.id, n)
                .await
                .map_err(|e| {
                    if matches!(e, Error::NotFound) { Error::Validation(vec![FieldError::new("Issue", "milestone", "invalid")]) } else { e }
                })?
                .id,
        ),
        None => None,
    };
    let issue = s
        .issues
        .create(
            &actor,
            NewIssue {
                repo_id: repo.id,
                title: body.title,
                body: body.body.unwrap_or_default(),
                labels: body.labels,
                assignees,
                milestone_id,
            },
            &client,
        )
        .await?;
    let view = s.issues.view(&actor, repo.id, issue.number).await?;
    Ok(created(ctx.issue(&view)))
}

async fn get_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(ctx.issue(&s.issues.view(&actor, repo.id, number).await?)))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LabelRef {
    Name(String),
    Obj { name: String },
    Other(serde::de::IgnoredAny),
}

impl LabelRef {
    fn name(self) -> Option<String> {
        match self {
            LabelRef::Name(n) | LabelRef::Obj { name: n } => Some(n),
            LabelRef::Other(_) => None,
        }
    }
}

fn label_names(v: Vec<LabelRef>) -> Vec<String> {
    v.into_iter().filter_map(LabelRef::name).collect()
}

#[derive(Debug, Default, Deserialize)]
struct PatchIssue {
    title: Option<String>,
    body: Option<String>,
    state: Option<String>,
    state_reason: Option<String>,
    labels: Option<Vec<LabelRef>>,
    assignees: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option")]
    milestone: Option<Option<i64>>,
}

async fn patch_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<PatchIssue>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let open = match body.state.as_deref() {
        None => None,
        Some("open") => Some(true),
        Some("closed") => Some(false),
        Some(_) => return Err(Error::invalid("Issue", "state", "state must be open or closed")),
    };
    let assignees = match &body.assignees {
        Some(l) => Some(user_ids(&s, &actor, l, "assignees").await?),
        None => None,
    };
    let patch = IssuePatch {
        title: body.title,
        body: body.body,
        open,
        state_reason: body.state_reason,
        labels: body.labels.map(label_names),
        assignees,
        milestone: body.milestone,
    };
    Ok(ok(ctx.issue(&s.issues.update(&actor, repo.id, number, patch, &client).await?)))
}

#[derive(Debug, Default, Deserialize)]
struct LockBody {
    lock_reason: Option<String>,
}

async fn lock_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    OptionalJson(body): OptionalJson<LockBody>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.set_locked(&actor, repo.id, number, true, body.lock_reason, &client).await?;
    Ok(no_content())
}

async fn unlock_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.set_locked(&actor, repo.id, number, false, None, &client).await?;
    Ok(no_content())
}

async fn pin_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.set_pinned(&actor, repo.id, number, true, &client).await?;
    Ok(no_content())
}

async fn unpin_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.set_pinned(&actor, repo.id, number, false, &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- pinned order, delete, transfer (Codehaven's own)

/// `GET /repos/{owner}/{repo}/codehaven/issues/pinned`: the pinned issues in their order.
async fn list_pinned(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let views = s.issues.pinned(&actor, repo.id).await?;
    Ok(ok(views.iter().map(|v| ctx.issue(v)).collect::<Vec<_>>()))
}

#[derive(Debug, Deserialize)]
struct PinnedOrder {
    numbers: Vec<i64>,
}

/// `PUT .../codehaven/issues/pinned/order` with `{"numbers": [3, 1, 2]}`: every pinned issue, once.
async fn order_pinned(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<PinnedOrder>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let views = s.issues.reorder_pinned(&actor, repo.id, body.numbers, &client).await?;
    Ok(ok(views.iter().map(|v| ctx.issue(v)).collect::<Vec<_>>()))
}

/// `DELETE .../codehaven/issues/{number}`: repository admins only; audited.
async fn delete_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.delete(&actor, repo.id, number, &client).await?;
    Ok(no_content())
}

#[derive(Debug, Deserialize)]
struct TransferBody {
    /// `owner/name` of the repository to move the issue to.
    repository: String,
}

/// `POST .../codehaven/issues/{number}/transfer` with `{"repository": "owner/name"}`: write access
/// on both; the target may not be more visible. Answers with the issue at its new address.
async fn transfer_issue(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<TransferBody>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let (to_owner, to_name) = body
        .repository
        .split_once('/')
        .ok_or_else(|| Error::Validation(vec![FieldError::custom("Issue", "repository", "Use owner/name")]))?;
    // A repository the person cannot see answers like one that is not there.
    let target = s
        .repos
        .get_by_name(&actor, to_owner, to_name)
        .await
        .map_err(|_| Error::Validation(vec![FieldError::custom("Issue", "repository", "No such repository")]))?;
    let moved = s.issues.transfer(&actor, repo.id, number, target.id, &client).await?;
    let ctx = Ctx::new(&s.config.server.external_url, &target.owner_name, &target.name);
    Ok(ok(ctx.issue(&moved)))
}

// ---------------------------------------------------------------- comments

async fn list_comments(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let all = s.issues.comment_views(&actor, repo.id, number).await?;
    let per = paging.per_page() as usize;
    let page: Vec<_> = all.iter().skip((paging.page() as usize - 1) * per).take(per).map(|c| ctx.comment(c, number)).collect();
    Ok(page_of(page, &uri, base(&s), paging))
}

/// `since`, `sort` and `direction`, the query both repository-wide comment lists share.
#[derive(Debug, Default, Deserialize)]
pub(super) struct CommentListQuery {
    since: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
}

/// The parsed query, with `sort` and `direction` already defaulted.
pub(super) struct CommentList {
    pub since: Option<DateTime<Utc>>,
    pub sort: &'static str,
    pub direction: &'static str,
}

pub(super) fn comment_list_of(q: &CommentListQuery) -> Result<CommentList> {
    let since = match &q.since {
        None => None,
        Some(s) => Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|_| Error::invalid("Comment", "since", "since must be an ISO 8601 time"))?
                .with_timezone(&Utc),
        ),
    };
    let sort = match q.sort.as_deref() {
        None | Some("created") => "created",
        Some("updated") => "updated",
        Some(_) => return Err(Error::invalid("Comment", "sort", "sort must be created or updated")),
    };
    let direction = match q.direction.as_deref() {
        None | Some("asc") => "asc",
        Some("desc") => "desc",
        Some(_) => return Err(Error::invalid("Comment", "direction", "direction must be asc or desc")),
    };
    Ok(CommentList { since, sort, direction })
}

/// The collab engine over the finished state, the way `workers` builds one. The repository-wide
/// comment lists are the one pair of routes that read from it directly rather than through the
/// service traits.
pub(super) fn core_of(s: &AppState) -> crate::Core {
    crate::Core::new(crate::deps::Deps::from_state(s))
}

/// Every conversation comment on the repository's issues and pull requests, oldest first.
async fn list_repo_comments(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<CommentListQuery>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let list = comment_list_of(&q)?;
    let views =
        core_of(&s).repo_comment_views(&actor, repo.id, list.since, list.sort, list.direction, paging.page(), paging.per_page()).await?;
    Ok(page_of(views.iter().map(|(c, n)| ctx.comment(c, *n)).collect(), &uri, base(&s), paging))
}

async fn add_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::CreateComment>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let c = s.issues.add_comment(&actor, repo.id, number, &body.body, &client).await?;
    let view = s.issues.comment_view(&actor, repo.id, c.id).await?;
    Ok(created(ctx.comment(&view, number)))
}

async fn issue_number_of(s: &AppState, actor: &Actor, repo: ch_core::RepoId, view: &CommentView) -> Result<i64> {
    // A comment knows its issue row, not its number. Find the number through the repository.
    let n: Option<i64> = sqlx::query_scalar("SELECT number FROM issues WHERE id = $1 AND repo_id = $2")
        .bind(view.comment.issue_id)
        .bind(repo)
        .fetch_optional(&s.db)
        .await?;
    let _ = actor;
    n.ok_or(Error::NotFound)
}

async fn get_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.issues.comment_view(&actor, repo.id, CommentId(id)).await?;
    let n = issue_number_of(&s, &actor, repo.id, &v).await?;
    Ok(ok(ctx.comment(&v, n)))
}

async fn edit_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::CreateComment>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.issues.edit_comment(&actor, repo.id, CommentId(id), &body.body, &client).await?;
    let n = issue_number_of(&s, &actor, repo.id, &v).await?;
    Ok(ok(ctx.comment(&v, n)))
}

async fn delete_comment(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.delete_comment(&actor, repo.id, CommentId(id), &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- labels on an issue

async fn issue_labels(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.issues.view(&actor, repo.id, number).await?;
    Ok(ok(v.labels.iter().map(|l| ctx.label(l)).collect::<Vec<_>>()))
}

/// Both `["bug"]` and `{"labels": ["bug"]}` are accepted, like GitHub.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LabelsBody {
    List(Vec<LabelRef>),
    Object { labels: Vec<LabelRef> },
}

impl LabelsBody {
    fn names(self) -> Vec<String> {
        match self {
            LabelsBody::List(l) | LabelsBody::Object { labels: l } => label_names(l),
        }
    }
}

async fn add_labels(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<LabelsBody>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let labels = s.issues.add_labels(&actor, repo.id, number, body.names(), &client).await?;
    Ok(ok(labels.iter().map(|l| ctx.label(l)).collect::<Vec<_>>()))
}

async fn set_labels(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<LabelsBody>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let v = s.issues.update(&actor, repo.id, number, IssuePatch { labels: Some(body.names()), ..Default::default() }, &client).await?;
    Ok(ok(v.labels.iter().map(|l| ctx.label(l)).collect::<Vec<_>>()))
}

async fn clear_labels(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.update(&actor, repo.id, number, IssuePatch { labels: Some(Vec::new()), ..Default::default() }, &client).await?;
    Ok(no_content())
}

async fn remove_label(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number, label)): Path<(String, String, i64, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let labels = s.issues.remove_label(&actor, repo.id, number, &label, &client).await?;
    Ok(ok(labels.iter().map(|l| ctx.label(l)).collect::<Vec<_>>()))
}

// ---------------------------------------------------------------- assignees

#[derive(Debug, Default, Deserialize)]
struct AssigneesBody {
    #[serde(default)]
    assignees: Vec<String>,
}

async fn add_assignees(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<AssigneesBody>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let ids = user_ids(&s, &actor, &body.assignees, "assignees").await?;
    Ok(created(ctx.issue(&s.issues.add_assignees(&actor, repo.id, number, ids, &client).await?)))
}

async fn remove_assignees(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<AssigneesBody>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let ids = user_ids(&s, &actor, &body.assignees, "assignees").await?;
    Ok(ok(ctx.issue(&s.issues.remove_assignees(&actor, repo.id, number, ids, &client).await?)))
}

async fn list_assignees(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(s.issues.assignable(&actor, repo.id).await?.iter().map(|u| ctx.user(u)).collect::<Vec<_>>()))
}

async fn check_assignee(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, username)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let found = s.issues.assignable(&actor, repo.id).await?.iter().any(|u| u.username.eq_ignore_ascii_case(&username));
    if found { Ok(no_content()) } else { Err(Error::NotFound) }
}

// ---------------------------------------------------------------- events and timeline

async fn issue_events(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let t = s.issues.timeline(&actor, repo.id, number).await?;
    Ok(ok(t.iter().filter(|i| i.kind != "comment").filter_map(|i| ctx.issue_event(i)).collect::<Vec<_>>()))
}

async fn issue_timeline(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let t = s.issues.timeline(&actor, repo.id, number).await?;
    Ok(ok(t.iter().map(|i| ctx.timeline_item(i)).collect::<Vec<_>>()))
}

// ---------------------------------------------------------------- reactions

async fn issue_reactions(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(s.issues.reactions(&actor, repo.id, ReactionTarget::Issue(number)).await?.iter().map(|r| ctx.reaction(r)).collect::<Vec<_>>()))
}

async fn add_issue_reaction(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::CreateReaction>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(created(ctx.reaction(&s.issues.react(&actor, repo.id, ReactionTarget::Issue(number), &body.content, &client).await?)))
}

async fn del_issue_reaction(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.unreact(&actor, repo.id, ReactionTarget::Issue(number), id, &client).await?;
    Ok(no_content())
}

async fn comment_reactions(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(s
        .issues
        .reactions(&actor, repo.id, ReactionTarget::IssueComment(CommentId(id)))
        .await?
        .iter()
        .map(|r| ctx.reaction(r))
        .collect::<Vec<_>>()))
}

async fn add_comment_reaction(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<api::CreateReaction>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(created(ctx.reaction(&s.issues.react(&actor, repo.id, ReactionTarget::IssueComment(CommentId(id)), &body.content, &client).await?)))
}

async fn del_comment_reaction(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, id, rid)): Path<(String, String, i64, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.unreact(&actor, repo.id, ReactionTarget::IssueComment(CommentId(id)), rid, &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- subscription

async fn get_subscription(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(serde_json::json!({ "subscribed": s.issues.subscription(&actor, repo.id, number).await? })))
}

#[derive(Debug, Deserialize)]
struct SubscriptionBody {
    subscribed: bool,
}

async fn put_subscription(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<SubscriptionBody>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.set_subscription(&actor, repo.id, number, body.subscribed).await?;
    Ok(ok(serde_json::json!({ "subscribed": body.subscribed })))
}

async fn delete_subscription(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.set_subscription(&actor, repo.id, number, false).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- labels

async fn list_labels(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let all = s.issues.labels(&actor, repo.id).await?;
    let per = paging.per_page() as usize;
    let page: Vec<_> = all.iter().skip((paging.page() as usize - 1) * per).take(per).map(|l| ctx.label(l)).collect();
    Ok(page_of(page, &uri, base(&s), paging))
}

async fn create_label(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<api::CreateLabel>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let l = s
        .issues
        .create_label(&actor, repo.id, LabelDraft { name: body.name, color: Some(body.color), description: body.description }, &client)
        .await?;
    Ok(created(ctx.label(&l)))
}

async fn get_label(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, label)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(ctx.label(&s.issues.label(&actor, repo.id, &label).await?)))
}

async fn update_label(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, label)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<api::UpdateLabel>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let l = s
        .issues
        .update_label(
            &actor,
            repo.id,
            &label,
            LabelEdit { new_name: body.new_name, color: body.color, description: body.description },
            &client,
        )
        .await?;
    Ok(ok(ctx.label(&l)))
}

async fn delete_label(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, label)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.delete_label(&actor, repo.id, &label, &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- milestones

#[derive(Debug, Default, Deserialize)]
struct MilestoneQuery {
    state: Option<String>,
}

async fn list_milestones(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<MilestoneQuery>,
    Query(paging): Query<Paging>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let open = match q.state.as_deref() {
        Some("closed") => Some(false),
        Some("all") => None,
        _ => Some(true),
    };
    let all = s.issues.milestones(&actor, repo.id, open).await?;
    let per = paging.per_page() as usize;
    let page: Vec<_> = all.iter().skip((paging.page() as usize - 1) * per).take(per).map(|m| ctx.milestone(m)).collect();
    Ok(page_of(page, &uri, base(&s), paging))
}

async fn create_milestone(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<api::CreateMilestone>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let open = state_to_open(body.state.as_deref())?;
    let m = s
        .issues
        .create_milestone(
            &actor,
            repo.id,
            MilestoneDraft { title: body.title, description: body.description, due_on: body.due_on, open },
            &client,
        )
        .await?;
    Ok(created(ctx.milestone(&m)))
}

fn state_to_open(s: Option<&str>) -> Result<Option<bool>> {
    match s {
        None => Ok(None),
        Some("open") => Ok(Some(true)),
        Some("closed") => Ok(Some(false)),
        Some(_) => Err(Error::invalid("Milestone", "state", "state must be open or closed")),
    }
}

async fn get_milestone(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    Ok(ok(ctx.milestone(&s.issues.milestone(&actor, repo.id, number).await?)))
}

#[derive(Debug, Default, Deserialize)]
struct PatchMilestone {
    title: Option<String>,
    description: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    due_on: Option<Option<chrono::DateTime<chrono::Utc>>>,
    state: Option<String>,
}

async fn update_milestone(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
    ApiJson(body): ApiJson<PatchMilestone>,
) -> Result<Response> {
    let (repo, ctx) = repo_of(&s, &actor, &owner, &name).await?;
    let open = state_to_open(body.state.as_deref())?;
    let m = s
        .issues
        .update_milestone(
            &actor,
            repo.id,
            number,
            MilestoneEdit { title: body.title, description: body.description, due_on: body.due_on, open },
            &client,
        )
        .await?;
    Ok(ok(ctx.milestone(&m)))
}

async fn delete_milestone(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Client(client): Client,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    s.issues.delete_milestone(&actor, repo.id, number, &client).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------- templates

async fn list_templates(
    State(s): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, _) = repo_of(&s, &actor, &owner, &name).await?;
    let t = s.issues.templates(&actor, repo.id).await?;
    Ok(ok(t.into_iter().map(|t| serde_json::json!({ "name": t.name, "about": t.about, "title": t.title, "labels": t.labels, "body": t.body, "path": t.path })).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_filter_maps_github_params() {
        let q = ListQuery {
            state: Some("closed".into()),
            labels: Some("bug, good first issue".into()),
            assignee: Some("alice".into()),
            creator: Some("bob".into()),
            milestone: Some("none".into()),
            sort: Some("updated".into()),
            direction: Some("asc".into()),
            q: Some("is:issue crash".into()),
            ..Default::default()
        };
        let f = filter_of(&q, Paging { page: Some(2), per_page: Some(10) });
        assert_eq!(f.open, Some(false));
        assert_eq!(f.labels, vec!["bug", "good first issue"]);
        assert_eq!(f.sort.as_deref(), Some("updated-asc"));
        assert_eq!((f.page, f.per_page), (2, 10));
        let text = f.query.unwrap();
        assert!(text.contains("assignee:\"alice\""));
        assert!(text.contains("author:\"bob\""));
        assert!(text.contains("no:milestone"));
        assert!(text.contains("is:issue crash"));
    }

    #[test]
    fn default_state_is_open_and_all_means_both() {
        assert_eq!(filter_of(&ListQuery::default(), Paging::default()).open, Some(true));
        let q = ListQuery { state: Some("all".into()), ..Default::default() };
        assert_eq!(filter_of(&q, Paging::default()).open, None);
    }

    #[test]
    fn label_bodies_accept_both_shapes() {
        let a: LabelsBody = serde_json::from_str(r#"["bug","x"]"#).unwrap();
        assert_eq!(a.names(), vec!["bug", "x"]);
        let b: LabelsBody = serde_json::from_str(r#"{"labels":["bug",{"name":"y"}]}"#).unwrap();
        assert_eq!(b.names(), vec!["bug", "y"]);
    }

    #[test]
    fn patch_milestone_null_clears_the_date() {
        let p: PatchMilestone = serde_json::from_str(r#"{"due_on":null}"#).unwrap();
        assert_eq!(p.due_on, Some(None));
        let p: PatchMilestone = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.due_on, None);
    }
}
