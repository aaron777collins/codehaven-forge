#![allow(dead_code)]
//! Shared setup for the integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ch_api_types::runner::JobPayload;
use haven_runner::backend::Backend;
use haven_runner::client::{Client, RetryPolicy};
use haven_runner::config::Config;
use haven_runner::job::{JobControl, JobDeps, JobResult, RunnerInfo};
use haven_runner::testing::{FakeServer, LocalBackend};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub struct Harness {
    pub server: FakeServer,
    pub backend: Arc<LocalBackend>,
    pub work: tempfile::TempDir,
    pub deps: JobDeps,
    pub client: Arc<Client>,
    pub credential: String,
}

pub fn fast_policy() -> RetryPolicy {
    RetryPolicy { base: Duration::from_millis(10), max: Duration::from_millis(80) }
}

impl Harness {
    pub async fn new() -> Harness {
        Harness::with_config(|_| {}).await
    }

    pub async fn with_config(f: impl FnOnce(&mut Config)) -> Harness {
        let server = FakeServer::start().await;
        let work = tempfile::tempdir().expect("tempdir");
        let (id, credential) = server.add_runner("test-runner", &["ubuntu-latest", "linux"]);
        let mut cfg = Config::default();
        cfg.runner.work_dir = work.path().join("work");
        cfg.disk.guard_paths = vec![cfg.runner.work_dir.clone()];
        f(&mut cfg);
        let client = Arc::new(Client::with_policy(&server.url, Some(credential.clone()), fast_policy()).expect("client"));
        let backend = Arc::new(LocalBackend::new());
        let dyn_backend: Arc<dyn Backend> = backend.clone();
        let deps = JobDeps {
            cfg: Arc::new(cfg),
            client: client.clone(),
            backend: dyn_backend,
            runner: RunnerInfo { id, name: "test-runner".into(), credential: credential.clone() },
            // A "minute" lasts a second, so timeouts can be tested.
            minute: Duration::from_secs(1),
        };
        Harness { server, backend, work, deps, client, credential }
    }

    pub fn work_dir(&self) -> &Path {
        &self.deps.cfg.runner.work_dir
    }

    /// Runs a job through the engine, with the heartbeat and the completion report, and returns
    /// what the server was told.
    pub async fn run(&self, job: JobPayload) -> JobResult {
        self.run_with(job, JobControl::default()).await
    }

    pub async fn run_with(&self, job: JobPayload, ctl: JobControl) -> JobResult {
        let id = job.job_id;
        self.server.register_job(&job);
        haven_runner::runner::execute(&self.deps, &self.client, job, ctl).await;
        let c = self.server.wait_complete(id, Duration::from_secs(20)).await;
        JobResult { conclusion: c.conclusion, outputs: c.outputs }
    }

    /// Everything left in the work directory.
    pub fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(self.work_dir())
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    }
}

/// True when a process whose command line contains `needle` is running.
pub fn process_running(needle: &str) -> bool {
    let Ok(rd) = std::fs::read_dir("/proc") else { return false };
    for e in rd.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmd) = std::fs::read(e.path().join("cmdline")) {
            let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
            if cmd.contains(needle) && !cmd.contains("process_running") {
                return true;
            }
        }
    }
    false
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
