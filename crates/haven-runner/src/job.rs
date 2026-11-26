//! Runs one job: start a container, run each step in it, report as it goes, and clean up
//! whatever happens (ARCHITECTURE.md section 9.5).
//!
//! The order of a job:
//!
//! 1. Create `<work_dir>/job-<id>-<n>/` and a container with that directory bound at the same
//!    path, limited in CPU, memory and processes, never privileged.
//! 2. For each step, in order: evaluate its `if:` against the job status so far, evaluate its
//!    `env`, `with` and `run`, run it, read back `$GITHUB_OUTPUT`, `$GITHUB_ENV` and
//!    `$GITHUB_PATH`, and report the result.
//! 3. After the steps: post actions (cache saves), job outputs.
//! 4. Always: remove the container, take back ownership of the files the container made, and
//!    delete the directory.

use crate::actions::{self, ActionCtx, ActionOutput, Builtin, PostAction, remote};
use crate::backend::{Backend, Chunk, ContainerSpec, ExecSpec, StreamKind};
use crate::client::Client;
use crate::commands::{self, CommandParser};
use crate::config::Config;
use crate::docker::{LABEL_JOB, LABEL_RUNNER};
use crate::expr::{self, EvalContext, JobStatus};
use crate::fsglob;
use crate::jobenv::{self, JobDirs, StepFiles};
use crate::logs::{self, LineSplitter, LogWriter};
use crate::mask::Masker;
use crate::options::{self, Effective};
use crate::shell;
use ch_api_types::runner::{CompleteRequest, JobPayload, StepKind, StepSpec, StepUpdate};
use ch_workflow::action_def::ActionRuns;
use futures::FutureExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Who this runner is, for labels, logs and masks.
#[derive(Debug, Clone)]
pub struct RunnerInfo {
    pub id: i64,
    pub name: String,
    pub credential: String,
}

pub struct JobDeps {
    pub cfg: Arc<Config>,
    pub client: Arc<Client>,
    pub backend: Arc<dyn Backend>,
    pub runner: RunnerInfo,
    /// How long a "minute" in `timeout-minutes` is. 60 seconds, except in tests that need a
    /// timeout to fire quickly.
    pub minute: Duration,
}

/// How the outside world can stop a job.
#[derive(Clone, Default)]
pub struct JobControl {
    /// A cancel from the server, or a shutdown past its grace period. The running step is
    /// stopped, and steps marked `always()` or `cancelled()` still run.
    pub cancel: CancellationToken,
    /// Stop everything now, cleanup steps included.
    pub hard_stop: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobResult {
    /// `success`, `failure`, `cancelled` or `timed_out`.
    pub conclusion: String,
    pub outputs: BTreeMap<String, String>,
}

/// How long cleanup steps get after a cancel or timeout.
const CLEANUP_GRACE: Duration = Duration::from_secs(300);

pub async fn run_job(deps: &JobDeps, payload: &JobPayload, ctl: &JobControl) -> JobResult {
    let masker = Arc::new(Masker::new());
    for v in payload.secrets.values() {
        masker.add(v);
    }
    masker.add(&payload.token);
    masker.add(&deps.runner.credential);
    let (log, shipper) =
        logs::spawn(deps.client.clone(), payload.job_id, masker, deps.cfg.limits.max_log_megabytes.saturating_mul(1024 * 1024));

    let outcome = AssertUnwindSafe(Engine::run(deps, payload, ctl, log.clone())).catch_unwind().await;
    let result = match outcome {
        Ok(r) => r,
        Err(_) => {
            log.line(0, "##[error]The runner hit an internal error while running this job.").await;
            JobResult { conclusion: "failure".into(), outputs: BTreeMap::new() }
        }
    };
    log.flush().await;
    drop(log);
    if let Ok(stats) = shipper.await
        && stats.lines_lost > 0
    {
        tracing::warn!(job_id = payload.job_id, "{} log lines could not be delivered", stats.lines_lost);
    }
    result
}

/// Sends the result to the server. Retried for minutes, since losing it fails the job for
/// everyone watching it.
pub async fn report(deps: &JobDeps, job_id: i64, result: &JobResult) -> Result<(), crate::client::ApiError> {
    deps.client.complete(job_id, &CompleteRequest { conclusion: result.conclusion.clone(), outputs: result.outputs.clone() }).await
}

#[derive(Debug, Clone, Default)]
struct StepCtx {
    outputs: BTreeMap<String, String>,
    outcome: String,
    conclusion: String,
}

enum StepEnd {
    Success,
    /// The reason has been logged already.
    Failure,
    Cancelled,
    TimedOut,
}

struct StepRun {
    end: StepEnd,
    outputs: BTreeMap<String, String>,
}

impl StepRun {
    fn failed() -> StepRun {
        StepRun { end: StepEnd::Failure, outputs: BTreeMap::new() }
    }
}

/// Which limit ended a step.
#[derive(Clone, Copy)]
enum Limit {
    Step(u32),
    Job,
    Cleanup,
}

enum ExecEnd {
    Exit(i64),
    Error(String),
    Timeout,
    Cancel,
    HardStop,
}

/// Who owns the files in the workspace right now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Runner,
    Container,
}

struct Engine<'a> {
    deps: &'a JobDeps,
    payload: &'a JobPayload,
    ctl: &'a JobControl,
    log: LogWriter,
    dirs: JobDirs,
    effective: Effective,
    container: Option<String>,
    image: String,

    base_env: BTreeMap<String, String>,
    job_env: BTreeMap<String, String>,
    github_env: BTreeMap<String, String>,
    /// Newest first.
    extra_path: Vec<String>,
    has_bash: bool,
    base_path: Option<String>,

    github_v: Value,
    vars_v: Value,
    secrets_v: Value,
    needs_v: Value,
    matrix_v: Value,
    inputs_v: Value,
    runner_v: Value,
    steps: BTreeMap<String, StepCtx>,

    failed: bool,
    timed_out: bool,
    cancelled: bool,
    aborted: bool,
    deadline: Instant,
    cleanup_deadline: Option<Instant>,
    owner: Owner,
    dirty: bool,
    posts: Vec<PostAction>,
    debug: bool,
    uid_gid: Option<(u32, u32)>,
    /// The number the next step the runner adds gets (an action's post step).
    next_step: i32,
    /// How many composite actions deep the current step is.
    depth: u32,
    /// The env a composite action's steps inherit from the step that used it.
    scope_env: BTreeMap<String, String>,
    /// Service containers that are running: (id under `services:`, container id).
    services: Vec<(String, String)>,
    /// Whether the job's image has `node`, once a JavaScript action has asked.
    node_found: Option<bool>,
}

impl<'a> Engine<'a> {
    async fn run(deps: &'a JobDeps, payload: &'a JobPayload, ctl: &'a JobControl, log: LogWriter) -> JobResult {
        let cfg = &deps.cfg;
        log.line(0, &format!("Runner {} picked up job {} ({}).", deps.runner.name, payload.job_id, payload.name)).await;

        let dirs = match JobDirs::create(&cfg.runner.work_dir, payload.job_id) {
            Ok(d) => d,
            Err(e) => {
                log.line(0, &format!("##[error]Cannot create the job directory under {}: {e}", cfg.runner.work_dir.display())).await;
                return JobResult { conclusion: "failure".into(), outputs: BTreeMap::new() };
            }
        };
        let uid_gid = owner_of(&dirs.root);
        let opts = match payload.container.as_ref().and_then(|c| c.options.as_deref()).map(options::parse).transpose() {
            Ok(o) => o.unwrap_or_default(),
            Err(e) => {
                log.line(0, &format!("##[error]{e}")).await;
                let _ = dirs.remove();
                return JobResult { conclusion: "failure".into(), outputs: BTreeMap::new() };
            }
        };
        let effective = options::apply(&opts, &cfg.limits());
        let image = cfg.image_for(&payload.runs_on, payload.container.as_ref().map(|c| c.image.as_str()));

        let event_path = dirs.runner.join("event.json");
        let _ = dirs.write_file(&event_path, &serde_json::to_vec(&payload.context.event).unwrap_or_default(), 0o644);
        let debug = cfg.runner.debug
            || payload.vars.get("ACTIONS_STEP_DEBUG").is_some_and(|v| v == "true")
            || payload.secrets.get("ACTIONS_STEP_DEBUG").is_some_and(|v| v == "true");

        let mut secrets: BTreeMap<String, String> = payload.secrets.clone();
        secrets.insert("GITHUB_TOKEN".into(), payload.token.clone());
        secrets.insert("CODEHAVEN_TOKEN".into(), payload.token.clone());
        let needs_v = Value::Object(
            payload
                .needs
                .iter()
                .map(|(k, n)| (k.clone(), json!({"result": n.result, "outputs": jobenv::map_to_value(&n.outputs)})))
                .collect(),
        );

        let mut e = Engine {
            deps,
            payload,
            ctl,
            log,
            base_env: jobenv::base_env(payload, &dirs, &deps.runner.name, &event_path),
            github_v: jobenv::github_context(payload, &dirs.workspace, &event_path),
            vars_v: jobenv::map_to_value(&payload.vars),
            secrets_v: jobenv::map_to_value(&secrets),
            needs_v,
            matrix_v: if payload.matrix.is_null() { json!({}) } else { payload.matrix.clone() },
            inputs_v: jobenv::inputs_context(payload),
            runner_v: jobenv::runner_context(&deps.runner.name, &dirs),
            dirs,
            effective,
            container: None,
            image,
            job_env: BTreeMap::new(),
            github_env: BTreeMap::new(),
            extra_path: Vec::new(),
            has_bash: false,
            base_path: None,
            steps: BTreeMap::new(),
            failed: false,
            timed_out: false,
            cancelled: false,
            aborted: false,
            deadline: Instant::now() + deps.minute * cfg.job_minutes(payload.timeout_minutes),
            cleanup_deadline: None,
            owner: Owner::Runner,
            dirty: false,
            posts: Vec::new(),
            debug,
            uid_gid,
            next_step: payload.steps.iter().map(|s| s.number).max().unwrap_or(0) + 1,
            depth: 0,
            scope_env: BTreeMap::new(),
            services: Vec::new(),
            node_found: None,
        };
        let result = e.execute().await;
        e.teardown().await;
        result
    }

    async fn execute(&mut self) -> JobResult {
        // Job-level env may use expressions (`env: V: ${{ matrix.os }}`).
        let mut job_env = BTreeMap::new();
        for (k, v) in &self.payload.env {
            match self.interp(v, None) {
                Ok(s) => {
                    job_env.insert(k.clone(), s);
                }
                Err(e) => {
                    self.log.line(0, &format!("##[error]env {k}: {e}")).await;
                    return self.finish(false).await;
                }
            }
        }
        self.job_env = job_env;

        if let Err(e) = self.setup_container().await {
            self.log.line(0, &format!("##[error]{e:#}")).await;
            // A cancel that arrives while the image is still being pulled is a cancel, not a failure.
            if self.ctl.cancel.is_cancelled() {
                self.cancelled = true;
            } else {
                self.failed = true;
            }
            return self.finish(false).await;
        }

        let steps: Vec<&StepSpec> = self.payload.steps.iter().collect();
        for step in steps {
            self.observe_stops();
            if self.aborted {
                break;
            }
            self.run_step(step).await;
        }
        self.finish(true).await
    }

    async fn finish(&mut self, ran: bool) -> JobResult {
        if ran {
            // Post steps run last first, as on GitHub. A cache is saved only after a job that
            // went well; an action's post step runs when its `post-if` says (default always()).
            let ok = !self.failed && !self.cancelled && !self.timed_out && !self.aborted;
            let posts = std::mem::take(&mut self.posts);
            for p in posts.into_iter().rev() {
                self.observe_stops();
                if self.aborted {
                    break;
                }
                match p {
                    PostAction::CacheSave { .. } if !ok => {}
                    other => self.run_post(other).await,
                }
            }
        }
        let mut outputs = BTreeMap::new();
        if ran && !self.payload.outputs.is_empty() {
            for (k, expr_src) in &self.payload.outputs {
                match self.interp(expr_src, None) {
                    Ok(v) => {
                        if self.log.masker().mask(&v) != v {
                            self.log.line(0, &format!("##[warning]Skipped job output {k:?}: its value contains a secret.")).await;
                        } else {
                            outputs.insert(k.clone(), v);
                        }
                    }
                    Err(e) => self.log.line(0, &format!("##[warning]Job output {k:?} could not be evaluated: {e}")).await,
                }
            }
        }
        let conclusion = if self.timed_out {
            "timed_out"
        } else if self.cancelled || self.aborted {
            "cancelled"
        } else if self.failed {
            "failure"
        } else {
            "success"
        };
        JobResult { conclusion: conclusion.into(), outputs }
    }

    // ---- container -------------------------------------------------------------------------

    async fn setup_container(&mut self) -> anyhow::Result<()> {
        let cfg = self.deps.cfg.clone();
        let backend = self.deps.backend.clone();
        self.log.line(0, &format!("Image: {}", self.image)).await;
        for n in &self.effective.notes {
            self.log.line(0, &format!("##[warning]{n}")).await;
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let log = self.log.clone();
        let forward = tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                log.line(0, &l).await;
            }
        });
        let pulled = tokio::select! {
            r = backend.ensure_image(&self.image, cfg.always_pull(), &tx) => r,
            _ = self.ctl.cancel.cancelled() => Err(anyhow::anyhow!("cancelled while pulling the image")),
        };
        drop(tx);
        let _ = forward.await;
        pulled?;

        let mut env: BTreeMap<String, String> = BTreeMap::new();
        if let Some(c) = &self.payload.container {
            env.extend(c.env.clone());
        }
        env.extend(self.effective.env.clone());
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_RUNNER.to_string(), self.deps.runner.id.to_string());
        labels.insert(LABEL_JOB.to_string(), self.payload.job_id.to_string());
        labels.insert("org.codehaven".to_string(), "1".to_string());
        let root = self.dirs.root.to_string_lossy().into_owned();
        let spec = ContainerSpec {
            name: format!("codehaven-job-{}-{}-{:04x}", self.deps.runner.id, self.payload.job_id, rand::random::<u16>()),
            image: self.image.clone(),
            labels,
            env,
            binds: vec![format!("{root}:{root}")],
            workdir: Some(self.dirs.workspace.to_string_lossy().into_owned()),
            user: self.effective.user.clone(),
            cpus: self.effective.cpus,
            memory: self.effective.memory,
            pids: self.effective.pids,
            shm_size: self.effective.shm_size,
            entrypoint: Some(vec!["tail".into()]),
            cmd: Some(vec!["-f".into(), "/dev/null".into()]),
            network_of: None,
            // Each service's id names the loopback the job shares with it (ch-ctk.7).
            extra_hosts: self.payload.services.iter().map(|s| format!("{}:127.0.0.1", s.id)).collect(),
        };
        let id = backend.create_and_start(&spec).await?;
        self.container = Some(id.clone());
        self.start_services(&id).await?;

        // What shell and PATH does this image have?
        let probe = ExecSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), r#"command -v bash >/dev/null 2>&1 && echo has-bash=1; echo "path=$PATH""#.into()],
            env: BTreeMap::new(),
            workdir: Some(self.dirs.workspace.to_string_lossy().into_owned()),
            user: None,
        };
        let (out, code) = self.exec_collect(&id, &probe).await?;
        if code != 0 {
            anyhow::bail!("the image {} has no /bin/sh, so steps cannot run in it", self.image);
        }
        for l in out.lines() {
            if l == "has-bash=1" {
                self.has_bash = true;
            } else if let Some(p) = l.strip_prefix("path=") {
                self.base_path = Some(p.to_string());
            }
        }
        Ok(())
    }

    /// Runs a short command and returns what it printed.
    async fn exec_collect(&self, id: &str, spec: &ExecSpec) -> anyhow::Result<(String, i64)> {
        let (tx, mut rx) = mpsc::channel::<Chunk>(64);
        let reader = tokio::spawn(async move {
            let mut s = String::new();
            while let Some(c) = rx.recv().await {
                if c.stream == StreamKind::Stdout {
                    s.push_str(&String::from_utf8_lossy(&c.data));
                }
            }
            s
        });
        let code = self.deps.backend.exec(id, spec, tx).await;
        let out = reader.await.unwrap_or_default();
        Ok((out, code?))
    }

    /// Hands the workspace to the runner's user, so the runner's own file operations work.
    async fn own_for_runner(&mut self) {
        if self.owner == Owner::Runner || !self.dirty {
            return;
        }
        let (Some(id), Some((uid, gid))) = (self.container.clone(), self.uid_gid) else { return };
        if self.chown(&id, &format!("{uid}:{gid}")).await {
            self.owner = Owner::Runner;
            self.dirty = false;
        }
    }

    /// Hands it back to a non-root container user before that user's step runs.
    async fn own_for_container(&mut self) {
        if self.owner == Owner::Container {
            return;
        }
        if let (Some(user), Some(id)) = (self.effective.user.clone(), self.container.clone()) {
            let owner = if user.contains(':') { user } else { format!("{user}:") };
            if self.chown(&id, &owner).await {
                self.owner = Owner::Container;
            }
        }
    }

    async fn chown(&self, id: &str, owner: &str) -> bool {
        let root = self.dirs.root.to_string_lossy().into_owned();
        let spec = ExecSpec {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"chown -R "$1" "$2" 2>/dev/null; chmod -R u+rwX "$2" 2>/dev/null; exit 0"#.into(),
                "haven-chown".into(),
                owner.into(),
                root,
            ],
            env: BTreeMap::new(),
            workdir: None,
            user: Some("0".into()),
        };
        matches!(tokio::time::timeout(Duration::from_secs(300), self.exec_collect(id, &spec)).await, Ok(Ok((_, 0))))
    }

    async fn teardown(&mut self) {
        for (sid, id) in std::mem::take(&mut self.services) {
            if let Err(e) = self.deps.backend.remove(&id).await {
                tracing::warn!(job_id = self.payload.job_id, "cannot remove the {sid} service container {id}: {e:#}");
            }
        }
        // Take the files back before the container goes: it is the only thing that can.
        if let Some(id) = self.container.clone() {
            if let Some((uid, gid)) = self.uid_gid
                && !self.chown(&id, &format!("{uid}:{gid}")).await
            {
                self.reclaim_with_helper(uid, gid).await;
            }
            if let Err(e) = self.deps.backend.remove(&id).await {
                tracing::warn!(job_id = self.payload.job_id, "cannot remove container {id}: {e:#}");
            }
        }
        if let Err(e) = self.dirs.remove() {
            tracing::warn!(job_id = self.payload.job_id, "cannot remove {}: {e}", self.dirs.root.display());
        }
    }

    /// The job container died, so it cannot chown its own files. A throwaway container can.
    async fn reclaim_with_helper(&self, uid: u32, gid: u32) {
        let root = self.dirs.root.to_string_lossy().into_owned();
        let spec = ContainerSpec {
            name: format!("codehaven-reclaim-{}-{:04x}", self.payload.job_id, rand::random::<u16>()),
            image: self.image.clone(),
            labels: BTreeMap::from([(LABEL_RUNNER.to_string(), self.deps.runner.id.to_string())]),
            env: BTreeMap::new(),
            binds: vec![format!("{root}:{root}")],
            workdir: None,
            user: Some("0".into()),
            cpus: 1.0,
            memory: 512 << 20,
            pids: 256,
            shm_size: 64 << 20,
            entrypoint: Some(vec!["/bin/sh".into()]),
            cmd: Some(vec!["-c".into(), format!("chown -R {uid}:{gid} '{root}'; chmod -R u+rwX '{root}'; exit 0")]),
            network_of: None,
            extra_hosts: Vec::new(),
        };
        if let Ok(id) = self.deps.backend.create_and_start(&spec).await {
            let (tx, mut rx) = mpsc::channel::<Chunk>(8);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let _ = tokio::time::timeout(Duration::from_secs(120), self.deps.backend.stream_until_exit(&id, tx)).await;
            let _ = drain.await;
            let _ = self.deps.backend.remove(&id).await;
        }
    }

    // ---- expressions -----------------------------------------------------------------------

    fn status(&self) -> JobStatus {
        if self.cancelled || self.timed_out || self.aborted {
            JobStatus::Cancelled
        } else if self.failed {
            JobStatus::Failure
        } else {
            JobStatus::Success
        }
    }

    fn user_env(&self, step_env: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
        let mut m = self.job_env.clone();
        m.extend(self.github_env.clone());
        m.extend(self.scope_env.clone());
        if let Some(s) = step_env {
            m.extend(s.clone());
        }
        m
    }

    /// Builds the expression context and runs `f` with it.
    fn with_ctx<R>(&self, step_env: Option<&BTreeMap<String, String>>, f: impl FnOnce(&EvalContext<'_>) -> R) -> R {
        let ws = self.dirs.workspace.clone();
        let hash = move |p: &[String]| fsglob::hash_files(&ws, p).map_err(|e| e.to_string());
        let mut ctx = EvalContext::new(self.status());
        ctx.hash_files = Some(&hash);
        ctx.set("github", self.github_v.clone());
        ctx.set("codehaven", self.github_v.clone());
        ctx.set("env", jobenv::map_to_value(&self.user_env(step_env)));
        ctx.set("vars", self.vars_v.clone());
        ctx.set("secrets", self.secrets_v.clone());
        ctx.set("needs", self.needs_v.clone());
        ctx.set("matrix", self.matrix_v.clone());
        ctx.set("inputs", self.inputs_v.clone());
        ctx.set("runner", self.runner_v.clone());
        ctx.set("strategy", json!({}));
        let services: serde_json::Map<String, Value> = self
            .payload
            .services
            .iter()
            .map(|svc| {
                let id = self.services.iter().find(|(k, _)| *k == svc.id).map(|(_, c)| c.clone()).unwrap_or_default();
                let ports: serde_json::Map<String, Value> = svc.ports.iter().map(|p| (p.to_string(), json!(p.to_string()))).collect();
                (svc.id.clone(), json!({"id": id, "network": "", "ports": ports}))
            })
            .collect();
        ctx.set(
            "job",
            json!({"status": status_word(self.status()), "container": {"id": self.container.clone().unwrap_or_default(), "network": ""}, "services": services}),
        );
        let steps: serde_json::Map<String, Value> = self
            .steps
            .iter()
            .map(|(k, s)| {
                (k.clone(), json!({"outputs": jobenv::map_to_value(&s.outputs), "outcome": s.outcome, "conclusion": s.conclusion}))
            })
            .collect();
        ctx.set("steps", Value::Object(steps));
        f(&ctx)
    }

    /// Takes the contexts a composite action may not read out of sight: `secrets`, `vars`,
    /// `needs` and `matrix`. Whatever it needs from them, the step that uses it passes in `with:`
    /// (or `env:`), which is evaluated before this. Gives them back with [`Self::show_job_contexts`].
    fn hide_job_contexts(&mut self) -> [Value; 4] {
        [
            std::mem::replace(&mut self.secrets_v, json!({})),
            std::mem::replace(&mut self.vars_v, json!({})),
            std::mem::replace(&mut self.needs_v, json!({})),
            std::mem::replace(&mut self.matrix_v, json!({})),
        ]
    }

    fn show_job_contexts(&mut self, held: [Value; 4]) {
        let [secrets, vars, needs, matrix] = held;
        self.secrets_v = secrets;
        self.vars_v = vars;
        self.needs_v = needs;
        self.matrix_v = matrix;
    }

    fn interp_hiding_job_contexts(&mut self, template: &str, step_env: Option<&BTreeMap<String, String>>) -> Result<String, String> {
        let held = self.hide_job_contexts();
        let r = self.interp(template, step_env);
        self.show_job_contexts(held);
        r
    }

    fn interp(&self, template: &str, step_env: Option<&BTreeMap<String, String>>) -> Result<String, String> {
        self.with_ctx(step_env, |c| expr::interpolate(template, c)).map_err(|e| e.message)
    }

    // ---- steps -----------------------------------------------------------------------------

    async fn run_step(&mut self, step: &StepSpec) {
        let n = step.number;
        self.observe_stops();

        let condition = step.condition.as_deref().unwrap_or("success()");
        if mentions_hash_files(&[condition]) {
            self.own_for_runner().await;
        }
        let go = self.with_ctx(None, |c| expr::eval_condition(condition, c));
        match go {
            Ok(true) => {}
            Ok(false) => {
                self.record(step, "skipped", "skipped", BTreeMap::new());
                self.update(
                    n,
                    StepUpdate { name: None, status: "completed".into(), conclusion: Some("skipped".into()), outputs: BTreeMap::new() },
                )
                .await;
                return;
            }
            Err(e) => {
                self.update(n, StepUpdate { name: None, status: "in_progress".into(), conclusion: None, outputs: BTreeMap::new() }).await;
                self.log.line(n, &format!("##[error]Cannot evaluate the condition of this step: {e}")).await;
                self.failed = true;
                self.record(step, "failure", "failure", BTreeMap::new());
                self.update(
                    n,
                    StepUpdate { name: None, status: "completed".into(), conclusion: Some("failure".into()), outputs: BTreeMap::new() },
                )
                .await;
                return;
            }
        }

        self.update(n, StepUpdate { name: None, status: "in_progress".into(), conclusion: None, outputs: BTreeMap::new() }).await;
        let run = AssertUnwindSafe(self.run_one(step)).catch_unwind().await.unwrap_or_else(|_| StepRun::failed());

        let (outcome, mut conclusion) = match run.end {
            StepEnd::Success => ("success", "success"),
            StepEnd::Failure => ("failure", "failure"),
            StepEnd::Cancelled => ("cancelled", "cancelled"),
            StepEnd::TimedOut => ("failure", "failure"),
        };
        match run.end {
            StepEnd::Success => {}
            StepEnd::Cancelled => {
                self.cancelled = true;
            }
            StepEnd::TimedOut | StepEnd::Failure => {
                if step.continue_on_error {
                    conclusion = "success";
                } else {
                    self.failed = true;
                }
            }
        }
        self.record(step, outcome, conclusion, run.outputs.clone());
        self.update(n, StepUpdate { name: None, status: "completed".into(), conclusion: Some(conclusion.into()), outputs: run.outputs })
            .await;
    }

    fn record(&mut self, step: &StepSpec, outcome: &str, conclusion: &str, outputs: BTreeMap<String, String>) {
        if let Some(id) = step.id.as_ref().filter(|i| !i.is_empty()) {
            self.steps.insert(id.clone(), StepCtx { outputs, outcome: outcome.into(), conclusion: conclusion.into() });
        }
    }

    async fn update(&self, n: i32, u: StepUpdate) {
        if let Err(e) = self.deps.client.update_step(self.payload.job_id, n, &u).await {
            tracing::warn!(job_id = self.payload.job_id, step = n, "cannot report step state: {e}");
        }
    }

    /// Notices a cancel or timeout that happened between steps.
    fn observe_stops(&mut self) {
        if self.ctl.hard_stop.is_cancelled() {
            self.aborted = true;
        }
        if self.ctl.cancel.is_cancelled() && !self.cancelled {
            self.cancelled = true;
            self.cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
        }
        if !self.timed_out && Instant::now() >= self.deadline {
            self.timed_out = true;
            self.cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
        }
    }

    async fn run_one(&mut self, step: &StepSpec) -> StepRun {
        let n = step.number;
        // The step's own env, evaluated against the job's env and what earlier steps exported.
        let mut step_env = BTreeMap::new();
        for (k, v) in &step.env {
            match self.interp(v, None) {
                Ok(s) => {
                    step_env.insert(k.clone(), s);
                }
                Err(e) => {
                    self.log.line(n, &format!("##[error]env {k}: {e}")).await;
                    return StepRun::failed();
                }
            }
        }
        let files = match self.dirs.step_files(n) {
            Ok(f) => f,
            Err(e) => {
                self.log.line(n, &format!("##[error]Cannot create the step's files: {e}")).await;
                return StepRun::failed();
            }
        };
        let mut run = match &step.kind {
            StepKind::Run { run, shell, working_directory } => {
                self.run_script(step, &step_env, &files, run, shell.as_deref(), working_directory.as_deref()).await
            }
            StepKind::Uses { uses, inputs } => self.run_uses(step, &step_env, &files, uses, inputs).await,
        };
        self.collect_files(n, &files, &mut run).await;
        run
    }

    /// When this step must be stopped, and which limit that is.
    fn step_timeout(&self, step: &StepSpec) -> (Instant, Limit) {
        let (hard, hard_kind) = match self.cleanup_deadline {
            Some(c) => (c, Limit::Cleanup),
            None => (self.deadline, Limit::Job),
        };
        match step.timeout_minutes.filter(|m| *m > 0) {
            Some(m) => {
                let t = Instant::now() + self.deps.minute * m;
                if t < hard { (t, Limit::Step(m)) } else { (hard, hard_kind) }
            }
            None => (hard, hard_kind),
        }
    }

    fn exec_env(&self, step_env: &BTreeMap<String, String>, files: &StepFiles) -> BTreeMap<String, String> {
        let mut env = self.base_env.clone();
        env.extend(self.user_env(Some(step_env)));
        for (k, p) in [("OUTPUT", &files.output), ("ENV", &files.env), ("PATH", &files.path), ("STEP_SUMMARY", &files.summary)] {
            let v = p.to_string_lossy().into_owned();
            env.insert(format!("GITHUB_{k}"), v.clone());
            env.insert(format!("CODEHAVEN_{k}"), v);
        }
        if !self.extra_path.is_empty() {
            let current = env.get("PATH").cloned().or_else(|| self.base_path.clone()).unwrap_or_default();
            let mut parts = self.extra_path.clone();
            if !current.is_empty() {
                parts.push(current);
            }
            env.insert("PATH".into(), parts.join(":"));
        }
        env
    }

    async fn run_script(
        &mut self,
        step: &StepSpec,
        step_env: &BTreeMap<String, String>,
        files: &StepFiles,
        run: &str,
        shell: Option<&str>,
        wd: Option<&str>,
    ) -> StepRun {
        let n = step.number;
        if mentions_hash_files(&[run]) {
            self.own_for_runner().await;
        }
        let script = match self.interp(run, Some(step_env)) {
            Ok(s) => s,
            Err(e) => {
                self.log.line(n, &format!("##[error]{e}")).await;
                return StepRun::failed();
            }
        };
        let shell_name = shell.map(String::from).or_else(|| self.payload.default_shell.clone());
        let plan = match shell::plan(shell_name.as_deref(), self.has_bash) {
            Ok(p) => p,
            Err(e) => {
                self.log.line(n, &format!("##[error]{e}")).await;
                return StepRun::failed();
            }
        };
        let wd = wd.map(String::from).or_else(|| self.payload.default_working_directory.clone());
        let workdir = match wd {
            Some(w) => match self.interp(&w, Some(step_env)) {
                Ok(w) if w.starts_with('/') => w,
                Ok(w) => self.dirs.workspace.join(w).to_string_lossy().into_owned(),
                Err(e) => {
                    self.log.line(n, &format!("##[error]working-directory: {e}")).await;
                    return StepRun::failed();
                }
            },
            None => self.dirs.workspace.to_string_lossy().into_owned(),
        };

        let script_path = self.dirs.script_path(n, plan.ext);
        if let Err(e) = self.dirs.write_file(&script_path, format!("{script}\n").as_bytes(), 0o644) {
            self.log.line(n, &format!("##[error]Cannot write the step's script: {e}")).await;
            return StepRun::failed();
        }
        self.log.line(n, &format!("##[group]Run {}", crate::fmt_first_line(&script))).await;
        for l in script.lines() {
            self.log.line(n, l).await;
        }
        self.log.line(n, "##[endgroup]").await;

        let pid_path = self.dirs.pid_path(n);
        let argv = shell::wrap_with_pidfile(&pid_path.to_string_lossy(), &plan.argv(&script_path.to_string_lossy()));
        let spec = ExecSpec { argv, env: self.exec_env(step_env, files), workdir: Some(workdir), user: None };
        self.own_for_container().await;
        self.dirty = true;
        self.owner = Owner::Container;
        let Some(id) = self.container.clone() else { return StepRun::failed() };
        let (until, limit) = self.step_timeout(step);
        let (end, set_outputs) = self.run_exec(&id, n, spec, &pid_path, until, limit).await;
        let mut outputs = BTreeMap::new();
        outputs.extend(set_outputs);
        StepRun { end, outputs }
    }

    /// Runs one command in the job container with output going to the log, cancellable and
    /// bounded in time. Returns how it ended and any outputs set with `::set-output`.
    async fn run_exec(
        &mut self,
        id: &str,
        n: i32,
        spec: ExecSpec,
        pid_path: &std::path::Path,
        until: Instant,
        limit: Limit,
    ) -> (StepEnd, Vec<(String, String)>) {
        let (tx, rx) = mpsc::channel::<Chunk>(64);
        let reader = tokio::spawn(read_output(rx, self.log.clone(), n, self.debug));
        let backend = self.deps.backend.clone();
        let spec2 = spec.clone();
        let id2 = id.to_string();
        let already_cancelled = self.cancelled;
        // The command's future owns the output channel's sender. It is dropped at the end of
        // this block, which closes the channel and lets the reader finish, even when the
        // command had to be abandoned.
        let ended = {
            let exec = async move { backend.exec(&id2, &spec2, tx).await };
            tokio::pin!(exec);
            let ended = tokio::select! {
                r = &mut exec => match r { Ok(code) => ExecEnd::Exit(code), Err(e) => ExecEnd::Error(format!("{e:#}")) },
                _ = tokio::time::sleep_until(until.into()) => ExecEnd::Timeout,
                _ = self.ctl.cancel.cancelled(), if !already_cancelled => ExecEnd::Cancel,
                _ = self.ctl.hard_stop.cancelled() => ExecEnd::HardStop,
            };
            if matches!(ended, ExecEnd::Timeout | ExecEnd::Cancel | ExecEnd::HardStop) {
                self.kill_step(id, pid_path).await;
                // The command's output ends once it is dead.
                let _ = tokio::time::timeout(Duration::from_secs(10), &mut exec).await;
            }
            ended
        };
        let set_outputs = reader.await.unwrap_or_default();

        let end = match ended {
            ExecEnd::Exit(0) => StepEnd::Success,
            ExecEnd::Exit(code) => {
                self.log.line(n, &format!("##[error]Process completed with exit code {code}.")).await;
                StepEnd::Failure
            }
            ExecEnd::Error(e) => {
                self.log.line(n, &format!("##[error]{e}")).await;
                StepEnd::Failure
            }
            ExecEnd::Timeout => {
                self.timeout_end(n, limit).await;
                StepEnd::TimedOut
            }
            ExecEnd::Cancel => {
                self.log.line(n, "##[error]The run was cancelled.").await;
                self.cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
                StepEnd::Cancelled
            }
            ExecEnd::HardStop => {
                self.log.line(n, "##[error]The runner is shutting down. The job was stopped.").await;
                self.aborted = true;
                self.cancelled = true;
                StepEnd::Cancelled
            }
        };
        (end, set_outputs)
    }

    /// Logs why a limit ended a step, and starts the cleanup window when it was the job's.
    async fn timeout_end(&mut self, n: i32, limit: Limit) {
        match limit {
            Limit::Step(m) => self.log.line(n, &format!("##[error]The step exceeded its time limit of {m} minutes.")).await,
            Limit::Job => {
                self.timed_out = true;
                self.cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
                let m = self.deps.cfg.job_minutes(self.payload.timeout_minutes);
                self.log.line(n, &format!("##[error]The job exceeded its time limit of {m} minutes.")).await;
            }
            Limit::Cleanup => self.log.line(n, "##[error]The cleanup step ran past the time allowed after a cancel or timeout.").await,
        }
    }

    async fn kill_step(&self, id: &str, pid_path: &std::path::Path) {
        let spec = ExecSpec {
            argv: shell::kill_tree_argv(&pid_path.to_string_lossy()),
            env: BTreeMap::new(),
            workdir: None,
            user: Some("0".into()),
        };
        let _ = tokio::time::timeout(Duration::from_secs(20), self.exec_collect(id, &spec)).await;
    }

    async fn run_uses(
        &mut self,
        step: &StepSpec,
        step_env: &BTreeMap<String, String>,
        files: &StepFiles,
        uses: &str,
        raw_inputs: &BTreeMap<String, String>,
    ) -> StepRun {
        let n = step.number;
        let mut inputs = BTreeMap::new();
        for (k, v) in raw_inputs {
            match self.interp(v, Some(step_env)) {
                Ok(s) => {
                    inputs.insert(k.to_ascii_lowercase(), s);
                }
                Err(e) => {
                    self.log.line(n, &format!("##[error]with.{k}: {e}")).await;
                    return StepRun::failed();
                }
            }
        }
        if raw_inputs.values().any(|v| mentions_hash_files(&[v])) {
            self.own_for_runner().await;
        }
        self.log.line(n, &format!("##[group]Run {uses}")).await;
        for (k, v) in &inputs {
            self.log.line(n, &format!("  {k}: {v}")).await;
        }
        self.log.line(n, "##[endgroup]").await;

        let builtin = actions::resolve(uses);
        if matches!(builtin, Builtin::Remote(_) | Builtin::Local(_)) {
            return self.run_action(step, step_env, files, &builtin, &inputs, uses).await;
        }
        if let Builtin::Docker(image) = &builtin {
            return self.run_docker_step(step, step_env, files, image, &inputs).await;
        }
        if let Builtin::Unsupported(msg) = &builtin {
            self.log.line(n, &format!("##[error]{msg}")).await;
            return StepRun::failed();
        }

        // Built-in actions work on the workspace from the runner's side.
        self.own_for_runner().await;
        let local_cancel = CancellationToken::new();
        let (cancel, hard) = (self.ctl.cancel.clone(), self.ctl.hard_stop.clone());
        let ac = ActionCtx {
            step: n,
            inputs: &inputs,
            dirs: &self.dirs,
            payload: self.payload,
            client: &self.deps.client,
            log: &self.log,
            cancel: &local_cancel,
        };
        let already_cancelled = self.cancelled;
        let fut = async {
            match &builtin {
                Builtin::Checkout => actions::checkout::run(&ac).await,
                Builtin::Cache(mode) => actions::cache::run(&ac, *mode).await,
                Builtin::UploadArtifact => actions::artifact::upload(&ac).await,
                Builtin::DownloadArtifact => actions::artifact::download(&ac).await,
                Builtin::Docker(_) | Builtin::Unsupported(_) | Builtin::Remote(_) | Builtin::Local(_) => Ok(ActionOutput::default()),
            }
        };
        let (until, limit) = self.step_timeout(step);
        let mut timed_out_limit = None;
        let result = tokio::select! {
            r = fut => Some(r),
            _ = cancel.cancelled(), if !already_cancelled => { local_cancel.cancel(); None },
            _ = hard.cancelled() => { local_cancel.cancel(); self.aborted = true; None },
            _ = tokio::time::sleep_until(until.into()) => { local_cancel.cancel(); timed_out_limit = Some(limit); None },
        };
        if let Some(l) = timed_out_limit {
            self.timeout_end(n, l).await;
            return StepRun { end: StepEnd::TimedOut, outputs: BTreeMap::new() };
        }
        match result {
            None => {
                self.log.line(n, "##[error]The run was cancelled.").await;
                self.cancelled = true;
                StepRun { end: StepEnd::Cancelled, outputs: BTreeMap::new() }
            }
            Some(Ok(out)) => {
                if let Some(p) = out.post {
                    self.posts.push(p);
                }
                StepRun { end: StepEnd::Success, outputs: out.outputs }
            }
            Some(Err(e)) => {
                self.log.line(n, &format!("##[error]{e:#}")).await;
                StepRun::failed()
            }
        }
    }

    async fn run_docker_step(
        &mut self,
        step: &StepSpec,
        step_env: &BTreeMap<String, String>,
        files: &StepFiles,
        image: &str,
        inputs: &BTreeMap<String, String>,
    ) -> StepRun {
        let n = step.number;
        let backend = self.deps.backend.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let log = self.log.clone();
        let fwd = tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                log.line(n, &l).await;
            }
        });
        let pulled = backend.ensure_image(image, self.deps.cfg.always_pull(), &tx).await;
        drop(tx);
        let _ = fwd.await;
        if let Err(e) = pulled {
            self.log.line(n, &format!("##[error]{e:#}")).await;
            return StepRun::failed();
        }
        let cmd = match inputs.get("args").map(|a| shell_words::split(a)).transpose() {
            Ok(c) => c.filter(|c| !c.is_empty()),
            Err(e) => {
                self.log.line(n, &format!("##[error]args: {e}")).await;
                return StepRun::failed();
            }
        };
        let entrypoint = match inputs.get("entrypoint").filter(|e| !e.trim().is_empty()).map(|e| shell_words::split(e)).transpose() {
            Ok(c) => c,
            Err(e) => {
                self.log.line(n, &format!("##[error]entrypoint: {e}")).await;
                return StepRun::failed();
            }
        };
        let root = self.dirs.root.to_string_lossy().into_owned();
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_RUNNER.to_string(), self.deps.runner.id.to_string());
        labels.insert(LABEL_JOB.to_string(), self.payload.job_id.to_string());
        let spec = ContainerSpec {
            name: format!("codehaven-step-{}-{}-{:04x}", self.deps.runner.id, self.payload.job_id, rand::random::<u16>()),
            image: image.to_string(),
            labels,
            env: self.exec_env(step_env, files),
            binds: vec![format!("{root}:{root}")],
            workdir: Some(self.dirs.workspace.to_string_lossy().into_owned()),
            user: self.effective.user.clone(),
            cpus: self.effective.cpus,
            memory: self.effective.memory,
            pids: self.effective.pids,
            shm_size: self.effective.shm_size,
            entrypoint,
            cmd,
            network_of: None,
            extra_hosts: Vec::new(),
        };
        self.own_for_container().await;
        self.dirty = true;
        self.owner = Owner::Container;
        let id = match backend.create_and_start(&spec).await {
            Ok(id) => id,
            Err(e) => {
                self.log.line(n, &format!("##[error]{e:#}")).await;
                return StepRun::failed();
            }
        };
        let (ctx_tx, ctx_rx) = mpsc::channel::<Chunk>(64);
        let reader = tokio::spawn(read_output(ctx_rx, self.log.clone(), n, self.debug));
        let (until, limit) = self.step_timeout(step);
        let b2 = backend.clone();
        let id2 = id.clone();
        let already_cancelled = self.cancelled;
        let ended = {
            let stream = async move { b2.stream_until_exit(&id2, ctx_tx).await };
            tokio::pin!(stream);
            let ended = tokio::select! {
                r = &mut stream => match r { Ok(code) => ExecEnd::Exit(code), Err(e) => ExecEnd::Error(format!("{e:#}")) },
                _ = tokio::time::sleep_until(until.into()) => ExecEnd::Timeout,
                _ = self.ctl.cancel.cancelled(), if !already_cancelled => ExecEnd::Cancel,
                _ = self.ctl.hard_stop.cancelled() => ExecEnd::HardStop,
            };
            // Removing the container stops it and ends the output stream.
            if let Err(e) = backend.remove(&id).await {
                tracing::warn!("cannot remove step container {id}: {e:#}");
            }
            ended
        };
        let set_outputs = reader.await.unwrap_or_default();
        let end = match ended {
            ExecEnd::Exit(0) => StepEnd::Success,
            ExecEnd::Exit(c) => {
                self.log.line(n, &format!("##[error]Process completed with exit code {c}.")).await;
                StepEnd::Failure
            }
            ExecEnd::Error(e) => {
                self.log.line(n, &format!("##[error]{e}")).await;
                StepEnd::Failure
            }
            ExecEnd::Timeout => {
                self.timeout_end(n, limit).await;
                StepEnd::TimedOut
            }
            ExecEnd::Cancel => {
                self.log.line(n, "##[error]The run was cancelled.").await;
                self.cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
                StepEnd::Cancelled
            }
            ExecEnd::HardStop => {
                self.aborted = true;
                StepEnd::Cancelled
            }
        };
        StepRun { end, outputs: set_outputs.into_iter().collect() }
    }

    /// One of a step's files. A file over the size limit fails the step with an error saying so;
    /// one the job removed or replaced with a link or something else is quietly left out.
    async fn read_step_file(&mut self, n: i32, path: &std::path::Path, name: &str, run: &mut StepRun) -> Option<String> {
        match self.dirs.read_file(path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                let max = crate::jobenv::MAX_STEP_FILE / (1024 * 1024);
                self.log.line(n, &format!("##[error]{name} is larger than {max} MiB, so the runner did not read it.")).await;
                if matches!(run.end, StepEnd::Success) {
                    run.end = StepEnd::Failure;
                }
                None
            }
            Err(_) => None,
        }
    }

    /// Reads `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_PATH` and `$GITHUB_STEP_SUMMARY` after a step.
    async fn collect_files(&mut self, n: i32, files: &StepFiles, run: &mut StepRun) {
        if let Some(t) = self.read_step_file(n, &files.output, "$GITHUB_OUTPUT", run).await {
            match commands::parse_env_file(&t) {
                Ok(pairs) => {
                    for (k, v) in pairs {
                        run.outputs.insert(k, v);
                    }
                }
                Err(e) => {
                    self.log.line(n, &format!("##[error]$GITHUB_OUTPUT: {e}")).await;
                    if matches!(run.end, StepEnd::Success) {
                        run.end = StepEnd::Failure;
                    }
                }
            }
        }
        if let Some(t) = self.read_step_file(n, &files.env, "$GITHUB_ENV", run).await {
            match commands::parse_env_file(&t) {
                Ok(pairs) => {
                    for (k, v) in pairs {
                        if commands::is_reserved_env_name(&k) {
                            self.log.line(n, &format!("##[warning]$GITHUB_ENV cannot set {k}: the runner owns that name.")).await;
                        } else {
                            self.github_env.insert(k, v);
                        }
                    }
                }
                Err(e) => {
                    self.log.line(n, &format!("##[error]$GITHUB_ENV: {e}")).await;
                    if matches!(run.end, StepEnd::Success) {
                        run.end = StepEnd::Failure;
                    }
                }
            }
        }
        if let Some(t) = self.read_step_file(n, &files.path, "$GITHUB_PATH", run).await {
            for p in commands::parse_path_file(&t) {
                self.extra_path.insert(0, p);
            }
        }
        if let Some(t) = self.read_step_file(n, &files.summary, "$GITHUB_STEP_SUMMARY", run).await
            && !t.trim().is_empty()
        {
            self.log.line(n, "##[group]Step summary").await;
            let lines: Vec<String> = t.lines().map(String::from).collect();
            self.log.lines(n, &lines).await;
            self.log.line(n, "##[endgroup]").await;
        }
    }

    async fn run_post(&mut self, p: PostAction) {
        match p {
            PostAction::Node { title, script, condition, env } => self.run_node_post(&title, &script, &condition, env).await,
            PostAction::CacheSave { key, version, patterns } => {
                self.own_for_runner().await;
                self.log.line(0, &format!("##[group]Post job cleanup: save cache {key}")).await;
                let local = CancellationToken::new();
                let inputs = BTreeMap::new();
                let ac = ActionCtx {
                    step: 0,
                    inputs: &inputs,
                    dirs: &self.dirs,
                    payload: self.payload,
                    client: &self.deps.client,
                    log: &self.log,
                    cancel: &local,
                };
                if let Err(e) = actions::cache::save(&ac, &key, &version, &patterns).await {
                    self.log.line(0, &format!("##[warning]The cache could not be saved: {e:#}")).await;
                }
                self.log.line(0, "##[endgroup]").await;
            }
        }
    }
}

// ---- service containers (ch-ctk.7) -----------------------------------------------------------

impl Engine<'_> {
    /// Starts each service in the job container's network, then waits until each port it lists
    /// takes a connection from inside the job, as `--health-cmd` would on GitHub.
    async fn start_services(&mut self, job_container: &str) -> anyhow::Result<()> {
        let backend = self.deps.backend.clone();
        let cfg = self.deps.cfg.clone();
        for svc in self.payload.services.clone() {
            let image = self.interp(&svc.image, None).map_err(|e| anyhow::anyhow!("services.{}.image: {e}", svc.id))?;
            let mut env = BTreeMap::new();
            for (k, v) in &svc.env {
                env.insert(k.clone(), self.interp(v, None).map_err(|e| anyhow::anyhow!("services.{}.env.{k}: {e}", svc.id))?);
            }
            // The same safe list as a job container, plus health checks, which are not run.
            let opts = match &svc.options {
                Some(o) => {
                    let o = self.interp(o, None).map_err(|e| anyhow::anyhow!("services.{}.options: {e}", svc.id))?;
                    ch_workflow::options::validate_service(&o).map_err(|e| anyhow::anyhow!("services.{}.options: {e}", svc.id))?
                }
                None => Default::default(),
            };
            env.extend(opts.env.iter().cloned());
            self.log.line(0, &format!("Starting service {} ({image})", svc.id)).await;
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let log = self.log.clone();
            let forward = tokio::spawn(async move {
                while let Some(l) = rx.recv().await {
                    log.line(0, &l).await;
                }
            });
            let pulled = backend.ensure_image(&image, cfg.always_pull(), &tx).await;
            drop(tx);
            let _ = forward.await;
            pulled?;
            let mut labels = BTreeMap::new();
            labels.insert(LABEL_RUNNER.to_string(), self.deps.runner.id.to_string());
            labels.insert(LABEL_JOB.to_string(), self.payload.job_id.to_string());
            labels.insert("org.codehaven".to_string(), "1".to_string());
            let spec = ContainerSpec {
                name: format!("codehaven-svc-{}-{}-{}-{:04x}", self.deps.runner.id, self.payload.job_id, svc.id, rand::random::<u16>()),
                image: image.clone(),
                labels,
                env,
                binds: Vec::new(),
                workdir: None,
                user: opts.user.clone(),
                cpus: self.effective.cpus,
                memory: self.effective.memory,
                pids: self.effective.pids,
                shm_size: opts.shm_bytes.unwrap_or(self.effective.shm_size),
                entrypoint: None,
                cmd: None,
                network_of: Some(job_container.to_string()),
                extra_hosts: Vec::new(),
            };
            let id = backend.create_and_start(&spec).await?;
            self.services.push((svc.id.clone(), id.clone()));
            for port in &svc.ports {
                self.wait_for_port(job_container, &svc.id, *port).await?;
            }
        }
        Ok(())
    }

    async fn wait_for_port(&self, job_container: &str, id: &str, port: u16) -> anyhow::Result<()> {
        let secs = self.deps.cfg.limits.service_wait_seconds.max(1);
        let script = format!(
            "i=0; while [ $i -lt {secs} ]; do (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && exit 0; i=$((i+1)); sleep 1; done; exit 1"
        );
        let probe = ExecSpec { argv: vec!["bash".into(), "-c".into(), script], env: BTreeMap::new(), workdir: None, user: None };
        let (_, code) = self.exec_collect(job_container, &probe).await?;
        if code != 0 {
            anyhow::bail!("the service {id} did not take connections on port {port} within {secs} seconds");
        }
        self.log.line(0, &format!("Service {id} is ready on port {port}.")).await;
        Ok(())
    }
}

// ---- actions that are not built in (ch-ctk.4) ------------------------------------------------

impl Engine<'_> {
    /// Runs a JavaScript or composite action from GitHub, from a repository here or from the
    /// workspace. `given` are the step's `with:` values, already evaluated, keys in lower case.
    async fn run_action(
        &mut self,
        step: &StepSpec,
        step_env: &BTreeMap<String, String>,
        files: &StepFiles,
        builtin: &Builtin,
        given: &BTreeMap<String, String>,
        uses: &str,
    ) -> StepRun {
        let n = step.number;
        if self.depth >= remote::MAX_DEPTH {
            self.log.line(n, &format!("##[error]{uses}: composite actions may be nested at most {} deep.", remote::MAX_DEPTH)).await;
            return StepRun::failed();
        }
        self.own_for_runner().await;
        let fetched = match builtin {
            Builtin::Remote(u) => remote::fetch_remote(&self.deps.client, &self.dirs, self.payload, u).await,
            Builtin::Local(u) => remote::fetch_local(&self.dirs, u),
            _ => Err(anyhow::anyhow!("{uses} is not an action the runner fetches")),
        };
        let f = match fetched {
            Ok(f) => f,
            Err(e) => {
                self.log.line(n, &format!("##[error]{e:#}")).await;
                return StepRun::failed();
            }
        };
        if let Some(sha) = &f.sha {
            self.log.line(n, &format!("Using {} at {sha}", f.repository)).await;
        }

        // Inputs: what the step gave, else the action's default (which may be an expression
        // such as `${{ github.token }}`). Inputs it does not declare are passed on with a warning.
        let mut inputs = BTreeMap::new();
        for i in &f.def.inputs {
            let v = match given.get(&i.id.to_ascii_lowercase()) {
                Some(v) => v.clone(),
                None => match &i.default {
                    // An action's defaults are the action author's text, so they cannot read the
                    // job's secrets, variables, needs or matrix (as on GitHub).
                    Some(d) => match self.interp_hiding_job_contexts(d, Some(step_env)) {
                        Ok(v) => v,
                        Err(e) => {
                            self.log.line(n, &format!("##[error]The default of input {}: {e}", i.id)).await;
                            return StepRun::failed();
                        }
                    },
                    None => {
                        if i.required {
                            self.log.line(n, &format!("##[warning]Input required and not supplied: {}", i.id)).await;
                        }
                        String::new()
                    }
                },
            };
            inputs.insert(i.id.clone(), v);
        }
        for (k, v) in given {
            if !f.def.inputs.iter().any(|i| i.id.eq_ignore_ascii_case(k)) {
                self.log.line(n, &format!("##[warning]Unexpected input '{k}'")).await;
                inputs.insert(k.clone(), v.clone());
            }
        }

        match f.def.runs.clone() {
            ActionRuns::Docker { .. } => {
                self.log
                    .line(n, &format!("##[error]{uses} is a Docker container action. Jobs here have no Docker daemon, so use a `docker://image` step or a `run:` step instead."))
                    .await;
                StepRun::failed()
            }
            ActionRuns::Node { main, pre, pre_if, post, post_if, .. } => {
                self.run_node(
                    step,
                    step_env,
                    files,
                    &f,
                    &inputs,
                    (&main, pre.as_deref(), pre_if.as_deref(), post.as_deref(), post_if.as_deref()),
                )
                .await
            }
            ActionRuns::Composite { steps } => self.run_composite(step, step_env, &f, &inputs, &steps).await,
        }
    }

    /// What every phase of an action sees: its inputs as `INPUT_*` and where it is.
    fn action_env(&self, step: &StepSpec, f: &remote::Fetched, inputs: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for (k, v) in inputs {
            env.insert(remote::input_env(k), v.clone());
        }
        env.insert("GITHUB_ACTION_PATH".into(), f.dir.to_string_lossy().into_owned());
        env.insert("GITHUB_ACTION_REPOSITORY".into(), f.repository.clone());
        env.insert("GITHUB_ACTION_REF".into(), f.git_ref.clone());
        let action = step.id.clone().filter(|i| !i.is_empty()).unwrap_or_else(|| format!("__{}", f.repository.replace('/', "_")));
        env.insert("GITHUB_ACTION".into(), action);
        // The toolkit's cache service (ch-ctk.6), at the public address, which job containers
        // can reach. The token is the job's own, in the shape the toolkit reads.
        let server = self.payload.context.server_url.trim_end_matches('/');
        let left = self.deadline.saturating_duration_since(Instant::now()) + CLEANUP_GRACE;
        let rt = remote::runtime_token(self.payload, left);
        self.log.masker().add(&rt);
        env.insert("ACTIONS_RUNTIME_TOKEN".into(), rt);
        env.insert("ACTIONS_CACHE_URL".into(), format!("{server}/api/runner/v1/toolkit/"));
        env.insert("ACTIONS_RUNTIME_URL".into(), format!("{server}/api/runner/v1/toolkit/"));
        env.insert("ACTIONS_RESULTS_URL".into(), format!("{server}/"));
        env
    }

    #[allow(clippy::type_complexity)]
    async fn run_node(
        &mut self,
        step: &StepSpec,
        step_env: &BTreeMap<String, String>,
        files: &StepFiles,
        f: &remote::Fetched,
        inputs: &BTreeMap<String, String>,
        phases: (&str, Option<&str>, Option<&str>, Option<&str>, Option<&str>),
    ) -> StepRun {
        let (main, pre, pre_if, post, post_if) = phases;
        let n = step.number;
        let mut env = step_env.clone();
        env.extend(self.action_env(step, f, inputs));
        let state = self.dirs.files.join(format!("step-{n}-state"));
        if let Err(e) = crate::safefs::create_new(&self.dirs.root, &state, 0o666) {
            self.log.line(n, &format!("##[error]Cannot create the step's state file: {e}")).await;
            return StepRun::failed();
        }
        env.insert("GITHUB_STATE".into(), state.to_string_lossy().into_owned());

        if let Some(pre) = pre {
            let go = self.with_ctx(Some(step_env), |c| expr::eval_condition(pre_if.unwrap_or("always()"), c)).unwrap_or(false);
            if go {
                self.log.line(n, "##[group]Pre step").await;
                let run = self.exec_node(step, n, &env, files, &f.dir.join(pre)).await;
                self.log.line(n, "##[endgroup]").await;
                if !matches!(run.end, StepEnd::Success) {
                    return run;
                }
            }
        }
        let run = self.exec_node(step, n, &env, files, &f.dir.join(main)).await;

        if let Some(post) = post {
            let mut penv = env.clone();
            if let Ok(t) = self.dirs.read_file(&state)
                && let Ok(pairs) = commands::parse_env_file(&t)
            {
                for (k, v) in pairs {
                    penv.insert(format!("STATE_{k}"), v);
                }
            }
            let title = step
                .name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| if f.repository.is_empty() { "action".to_string() } else { f.repository.clone() });
            self.posts.push(PostAction::Node {
                title,
                script: f.dir.join(post).to_string_lossy().into_owned(),
                condition: post_if.unwrap_or("always()").to_string(),
                env: penv,
            });
        }
        run
    }

    /// One `node <script>` in the job container, with the step's files and limits.
    async fn exec_node(
        &mut self,
        step: &StepSpec,
        n: i32,
        env: &BTreeMap<String, String>,
        files: &StepFiles,
        script: &std::path::Path,
    ) -> StepRun {
        if !self.has_node(env).await {
            self.log
                .line(n, "##[error]This action needs Node.js, and the job's image has no `node`. Use the default image, or a `container:` image with Node.")
                .await;
            return StepRun::failed();
        }
        let pid_path = self.dirs.pid_path(n);
        let argv = shell::wrap_with_pidfile(&pid_path.to_string_lossy(), &["node".to_string(), script.to_string_lossy().into_owned()]);
        let spec = ExecSpec {
            argv,
            env: self.exec_env(env, files),
            workdir: Some(self.dirs.workspace.to_string_lossy().into_owned()),
            user: None,
        };
        self.own_for_container().await;
        self.dirty = true;
        self.owner = Owner::Container;
        let Some(id) = self.container.clone() else { return StepRun::failed() };
        let (until, limit) = self.step_timeout(step);
        let (end, set_outputs) = self.run_exec(&id, n, spec, &pid_path, until, limit).await;
        StepRun { end, outputs: set_outputs.into_iter().collect() }
    }

    /// Whether `node` is on the job's PATH, asked once per job.
    async fn has_node(&mut self, env: &BTreeMap<String, String>) -> bool {
        if let Some(v) = self.node_found {
            return v;
        }
        let Some(id) = self.container.clone() else { return false };
        let mut probe_env = BTreeMap::new();
        if let Some(p) = self.exec_env(env, &StepFiles::none()).get("PATH") {
            probe_env.insert("PATH".to_string(), p.clone());
        }
        let probe =
            ExecSpec { argv: vec!["/bin/sh".into(), "-c".into(), "command -v node".into()], env: probe_env, workdir: None, user: None };
        let found = matches!(self.exec_collect(&id, &probe).await, Ok((_, 0)));
        self.node_found = Some(found);
        found
    }

    /// An action's post step, as a step of its own after the workflow's steps.
    async fn run_node_post(&mut self, title: &str, script: &str, condition: &str, env: BTreeMap<String, String>) {
        let go = self.with_ctx(None, |c| expr::eval_condition(condition, c)).unwrap_or(false);
        if !go {
            return;
        }
        let n = self.next_step;
        self.next_step += 1;
        let name = format!("Post {title}");
        self.update(n, StepUpdate { name: Some(name), status: "in_progress".into(), conclusion: None, outputs: BTreeMap::new() }).await;
        let files = match self.dirs.step_files(n) {
            Ok(f) => f,
            Err(e) => {
                self.log.line(n, &format!("##[error]Cannot create the step's files: {e}")).await;
                self.update(
                    n,
                    StepUpdate { name: None, status: "completed".into(), conclusion: Some("failure".into()), outputs: BTreeMap::new() },
                )
                .await;
                return;
            }
        };
        let spec = StepSpec {
            number: n,
            id: None,
            name: None,
            kind: StepKind::Run { run: String::new(), shell: None, working_directory: None },
            env: BTreeMap::new(),
            condition: None,
            continue_on_error: false,
            timeout_minutes: None,
        };
        let mut run = self.exec_node(&spec, n, &env, &files, std::path::Path::new(script)).await;
        self.collect_files(n, &files, &mut run).await;
        let conclusion = match run.end {
            StepEnd::Success => "success",
            StepEnd::Cancelled => "cancelled",
            StepEnd::Failure | StepEnd::TimedOut => {
                self.failed = true;
                "failure"
            }
        };
        self.update(
            n,
            StepUpdate { name: None, status: "completed".into(), conclusion: Some(conclusion.into()), outputs: BTreeMap::new() },
        )
        .await;
    }

    /// A composite action: its steps run one after another inside this step, with their own
    /// `steps` and `inputs` contexts, and its outputs are its `outputs.*.value` expressions.
    async fn run_composite(
        &mut self,
        step: &StepSpec,
        step_env: &BTreeMap<String, String>,
        f: &remote::Fetched,
        inputs: &BTreeMap<String, String>,
        steps: &[ch_workflow::Step],
    ) -> StepRun {
        let n = step.number;
        let saved_steps = std::mem::take(&mut self.steps);
        let saved_inputs = std::mem::replace(&mut self.inputs_v, jobenv::map_to_value(inputs));
        let saved_failed = std::mem::replace(&mut self.failed, false);
        let saved_github = self.github_v.clone();
        let mut scope = self.scope_env.clone();
        scope.extend(step_env.clone());
        scope.extend(self.action_env(step, f, inputs));
        let saved_scope = std::mem::replace(&mut self.scope_env, scope);
        if let Some(g) = self.github_v.as_object_mut() {
            g.insert("action_path".into(), json!(f.dir.to_string_lossy()));
            g.insert("action_repository".into(), json!(f.repository));
            g.insert("action_ref".into(), json!(f.git_ref));
        }
        self.depth += 1;
        // A composite action's steps read `github`, `env`, `inputs`, `steps` and `runner`, not the
        // job's secrets, variables, needs or matrix. An action from another repository must not be
        // able to read a secret it was not handed.
        let held = self.hide_job_contexts();

        for s in steps {
            self.observe_stops();
            if self.aborted {
                break;
            }
            let sub = StepSpec {
                number: n,
                id: s.id.clone(),
                name: s.name.clone(),
                kind: match &s.kind {
                    ch_workflow::StepKind::Run { run, shell, working_directory } => StepKind::Run {
                        run: run.clone(),
                        // A composite `run:` must name its shell on GitHub; bash is the usual one.
                        shell: Some(shell.clone().unwrap_or_else(|| "bash".into())),
                        working_directory: working_directory.clone(),
                    },
                    ch_workflow::StepKind::Uses { uses, with } => StepKind::Uses { uses: uses.clone(), inputs: with.clone() },
                },
                env: s.env.clone(),
                condition: s.condition.clone(),
                continue_on_error: s.continue_on_error.as_deref().is_some_and(|v| v.trim() == "true"),
                timeout_minutes: s.timeout_minutes.as_deref().and_then(|v| v.trim().parse().ok()),
            };
            Box::pin(self.run_sub_step(&sub)).await;
        }

        let mut outputs = BTreeMap::new();
        for (k, v) in &f.def.outputs {
            if let Some(src) = v {
                match self.interp(src, None) {
                    Ok(val) => {
                        outputs.insert(k.clone(), val);
                    }
                    Err(e) => self.log.line(n, &format!("##[warning]Output {k} of the action: {e}")).await,
                }
            }
        }
        let failed = self.failed;
        self.depth -= 1;
        self.show_job_contexts(held);
        self.steps = saved_steps;
        self.inputs_v = saved_inputs;
        self.failed = saved_failed;
        self.github_v = saved_github;
        self.scope_env = saved_scope;
        // The last inner step's files are read already; the step itself has none of its own.
        let _ = self.dirs.step_files(n);
        let end = if self.cancelled {
            StepEnd::Cancelled
        } else if failed {
            StepEnd::Failure
        } else {
            StepEnd::Success
        };
        StepRun { end, outputs }
    }

    /// One step of a composite action: like a workflow step, logged inside its parent and not
    /// reported to the server on its own.
    async fn run_sub_step(&mut self, step: &StepSpec) {
        let n = step.number;
        let condition = step.condition.as_deref().unwrap_or("success()");
        match self.with_ctx(None, |c| expr::eval_condition(condition, c)) {
            Ok(true) => {}
            Ok(false) => {
                self.record(step, "skipped", "skipped", BTreeMap::new());
                return;
            }
            Err(e) => {
                self.log.line(n, &format!("##[error]Cannot evaluate the condition of this step: {e}")).await;
                self.failed = true;
                self.record(step, "failure", "failure", BTreeMap::new());
                return;
            }
        }
        let run = self.run_one(step).await;
        let (outcome, mut conclusion) = match run.end {
            StepEnd::Success => ("success", "success"),
            StepEnd::Cancelled => ("cancelled", "cancelled"),
            StepEnd::Failure | StepEnd::TimedOut => ("failure", "failure"),
        };
        match run.end {
            StepEnd::Success => {}
            StepEnd::Cancelled => self.cancelled = true,
            StepEnd::Failure | StepEnd::TimedOut => {
                if step.continue_on_error {
                    conclusion = "success";
                } else {
                    self.failed = true;
                }
            }
        }
        self.record(step, outcome, conclusion, run.outputs);
    }
}

/// Reads chunks of output, turns them into masked log lines, and handles workflow commands.
/// Returns outputs set the old way with `::set-output`.
async fn read_output(mut rx: mpsc::Receiver<Chunk>, log: LogWriter, step: i32, debug: bool) -> Vec<(String, String)> {
    let mut split = LineSplitter::default();
    let mut parser = CommandParser::new(debug);
    let mut outputs = Vec::new();
    let mut handle = |lines: Vec<String>, log: &LogWriter, outputs: &mut Vec<(String, String)>| {
        let mut show = Vec::new();
        for l in lines {
            let h = parser.handle(&l);
            if let Some(m) = h.mask {
                log.masker().add(&m);
            }
            if let Some(o) = h.output {
                outputs.push(o);
            }
            if let Some(s) = h.show {
                show.push(s);
            }
        }
        show
    };
    while let Some(c) = rx.recv().await {
        let lines = split.push(c.stream == StreamKind::Stderr, &c.data);
        let show = handle(lines, &log, &mut outputs);
        log.lines(step, &show).await;
    }
    let show = handle(split.finish(), &log, &mut outputs);
    log.lines(step, &show).await;
    outputs
}

fn status_word(s: JobStatus) -> &'static str {
    match s {
        JobStatus::Success => "success",
        JobStatus::Failure => "failure",
        JobStatus::Cancelled => "cancelled",
    }
}

fn mentions_hash_files(texts: &[&str]) -> bool {
    texts.iter().any(|t| t.to_ascii_lowercase().contains("hashfiles"))
}

#[cfg(unix)]
fn owner_of(p: &std::path::Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| (m.uid(), m.gid()))
}

#[cfg(not(unix))]
fn owner_of(_p: &std::path::Path) -> Option<(u32, u32)> {
    None
}
