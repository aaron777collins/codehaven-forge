//! In-process stand-ins used by the tests: a fake runner-protocol server, a backend that runs
//! commands on the host, and a builder for job payloads. Compiled only with the `testing`
//! feature, which no release build enables.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::result_large_err)]

pub mod fake_server;
pub mod local_backend;

pub use fake_server::FakeServer;
pub use local_backend::LocalBackend;

use ch_api_types::runner::{ContainerSpec, JobPayload, RepoInfo, RunContext, StepKind, StepSpec};
use serde_json::json;
use std::collections::BTreeMap;

/// Builds a `JobPayload` step by step.
pub struct TestJob {
    p: JobPayload,
}

impl TestJob {
    pub fn new(job_id: i64) -> TestJob {
        TestJob {
            p: JobPayload {
                job_id,
                run_id: 100 + job_id,
                repo: RepoInfo {
                    id: 1,
                    full_name: "alice/demo".into(),
                    clone_url: "http://127.0.0.1:1/alice/demo.git".into(),
                    default_branch: "main".into(),
                },
                job_key: "test".into(),
                name: format!("ci / test {job_id}"),
                runs_on: vec!["ubuntu-latest".into()],
                container: None,
                env: BTreeMap::new(),
                matrix: json!({}),
                needs: BTreeMap::new(),
                steps: vec![],
                outputs: BTreeMap::new(),
                secrets: BTreeMap::new(),
                vars: BTreeMap::new(),
                context: RunContext {
                    event_name: "push".into(),
                    git_ref: "refs/heads/main".into(),
                    sha: "a".repeat(40),
                    actor: "alice".into(),
                    repository: "alice/demo".into(),
                    run_id: 100 + job_id,
                    run_number: 1,
                    run_attempt: 1,
                    workflow: "ci".into(),
                    server_url: "http://127.0.0.1:1".into(),
                    api_url: "http://127.0.0.1:1/api/v3".into(),
                    event: json!({}),
                },
                default_shell: None,
                default_working_directory: None,
                timeout_minutes: 10,
                token: format!("job_token_{job_id}"),
                lease_seconds: 30,
                services: Vec::new(),
            },
        }
    }

    fn push(&mut self, kind: StepKind, id: Option<&str>) -> &mut StepSpec {
        let number = self.p.steps.len() as i32 + 1;
        self.p.steps.push(StepSpec {
            number,
            id: id.map(String::from),
            name: None,
            kind,
            env: BTreeMap::new(),
            condition: None,
            continue_on_error: false,
            timeout_minutes: None,
        });
        self.p.steps.last_mut().expect("just pushed")
    }

    pub fn run(mut self, script: &str) -> TestJob {
        self.push(StepKind::Run { run: script.into(), shell: None, working_directory: None }, None);
        self
    }

    pub fn run_id(mut self, id: &str, script: &str) -> TestJob {
        self.push(StepKind::Run { run: script.into(), shell: None, working_directory: None }, Some(id));
        self
    }

    pub fn run_shell(mut self, shell: &str, script: &str) -> TestJob {
        self.push(StepKind::Run { run: script.into(), shell: Some(shell.into()), working_directory: None }, None);
        self
    }

    pub fn uses(mut self, uses: &str, inputs: &[(&str, &str)]) -> TestJob {
        let inputs = inputs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.push(StepKind::Uses { uses: uses.into(), inputs }, None);
        self
    }

    pub fn uses_id(mut self, id: &str, uses: &str, inputs: &[(&str, &str)]) -> TestJob {
        let inputs = inputs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.push(StepKind::Uses { uses: uses.into(), inputs }, Some(id));
        self
    }

    /// Sets a modifier on the last step added.
    pub fn with(mut self, f: impl FnOnce(&mut StepSpec)) -> TestJob {
        if let Some(s) = self.p.steps.last_mut() {
            f(s);
        }
        self
    }

    pub fn condition(self, c: &str) -> TestJob {
        let c = c.to_string();
        self.with(move |s| s.condition = Some(c))
    }

    pub fn step_env(self, k: &str, v: &str) -> TestJob {
        let (k, v) = (k.to_string(), v.to_string());
        self.with(move |s| {
            s.env.insert(k, v);
        })
    }

    pub fn continue_on_error(self) -> TestJob {
        self.with(|s| s.continue_on_error = true)
    }

    pub fn step_timeout(self, minutes: u32) -> TestJob {
        self.with(move |s| s.timeout_minutes = Some(minutes))
    }

    pub fn env(mut self, k: &str, v: &str) -> TestJob {
        self.p.env.insert(k.into(), v.into());
        self
    }

    pub fn secret(mut self, k: &str, v: &str) -> TestJob {
        self.p.secrets.insert(k.into(), v.into());
        self
    }

    pub fn var(mut self, k: &str, v: &str) -> TestJob {
        self.p.vars.insert(k.into(), v.into());
        self
    }

    pub fn output(mut self, k: &str, expr: &str) -> TestJob {
        self.p.outputs.insert(k.into(), expr.into());
        self
    }

    pub fn timeout_minutes(mut self, m: u32) -> TestJob {
        self.p.timeout_minutes = m;
        self
    }

    pub fn container(mut self, image: &str, options: Option<&str>) -> TestJob {
        self.p.container = Some(ContainerSpec { image: image.into(), env: BTreeMap::new(), options: options.map(String::from) });
        self
    }

    pub fn clone_url(mut self, url: &str) -> TestJob {
        self.p.repo.clone_url = url.into();
        self
    }

    pub fn context(mut self, f: impl FnOnce(&mut RunContext)) -> TestJob {
        f(&mut self.p.context);
        self
    }

    pub fn matrix(mut self, v: serde_json::Value) -> TestJob {
        self.p.matrix = v;
        self
    }

    pub fn build(self) -> JobPayload {
        self.p
    }
}
