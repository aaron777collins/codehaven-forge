//! The run panel: runs, jobs, live logs, workflows, dispatch, re-run and cancel (spec section 10).
//! Every write goes through the existing CI services, so permissions, audit and the dispatch
//! ref rules are the same as on the Actions pages.

use super::ctx::{JsonData, Key, Need, WorkspaceCtx, WsError, WsResult, json};
use super::files::{Q, head_of, pinned, valid_sha};
use super::limits::{Kind, Permit};
use crate::ctx::Ctx;
use crate::fmt;
use crate::views::actions::route_log;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use ch_core::model::{CiAvailability, LogRange, WorkflowRun};
use ch_core::{JobId, RunId};
use serde::Deserialize;
use serde_json::{Value, json as j};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;

fn run_json(r: &WorkflowRun, names: &HashMap<i64, String>) -> Value {
    j!({
        "id": r.id.0,
        "number": r.run_number,
        "event": r.event,
        "head_sha": r.head_sha,
        "branch": r.head_branch,
        "status": r.status,
        "conclusion": r.conclusion,
        "created_at": r.created_at,
        "workflow_id": r.workflow_id.map(|w| w.0),
        "workflow": r.workflow_id.and_then(|w| names.get(&w.0)).cloned(),
    })
}

/// Workflow names by id, for the run list. A failure just leaves the names out.
async fn workflow_names(ws: &WorkspaceCtx) -> HashMap<i64, String> {
    ws.ctx
        .state
        .ci
        .workflows(&ws.ctx.actor, ws.rc.id(), None)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|w| !w.name.is_empty())
        .filter_map(|w| w.id.map(|id| (id.0, w.name)))
        .collect()
}

/// Why nothing can run now, or `None`.
async fn paused(ws: &WorkspaceCtx) -> Option<String> {
    match ws.ctx.state.ci.availability(ws.rc.id()).await {
        Ok(CiAvailability::Enabled) => {}
        Ok(_) => return Some("Workflows are off for this repository.".into()),
        Err(_) => return Some("Workflows are paused on this server.".into()),
    }
    match ws.ctx.state.pipelines.runners(&ws.ctx.actor, Some(ws.rc.id())).await {
        Ok(rs) if rs.iter().any(|r| r.status == "online" || r.status == "busy") => None,
        _ => Some("Workflows are paused on this server. No runner has checked in.".into()),
    }
}

/// The run, checked to be in this repository. A foreign id is the same 404 as a missing one.
async fn run_in_repo(ws: &WorkspaceCtx, run: i64) -> WsResult<WorkflowRun> {
    let r = ws.ctx.state.ci.get_run(&ws.ctx.actor, ws.rc.id(), RunId(run)).await?;
    if r.repo_id != ws.rc.id() {
        return Err(WsError::Web(crate::error::WebError::not_found()));
    }
    Ok(r)
}

/// `GET B/runs?branch=&sha=`: this branch's recent runs, the current head and `can_write`.
pub async fn runs(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let branch = super::files::branch_param(&q, &ws.rc.repo.default_branch)?;
    let head = head_of(&ws, &branch).await?;
    let all = ws.ctx.state.ci.runs(&ws.ctx.actor, ws.rc.id(), 1, 100).await.unwrap_or_default();
    let names = workflow_names(&ws).await;
    let mine: Vec<Value> =
        all.iter().filter(|r| r.head_branch.as_deref() == Some(branch.as_str())).take(30).map(|r| run_json(r, &names)).collect();
    let reason = paused(&ws).await;
    Ok(json(&j!({
        "head": head,
        "can_write": ws.can_write(),
        "paused": { "paused": reason.is_some(), "reason": reason.unwrap_or_default() },
        "runs": mine,
    })))
}

/// `GET B/runs/{run}`: the run's jobs and steps.
pub async fn run(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let r = run_in_repo(&ws, run).await?;
    let names = workflow_names(&ws).await;
    let jobs = ws.ctx.state.ci.jobs(&ws.ctx.actor, ws.rc.id(), r.id).await.unwrap_or_default();
    let jobs: Vec<Value> = jobs
        .iter()
        .filter(|jb| jb.run_id == r.id && jb.repo_id == ws.rc.id())
        .map(|jb| {
            j!({
                "id": jb.id.0, "name": jb.name, "status": jb.status, "conclusion": jb.conclusion,
                "steps": jb.steps.iter().map(|s| j!({"number": s.number, "name": s.name, "status": s.status, "conclusion": s.conclusion})).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json(&j!({ "run": run_json(&r, &names), "jobs": jobs })))
}

struct LogState {
    ws: WorkspaceCtx,
    run: RunId,
    job: JobId,
    offset: u64,
    line: u64,
    last_steps: String,
    pending: VecDeque<Event>,
    closing: bool,
    finished: bool,
    _permit: Permit,
}

/// One poll: new bytes routed to the step that owns them, as text spans, and the step list when
/// it changed. Never HTML.
async fn next_log(mut st: LogState) -> Option<(Result<Event, Infallible>, LogState)> {
    if st.finished {
        return None;
    }
    if let Some(ev) = st.pending.pop_front() {
        return Some((Ok(ev), st));
    }
    if st.closing {
        st.finished = true;
        return Some((Ok(Event::default().event("done").data("{}")), st));
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let state = &st.ws.ctx.state;
    let repo = st.ws.rc.id();
    let bytes = match state.ci.log_range(&st.ws.ctx.actor, repo, st.job, LogRange { from: st.offset, to: None }).await {
        Ok(b) => b,
        Err(_) => {
            st.finished = true;
            return Some((Ok(ch_core::live::retry_event()), st));
        }
    };
    let (steps, active) = match state.pipelines.jobs(&st.ws.ctx.actor, repo, st.run).await {
        Ok(js) => match js.iter().find(|x| x.id == st.job) {
            Some(x) => (x.steps.clone(), fmt::is_active(&x.status)),
            None => (Vec::new(), false),
        },
        Err(_) => (Vec::new(), true),
    };
    let (segments, consumed) = route_log(&steps, st.offset, &bytes, active);
    let mut at = st.offset;
    for (number, seg) in segments {
        at += seg.len() as u64;
        let text = String::from_utf8_lossy(&seg);
        let lines = ch_render::ansi::to_spans(&text);
        let first = st.line;
        st.line += lines.len() as u64;
        let lines: Vec<Value> = lines.into_iter().map(|l| l.into_iter().map(|s| j!({"t": s.t, "c": s.c})).collect::<Value>()).collect();
        st.pending.push_back(
            Event::default().event("log").id(at.to_string()).data(j!({"step": number, "line": first, "lines": lines}).to_string()),
        );
    }
    st.offset += consumed;
    let summary = serde_json::to_string(
        &steps.iter().map(|s| j!({"number": s.number, "name": s.name, "status": s.status, "conclusion": s.conclusion})).collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    if summary != st.last_steps {
        st.last_steps = summary.clone();
        st.pending.push_back(Event::default().event("steps").data(summary));
    }
    if !active {
        st.closing = true;
    }
    let ev = st.pending.pop_front().unwrap_or_else(|| Event::default().comment("tick"));
    Some((Ok(ev), st))
}

/// `GET B/runs/{run}/jobs/{job}/log/stream?offset=&line=`: read with `fetch`, so it carries the
/// workspace key. Checked again every second (`LiveAuth`), including the editor switch.
pub async fn log_stream(
    ctx: Ctx,
    Path((owner, name, run, job)): Path<(String, String, i64, i64)>,
    Query(q): Q,
    headers: HeaderMap,
) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let r = run_in_repo(&ws, run).await?;
    let jb = ws.ctx.state.ci.job(&ws.ctx.actor, ws.rc.id(), JobId(job)).await?;
    if jb.run_id != r.id || jb.repo_id != ws.rc.id() {
        return Err(WsError::Web(crate::error::WebError::not_found()));
    }
    let Some(credential) = ws.ctx.credential.clone() else { return Err(WsError::Web(crate::error::WebError::not_found())) };
    let permit = ws.limit(Kind::Stream)?;
    let offset: u64 = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let line: u64 = q.get("line").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let switch: ch_core::live::ExtraCheck = std::sync::Arc::new(|state: ch_core::state::AppState| {
        Box::pin(async move { Ok(state.repos.workspace_settings().await?.editor_on(&state.config)) })
    });
    let live = std::sync::Arc::new(ch_core::live::LiveAuth::new(ws.ctx.state.clone(), credential, ws.rc.id()).extra(switch));
    let st = LogState {
        ws,
        run: r.id,
        job: JobId(job),
        offset,
        line,
        last_steps: String::new(),
        pending: VecDeque::new(),
        closing: false,
        finished: false,
        _permit: permit,
    };
    let stream = ch_core::live::guarded(live, futures::stream::unfold(st, next_log));
    let mut resp =
        Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("keepalive")).into_response();
    let h = resp.headers_mut();
    super::ctx::data_headers(h);
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(resp)
}

/// The most a `changed_b64` list may hold: a commit changes at most 200 files, and a rename
/// names two.
const MAX_CHANGED: usize = 1000;

/// The files a push changes, from `changed_b64`: base64 of UTF-8 paths with NUL between them.
/// `None` when the page sent no list, or for a new branch (`new=1`), whose push names no
/// earlier commit: the push trigger cannot know its files either, and lets path filters pass.
fn changed_param(q: &HashMap<String, String>) -> WsResult<Option<Vec<String>>> {
    if q.get("new").is_some_and(|v| v == "1") {
        return Ok(None);
    }
    let Some(raw) = q.get("changed_b64") else { return Ok(None) };
    let bad = || WsError::invalid("changed_b64", "That is not a list of files.");
    if raw.len() > 256 * 1024 {
        return Err(bad());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).map_err(|_| bad())?;
    let text = String::from_utf8(bytes).map_err(|_| bad())?;
    if text.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let paths: Vec<String> = text.split('\0').map(String::from).collect();
    if paths.len() > MAX_CHANGED || paths.iter().any(|p| p.is_empty()) {
        return Err(bad());
    }
    Ok(Some(paths))
}

/// True when a push of `event` starts a workflow with these filters, by the same rule the push
/// trigger uses.
fn push_starts(filters: &ch_core::model::PushFilters, event: &ch_workflow::trigger::EventInfo) -> bool {
    ch_workflow::PushTrigger {
        branches: filters.branches.clone(),
        branches_ignore: filters.branches_ignore.clone(),
        tags: filters.tags.clone(),
        tags_ignore: filters.tags_ignore.clone(),
        paths: filters.paths.clone(),
        paths_ignore: filters.paths_ignore.clone(),
    }
    .matches(event)
}

/// `GET B/workflows?branch=&sha=&target=&new=&changed_b64=`: the workflow files at that commit,
/// parsed. Writes nothing. `on_push` says whether a push of the drafted files to `target` (this
/// branch when not given) would start the workflow, with its branch and path filters applied.
pub async fn workflows(ctx: Ctx, Path((owner, name)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> WsResult<Response> {
    let ws = WorkspaceCtx::load(ctx, &owner, &name, Key::Needed, Need::Read, &headers).await?;
    let _permit = ws.limit(Kind::Workflows)?;
    let target = match q.get("target") {
        Some(t) if !super::files::valid_branch(t) => return Err(WsError::invalid("target", "That is not a branch name.")),
        Some(t) => Some(t.clone()),
        None => None,
    };
    let changed = changed_param(&q)?;
    let (branch, sha) = pinned(&ws, &q).await?;
    let event = ch_workflow::trigger::EventInfo::push(&format!("refs/heads/{}", target.unwrap_or(branch)), changed);
    let files = ws.ctx.state.ci.workflows(&ws.ctx.actor, ws.rc.id(), Some(&sha)).await.unwrap_or_default();
    let list: Vec<Value> = files
        .iter()
        .map(|w| {
            let active = w.state == "active";
            j!({
                "path": w.path,
                "name": w.name,
                "listens_push": active && w.push.is_some(),
                "on_push": active && w.push.as_ref().is_some_and(|p| push_starts(p, &event)),
                "dispatch": active && w.events.iter().any(|e| e == "workflow_dispatch"),
                "error": w.errors.first(),
                "inputs": w.dispatch_inputs.iter().map(|i| j!({
                    "name": i.name, "description": i.description, "required": i.required,
                    "type": i.kind, "default": i.default, "options": i.options,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json(&j!({ "workflows": list })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchBody {
    pub workflow: String,
    pub branch: String,
    pub expected_head: String,
    #[serde(default)]
    pub inputs: serde_json::Map<String, Value>,
}

/// `POST B/dispatch`: only an existing branch, by exact name, and only the head the page saw.
pub async fn dispatch(ctx: Ctx, Path((owner, name)): Path<(String, String)>, headers: HeaderMap, body: Body) -> WsResult<Response> {
    let ws = WorkspaceCtx::load_post(ctx, &owner, &name, Need::Write, &headers).await?;
    let _permit = ws.limit(Kind::Workflows)?;
    let JsonData(req) = JsonData::<DispatchBody>::read(&ws, &headers, body, 64 * 1024).await?;
    if !super::files::valid_branch(&req.branch) {
        return Err(WsError::invalid("branch", "That is not a branch name."));
    }
    if req.workflow.is_empty() || req.workflow.len() > 512 || req.workflow.contains("..") || req.workflow.chars().any(|c| c.is_control()) {
        return Err(WsError::invalid("workflow", "That is not a workflow."));
    }
    let Some(head) = head_of(&ws, &req.branch).await? else {
        return Err(WsError::code(StatusCode::CONFLICT, "branch_gone", "That branch no longer exists."));
    };
    if !valid_sha(&req.expected_head) || req.expected_head != head {
        return Err(WsError::code(StatusCode::CONFLICT, "head_moved", "The branch moved. Reload it to run the newest commit."));
    }
    let full = format!("refs/heads/{}", req.branch);
    let id = ws.ctx.state.ci.dispatch(&ws.ctx.actor, ws.rc.id(), &req.workflow, &full, Value::Object(req.inputs), &ws.ctx.client).await?;
    Ok(json(&j!({ "run": id.0 })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RerunBody {
    #[serde(default)]
    pub failed_only: bool,
}

/// `POST B/runs/{run}/rerun`. The service records the person who clicked.
pub async fn rerun(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>, headers: HeaderMap, body: Body) -> WsResult<Response> {
    let ws = WorkspaceCtx::load_post(ctx, &owner, &name, Need::Write, &headers).await?;
    let _permit = ws.limit(Kind::Workflows)?;
    let JsonData(req) = JsonData::<RerunBody>::read(&ws, &headers, body, 1024).await?;
    let r = run_in_repo(&ws, run).await?;
    let id = ws.ctx.state.ci.rerun(&ws.ctx.actor, ws.rc.id(), r.id, req.failed_only, &ws.ctx.client).await?;
    Ok(json(&j!({ "run": id.0, "from_sha": r.head_sha })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelBody {}

/// `POST B/runs/{run}/cancel`.
pub async fn cancel(ctx: Ctx, Path((owner, name, run)): Path<(String, String, i64)>, headers: HeaderMap, body: Body) -> WsResult<Response> {
    let ws = WorkspaceCtx::load_post(ctx, &owner, &name, Need::Write, &headers).await?;
    let _permit = ws.limit(Kind::Workflows)?;
    let JsonData(_req) = JsonData::<CancelBody>::read(&ws, &headers, body, 1024).await?;
    let r = run_in_repo(&ws, run).await?;
    ws.ctx.state.ci.cancel(&ws.ctx.actor, ws.rc.id(), r.id, &ws.ctx.client).await?;
    let mut resp = json(&j!({ "ok": true }));
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}
