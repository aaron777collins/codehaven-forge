//! The Actions (CI) pages: the run list, starting a workflow by hand, one run with its job
//! graph, one job with live logs over Server-Sent Events, artifact downloads, and the CI
//! settings pages (secrets, variables, runners).

use super::repo_common::{RepoCtx, branch_names};
use super::{FieldErrors, Pager, Params, Query};
use crate::chrome::Chrome;
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt::{self, StatusView, TimeView};
use crate::forms::FormData;
use askama::Template;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use ch_core::model::{CiAvailability, LogRange};
use ch_core::web_contract::{WorkflowDef, WorkflowJob, WorkflowStep};
use ch_core::{JobId, Permission, RepoId, RunId};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;

const PAGE: u32 = 30;
/// How much of a job log the job page renders inline. The rest is in the raw download.
const LOG_VIEW_LIMIT: u64 = 1_000_000;
const LOG_DOWNLOAD_LIMIT: u64 = 50_000_000;
/// A live log stream gives up after half an hour and tells the browser it is done.
const STREAM_CAP_SECS: u64 = 30 * 60;
const MAX_VALUE_LEN: usize = 64 * 1024;

// ------------------------------------------------------------------ pure helpers

/// A secret name after normalising: upper case, `[A-Z_][A-Z0-9_]*`.
pub fn secret_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_uppercase();
    let mut chars = name.chars();
    let first = chars.next()?;
    if name.len() > 100 || !(first == '_' || first.is_ascii_uppercase()) {
        return None;
    }
    if !chars.all(|c| c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return None;
    }
    Some(name)
}

/// Whether a run belongs in a filter chip. Cancelled runs only show under All.
pub fn run_matches(status: &str, conclusion: Option<&str>, filter: &str) -> bool {
    match filter {
        "running" => fmt::is_active(status),
        "failed" => matches!(conclusion, Some("failure" | "timed_out" | "error")),
        "passed" => matches!(conclusion, Some("success")),
        _ => true,
    }
}

/// Groups jobs into columns by dependency depth: a job with no needs is level 0, otherwise
/// one more than the deepest job it needs. Unknown needs are ignored. A cycle cannot hang
/// this: the relaxation stops after one pass per job and depths are capped.
pub fn depth_levels(jobs: &[(String, Vec<String>)]) -> Vec<Vec<usize>> {
    let n = jobs.len();
    let mut depth = vec![0usize; n];
    let idx: HashMap<&str, usize> = jobs.iter().enumerate().map(|(i, (name, _))| (name.as_str(), i)).collect();
    for _ in 0..n {
        let mut changed = false;
        for i in 0..n {
            let mut d = 0usize;
            let mut found = false;
            for need in &jobs[i].1 {
                if let Some(&j) = idx.get(need.as_str()) {
                    found = true;
                    d = d.max(depth[j] + 1);
                }
            }
            if found && d > depth[i] {
                depth[i] = d.min(n);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let columns = depth.iter().copied().max().unwrap_or(0) + 1;
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); columns];
    for (i, d) in depth.iter().enumerate() {
        out[*d].push(i);
    }
    out
}

/// A byte range of a raw log, clamped to what is actually there.
pub fn slice_log(raw: &[u8], from: u64, to: Option<u64>) -> &[u8] {
    let len = raw.len() as u64;
    let start = from.min(len) as usize;
    let end = to.unwrap_or(len).min(len) as usize;
    if end <= start { &[] } else { &raw[start..end] }
}

/// Log lines as one html string, numbered from `first_line`. Also returns the next line
/// number, so a following chunk or the live stream can carry on counting.
pub fn log_html(raw: &[u8], first_line: u64) -> (String, u64) {
    let text = String::from_utf8_lossy(raw);
    let mut n = first_line;
    let mut out = String::new();
    for html in ch_render::ansi::to_lines(&text) {
        out.push_str(&format!(
            "<div class=\"log-line\" id=\"L{n}\"><a class=\"log-ln\" href=\"#L{n}\">{n}</a><span class=\"log-text\">{html}</span></div>\n"
        ));
        n += 1;
    }
    (out, n)
}

fn step_open(status: &str, conclusion: Option<&str>) -> bool {
    fmt::is_active(status) || matches!(conclusion, Some("failure" | "timed_out" | "error"))
}

/// A step counts as started once the runner has picked it up. Until then it owns no log
/// bytes at all, whatever its offsets say.
fn step_started(s: &WorkflowStep) -> bool {
    !matches!(s.status.as_str(), "queued" | "pending" | "waiting")
}

/// `queued`, `running` or `done`: the coarse state the page and the `steps` events speak.
fn step_state(s: &WorkflowStep) -> &'static str {
    if !step_started(s) {
        "queued"
    } else if fmt::is_active(&s.status) {
        "running"
    } else {
        "done"
    }
}

/// The steps of a job as collapsible sections. A started step shows the slice of the raw log
/// between its byte offsets, numbered as one continuous log; a queued step shows the same
/// panel empty. Every panel carries its step number (`data-step-log`), and the stream
/// routes new bytes into the panel whose step owns them, so no step is special here.
pub fn step_views(raw: &[u8], steps: &[WorkflowStep]) -> (Vec<StepView>, u64) {
    let mut line: u64 = 1;
    let mut out = Vec::with_capacity(steps.len());
    for s in steps {
        let started = step_started(s);
        let chunk: &[u8] = if started { slice_log(raw, s.log_from, s.log_to) } else { &[] };
        let (html, next) = log_html(chunk, line);
        line = next;
        let state = step_state(s);
        out.push(StepView {
            number: s.number,
            name: if s.name.is_empty() { format!("Step {}", s.number) } else { s.name.clone() },
            state: state.to_string(),
            status: fmt::status(&s.status, s.conclusion.as_deref()),
            duration: if started { fmt::duration_between(s.started_at, s.completed_at) } else { None },
            started_ms: if state == "running" { s.started_at.map(|t| t.timestamp_millis()) } else { None },
            open: step_open(&s.status, s.conclusion.as_deref()),
            html,
            empty: chunk.is_empty(),
        });
    }
    (out, line)
}

/// New log bytes, split into the step each byte belongs to. A started step owns
/// `[log_from, log_to)`, and a step still writing owns to the end. The second return says
/// how many bytes the caller may treat as read: while the job is active, a trailing stretch
/// no started step owns yet is held back, cut at a newline, so the next poll routes it once
/// the step metadata catches up. Once the job is inactive everything is consumed: nobody
/// will ever claim those bytes, and the next poll must not re-read them.
pub fn route_log(steps: &[WorkflowStep], from: u64, bytes: &[u8], job_active: bool) -> (Vec<(u32, Vec<u8>)>, u64) {
    let len = bytes.len() as u64;
    let to = from.saturating_add(len);
    // The frontier of settled log: everything a started step has claimed, up to the new bytes.
    let mut settled = from;
    for s in steps.iter().filter(|s| step_started(s)) {
        settled = settled.max(s.log_to.unwrap_or(u64::MAX).min(to));
    }
    let consumed = if !job_active || settled >= to {
        len
    } else {
        let cut = &bytes[..(settled - from) as usize];
        cut.iter().rposition(|&b| b == b'\n').map_or(0, |i| i as u64 + 1)
    };
    let end = from + consumed;
    let mut out = Vec::new();
    for s in steps.iter().filter(|s| step_started(s)) {
        let lo = s.log_from.max(from);
        let hi = s.log_to.unwrap_or(u64::MAX).min(end);
        if hi > lo {
            out.push((s.number, bytes[(lo - from) as usize..(hi - from) as usize].to_vec()));
        }
    }
    (out, consumed)
}

/// The per-step summary a `steps` event carries: everything the page updates without a
/// reload. Serialised once so it can also be compared against the previous poll's string.
fn step_summary(steps: &[WorkflowStep]) -> String {
    let items: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            let sv = fmt::status(&s.status, s.conclusion.as_deref());
            serde_json::json!({
                "n": s.number,
                "state": step_state(s),
                "class": sv.class,
                "icon": sv.icon,
                "label": sv.label,
                "duration": fmt::duration_between(s.started_at, s.completed_at),
                "started_ms": if step_state(s) == "running" { s.started_at.map(|t| t.timestamp_millis()) } else { None },
                "open": step_open(&s.status, s.conclusion.as_deref()),
            })
        })
        .collect();
    serde_json::Value::Array(items).to_string()
}

/// The wall time of a run: from the first job that started to the last one that finished.
pub fn run_duration(jobs: &[WorkflowJob]) -> Option<String> {
    let first = jobs.iter().filter_map(|j| j.started_at).min()?;
    let last = jobs.iter().filter_map(|j| j.completed_at).max();
    fmt::duration_between(Some(first), last)
}

/// The chip a runner status gets. Anything unknown reads as offline.
fn runner_chip(status: &str) -> (&'static str, &'static str) {
    match status {
        "online" => ("chip-ok", "Online"),
        "busy" => ("chip-warn", "Busy"),
        _ => ("chip-muted", "Offline"),
    }
}

/// A file name a Content-Disposition header can carry.
pub fn safe_filename(name: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    let trimmed = clean.trim_matches('.').to_string();
    if trimmed.is_empty() { "artifact".into() } else { trimmed }
}

fn workflow_views(defs: &[WorkflowDef]) -> Vec<WorkflowView> {
    defs.iter()
        .map(|w| WorkflowView {
            name: if w.name.is_empty() { w.path.rsplit('/').next().unwrap_or(&w.path).to_string() } else { w.name.clone() },
            triggers: w.triggers.join(", "),
            error: w.error.clone(),
            can_dispatch: w.can_dispatch,
        })
        .collect()
}

fn filter_chips(base: &str, current: &str, counts: [usize; 4]) -> Vec<ChipView> {
    let specs = [("all", "All"), ("running", "Running"), ("failed", "Failed"), ("passed", "Passed")];
    specs
        .iter()
        .enumerate()
        .map(|(i, (key, label))| ChipView {
            label: (*label).to_string(),
            href: if *key == "all" { format!("{base}/actions") } else { format!("{base}/actions?status={key}") },
            current: current == *key,
            count: counts[i],
        })
        .collect()
}

pub struct TabLink {
    pub name: String,
    pub href: String,
    pub current: bool,
}

/// The tab bar the repo settings pages share until the shared macro lands.
fn settings_tabs(base: &str, current: &str) -> Vec<TabLink> {
    vec![
        TabLink { name: "Settings".into(), href: format!("{base}/settings"), current: current == "settings" },
        TabLink { name: "Secrets".into(), href: format!("{base}/settings/secrets"), current: current == "secrets" },
        TabLink { name: "Runners".into(), href: format!("{base}/settings/runners"), current: current == "runners" },
    ]
}

/// What to tell someone when workflows cannot run here. Empty when they can.
pub fn ci_notice(owner: &str, availability: CiAvailability) -> String {
    match availability {
        CiAvailability::Enabled => String::new(),
        CiAvailability::OwnerNotAllowed => format!(
            "Workflows cannot run here yet: {owner} is not allowed to run them. A site admin can allow it from the admin pages. Until then pushes start nothing and Run workflow is refused."
        ),
        CiAvailability::TurnedOff => "Actions is turned off for this repository, so workflows do not run.".to_string(),
    }
}

// ------------------------------------------------------------------ view structs

pub struct WorkflowView {
    pub name: String,
    pub triggers: String,
    pub error: Option<String>,
    pub can_dispatch: bool,
}

pub struct ChipView {
    pub label: String,
    pub href: String,
    pub current: bool,
    pub count: usize,
}

pub struct RunRow {
    pub href: String,
    pub status: StatusView,
    pub title: String,
    pub branch: Option<String>,
    pub branch_href: String,
    pub short: String,
    pub commit_href: String,
    pub when: TimeView,
}

#[derive(Template)]
#[template(path = "actions/runs.html")]
struct RunsPage {
    c: Chrome,
    notice: String,
    run_workflow_href: String,
    can_write: bool,
    any_dispatch: bool,
    workflows: Vec<WorkflowView>,
    chips: Vec<ChipView>,
    rows: Vec<RunRow>,
    pager: Pager,
}

pub struct WorkflowPick {
    pub path: String,
    pub name: String,
    pub current: bool,
}

pub struct BranchOpt {
    pub name: String,
    pub current: bool,
}

pub struct InputField {
    pub label: String,
    pub required: bool,
    pub default: String,
    pub description: String,
}

#[derive(Template)]
#[template(path = "actions/dispatch.html")]
struct DispatchPage {
    c: Chrome,
    notice: String,
    action: String,
    pick_action: String,
    selected_path: String,
    has_workflows: bool,
    workflows: Vec<WorkflowPick>,
    inputs: Vec<InputField>,
    branches: Vec<BranchOpt>,
    error: String,
}

pub struct JobBox {
    pub href: String,
    pub name: String,
    pub status: StatusView,
    pub duration: Option<String>,
}

pub struct JobCol {
    pub boxes: Vec<JobBox>,
}

pub struct ArtifactRow {
    pub name: String,
    pub size: String,
    pub when: TimeView,
    pub href: String,
    pub expired: bool,
}

#[derive(Template)]
#[template(path = "actions/run.html")]
struct RunPage {
    c: Chrome,
    number: i64,
    event: String,
    status: StatusView,
    branch: Option<String>,
    branch_href: String,
    short: String,
    commit_href: String,
    when: TimeView,
    duration: Option<String>,
    show_cancel: bool,
    show_rerun: bool,
    show_approve: bool,
    hold_reason: Option<String>,
    approve_action: String,
    cancel_action: String,
    rerun_action: String,
    columns: Vec<JobCol>,
    has_graph: bool,
    artifacts: Vec<ArtifactRow>,
}

pub struct StepView {
    pub number: u32,
    pub name: String,
    pub state: String,
    pub status: StatusView,
    pub duration: Option<String>,
    /// Unix milliseconds, for the browser's ticking elapsed timer. Running steps only.
    pub started_ms: Option<i64>,
    pub open: bool,
    pub html: String,
    pub empty: bool,
}

pub struct JobLink {
    pub href: String,
    pub name: String,
    pub status: StatusView,
    pub current: bool,
}

#[derive(Template)]
#[template(path = "actions/job.html")]
struct JobPage {
    c: Chrome,
    job_name: String,
    status: StatusView,
    active: bool,
    duration: Option<String>,
    /// Unix milliseconds, for the ticking job elapsed timer. Active, started jobs only.
    job_started_ms: Option<i64>,
    runner: Option<String>,
    steps: Vec<StepView>,
    jobs: Vec<JobLink>,
    log_txt_href: String,
    stream_url: String,
    offset: u64,
    line: u64,
    run_href: String,
    run_number: String,
}

pub struct SecretRow {
    pub name: String,
    pub when: TimeView,
    pub delete_action: String,
}

pub struct VarRow {
    pub name: String,
    pub value: String,
    pub when: TimeView,
    pub delete_action: String,
}

#[derive(Template)]
#[template(path = "actions/secrets.html")]
struct SecretsPage {
    c: Chrome,
    tabs: Vec<TabLink>,
    action: String,
    secrets: Vec<SecretRow>,
    vars: Vec<VarRow>,
    error: String,
    name_value: String,
}

pub struct RunnerRow {
    pub name: String,
    pub labels: Vec<String>,
    pub chip_class: &'static str,
    pub chip_label: &'static str,
    pub version: String,
    pub last_seen: Option<TimeView>,
}

#[derive(Template)]
#[template(path = "actions/runners.html")]
struct RunnersPage {
    c: Chrome,
    tabs: Vec<TabLink>,
    rows: Vec<RunnerRow>,
    admin_href: String,
}

// ------------------------------------------------------------------ the runs list

pub async fn runs(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let base = rc.base();
    let page: u32 = q.get("page").and_then(|p| p.parse().ok()).filter(|p| *p >= 1).unwrap_or(1);
    let filter = match q.get("status").map(String::as_str) {
        Some("running") => "running",
        Some("failed") => "failed",
        Some("passed") => "passed",
        _ => "all",
    };
    let mut list = ctx.state.ci.runs(&ctx.actor, rc.id(), page, PAGE + 1).await?;
    let more = list.len() as u32 > PAGE;
    list.truncate(PAGE as usize);
    let defs = ctx.state.pipelines.workflows(&ctx.actor, rc.id(), &rc.repo.default_branch).await.unwrap_or_default();
    let workflows = workflow_views(&defs);
    let counts = [
        list.len(),
        list.iter().filter(|r| run_matches(&r.status, r.conclusion.as_deref(), "running")).count(),
        list.iter().filter(|r| run_matches(&r.status, r.conclusion.as_deref(), "failed")).count(),
        list.iter().filter(|r| run_matches(&r.status, r.conclusion.as_deref(), "passed")).count(),
    ];
    let rows: Vec<RunRow> = list
        .iter()
        .filter(|r| run_matches(&r.status, r.conclusion.as_deref(), filter))
        .map(|r| RunRow {
            href: format!("{base}/actions/runs/{}", r.id),
            status: fmt::status(&r.status, r.conclusion.as_deref()),
            title: format!("#{} on {}", r.run_number, r.event),
            branch: r.head_branch.clone(),
            branch_href: format!("{base}/tree/{}", fmt::enc_path(r.head_branch.as_deref().unwrap_or(""))),
            short: fmt::short_sha(&r.head_sha),
            commit_href: format!("{base}/commit/{}", r.head_sha),
            when: fmt::time(r.created_at),
        })
        .collect();
    let here = if filter == "all" { format!("{base}/actions") } else { format!("{base}/actions?status={filter}") };
    let c = rc.chrome(&ctx, "Actions", "actions").await;
    let notice = ci_notice(&rc.repo.owner_name, ctx.state.ci.availability(rc.id()).await.unwrap_or(CiAvailability::Enabled));
    ctx.page(&RunsPage {
        c,
        notice,
        run_workflow_href: format!("{base}/actions/run"),
        can_write: rc.perm.allows(Permission::Write) && !rc.repo.archived,
        any_dispatch: workflows.iter().any(|w| w.can_dispatch),
        workflows,
        chips: filter_chips(&base, filter, counts),
        rows,
        pager: Pager::new(&here, page, more),
    })
}

// ------------------------------------------------------------------ manual dispatch

pub async fn dispatch_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Query<Params>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let selected = q.get("workflow").cloned().unwrap_or_default();
    let git_ref = q.get("ref").cloned().unwrap_or_else(|| rc.repo.default_branch.clone());
    dispatch_page(&ctx, &rc, &selected, &git_ref, &HashMap::new(), "", StatusCode::OK).await
}

pub async fn dispatch_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let path = form.get("workflow_path").trim().to_string();
    let git_ref = form.opt("ref").unwrap_or(&rc.repo.default_branch).trim().to_string();
    let availability = ctx.state.ci.availability(rc.id()).await.unwrap_or(CiAvailability::Enabled);
    if availability != CiAvailability::Enabled {
        return dispatch_page(&ctx, &rc, &path, &git_ref, &HashMap::new(), "Nothing was started.", StatusCode::FORBIDDEN).await;
    }
    let defs = ctx.state.pipelines.workflows(&ctx.actor, rc.id(), &rc.repo.default_branch).await.unwrap_or_default();
    let Some(wf) = defs.iter().find(|w| w.path == path && w.can_dispatch) else {
        let msg = "Pick a workflow that can be started by hand.";
        return dispatch_page(&ctx, &rc, &path, &git_ref, &HashMap::new(), msg, StatusCode::UNPROCESSABLE_ENTITY).await;
    };
    let entered: HashMap<String, String> =
        wf.inputs.iter().filter_map(|i| form.opt(&format!("input_{}", i.name)).map(|v| (i.name.clone(), v.to_string()))).collect();
    if git_ref.is_empty() {
        return dispatch_page(
            &ctx,
            &rc,
            &path,
            &git_ref,
            &entered,
            "Pick a branch to run the workflow on.",
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let mut inputs = serde_json::Map::new();
    let mut missing: Vec<String> = Vec::new();
    for i in &wf.inputs {
        match form.opt(&format!("input_{}", i.name)) {
            Some(v) => {
                inputs.insert(i.name.clone(), serde_json::Value::String(v.to_string()));
            }
            None if i.required => missing.push(i.name.clone()),
            None => {}
        }
    }
    if !missing.is_empty() {
        return dispatch_page(
            &ctx,
            &rc,
            &path,
            &git_ref,
            &entered,
            &format!("Please fill in {}.", missing.join(", ")),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    match ctx.state.ci.dispatch(&ctx.actor, rc.id(), &path, &git_ref, serde_json::Value::Object(inputs), &ctx.client).await {
        Ok(run) => {
            Ok(ctx.redirect_flash(&format!("{}/actions/runs/{run}", rc.base()), "ok", &format!("Started {} on {git_ref}.", wf.name)))
        }
        Err(e) => match FieldErrors::from_error(&e) {
            Some(errs) => {
                let msg = if errs.general.is_empty() { "Check the form and try again.".to_string() } else { errs.general };
                dispatch_page(&ctx, &rc, &path, &git_ref, &entered, &msg, StatusCode::UNPROCESSABLE_ENTITY).await
            }
            None => Err(e.into()),
        },
    }
}

async fn dispatch_page(
    ctx: &Ctx,
    rc: &RepoCtx,
    selected: &str,
    git_ref: &str,
    entered: &HashMap<String, String>,
    error: &str,
    status: StatusCode,
) -> WebResult<Response> {
    let defs = ctx.state.pipelines.workflows(&ctx.actor, rc.id(), &rc.repo.default_branch).await.unwrap_or_default();
    let dispatchable: Vec<&WorkflowDef> = defs.iter().filter(|w| w.can_dispatch).collect();
    let chosen = dispatchable.iter().find(|w| w.path == selected).or_else(|| dispatchable.first()).copied();
    let branches = branch_names(ctx, rc.id()).await.unwrap_or_default();
    let inputs: Vec<InputField> = chosen
        .map(|w| {
            w.inputs
                .iter()
                .map(|i| InputField {
                    label: i.name.clone(),
                    required: i.required,
                    default: entered.get(&i.name).cloned().or_else(|| i.default.clone()).unwrap_or_default(),
                    description: i.description.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let workflows: Vec<WorkflowPick> = dispatchable
        .iter()
        .map(|w| WorkflowPick {
            path: w.path.clone(),
            name: if w.name.is_empty() { w.path.clone() } else { w.name.clone() },
            current: chosen.is_some_and(|c| c.path == w.path),
        })
        .collect();
    let c = rc.chrome(ctx, "Run a workflow", "actions").await;
    let notice = ci_notice(&rc.repo.owner_name, ctx.state.ci.availability(rc.id()).await.unwrap_or(CiAvailability::Enabled));
    ctx.page_status(
        status,
        &DispatchPage {
            c,
            notice,
            action: format!("{}/actions/run", rc.base()),
            pick_action: format!("{}/actions/run", rc.base()),
            selected_path: chosen.map(|w| w.path.clone()).unwrap_or_default(),
            has_workflows: !workflows.is_empty(),
            workflows,
            inputs,
            branches: branches.iter().map(|b| BranchOpt { name: b.clone(), current: b == git_ref }).collect(),
            error: error.to_string(),
        },
    )
}

// ------------------------------------------------------------------ one run

pub async fn run_get(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let base = rc.base();
    let info = ctx.state.ci.get_run(&ctx.actor, rc.id(), RunId(run)).await?;
    let jobs = ctx.state.pipelines.jobs(&ctx.actor, rc.id(), RunId(run)).await?;
    let artifacts = ctx.state.pipelines.artifacts(&ctx.actor, rc.id(), RunId(run)).await?;
    let active = fmt::is_active(&info.status);
    let can_write = rc.perm.allows(Permission::Write) && !rc.repo.archived;

    let pairs: Vec<(String, Vec<String>)> = jobs.iter().map(|j| (j.name.clone(), j.needs.clone())).collect();
    let columns: Vec<JobCol> = depth_levels(&pairs)
        .into_iter()
        .map(|idxs| JobCol {
            boxes: idxs
                .iter()
                .map(|&i| {
                    let j = &jobs[i];
                    JobBox {
                        href: format!("{base}/actions/runs/{run}/jobs/{}", j.id),
                        name: j.name.clone(),
                        status: fmt::status(&j.status, j.conclusion.as_deref()),
                        duration: fmt::duration_between(j.started_at, j.completed_at),
                    }
                })
                .collect(),
        })
        .collect();

    let waiting = info.status == "waiting";
    let hold_reason = if waiting { ctx.state.ci.run_hold_reason(&ctx.actor, rc.id(), RunId(run)).await.ok().flatten() } else { None };
    let c = rc.chrome(&ctx, &format!("Run #{}", info.run_number), "actions").await;
    ctx.page(&RunPage {
        c,
        number: info.run_number,
        event: info.event.clone(),
        status: if waiting {
            fmt::StatusView { class: "queued".into(), icon: "circle-dashed".into(), label: "Waiting for approval".into() }
        } else {
            fmt::status(&info.status, info.conclusion.as_deref())
        },
        branch: info.head_branch.clone(),
        branch_href: format!("{base}/tree/{}", fmt::enc_path(info.head_branch.as_deref().unwrap_or(""))),
        short: fmt::short_sha(&info.head_sha),
        commit_href: format!("{base}/commit/{}", info.head_sha),
        when: fmt::time(info.created_at),
        duration: run_duration(&jobs),
        show_cancel: can_write && active,
        show_rerun: can_write && !active,
        show_approve: can_write && waiting,
        hold_reason,
        approve_action: format!("{base}/actions/runs/{run}/approve"),
        cancel_action: format!("{base}/actions/runs/{run}/cancel"),
        rerun_action: format!("{base}/actions/runs/{run}/rerun"),
        has_graph: !columns.is_empty(),
        columns,
        artifacts: artifacts
            .iter()
            .map(|a| ArtifactRow {
                name: a.name.clone(),
                size: ch_render::human_bytes(a.size),
                when: fmt::time(a.created_at),
                href: format!("{base}/actions/runs/{run}/artifacts/{}", a.id),
                expired: a.expired,
            })
            .collect(),
    })
}

pub async fn run_cancel(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let _ = form;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    ctx.state.ci.cancel(&ctx.actor, rc.id(), RunId(run), &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/actions/runs/{run}", rc.base()), "ok", "Cancelling the run."))
}

/// A person with write access lets a held run go ahead: a pull request from a fork, or a run an
/// app started. The service refuses anything but a person, and the approval is audited.
pub async fn run_approve(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let _ = form;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    ctx.state.ci.approve_run(&ctx.actor, rc.id(), RunId(run), &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/actions/runs/{run}", rc.base()), "ok", "Run approved. It will start when a runner is free."))
}

pub async fn run_rerun(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require_writable(Permission::Write)?;
    let failed_only = form.get("failed_only") == "1";
    let new_run = ctx.state.ci.rerun(&ctx.actor, rc.id(), RunId(run), failed_only, &ctx.client).await?;
    Ok(ctx.redirect_flash(
        &format!("{}/actions/runs/{new_run}", rc.base()),
        "ok",
        if failed_only { "Started the failed jobs again." } else { "Started the run again." },
    ))
}

pub async fn artifact_download(ctx: Ctx, Path((owner, name, run, id)): Path<(String, String, i64, i64)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let list = ctx.state.pipelines.artifacts(&ctx.actor, rc.id(), RunId(run)).await?;
    let Some(a) = list.iter().find(|a| a.id == id) else { return Err(WebError::not_found()) };
    if a.expired {
        return Err(WebError::not_found());
    }
    let stream = ctx.state.blobs.get_stream(&a.sha256).await?;
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", safe_filename(&a.name))) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(r)
}

// ------------------------------------------------------------------ one job

pub async fn job_get(ctx: Ctx, Path((owner, name, run, job)): Path<(String, String, i64, i64)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let base = rc.base();
    let jobs = ctx.state.pipelines.jobs(&ctx.actor, rc.id(), RunId(run)).await?;
    let Some(j) = jobs.iter().find(|j| j.id == JobId(job)) else { return Err(WebError::not_found()) };
    let info = ctx.state.ci.get_run(&ctx.actor, rc.id(), RunId(run)).await?;
    let raw =
        ctx.state.ci.log_range(&ctx.actor, rc.id(), JobId(job), LogRange { from: 0, to: Some(LOG_VIEW_LIMIT) }).await.unwrap_or_default();
    let active = fmt::is_active(&j.status);
    let (steps, line) = step_views(&raw, &j.steps);
    let links: Vec<JobLink> = jobs
        .iter()
        .map(|jj| JobLink {
            href: format!("{base}/actions/runs/{run}/jobs/{}", jj.id),
            name: jj.name.clone(),
            status: fmt::status(&jj.status, jj.conclusion.as_deref()),
            current: jj.id == JobId(job),
        })
        .collect();
    let c = rc.chrome(&ctx, &j.name, "actions").await;
    ctx.page(&JobPage {
        c,
        job_name: j.name.clone(),
        status: fmt::status(&j.status, j.conclusion.as_deref()),
        active,
        duration: fmt::duration_between(j.started_at, j.completed_at),
        job_started_ms: if active { j.started_at.map(|t| t.timestamp_millis()) } else { None },
        runner: j.runner_name.clone(),
        steps,
        jobs: links,
        log_txt_href: format!("{base}/actions/runs/{run}/jobs/{job}/log.txt"),
        stream_url: format!("{base}/actions/runs/{run}/jobs/{job}/log/stream"),
        offset: raw.len() as u64,
        line,
        run_href: format!("{base}/actions/runs/{run}"),
        run_number: info.run_number.to_string(),
    })
}

pub async fn job_log_txt(ctx: Ctx, Path((owner, name, run, job)): Path<(String, String, i64, i64)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let raw = ctx.state.ci.log_range(&ctx.actor, rc.id(), JobId(job), LogRange { from: 0, to: Some(LOG_DOWNLOAD_LIMIT) }).await?;
    let mut r = (StatusCode::OK, raw).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"run-{run}-job-{job}.log\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(r)
}

// ------------------------------------------------------------------ the live log stream

struct LogStream {
    ctx: Ctx,
    repo: RepoId,
    run: RunId,
    job: JobId,
    offset: u64,
    line: u64,
    /// The steps summary as sent last time. The first poll always differs (it starts empty),
    /// so the page gets one `steps` event straight away.
    last_steps: String,
    /// Events a poll produced but has not sent yet: one poll can yield several `log` events
    /// (one per step) plus a `steps` event, and they go out in order on the calls that follow.
    pending: VecDeque<Event>,
    /// The job has finished: send `done` on the next poll, then end.
    closing: bool,
    /// `done` has been sent: end on the next poll.
    finished: bool,
    deadline: std::time::Instant,
}

/// One poll of the log and the job metadata. New bytes are routed to the steps that own
/// them (`log` events, one per step, each carrying the byte offset after its segment as
/// the event id) and a `steps` event goes out whenever the per-step summary changed. A
/// `done` event ends the stream once the job is no longer active or the cap is hit; any
/// error also ends it with `done`, so the browser never waits for nothing.
async fn next_event(mut st: LogStream) -> Option<(Result<Event, Infallible>, LogStream)> {
    if st.finished {
        return None;
    }
    if let Some(ev) = st.pending.pop_front() {
        return Some((Ok(ev), st));
    }
    let done = || Event::default().event("done").data("done");
    if st.closing || std::time::Instant::now() >= st.deadline {
        st.finished = true;
        return Some((Ok(done()), st));
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let bytes = match st.ctx.state.ci.log_range(&st.ctx.actor, st.repo, st.job, LogRange { from: st.offset, to: None }).await {
        Ok(b) => b,
        Err(_) => {
            st.finished = true;
            return Some((Ok(done()), st));
        }
    };
    let (steps, active) = match st.ctx.state.pipelines.jobs(&st.ctx.actor, st.repo, st.run).await {
        Ok(js) => match js.iter().find(|j| j.id == st.job) {
            Some(j) => (j.steps.clone(), fmt::is_active(&j.status)),
            // The job vanished from the list: treat it as over so the page reloads.
            None => (Vec::new(), false),
        },
        // A blip talking to the jobs stream must not cut the log short: hold the bytes
        // back (nobody owns them without step metadata) and try again next poll.
        Err(_) => (Vec::new(), true),
    };
    let (segments, consumed) = route_log(&steps, st.offset, &bytes, active);
    let mut at = st.offset;
    for (number, seg) in segments {
        at += seg.len() as u64;
        let (html, next) = log_html(&seg, st.line);
        st.line = next;
        st.pending.push_back(
            Event::default().event("log").id(at.to_string()).data(serde_json::json!({ "step": number, "html": html }).to_string()),
        );
    }
    st.offset += consumed;
    let summary = step_summary(&steps);
    if summary != st.last_steps {
        st.last_steps = summary.clone();
        st.pending.push_back(Event::default().event("steps").data(summary));
    }
    if !active {
        st.closing = true;
    }
    if st.pending.is_empty() {
        st.pending.push_back(Event::default().comment("tick"));
    }
    let ev = st.pending.pop_front().unwrap_or_else(|| Event::default().comment("tick"));
    Some((Ok(ev), st))
}

pub async fn job_stream(
    ctx: Ctx,
    Path((owner, name, run, job)): Path<(String, String, i64, i64)>,
    Query(q): Query<Params>,
) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    let offset: u64 = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let line: u64 = q.get("line").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let st = LogStream {
        ctx: ctx.clone(),
        repo: rc.id(),
        run: RunId(run),
        job: JobId(job),
        offset,
        line,
        last_steps: String::new(),
        pending: VecDeque::new(),
        closing: false,
        finished: false,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(STREAM_CAP_SECS),
    };
    let stream = futures::stream::unfold(st, next_event);
    let mut r =
        Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("keepalive")).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

// ------------------------------------------------------------------ CI settings

async fn secrets_page(ctx: &Ctx, rc: &RepoCtx, error: &str, name_value: &str, status: StatusCode) -> WebResult<Response> {
    let secrets = ctx.state.pipelines.secrets(&ctx.actor, rc.id()).await?;
    let vars = ctx.state.pipelines.variables(&ctx.actor, rc.id()).await?;
    let base = rc.base();
    let c = rc.chrome(ctx, "Secrets and variables", "settings").await;
    ctx.page_status(
        status,
        &SecretsPage {
            c,
            tabs: settings_tabs(&base, "secrets"),
            action: format!("{base}/settings/secrets"),
            secrets: secrets
                .iter()
                .map(|s| SecretRow {
                    name: s.name.clone(),
                    when: fmt::time(s.updated_at),
                    delete_action: format!("{base}/settings/secrets/{}/delete", fmt::enc(&s.name)),
                })
                .collect(),
            vars: vars
                .iter()
                .map(|v| VarRow {
                    name: v.name.clone(),
                    value: v.value.clone(),
                    when: fmt::time(v.updated_at),
                    delete_action: format!("{base}/settings/variables/{}/delete", fmt::enc(&v.name)),
                })
                .collect(),
            error: error.to_string(),
            name_value: name_value.to_string(),
        },
    )
}

pub async fn secrets_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    secrets_page(&ctx, &rc, "", "", StatusCode::OK).await
}

/// One endpoint for both forms on the page: the hidden `kind` field says which.
pub async fn secrets_post(ctx: Ctx, Path((owner, name)): Path<(String, String)>, form: FormData) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let kind = form.get("kind");
    let name_in = form.get("name").trim().to_string();
    let value = form.get("value");
    if !matches!(kind, "secret" | "variable") {
        return Err(WebError::bad("That form is not one we know."));
    }
    if value.len() > MAX_VALUE_LEN {
        return secrets_page(&ctx, &rc, "That value is too long. Keep it under 64 KB.", &name_in, StatusCode::UNPROCESSABLE_ENTITY).await;
    }
    if kind == "secret" {
        let Some(n) = secret_name(&name_in) else {
            let msg = "Secret names use upper case letters, digits and underscores, and start with a letter or underscore.";
            return secrets_page(&ctx, &rc, msg, &name_in, StatusCode::UNPROCESSABLE_ENTITY).await;
        };
        if value.trim().is_empty() {
            return secrets_page(&ctx, &rc, "Put a value in.", &n, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
        match ctx.state.pipelines.set_secret(&ctx.actor, rc.id(), &n, value, &ctx.client).await {
            Ok(()) => Ok(ctx.redirect_flash(&format!("{}/settings/secrets", rc.base()), "ok", &format!("Saved the secret {n}."))),
            Err(e) => match FieldErrors::from_error(&e) {
                Some(errs) => {
                    secrets_page(
                        &ctx,
                        &rc,
                        if errs.general.is_empty() { "Check the form and try again." } else { &errs.general },
                        &n,
                        StatusCode::UNPROCESSABLE_ENTITY,
                    )
                    .await
                }
                None => Err(e.into()),
            },
        }
    } else {
        if name_in.is_empty() || name_in.len() > 100 {
            return secrets_page(&ctx, &rc, "Give the variable a name up to 100 characters.", &name_in, StatusCode::UNPROCESSABLE_ENTITY)
                .await;
        }
        if value.trim().is_empty() {
            return secrets_page(&ctx, &rc, "Put a value in.", &name_in, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
        match ctx.state.pipelines.set_variable(&ctx.actor, rc.id(), &name_in, value, &ctx.client).await {
            Ok(()) => Ok(ctx.redirect_flash(&format!("{}/settings/secrets", rc.base()), "ok", &format!("Saved the variable {name_in}."))),
            Err(e) => match FieldErrors::from_error(&e) {
                Some(errs) => {
                    secrets_page(
                        &ctx,
                        &rc,
                        if errs.general.is_empty() { "Check the form and try again." } else { &errs.general },
                        &name_in,
                        StatusCode::UNPROCESSABLE_ENTITY,
                    )
                    .await
                }
                None => Err(e.into()),
            },
        }
    }
}

pub async fn secret_delete(ctx: Ctx, Path((owner, name, secret)): Path<(String, String, String)>, form: FormData) -> WebResult<Response> {
    let _ = form;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    ctx.state.pipelines.delete_secret(&ctx.actor, rc.id(), &secret, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/secrets", rc.base()), "ok", &format!("Deleted the secret {secret}.")))
}

pub async fn variable_delete(
    ctx: Ctx,
    Path((owner, name, variable)): Path<(String, String, String)>,
    form: FormData,
) -> WebResult<Response> {
    let _ = form;
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    ctx.state.pipelines.delete_variable(&ctx.actor, rc.id(), &variable, &ctx.client).await?;
    Ok(ctx.redirect_flash(&format!("{}/settings/secrets", rc.base()), "ok", &format!("Deleted the variable {variable}.")))
}

pub async fn runners_get(ctx: Ctx, Path((owner, name)): Path<(String, String)>) -> WebResult<Response> {
    let rc = RepoCtx::load(&ctx, &owner, &name).await?;
    rc.require(Permission::Admin)?;
    let list = ctx.state.pipelines.runners(&ctx.actor, Some(rc.id())).await?;
    let base = rc.base();
    let c = rc.chrome(&ctx, "Runners", "settings").await;
    ctx.page(&RunnersPage {
        c,
        tabs: settings_tabs(&base, "runners"),
        rows: list
            .iter()
            .map(|r| {
                let (chip_class, chip_label) = runner_chip(&r.status);
                RunnerRow {
                    name: r.name.clone(),
                    labels: r.labels.clone(),
                    chip_class,
                    chip_label,
                    version: r.version.clone(),
                    last_seen: fmt::time_opt(r.last_seen_at),
                }
            })
            .collect(),
        admin_href: "/admin/runners".to_string(),
    })
}

// ROUTES:
// GET /{owner}/{repo}/actions -> runs
// GET+POST /{owner}/{repo}/actions/run -> dispatch_get, dispatch_post
// GET /{owner}/{repo}/actions/runs/{run} -> run_get
// GET /{owner}/{repo}/actions/runs/{run}/jobs/{job} -> job_get
// GET /{owner}/{repo}/actions/runs/{run}/jobs/{job}/log/stream -> job_stream
// GET /{owner}/{repo}/actions/runs/{run}/jobs/{job}/log.txt -> job_log_txt
// POST /{owner}/{repo}/actions/runs/{run}/cancel -> run_cancel
// POST /{owner}/{repo}/actions/runs/{run}/approve -> run_approve
// POST /{owner}/{repo}/actions/runs/{run}/rerun -> run_rerun
// GET /{owner}/{repo}/actions/runs/{run}/artifacts/{id} -> artifact_download
// GET+POST /{owner}/{repo}/settings/secrets -> secrets_get, secrets_post
// POST /{owner}/{repo}/settings/secrets/{name}/delete -> secret_delete
// POST /{owner}/{repo}/settings/variables/{name}/delete -> variable_delete
// GET /{owner}/{repo}/settings/runners -> runners_get

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn the_actions_tab_says_why_workflows_do_not_run() {
        assert_eq!(ci_notice("acme", CiAvailability::Enabled), "");
        assert!(ci_notice("acme", CiAvailability::OwnerNotAllowed).contains("acme is not allowed"));
        assert!(ci_notice("acme", CiAvailability::TurnedOff).contains("turned off"));
    }

    fn at(h: u32, m: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 15, h, m, 0).unwrap()
    }

    fn step(no: u32, name: &str, from: u64, to: Option<u64>, status: &str, conclusion: Option<&str>) -> WorkflowStep {
        WorkflowStep {
            number: no,
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(str::to_string),
            started_at: None,
            completed_at: None,
            log_from: from,
            log_to: to,
        }
    }

    fn job(name: &str, needs: &[&str]) -> (String, Vec<String>) {
        (name.to_string(), needs.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn depth_levels_columns_by_longest_path() {
        let js = vec![job("build", &[]), job("linux", &["build"]), job("mac", &["build"]), job("sign", &["linux", "mac"])];
        let cols = depth_levels(&js);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0], vec![0]);
        assert_eq!(cols[1], vec![1, 2]);
        assert_eq!(cols[2], vec![3]);
    }

    #[test]
    fn unknown_needs_are_ignored() {
        let js = vec![job("a", &[]), job("b", &["ghost"])];
        let cols = depth_levels(&js);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0], vec![0, 1]);
    }

    #[test]
    fn a_cycle_terminates_and_keeps_every_job() {
        let js = vec![job("a", &["b"]), job("b", &["a"]), job("c", &[])];
        let cols = depth_levels(&js);
        let total: usize = cols.iter().map(Vec::len).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn run_filters() {
        assert!(run_matches("in_progress", None, "running"));
        assert!(run_matches("queued", None, "running"));
        assert!(!run_matches("completed", Some("failure"), "running"));
        assert!(run_matches("completed", Some("failure"), "failed"));
        assert!(run_matches("completed", Some("timed_out"), "failed"));
        assert!(!run_matches("completed", Some("cancelled"), "failed"));
        assert!(run_matches("completed", Some("cancelled"), "all"));
        assert!(run_matches("completed", Some("success"), "passed"));
        assert!(!run_matches("completed", Some("success"), "failed"));
    }

    #[test]
    fn secret_names_are_normalised_and_validated() {
        assert_eq!(secret_name("deploy_key").as_deref(), Some("DEPLOY_KEY"));
        assert_eq!(secret_name("API_2").as_deref(), Some("API_2"));
        assert_eq!(secret_name("_X").as_deref(), Some("_X"));
        assert_eq!(secret_name("2BAD"), None);
        assert_eq!(secret_name("has space"), None);
        assert_eq!(secret_name(""), None);
        assert_eq!(secret_name(&"L".repeat(101)), None);
    }

    #[test]
    fn log_slices_are_clamped() {
        let raw = b"0123456789".as_slice();
        assert_eq!(slice_log(raw, 3, Some(7)), b"3456");
        assert_eq!(slice_log(raw, 8, Some(99)), b"89");
        assert_eq!(slice_log(raw, 8, None), b"89");
        assert_eq!(slice_log(raw, 20, None), b"");
        assert_eq!(slice_log(raw, 5, Some(2)), b"");
        assert_eq!(slice_log(raw, 0, None), raw);
    }

    #[test]
    fn log_html_numbers_and_escapes() {
        let (html, next) = log_html(b"hello\n<script>\n", 4);
        assert!(html.contains("id=\"L4\""), "{html}");
        assert!(html.contains("href=\"#L5\""), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.starts_with("<div class=\"log-line\""), "{html}");
        assert_eq!(next, 6);
        let (empty, next) = log_html(b"", 9);
        assert_eq!(empty, "");
        assert_eq!(next, 9);
    }

    #[test]
    fn steps_share_one_line_counter_and_their_own_panels() {
        let raw = b"setup ok\nbuild start\nbuild done\ntest running".as_slice();
        let steps = vec![
            step(1, "Set up", 0, Some(9), "completed", Some("success")),
            step(2, "Build", 9, Some(32), "completed", Some("failure")),
            step(3, "Test", 32, None, "in_progress", None),
        ];
        let (views, line) = step_views(raw, &steps);
        assert_eq!(views.len(), 3, "no synthetic live section is added");
        assert!(!views[0].open, "a passed step is closed");
        assert!(views[1].open, "a failed step is open");
        assert!(views[0].html.contains("setup ok"));
        assert!(views[1].html.contains("build start"));
        assert!(views[1].html.contains("build done"));
        assert!(views[2].html.contains("test running"));
        assert_eq!(views[0].state, "done");
        assert_eq!(views[2].state, "running");
        assert_eq!(views[0].number, 1);
        assert_eq!(line, 5, "four lines of log were numbered 1 to 4");
    }

    #[test]
    fn a_queued_step_gets_an_empty_panel_and_owns_no_log() {
        let mut running = step(1, "Count", 0, None, "in_progress", None);
        running.started_at = Some(at(9, 0));
        let steps = vec![running, step(2, "Fail", 20, None, "queued", None)];
        let (views, line) = step_views(b"A1\nA2\n".as_slice(), &steps);
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].state, "running");
        assert!(views[0].started_ms.is_some(), "a running step carries its start for the ticking timer");
        assert!(views[0].html.contains("A1"));
        assert!(!views[0].empty);
        assert_eq!(views[1].state, "queued");
        assert!(views[1].empty, "the queued step renders the placeholder");
        assert_eq!(views[1].html, "");
        assert_eq!(views[1].started_ms, None);
        assert_eq!(line, 3, "only the started step's lines were numbered");
    }

    #[test]
    fn a_step_without_a_name_still_gets_one() {
        let steps = vec![step(2, "", 0, None, "queued", None)];
        let (views, _) = step_views(b"", &steps);
        assert_eq!(views[0].name, "Step 2");
    }

    #[test]
    fn route_log_sends_the_running_steps_bytes_to_it_alone() {
        let steps = vec![step(1, "Count", 20, None, "in_progress", None), step(2, "Fail", 50, None, "queued", None)];
        let (out, consumed) = route_log(&steps, 20, b"A1\nA2\nA3\n", true);
        assert_eq!(out, vec![(1, b"A1\nA2\nA3\n".to_vec())]);
        assert_eq!(consumed, 9, "every byte is owned by the running step");
    }

    #[test]
    fn route_log_splits_bytes_at_a_step_boundary() {
        let steps = vec![step(1, "One", 0, Some(30), "completed", Some("success")), step(2, "Two", 30, None, "in_progress", None)];
        let all = b"AAAAABBBBBBBBBBBBBBBBBBBB".as_slice();
        let (out, consumed) = route_log(&steps, 25, all, true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1);
        assert_eq!(out[0].1, all[..5].to_vec(), "bytes 25 to 30 belong to step 1");
        assert_eq!(out[1].0, 2);
        assert_eq!(out[1].1, all[5..].to_vec(), "the bytes from 30 on belong to step 2");
        assert_eq!(consumed, all.len() as u64);
    }

    #[test]
    fn bytes_before_any_step_are_held_while_the_job_runs() {
        let (out, consumed) = route_log(&[], 0, b"early\n", true);
        assert!(out.is_empty());
        assert_eq!(consumed, 0, "no step owns these bytes yet");
        let queued = vec![step(1, "Later", 10, None, "queued", None)];
        let (out, consumed) = route_log(&queued, 0, b"early\n", true);
        assert!(out.is_empty());
        assert_eq!(consumed, 0, "a queued step owns nothing either");
    }

    #[test]
    fn bytes_before_any_step_are_consumed_once_the_job_is_over() {
        let (out, consumed) = route_log(&[], 0, b"early\n", false);
        assert!(out.is_empty(), "still nobody to route them to");
        assert_eq!(consumed, 6, "but a finished job never re-reads them");
    }

    #[test]
    fn a_held_back_tail_is_cut_at_a_newline() {
        let steps = vec![step(1, "One", 0, Some(5), "completed", Some("success"))];
        let (out, consumed) = route_log(&steps, 0, b"ok\nxx yy\nzz\n", true);
        assert_eq!(out, vec![(1, b"ok\n".to_vec())]);
        assert_eq!(consumed, 3, "the partial line past the frontier is held back whole");
    }

    #[test]
    fn run_duration_spans_first_start_to_last_end() {
        let mk = |name: &str, s: Option<chrono::DateTime<Utc>>, e: Option<chrono::DateTime<Utc>>| WorkflowJob {
            id: JobId(0),
            run_id: RunId(0),
            name: name.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            needs: vec![],
            runner_name: None,
            started_at: s,
            completed_at: e,
            steps: vec![],
        };
        assert_eq!(run_duration(&[]), None);
        let jobs = vec![mk("a", Some(at(10, 0)), Some(at(10, 1))), mk("b", Some(at(10, 0)), Some(at(10, 2)))];
        assert_eq!(run_duration(&jobs).as_deref(), Some("2m 0s"));
        let open = vec![mk("a", Some(at(10, 0)), None)];
        assert!(run_duration(&open).is_some(), "a running job counts up to now");
    }

    #[test]
    fn chips_link_and_mark_the_current_filter() {
        let chips = filter_chips("/a/b", "failed", [3, 1, 1, 1]);
        assert_eq!(chips[0].href, "/a/b/actions");
        assert_eq!(chips[1].href, "/a/b/actions?status=running");
        assert_eq!(chips[2].href, "/a/b/actions?status=failed");
        assert!(chips[2].current && !chips[0].current);
        assert_eq!(chips[0].count, 3);
    }

    #[test]
    fn filenames_are_safe_for_a_header() {
        assert_eq!(safe_filename("my report/v1.zip"), "my_report_v1.zip");
        assert_eq!(safe_filename("a\"b"), "a_b");
        assert_eq!(safe_filename(".."), "artifact");
        assert_eq!(safe_filename(""), "artifact");
    }

    #[test]
    fn settings_tabs_mark_the_current_page() {
        let tabs = settings_tabs("/a/b", "secrets");
        assert_eq!(tabs[0].href, "/a/b/settings");
        assert!(tabs[1].current && !tabs[0].current && !tabs[2].current);
        assert_eq!(tabs[2].href, "/a/b/settings/runners");
    }

    #[test]
    fn runner_chips() {
        assert_eq!(runner_chip("online"), ("chip-ok", "Online"));
        assert_eq!(runner_chip("busy"), ("chip-warn", "Busy"));
        assert_eq!(runner_chip("nonsense"), ("chip-muted", "Offline"));
    }

    #[test]
    fn workflow_names_fall_back_to_the_path() {
        let defs = vec![WorkflowDef {
            path: ".codehaven/workflows/deploy.yml".into(),
            name: String::new(),
            triggers: vec!["push".into(), "workflow_dispatch".into()],
            error: None,
            can_dispatch: true,
            inputs: vec![],
        }];
        let views = workflow_views(&defs);
        assert_eq!(views[0].name, "deploy.yml");
        assert_eq!(views[0].triggers, "push, workflow_dispatch");
        assert!(views[0].can_dispatch);
    }

    #[test]
    fn durations_in_tests_compile_the_helper() {
        assert_eq!(fmt::duration_between(Some(at(9, 0)), Some(at(9, 0) + Duration::seconds(90))).as_deref(), Some("1m 30s"));
    }
}
