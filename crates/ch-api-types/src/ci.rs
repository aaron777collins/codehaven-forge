//! CI status, checks, Actions runs, jobs, artifacts, secrets, variables and runners.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::SimpleUser;
use crate::repos::Repository;

/// A commit status, as returned by `GET/POST /repos/{owner}/{repo}/statuses/{sha}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitStatus {
    pub id: i64,
    /// `pending`, `success`, `failure` or `error`.
    pub state: String,
    pub context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<SimpleUser>,
    pub url: String,
}

/// Request body for `POST /repos/{owner}/{repo}/statuses/{sha}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateStatus {
    /// `pending`, `success`, `failure` or `error`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_url: Option<String>,
}

/// Combined status of a commit, `GET /repos/{owner}/{repo}/commits/{ref}/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CombinedStatus {
    pub state: String,
    pub sha: String,
    pub total_count: u32,
    #[serde(default)]
    pub statuses: Vec<CommitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    pub commit_url: String,
    pub url: String,
}

/// Output block of a check run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRunOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A check run, `GET /repos/{owner}/{repo}/commits/{ref}/check-runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: i64,
    pub name: String,
    pub head_sha: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// `success`, `failure`, `neutral`, `cancelled`, `skipped`, `timed_out`, `action_required`, `startup_failure` or `stale`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    pub output: CheckRunOutput,
    pub html_url: String,
    pub url: String,
}

/// Page of check runs for a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRunsResponse {
    pub total_count: u32,
    #[serde(default)]
    pub check_runs: Vec<CheckRun>,
}

/// A workflow file, `GET /repos/{owner}/{repo}/actions/workflows`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: i64,
    pub name: String,
    pub path: String,
    /// `active` or `disabled`.
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub url: String,
    pub html_url: String,
    pub badge_url: String,
}

/// Page of workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowsResponse {
    pub total_count: u32,
    #[serde(default)]
    pub workflows: Vec<Workflow>,
}

/// A run of a workflow, `GET /repos/{owner}/{repo}/actions/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub run_number: u32,
    pub run_attempt: u32,
    pub event: String,
    /// `queued`, `in_progress` or `completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `success`, `failure`, `neutral`, `cancelled`, `skipped`, `timed_out`, `action_required`, `startup_failure` or `stale`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    pub workflow_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub path: String,
    pub display_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<SimpleUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggering_actor: Option<SimpleUser>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<DateTime<Utc>>,
    pub html_url: String,
    pub url: String,
    pub jobs_url: String,
    pub logs_url: String,
    pub cancel_url: String,
    pub rerun_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
}

/// Page of workflow runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunsResponse {
    pub total_count: u32,
    #[serde(default)]
    pub workflow_runs: Vec<WorkflowRun>,
}

/// One step of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// A job of a workflow run, `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowJob {
    pub id: i64,
    pub run_id: i64,
    pub run_attempt: u32,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_name: Option<String>,
    pub html_url: String,
    pub url: String,
    pub check_run_url: String,
}

/// Page of jobs for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowJobsResponse {
    pub total_count: u32,
    #[serde(default)]
    pub jobs: Vec<WorkflowJob>,
}

/// Request body for `POST /repos/{owner}/{repo}/dispatches`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchRequest {
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
}

/// A build artifact, `GET /repos/{owner}/{repo}/actions/artifacts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: i64,
    pub name: String,
    pub size_in_bytes: i64,
    pub expired: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub url: String,
    pub archive_download_url: String,
}

/// Page of artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactsResponse {
    pub total_count: u32,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// A CI secret's metadata, `GET /repos/{owner}/{repo}/actions/secrets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMeta {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Page of secret metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsResponse {
    pub total_count: u32,
    #[serde(default)]
    pub secrets: Vec<SecretMeta>,
}

/// Request body for `PUT /repos/{owner}/{repo}/actions/secrets/{name}`. Codehaven takes the plaintext value and encrypts server side; GitHub's `encrypted_value` is not used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutSecret {
    pub value: String,
}

/// A CI variable, `GET /repos/{owner}/{repo}/actions/variables`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Page of variables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariablesResponse {
    pub total_count: u32,
    #[serde(default)]
    pub variables: Vec<Variable>,
}

/// Request body for `PUT /repos/{owner}/{repo}/actions/variables/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutVariable {
    pub name: String,
    pub value: String,
}

/// A label attached to a self-hosted runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerLabel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
}

/// A self-hosted runner, `GET /repos/{owner}/{repo}/actions/runners`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerInfo {
    pub id: i64,
    pub name: String,
    pub os: String,
    /// `online` or `offline`.
    pub status: String,
    pub busy: bool,
    #[serde(default)]
    pub labels: Vec<RunnerLabel>,
}

/// Page of self-hosted runners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnersResponse {
    pub total_count: u32,
    #[serde(default)]
    pub runners: Vec<RunnerInfo>,
}

/// Response of `POST /repos/{owner}/{repo}/actions/runners/registration-token`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerRegistrationToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "abc123def4567890abcdef0123456789abcdef01";

    fn repo() -> crate::repos::Repository {
        serde_json::from_value(serde_json::json!({
            "id": 1001,
            "node_id": "R_kgDO",
            "name": "codehaven",
            "full_name": "alice/codehaven",
            "private": true,
            "owner": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatars/1.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "html_url": "https://code.example.com/alice/codehaven",
            "description": "self-hosted forge",
            "fork": false,
            "url": "https://code.example.com/api/v3/repos/alice/codehaven",
            "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
            "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
            "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
            "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
            "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
            "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
            "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
            "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
            "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
            "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
            "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "pushed_at": "2026-01-02T03:04:05Z",
            "git_url": "git://code.example.com/alice/codehaven.git",
            "ssh_url": "git@code.example.com:alice/codehaven.git",
            "clone_url": "https://code.example.com/alice/codehaven.git",
            "svn_url": "https://code.example.com/alice/codehaven",
            "homepage": "",
            "size": 42,
            "stargazers_count": 1,
            "watchers_count": 1,
            "language": "Rust",
            "has_issues": true,
            "has_projects": true,
            "has_downloads": true,
            "has_wiki": true,
            "has_pages": false,
            "has_discussions": false,
            "forks_count": 0,
            "mirror_url": null,
            "archived": false,
            "disabled": false,
            "open_issues_count": 2,
            "license": null,
            "allow_forking": true,
            "is_template": false,
            "web_commit_signoff_required": false,
            "topics": ["forge"],
            "visibility": "private",
            "forks": 0,
            "open_issues": 2,
            "watchers": 1,
            "default_branch": "main",
            "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
            "empty": false,
            "allow_squash_merge": true,
            "allow_merge_commit": true,
            "allow_rebase_merge": false,
            "delete_branch_on_merge": true
        }))
        .unwrap()
    }

    fn status() -> CommitStatus {
        CommitStatus {
            id: 3001,
            state: "success".into(),
            context: "ci/build".into(),
            description: Some("build passed".into()),
            target_url: Some("https://code.example.com/alice/codehaven/actions/runs/900001".into()),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            creator: Some(crate::user("alice", 1)),
            url: format!("https://code.example.com/api/v3/repos/alice/codehaven/statuses/{SHA}"),
        }
    }

    fn check_run() -> CheckRun {
        CheckRun {
            id: 5001,
            name: "build".into(),
            head_sha: SHA.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            started_at: Some(crate::t0()),
            completed_at: Some(crate::t0()),
            details_url: Some("https://code.example.com/alice/codehaven/actions/runs/900001".into()),
            external_id: Some("900001".into()),
            output: CheckRunOutput { title: Some("build".into()), summary: Some("built in 42s".into()), text: Some("1 warning".into()) },
            html_url: "https://code.example.com/alice/codehaven/actions/runs/900001".into(),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/check-runs/5001".into(),
        }
    }

    fn workflow() -> Workflow {
        Workflow {
            id: 42,
            name: "ci".into(),
            path: ".codehaven/workflows/ci.yaml".into(),
            state: "active".into(),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/workflows/42".into(),
            html_url: "https://code.example.com/alice/codehaven/actions/workflows/ci.yaml".into(),
            badge_url: "https://code.example.com/alice/codehaven/actions/workflows/ci.yaml/badge.svg".into(),
        }
    }

    fn workflow_run() -> WorkflowRun {
        WorkflowRun {
            id: 900001,
            name: Some("ci".into()),
            run_number: 12,
            run_attempt: 1,
            event: "push".into(),
            status: Some("completed".into()),
            conclusion: Some("success".into()),
            workflow_id: 42,
            head_branch: Some("main".into()),
            head_sha: SHA.into(),
            path: ".codehaven/workflows/ci.yaml".into(),
            display_title: "add ci types".into(),
            actor: Some(crate::user("alice", 1)),
            triggering_actor: Some(crate::user("alice", 1)),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            run_started_at: Some(crate::t0()),
            html_url: "https://code.example.com/alice/codehaven/actions/runs/900001".into(),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001".into(),
            jobs_url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/jobs".into(),
            logs_url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/logs".into(),
            cancel_url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/cancel".into(),
            rerun_url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/rerun".into(),
            repository: Some(repo()),
        }
    }

    fn job() -> WorkflowJob {
        WorkflowJob {
            id: 60001,
            run_id: 900001,
            run_attempt: 1,
            name: "build".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            started_at: Some(crate::t0()),
            completed_at: Some(crate::t0()),
            steps: vec![WorkflowStep {
                name: "checkout".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                number: 1,
                started_at: Some(crate::t0()),
                completed_at: Some(crate::t0()),
            }],
            labels: vec!["self-hosted".into(), "linux".into()],
            runner_id: Some(3),
            runner_name: Some("runner-1".into()),
            html_url: "https://code.example.com/alice/codehaven/actions/runs/900001/job/60001".into(),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/jobs/60001".into(),
            check_run_url: "https://code.example.com/api/v3/repos/alice/codehaven/check-runs/5001".into(),
        }
    }

    fn artifact() -> Artifact {
        Artifact {
            id: 70001,
            name: "coverage-report".into(),
            size_in_bytes: 2048,
            expired: false,
            created_at: crate::t0(),
            expires_at: Some(crate::t0()),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/artifacts/70001".into(),
            archive_download_url: "https://code.example.com/api/v3/repos/alice/codehaven/actions/artifacts/70001/zip".into(),
        }
    }

    fn secret() -> SecretMeta {
        SecretMeta { name: "DEPLOY_KEY".into(), created_at: crate::t0(), updated_at: crate::t0() }
    }

    fn variable() -> Variable {
        Variable { name: "RUST_VERSION".into(), value: "1.100.0".into(), created_at: crate::t0(), updated_at: crate::t0() }
    }

    fn runner() -> RunnerInfo {
        RunnerInfo {
            id: 3,
            name: "runner-1".into(),
            os: "Linux".into(),
            status: "online".into(),
            busy: false,
            labels: vec![
                RunnerLabel { id: Some(1), name: "self-hosted".into(), type_: "read-only".into() },
                RunnerLabel { id: None, name: "linux".into(), type_: "custom".into() },
            ],
        }
    }

    #[test]
    fn ci_commit_status() {
        let value = status();
        crate::roundtrip("ci_commit_status", &value);
    }

    #[test]
    fn ci_create_status() {
        let value = CreateStatus {
            state: "success".into(),
            context: Some("ci/build".into()),
            description: Some("build passed".into()),
            target_url: Some("https://code.example.com/alice/codehaven/actions/runs/900001".into()),
        };
        crate::roundtrip("ci_create_status", &value);
    }

    #[test]
    fn ci_combined_status() {
        let value = CombinedStatus {
            state: "success".into(),
            sha: SHA.into(),
            total_count: 1,
            statuses: vec![status()],
            repository: Some(repo()),
            commit_url: format!("https://code.example.com/api/v3/repos/alice/codehaven/commits/{SHA}/status"),
            url: format!("https://code.example.com/api/v3/repos/alice/codehaven/commits/{SHA}/status"),
        };
        crate::roundtrip("ci_combined_status", &value);
    }

    #[test]
    fn ci_check_run() {
        let value = check_run();
        crate::roundtrip("ci_check_run", &value);
    }

    #[test]
    fn ci_check_runs_response() {
        let value = CheckRunsResponse { total_count: 1, check_runs: vec![check_run()] };
        crate::roundtrip("ci_check_runs_response", &value);
    }

    #[test]
    fn ci_workflow() {
        let value = workflow();
        crate::roundtrip("ci_workflow", &value);
    }

    #[test]
    fn ci_workflows_response() {
        let value = WorkflowsResponse { total_count: 1, workflows: vec![workflow()] };
        crate::roundtrip("ci_workflows_response", &value);
    }

    #[test]
    fn ci_workflow_run() {
        let value = workflow_run();
        crate::roundtrip("ci_workflow_run", &value);
    }

    #[test]
    fn ci_workflow_runs_response() {
        let value = WorkflowRunsResponse { total_count: 1, workflow_runs: vec![workflow_run()] };
        crate::roundtrip("ci_workflow_runs_response", &value);
    }

    #[test]
    fn ci_workflow_job() {
        let value = job();
        crate::roundtrip("ci_workflow_job", &value);
    }

    #[test]
    fn ci_workflow_jobs_response() {
        let value = WorkflowJobsResponse { total_count: 1, jobs: vec![job()] };
        crate::roundtrip("ci_workflow_jobs_response", &value);
    }

    #[test]
    fn ci_dispatch_request() {
        let mut inputs = serde_json::Map::new();
        inputs.insert("branch".to_string(), serde_json::json!("ci"));
        inputs.insert("count".to_string(), serde_json::json!(3));
        let value = DispatchRequest { git_ref: "main".into(), inputs: Some(inputs) };
        crate::roundtrip("ci_dispatch_request", &value);
    }

    #[test]
    fn ci_artifact() {
        let value = artifact();
        crate::roundtrip("ci_artifact", &value);
    }

    #[test]
    fn ci_artifacts_response() {
        let value = ArtifactsResponse { total_count: 1, artifacts: vec![artifact()] };
        crate::roundtrip("ci_artifacts_response", &value);
    }

    #[test]
    fn ci_secret_meta() {
        let value = secret();
        crate::roundtrip("ci_secret_meta", &value);
    }

    #[test]
    fn ci_secrets_response() {
        let value = SecretsResponse { total_count: 1, secrets: vec![secret()] };
        crate::roundtrip("ci_secrets_response", &value);
    }

    #[test]
    fn ci_put_secret() {
        let value = PutSecret { value: "s3cr3t".into() };
        crate::roundtrip("ci_put_secret", &value);
    }

    #[test]
    fn ci_variable() {
        let value = variable();
        crate::roundtrip("ci_variable", &value);
    }

    #[test]
    fn ci_variables_response() {
        let value = VariablesResponse { total_count: 1, variables: vec![variable()] };
        crate::roundtrip("ci_variables_response", &value);
    }

    #[test]
    fn ci_put_variable() {
        let value = PutVariable { name: "RUST_VERSION".into(), value: "1.100.0".into() };
        crate::roundtrip("ci_put_variable", &value);
    }

    #[test]
    fn ci_runner_info() {
        let value = runner();
        crate::roundtrip("ci_runner_info", &value);
    }

    #[test]
    fn ci_runners_response() {
        let value = RunnersResponse { total_count: 1, runners: vec![runner()] };
        crate::roundtrip("ci_runners_response", &value);
    }

    #[test]
    fn ci_runner_registration_token() {
        let value = RunnerRegistrationToken { token: "registration-token-1".into(), expires_at: crate::t0() };
        crate::roundtrip("ci_runner_registration_token", &value);
    }

    #[test]
    fn parses_github_workflow_run_json() {
        let json = r#"{
            "id": 900001,
            "name": "ci",
            "node_id": "WFR_kwDO",
            "head_branch": "main",
            "head_sha": "abc123def4567890abcdef0123456789abcdef01",
            "run_number": 12,
            "run_attempt": 1,
            "event": "push",
            "status": "completed",
            "conclusion": "success",
            "workflow_id": 42,
            "check_suite_id": 123456,
            "check_suite_url": "https://code.example.com/api/v3/repos/alice/codehaven/check-suites/123456",
            "url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001",
            "html_url": "https://code.example.com/alice/codehaven/actions/runs/900001",
            "pull_requests": [],
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-01-02T03:04:05Z",
            "actor": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatars/1.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "triggering_actor": {
                "login": "alice",
                "id": 1,
                "avatar_url": "https://code.example.com/avatars/1.svg",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "run_started_at": "2026-01-02T03:04:05Z",
            "previous_attempt_url": null,
            "workflow_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/workflows/42",
            "head_commit": {
                "id": "abc123def4567890abcdef0123456789abcdef01",
                "message": "add ci types",
                "timestamp": "2026-01-02T03:04:05Z"
            },
            "head_repository": {
                "id": 1001,
                "name": "codehaven",
                "full_name": "alice/codehaven"
            },
            "repository": {
                "id": 1001,
                "node_id": "R_kgDO",
                "name": "codehaven",
                "full_name": "alice/codehaven",
                "private": true,
                "owner": {
                    "login": "alice",
                    "id": 1,
                    "avatar_url": "https://code.example.com/avatars/1.svg",
                    "url": "https://code.example.com/api/v3/users/alice",
                    "html_url": "https://code.example.com/alice",
                    "type": "User",
                    "site_admin": false
                },
                "html_url": "https://code.example.com/alice/codehaven",
                "description": "self-hosted forge",
                "fork": false,
                "url": "https://code.example.com/api/v3/repos/alice/codehaven",
                "forks_url": "https://code.example.com/api/v3/repos/alice/codehaven/forks",
                "keys_url": "https://code.example.com/api/v3/repos/alice/codehaven/keys{/key_id}",
                "collaborators_url": "https://code.example.com/api/v3/repos/alice/codehaven/collaborators{/collaborator}",
                "teams_url": "https://code.example.com/api/v3/repos/alice/codehaven/teams",
                "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
                "issue_comment_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/comments{/number}",
                "issue_events_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues/events{/number}",
                "issues_url": "https://code.example.com/api/v3/repos/alice/codehaven/issues{/number}",
                "labels_url": "https://code.example.com/api/v3/repos/alice/codehaven/labels{/name}",
                "releases_url": "https://code.example.com/api/v3/repos/alice/codehaven/releases{/id}",
                "deployments_url": "https://code.example.com/api/v3/repos/alice/codehaven/deployments",
                "created_at": "2026-01-02T03:04:05Z",
                "updated_at": "2026-01-02T03:04:05Z",
                "pushed_at": "2026-01-02T03:04:05Z",
                "git_url": "git://code.example.com/alice/codehaven.git",
                "ssh_url": "git@code.example.com:alice/codehaven.git",
                "clone_url": "https://code.example.com/alice/codehaven.git",
                "svn_url": "https://code.example.com/alice/codehaven",
                "homepage": "",
                "size": 42,
                "stargazers_count": 1,
                "watchers_count": 1,
                "language": "Rust",
                "has_issues": true,
                "has_projects": true,
                "has_downloads": true,
                "has_wiki": true,
                "has_pages": false,
                "has_discussions": false,
                "forks_count": 0,
                "mirror_url": null,
                "archived": false,
                "disabled": false,
                "open_issues_count": 2,
                "license": null,
                "allow_forking": true,
                "is_template": false,
                "web_commit_signoff_required": false,
                "topics": ["forge"],
                "visibility": "private",
                "forks": 0,
                "open_issues": 2,
                "watchers": 1,
                "default_branch": "main",
                "permissions": { "admin": true, "maintain": true, "push": true, "triage": true, "pull": true },
                "empty": false,
                "allow_squash_merge": true,
                "allow_merge_commit": true,
                "allow_rebase_merge": false,
                "delete_branch_on_merge": true
            },
            "path": ".codehaven/workflows/ci.yaml",
            "display_title": "add ci types",
            "artifacts_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/artifacts",
            "cancel_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/cancel",
            "rerun_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/rerun",
            "jobs_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/jobs",
            "logs_url": "https://code.example.com/api/v3/repos/alice/codehaven/actions/runs/900001/logs",
            "unknown_future_field": { "keep": false }
        }"#;
        let run: WorkflowRun = serde_json::from_str(json).expect("deserialize workflow run");
        assert_eq!(run.run_number, 12);
        assert_eq!(run.event, "push");
        assert_eq!(run.head_sha, SHA);
    }
}
