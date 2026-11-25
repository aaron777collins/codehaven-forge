//! The runner protocol, `/api/runner/v1` (ARCHITECTURE.md section 9.4).
//!
//! All JSON over HTTP. The server and `haven-runner` both depend on these types, so a
//! change here breaks both builds. Expressions inside the payload (`${{ ... }}`, `if:`)
//! stay unevaluated: the runner evaluates them with `ch-workflow` once it knows step
//! outputs.
//!
//! Authentication:
//! - `POST /register` needs no credential, only a single-use registration token in the body.
//! - Every other call sends `Authorization: Bearer <runner credential>`.
//! - Calls made by a job on its own behalf (artifacts, cache, statuses) use the job token
//!   from the payload.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `POST /register`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Admin-issued, single use, valid for 1 hour by default.
    pub token: String,
    pub name: String,
    pub labels: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub runner_id: i64,
    pub name: String,
    /// Shown once. The runner saves it to `.runner` with mode 0600. The server keeps its hash.
    pub credential: String,
}

/// `POST /jobs/request`. A long poll: the server answers within `wait_seconds` (30 at most).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JobRequest {
    #[serde(default)]
    pub wait_seconds: Option<u32>,
    /// Labels the runner has right now. A job is offered only when its `runs-on` labels are a
    /// subset of these.
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobResponse {
    /// `None` means nothing was queued before the wait ended. Ask again.
    pub job: Option<JobPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub id: i64,
    pub full_name: String,
    /// Where the built-in `checkout` fetches from, with no credentials in it.
    pub clone_url: String,
    pub default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Checked against the safe list server side and again by the runner.
    #[serde(default)]
    pub options: Option<String>,
}

/// What a finished `needs` job hands to the jobs after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsResult {
    /// `success`, `failure`, `cancelled` or `skipped`.
    pub result: String,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    Run {
        run: String,
        #[serde(default)]
        shell: Option<String>,
        #[serde(default)]
        working_directory: Option<String>,
    },
    Uses {
        /// `actions/checkout@v4`, `checkout`, `docker://alpine:3.20`.
        uses: String,
        #[serde(default, rename = "with")]
        inputs: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    /// 1-based, in file order.
    pub number: i32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(flatten)]
    pub kind: StepKind,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Unevaluated expression. Default is `success()`.
    #[serde(default, rename = "if")]
    pub condition: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
}

/// Values behind the `github` and `codehaven` contexts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunContext {
    pub event_name: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub actor: String,
    pub repository: String,
    pub run_id: i64,
    pub run_number: i64,
    pub run_attempt: i32,
    pub workflow: String,
    pub server_url: String,
    pub api_url: String,
    /// The webhook-style event payload, for `github.event.*`.
    #[serde(default)]
    pub event: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobPayload {
    pub job_id: i64,
    pub run_id: i64,
    pub repo: RepoInfo,
    pub job_key: String,
    pub name: String,
    pub runs_on: Vec<String>,
    #[serde(default)]
    pub container: Option<ContainerSpec>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub matrix: serde_json::Value,
    #[serde(default)]
    pub needs: BTreeMap<String, NeedsResult>,
    pub steps: Vec<StepSpec>,
    /// Job `outputs:` as unevaluated expressions, evaluated by the runner at the end.
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    /// Only the secrets this job references. Never present for fork pull request jobs.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    pub context: RunContext,
    #[serde(default)]
    pub default_shell: Option<String>,
    #[serde(default)]
    pub default_working_directory: Option<String>,
    pub timeout_minutes: u32,
    /// Minted for this job, valid until it ends, scoped to its repo.
    pub token: String,
    /// The server expects a heartbeat within this many seconds.
    pub lease_seconds: u32,
    /// Service containers (ch-ctk.7), started before the first step in the job container's
    /// network. Image and env may hold `${{ }}`, which the runner fills in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// The key under `services:`, also its host name inside the job.
    pub id: String,
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Container ports the job waits for before its first step.
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub options: Option<String>,
}

/// `POST /jobs/{id}/heartbeat`, every 30 seconds.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeartbeatRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// True when the run was cancelled. The runner stops the job and reports `cancelled`.
    pub cancel: bool,
}

/// `POST /jobs/{id}/logs`. `seq` makes retries idempotent: a batch the server already has
/// is acknowledged and dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogBatch {
    /// Step number, or 0 for job-level lines such as image pulls.
    pub step: i32,
    pub seq: u64,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogAck {
    pub next_seq: u64,
}

/// `POST /jobs/{id}/steps/{n}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepUpdate {
    /// `in_progress` or `completed`.
    pub status: String,
    /// For `completed`: `success`, `failure`, `cancelled` or `skipped`.
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    /// A step the runner adds while the job runs, such as an action's post step, named here on
    /// its first update. Its number comes after the workflow's steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `POST /jobs/{id}/complete`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteRequest {
    /// `success`, `failure`, `cancelled` or `timed_out`.
    pub conclusion: String,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}

/// `PUT /jobs/{id}/artifacts/{name}` streams the zip as the body and answers with this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUploaded {
    pub id: i64,
    pub name: String,
    pub size: i64,
}

/// `POST /cache/lookup`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheLookupRequest {
    /// The exact key first, then restore keys in order, each matched as a prefix.
    pub keys: Vec<String>,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheLookupResponse {
    pub hit: Option<CacheHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHit {
    pub id: i64,
    pub key: String,
    pub size: i64,
    /// True when the key matched exactly, false when a restore key matched.
    pub exact: bool,
}

/// `POST /jobs/{id}/actions/resolve`: the `uses:` values a job wants, as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResolveRequest {
    pub actions: Vec<String>,
}

/// One answer: a commit and an archive to fetch with `GET /jobs/{id}/actions/archive/{archive}`,
/// or why the action may not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResolution {
    pub uses: String,
    pub sha: Option<String>,
    /// sha256 of the tar.gz. Its files are under one top directory.
    pub archive: Option<String>,
    /// The action's folder inside the repository, `""` for the top.
    #[serde(default)]
    pub path: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResolveResponse {
    pub actions: Vec<ActionResolution>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> JobPayload {
        JobPayload {
            job_id: 11,
            run_id: 5,
            repo: RepoInfo {
                id: 3,
                full_name: "alice/demo".into(),
                clone_url: "https://code.example.com/alice/demo.git".into(),
                default_branch: "main".into(),
            },
            job_key: "test".into(),
            name: "ci / test (a)".into(),
            runs_on: vec!["ubuntu-latest".into()],
            container: Some(ContainerSpec { image: "rust:1-trixie".into(), env: BTreeMap::new(), options: Some("--cpus 2".into()) }),
            env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
            matrix: serde_json::json!({"os": "a"}),
            needs: BTreeMap::from([("lint".into(), NeedsResult { result: "success".into(), outputs: BTreeMap::new() })]),
            steps: vec![
                StepSpec {
                    number: 1,
                    id: None,
                    name: Some("Check out".into()),
                    kind: StepKind::Uses {
                        uses: "actions/checkout@v4".into(),
                        inputs: BTreeMap::from([("fetch-depth".into(), "1".into())]),
                    },
                    env: BTreeMap::new(),
                    condition: None,
                    continue_on_error: false,
                    timeout_minutes: None,
                },
                StepSpec {
                    number: 2,
                    id: Some("v".into()),
                    name: None,
                    kind: StepKind::Run { run: "echo hello from codehaven".into(), shell: None, working_directory: None },
                    env: BTreeMap::new(),
                    condition: Some("success()".into()),
                    continue_on_error: false,
                    timeout_minutes: Some(5),
                },
            ],
            outputs: BTreeMap::from([("ver".into(), "${{ steps.v.outputs.ver }}".into())]),
            secrets: BTreeMap::new(),
            vars: BTreeMap::new(),
            context: RunContext {
                event_name: "push".into(),
                git_ref: "refs/heads/main".into(),
                sha: "a".repeat(40),
                actor: "alice".into(),
                repository: "alice/demo".into(),
                run_id: 5,
                run_number: 1,
                run_attempt: 1,
                workflow: "ci".into(),
                server_url: "https://code.example.com".into(),
                api_url: "https://code.example.com/api/v3".into(),
                event: serde_json::json!({}),
            },
            default_shell: None,
            default_working_directory: None,
            timeout_minutes: 60,
            token: "job_secret".into(),
            lease_seconds: 120,
            services: Vec::new(),
        }
    }

    #[test]
    fn register_round_trip() {
        crate::roundtrip(
            "runner_register_request",
            &RegisterRequest {
                token: "tok".into(),
                name: "runner-1".into(),
                labels: vec!["ubuntu-latest".into(), "linux".into()],
                version: Some("0.1.0".into()),
                os: Some("linux".into()),
                arch: Some("x64".into()),
            },
        );
        crate::roundtrip(
            "runner_register_response",
            &RegisterResponse { runner_id: 1, name: "runner-1".into(), credential: "cred".into() },
        );
    }

    #[test]
    fn job_payload_round_trip() {
        crate::roundtrip("runner_job_payload", &payload());
        crate::roundtrip("runner_job_response_empty", &JobResponse { job: None });
    }

    #[test]
    fn steps_use_the_workflow_file_names() {
        let v = serde_json::to_value(&payload().steps[0]).unwrap();
        assert_eq!(v["kind"], "uses");
        assert_eq!(v["uses"], "actions/checkout@v4");
        assert_eq!(v["with"]["fetch-depth"], "1");
        let v = serde_json::to_value(&payload().steps[1]).unwrap();
        assert_eq!(v["kind"], "run");
        assert_eq!(v["if"], "success()");
    }

    #[test]
    fn small_messages_round_trip() {
        crate::roundtrip("runner_heartbeat_response", &HeartbeatResponse { cancel: true });
        crate::roundtrip("runner_log_batch", &LogBatch { step: 2, seq: 7, lines: vec!["hello from codehaven".into()] });
        crate::roundtrip(
            "runner_step_update",
            &StepUpdate {
                name: None,
                status: "completed".into(),
                conclusion: Some("success".into()),
                outputs: BTreeMap::from([("ver".into(), "1.2".into())]),
            },
        );
        crate::roundtrip("runner_complete", &CompleteRequest { conclusion: "success".into(), outputs: BTreeMap::new() });
        crate::roundtrip(
            "runner_cache_lookup",
            &CacheLookupRequest { keys: vec!["cargo-abc".into(), "cargo-".into()], version: "v1".into() },
        );
        crate::roundtrip(
            "runner_cache_hit",
            &CacheLookupResponse { hit: Some(CacheHit { id: 4, key: "cargo-abc".into(), size: 1024, exact: true }) },
        );
        crate::roundtrip("runner_artifact_uploaded", &ArtifactUploaded { id: 1, name: "report".into(), size: 10 });
    }

    #[test]
    fn unknown_fields_from_a_newer_server_are_ignored() {
        let json = r#"{"cancel": false, "something_new": 1}"#;
        let r: HeartbeatResponse = serde_json::from_str(json).unwrap();
        assert!(!r.cancel);
    }
}
