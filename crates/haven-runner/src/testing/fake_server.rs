//! A runner-protocol server in memory, speaking the real HTTP routes, so the runner's client,
//! retries and job engine are exercised exactly as they run against `ch-ci`.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use ch_api_types::runner::{
    ArtifactUploaded, CacheHit, CacheLookupRequest, CacheLookupResponse, CompleteRequest, HeartbeatResponse, JobPayload, JobRequest,
    JobResponse, LogAck, LogBatch, RegisterRequest, RegisterResponse, StepUpdate,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct JobRec {
    payload: Option<JobPayload>,
    logs: BTreeMap<u64, (i32, Vec<String>)>,
    steps: BTreeMap<i32, Vec<StepUpdate>>,
    completed: Option<CompleteRequest>,
    heartbeats: u32,
    cancel: bool,
    gone: bool,
}

struct CacheRec {
    id: i64,
    key: String,
    version: String,
    data: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    reg_tokens: HashSet<String>,
    runners: HashMap<String, (i64, String, Vec<String>)>,
    queue: VecDeque<JobPayload>,
    jobs: HashMap<i64, JobRec>,
    artifacts: HashMap<(i64, String), Vec<u8>>,
    artifact_retention: HashMap<(i64, String), Option<u32>>,
    caches: Vec<CacheRec>,
    next_id: i64,
    fail: HashMap<String, VecDeque<u16>>,
    log_posts: u32,
    requests: Vec<String>,
}

type Shared = Arc<Mutex<Inner>>;

pub struct FakeServer {
    pub url: String,
    inner: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn bearer(h: &HeaderMap) -> Option<String> {
    h.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(String::from)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({"message": msg}))).into_response()
}

/// Pops an injected failure for this endpoint, if any.
fn injected(st: &Shared, endpoint: &str) -> Option<Response> {
    let mut g = st.lock().ok()?;
    g.requests.push(endpoint.to_string());
    let code = g.fail.get_mut(endpoint)?.pop_front()?;
    Some(err(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), "injected failure"))
}

fn runner_ok(st: &Shared, h: &HeaderMap) -> bool {
    match (bearer(h), st.lock()) {
        (Some(t), Ok(g)) => g.runners.contains_key(&t),
        _ => false,
    }
}

impl FakeServer {
    pub async fn start() -> FakeServer {
        let inner: Shared = Arc::new(Mutex::new(Inner { next_id: 1, ..Default::default() }));
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/runner/v1/register", post(register))
            .route("/api/runner/v1/jobs/request", post(request_job))
            .route("/api/runner/v1/jobs/{id}/heartbeat", post(heartbeat))
            .route("/api/runner/v1/jobs/{id}/logs", post(logs))
            .route("/api/runner/v1/jobs/{id}/steps/{n}", post(step))
            .route("/api/runner/v1/jobs/{id}/complete", post(complete))
            .route("/api/runner/v1/jobs/{id}/artifacts/{name}", put(put_artifact).get(get_artifact))
            .route("/api/runner/v1/cache/lookup", post(cache_lookup))
            .route("/api/runner/v1/cache/{key}", put(cache_put).get(cache_get))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a local port");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeServer { url: format!("http://{addr}"), inner, task }
    }

    fn g(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fake server state")
    }

    pub fn add_registration_token(&self, t: &str) {
        self.g().reg_tokens.insert(t.to_string());
    }

    /// Registers a runner directly and returns its credential, skipping the HTTP step.
    pub fn add_runner(&self, name: &str, labels: &[&str]) -> (i64, String) {
        let mut g = self.g();
        let id = g.next_id;
        g.next_id += 1;
        let cred = format!("chr_test_{id}");
        g.runners.insert(cred.clone(), (id, name.to_string(), labels.iter().map(|s| s.to_string()).collect()));
        (id, cred)
    }

    pub fn runner_count(&self) -> usize {
        self.g().runners.len()
    }

    /// Makes the server know a job without queueing it, for tests that run a job directly.
    pub fn register_job(&self, payload: &JobPayload) {
        self.g().jobs.insert(payload.job_id, JobRec { payload: Some(payload.clone()), ..Default::default() });
    }

    pub fn enqueue(&self, payload: JobPayload) {
        let mut g = self.g();
        g.jobs.insert(payload.job_id, JobRec { payload: Some(payload.clone()), ..Default::default() });
        g.queue.push_back(payload);
    }

    /// Makes the next calls to `endpoint` answer with these statuses (`jobs/request`, `logs`,
    /// `steps`, `complete`, `heartbeat`, `register`, `artifact`, `cache`).
    pub fn fail_next(&self, endpoint: &str, statuses: &[u16]) {
        self.g().fail.entry(endpoint.to_string()).or_default().extend(statuses.iter().copied());
    }

    pub fn cancel_job(&self, job_id: i64) {
        if let Some(j) = self.g().jobs.get_mut(&job_id) {
            j.cancel = true;
        }
    }

    /// From now on the server answers 404 for this job, as when its lease was reassigned.
    pub fn drop_job(&self, job_id: i64) {
        if let Some(j) = self.g().jobs.get_mut(&job_id) {
            j.gone = true;
        }
    }

    pub fn completed(&self, job_id: i64) -> Option<CompleteRequest> {
        self.g().jobs.get(&job_id).and_then(|j| j.completed.clone())
    }

    pub async fn wait_complete(&self, job_id: i64, timeout: Duration) -> CompleteRequest {
        let end = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(c) = self.completed(job_id) {
                return c;
            }
            if tokio::time::Instant::now() > end {
                panic!("job {job_id} did not complete within {timeout:?}.\nlog so far:\n{}", self.log_text(job_id));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until the log contains `needle`.
    pub async fn wait_log(&self, job_id: i64, needle: &str, timeout: Duration) {
        let end = tokio::time::Instant::now() + timeout;
        while !self.log_text(job_id).contains(needle) {
            if tokio::time::Instant::now() > end {
                panic!("the log of job {job_id} never contained {needle:?}.\nlog so far:\n{}", self.log_text(job_id));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Every log line of the job, in the order the server received them.
    pub fn log_lines(&self, job_id: i64) -> Vec<(i32, String)> {
        let g = self.g();
        let Some(j) = g.jobs.get(&job_id) else { return vec![] };
        j.logs.values().flat_map(|(step, lines)| lines.iter().map(move |l| (*step, l.clone()))).collect()
    }

    pub fn log_text(&self, job_id: i64) -> String {
        self.log_lines(job_id).into_iter().map(|(s, l)| format!("[{s}] {l}")).collect::<Vec<_>>().join("\n")
    }

    pub fn step_log(&self, job_id: i64, step: i32) -> String {
        self.log_lines(job_id).into_iter().filter(|(s, _)| *s == step).map(|(_, l)| l).collect::<Vec<_>>().join("\n")
    }

    /// A step's log without the `##[group]Run ...` header that echoes the script, so tests can
    /// tell what a step printed from what it was told to run.
    pub fn step_output(&self, job_id: i64, step: i32) -> String {
        let mut out = Vec::new();
        let mut in_header = false;
        let mut seen_header = false;
        let log = self.step_log(job_id, step);
        for l in log.lines() {
            if !seen_header && l.starts_with("##[group]Run") {
                in_header = true;
                seen_header = true;
                continue;
            }
            if in_header {
                if l == "##[endgroup]" {
                    in_header = false;
                }
                continue;
            }
            out.push(l);
        }
        out.join("\n")
    }

    /// Every step's output (see `step_output`), job-level lines included.
    pub fn output_text(&self, job_id: i64) -> String {
        let steps: std::collections::BTreeSet<i32> = self.log_lines(job_id).into_iter().map(|(s, _)| s).collect();
        steps
            .into_iter()
            .map(|s| if s == 0 { self.step_log(job_id, 0) } else { self.step_output(job_id, s) })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn step_updates(&self, job_id: i64) -> BTreeMap<i32, Vec<StepUpdate>> {
        self.g().jobs.get(&job_id).map(|j| j.steps.clone()).unwrap_or_default()
    }

    /// The last reported state of a step: its conclusion, or `in_progress`.
    pub fn step_result(&self, job_id: i64, step: i32) -> Option<String> {
        let u = self.step_updates(job_id);
        let last = u.get(&step)?.last()?;
        Some(last.conclusion.clone().unwrap_or_else(|| last.status.clone()))
    }

    pub fn step_outputs(&self, job_id: i64, step: i32) -> BTreeMap<String, String> {
        self.step_updates(job_id).get(&step).and_then(|v| v.last()).map(|u| u.outputs.clone()).unwrap_or_default()
    }

    pub fn heartbeats(&self, job_id: i64) -> u32 {
        self.g().jobs.get(&job_id).map(|j| j.heartbeats).unwrap_or(0)
    }

    pub fn log_posts(&self) -> u32 {
        self.g().log_posts
    }

    pub fn requests_to(&self, endpoint: &str) -> usize {
        self.g().requests.iter().filter(|r| *r == endpoint).count()
    }

    pub fn artifact(&self, job_id: i64, name: &str) -> Option<Vec<u8>> {
        self.g().artifacts.get(&(job_id, name.to_string())).cloned()
    }

    pub fn artifact_retention(&self, job_id: i64, name: &str) -> Option<Option<u32>> {
        self.g().artifact_retention.get(&(job_id, name.to_string())).copied()
    }

    /// Plants an artifact, as if an earlier job of the run had uploaded it.
    pub fn put_artifact(&self, job_id: i64, name: &str, data: Vec<u8>) {
        self.g().artifacts.insert((job_id, name.to_string()), data);
    }

    pub fn cache_keys(&self) -> Vec<String> {
        self.g().caches.iter().map(|c| c.key.clone()).collect()
    }

    pub fn put_cache(&self, key: &str, version: &str, data: Vec<u8>) {
        let mut g = self.g();
        let id = g.next_id;
        g.next_id += 1;
        g.caches.push(CacheRec { id, key: key.into(), version: version.into(), data });
    }

    pub fn cache_data(&self, key: &str) -> Option<Vec<u8>> {
        self.g().caches.iter().find(|c| c.key == key).map(|c| c.data.clone())
    }
}

// ---- handlers -------------------------------------------------------------------------------

async fn register(State(st): State<Shared>, Json(req): Json<RegisterRequest>) -> Response {
    if let Some(r) = injected(&st, "register") {
        return r;
    }
    let mut g = st.lock().expect("state");
    if !g.reg_tokens.remove(&req.token) {
        return err(StatusCode::UNAUTHORIZED, "invalid registration token");
    }
    let id = g.next_id;
    g.next_id += 1;
    let cred = format!("chr_{id}_{}", rand::random::<u64>());
    g.runners.insert(cred.clone(), (id, req.name.clone(), req.labels.clone()));
    Json(RegisterResponse { runner_id: id, name: req.name, credential: cred }).into_response()
}

async fn request_job(State(st): State<Shared>, headers: HeaderMap, Json(req): Json<JobRequest>) -> Response {
    if let Some(r) = injected(&st, "jobs/request") {
        return r;
    }
    if !runner_ok(&st, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad credential");
    }
    let wait = Duration::from_secs(u64::from(req.wait_seconds.unwrap_or(0).min(30)));
    let end = tokio::time::Instant::now() + wait;
    loop {
        {
            let mut g = st.lock().expect("state");
            let pos = g.queue.iter().position(|j| j.runs_on.iter().all(|l| req.labels.contains(l)));
            if let Some(i) = pos {
                let job = g.queue.remove(i).expect("position is valid");
                return Json(JobResponse { job: Some(job) }).into_response();
            }
        }
        if tokio::time::Instant::now() >= end {
            return Json(JobResponse { job: None }).into_response();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn with_job<T>(st: &Shared, id: i64, f: impl FnOnce(&mut JobRec) -> T) -> Result<T, Response> {
    let mut g = st.lock().expect("state");
    match g.jobs.get_mut(&id) {
        Some(j) if !j.gone => Ok(f(j)),
        _ => Err(err(StatusCode::NOT_FOUND, "no such job")),
    }
}

async fn heartbeat(State(st): State<Shared>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Some(r) = injected(&st, "heartbeat") {
        return r;
    }
    if !runner_ok(&st, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad credential");
    }
    match with_job(&st, id, |j| {
        j.heartbeats += 1;
        j.cancel
    }) {
        Ok(cancel) => Json(HeartbeatResponse { cancel }).into_response(),
        Err(r) => r,
    }
}

async fn logs(State(st): State<Shared>, headers: HeaderMap, Path(id): Path<i64>, Json(b): Json<LogBatch>) -> Response {
    if let Some(r) = injected(&st, "logs") {
        return r;
    }
    if !runner_ok(&st, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad credential");
    }
    st.lock().expect("state").log_posts += 1;
    match with_job(&st, id, |j| {
        // A batch the server already has is acknowledged and dropped.
        j.logs.entry(b.seq).or_insert((b.step, b.lines.clone()));
        b.seq + 1
    }) {
        Ok(next) => Json(LogAck { next_seq: next }).into_response(),
        Err(r) => r,
    }
}

async fn step(State(st): State<Shared>, headers: HeaderMap, Path((id, n)): Path<(i64, i32)>, Json(u): Json<StepUpdate>) -> Response {
    if let Some(r) = injected(&st, "steps") {
        return r;
    }
    if !runner_ok(&st, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad credential");
    }
    match with_job(&st, id, |j| j.steps.entry(n).or_default().push(u)) {
        Ok(()) => Json(serde_json::json!({})).into_response(),
        Err(r) => r,
    }
}

async fn complete(State(st): State<Shared>, headers: HeaderMap, Path(id): Path<i64>, Json(c): Json<CompleteRequest>) -> Response {
    if let Some(r) = injected(&st, "complete") {
        return r;
    }
    if !runner_ok(&st, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad credential");
    }
    match with_job(&st, id, |j| j.completed = Some(c)) {
        Ok(()) => Json(serde_json::json!({})).into_response(),
        Err(r) => r,
    }
}

fn job_token_ok(st: &Shared, job_id: Option<i64>, h: &HeaderMap) -> bool {
    let Some(t) = bearer(h) else { return false };
    let g = st.lock().expect("state");
    match job_id {
        Some(id) => g.jobs.get(&id).and_then(|j| j.payload.as_ref()).is_some_and(|p| p.token == t),
        None => g.jobs.values().filter_map(|j| j.payload.as_ref()).any(|p| p.token == t),
    }
}

#[derive(serde::Deserialize, Default)]
struct ArtifactQuery {
    retention_days: Option<u32>,
}

async fn put_artifact(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path((id, name)): Path<(i64, String)>,
    Query(q): Query<ArtifactQuery>,
    body: Bytes,
) -> Response {
    if let Some(r) = injected(&st, "artifact") {
        return r;
    }
    if !job_token_ok(&st, Some(id), &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad job token");
    }
    let mut g = st.lock().expect("state");
    let key = (id, name.clone());
    if g.artifacts.contains_key(&key) {
        return err(StatusCode::CONFLICT, "an artifact with this name already exists");
    }
    let n = g.artifacts.len() as i64 + 1;
    let size = body.len() as i64;
    g.artifact_retention.insert(key.clone(), q.retention_days);
    g.artifacts.insert(key, body.to_vec());
    Json(ArtifactUploaded { id: n, name, size }).into_response()
}

async fn get_artifact(State(st): State<Shared>, headers: HeaderMap, Path((id, name)): Path<(i64, String)>) -> Response {
    if let Some(r) = injected(&st, "artifact") {
        return r;
    }
    if !job_token_ok(&st, None, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad job token");
    }
    // `download-artifact` reads artifacts of the same run, so the job id in the URL decides.
    let g = st.lock().expect("state");
    match g.artifacts.get(&(id, name)) {
        Some(d) => ([(header::CONTENT_TYPE, "application/zip")], d.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such artifact"),
    }
}

async fn cache_lookup(State(st): State<Shared>, headers: HeaderMap, Json(req): Json<CacheLookupRequest>) -> Response {
    if let Some(r) = injected(&st, "cache") {
        return r;
    }
    if !job_token_ok(&st, None, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad job token");
    }
    let g = st.lock().expect("state");
    for (i, k) in req.keys.iter().enumerate() {
        // The first key is exact; the rest are prefixes, newest entry first.
        let found = if i == 0 {
            g.caches.iter().find(|c| c.key == *k && c.version == req.version)
        } else {
            g.caches.iter().rev().find(|c| c.key.starts_with(k.as_str()) && c.version == req.version)
        };
        if let Some(c) = found {
            return Json(CacheLookupResponse {
                hit: Some(CacheHit { id: c.id, key: c.key.clone(), size: c.data.len() as i64, exact: i == 0 }),
            })
            .into_response();
        }
    }
    Json(CacheLookupResponse { hit: None }).into_response()
}

#[derive(serde::Deserialize)]
struct CacheQuery {
    version: Option<String>,
}

async fn cache_put(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Query(q): Query<CacheQuery>,
    body: Bytes,
) -> Response {
    if let Some(r) = injected(&st, "cache") {
        return r;
    }
    if !job_token_ok(&st, None, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad job token");
    }
    let mut g = st.lock().expect("state");
    let version = q.version.unwrap_or_default();
    if g.caches.iter().any(|c| c.key == key && c.version == version) {
        return err(StatusCode::CONFLICT, "cache exists");
    }
    let id = g.next_id;
    g.next_id += 1;
    g.caches.push(CacheRec { id, key, version, data: body.to_vec() });
    StatusCode::CREATED.into_response()
}

async fn cache_get(State(st): State<Shared>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Some(r) = injected(&st, "cache") {
        return r;
    }
    if !job_token_ok(&st, None, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad job token");
    }
    let g = st.lock().expect("state");
    match g.caches.iter().find(|c| c.id == id) {
        Some(c) => ([(header::CONTENT_TYPE, "application/octet-stream")], c.data.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such cache"),
    }
}
