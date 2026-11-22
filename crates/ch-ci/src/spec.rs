//! What is stored as JSON on runs and jobs.

use ch_workflow::{Defaults, Job};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// A job as planned, kept whole so a re-run builds exactly what ran before even if the
/// workflow file has changed since. Lives in `workflow_jobs.spec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub job: Job,
    pub matrix: Option<BTreeMap<String, Json>>,
    /// Labels after expressions were filled in.
    pub runs_on: Vec<String>,
    pub timeout_minutes: u32,
    pub continue_on_error: bool,
    pub fail_fast: bool,
    pub max_parallel: Option<u32>,
    pub index: usize,
    pub total: usize,
    /// The job was told to stop (fail-fast, or its run was cancelled). The runner learns of it
    /// from the next heartbeat.
    #[serde(default)]
    pub cancel: bool,
    /// The next log batch the server expects, so a retried batch is acknowledged once.
    #[serde(default)]
    pub log_seq: u64,
    /// A matrix that reads `needs` outputs, expanded once the jobs it needs have finished.
    #[serde(default)]
    pub deferred_matrix: Option<ch_workflow::Matrix>,
}

/// Run-level facts that have no column. Lives in `workflow_runs.event_payload.meta`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunMeta {
    pub workflow_path: String,
    pub workflow_name: String,
    #[serde(default)]
    pub workflow_env: BTreeMap<String, String>,
    #[serde(default)]
    pub workflow_defaults: Defaults,
    #[serde(default)]
    pub inputs: Json,
    /// A pull request from another repository: no secrets, no write access.
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub cancel_requested: bool,
    /// Makes event delivery idempotent: the same event never starts the same workflow twice.
    #[serde(default)]
    pub trigger_id: Option<String>,
    #[serde(default)]
    pub base_ref: String,
    #[serde(default)]
    pub head_ref: String,
    #[serde(default)]
    pub actor_login: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub approved_by: Option<i64>,
    /// Why a run is `waiting` for approval, in words for the run page.
    #[serde(default)]
    pub hold_reason: Option<String>,
    /// Kept for a held run: its concurrency group is applied only when it is approved.
    #[serde(default)]
    pub cancel_in_progress: bool,
}

/// Splits `workflow_runs.event_payload` into the webhook-style event and the meta block.
pub fn split_payload(payload: &Json) -> (Json, RunMeta) {
    let event = payload.get("event").cloned().unwrap_or(Json::Null);
    let meta = payload.get("meta").cloned().and_then(|m| serde_json::from_value(m).ok()).unwrap_or_default();
    (event, meta)
}

pub fn join_payload(event: &Json, meta: &RunMeta) -> Json {
    serde_json::json!({ "event": event, "meta": meta })
}

/// The conclusions that count as a failure.
pub fn is_failure(conclusion: Option<&str>) -> bool {
    matches!(conclusion, Some("failure" | "timed_out"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips() {
        let meta = RunMeta { workflow_path: ".codehaven/workflows/ci.yml".into(), fork: true, ..Default::default() };
        let p = join_payload(&serde_json::json!({"a": 1}), &meta);
        let (e, m) = split_payload(&p);
        assert_eq!(e["a"], 1);
        assert!(m.fork);
        assert_eq!(m.workflow_path, ".codehaven/workflows/ci.yml");
    }

    #[test]
    fn a_damaged_payload_reads_as_defaults_not_a_panic() {
        let (e, m) = split_payload(&serde_json::json!({"meta": "nonsense"}));
        assert!(e.is_null());
        assert!(!m.fork && !m.cancel_requested);
        let (_, m) = split_payload(&Json::Null);
        assert_eq!(m.workflow_path, "");
    }
}
