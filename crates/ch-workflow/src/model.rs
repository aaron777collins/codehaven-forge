//! The typed workflow. Every part that can fail validation remembers its line, and every
//! string that may hold `${{ }}` is kept as written, because the runner evaluates most of
//! them once it knows step outputs.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    /// `name:`, or `None` when the file has none. Callers fall back to the file name.
    pub name: Option<String>,
    pub run_name: Option<String>,
    pub triggers: Triggers,
    pub env: BTreeMap<String, String>,
    pub defaults: Defaults,
    pub concurrency: Option<Concurrency>,
    /// `permissions:` for every job's token, unless a job has its own.
    #[serde(default)]
    pub permissions: Option<crate::permissions::Declared>,
    /// In file order.
    pub jobs: Vec<Job>,
}

impl Workflow {
    pub fn job(&self, key: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.key == key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Triggers {
    pub push: Option<PushTrigger>,
    pub pull_request: Option<PullRequestTrigger>,
    pub workflow_dispatch: Option<Dispatch>,
    pub schedule: Vec<ScheduleTrigger>,
    pub release: Option<ReleaseTrigger>,
    /// `on: workflow_call`: this file is a reusable workflow (ch-ctk.7). It starts no run of its
    /// own; a job of another workflow calls it with `uses:`.
    #[serde(default)]
    pub workflow_call: Option<WorkflowCall>,
}

impl Triggers {
    pub fn is_empty(&self) -> bool {
        self.push.is_none()
            && self.pull_request.is_none()
            && self.workflow_dispatch.is_none()
            && self.schedule.is_empty()
            && self.release.is_none()
            && self.workflow_call.is_none()
    }

    /// Names of the events this workflow listens to, in a stable order.
    pub fn event_names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.push.is_some() {
            v.push("push");
        }
        if self.pull_request.is_some() {
            v.push("pull_request");
        }
        if self.workflow_dispatch.is_some() {
            v.push("workflow_dispatch");
        }
        if !self.schedule.is_empty() {
            v.push("schedule");
        }
        if self.release.is_some() {
            v.push("release");
        }
        if self.workflow_call.is_some() {
            v.push("workflow_call");
        }
        v
    }
}

/// What a reusable workflow takes and gives back (`on.workflow_call`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowCall {
    pub inputs: Vec<CallInput>,
    pub secrets: Vec<CallSecret>,
    pub outputs: Vec<CallOutput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallInputType {
    String,
    Boolean,
    Number,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallInput {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub kind: CallInputType,
    pub default: Option<Json>,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallSecret {
    pub name: String,
    pub required: bool,
    pub line: u32,
}

/// An output of a reusable workflow. `value` is an expression over its jobs, such as
/// `${{ jobs.build.outputs.version }}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallOutput {
    pub name: String,
    pub description: String,
    pub value: String,
    pub line: u32,
}

/// The secrets a calling job passes on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CallSecrets {
    /// `secrets: inherit`: every secret of the calling repository.
    Inherit,
    /// `secrets: {name: ${{ secrets.OTHER }}}`: only these, by the called workflow's names.
    Map(BTreeMap<String, String>),
}

/// The part a server-side job plays for an inlined reusable workflow call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallRole {
    /// Stands for the calling job's `needs` and `if`: the called jobs wait for it, and it
    /// succeeds when the caller would have run, or is skipped when it would not.
    Entry,
    /// Takes the calling job's key, waits for every called job, and gives the call's result and
    /// outputs to the jobs that need the caller.
    Exit,
}

/// A job that calls a reusable workflow (`uses:` on the job, ch-ctk.7). It has no steps and no
/// runner of its own: the run inlines the called workflow's jobs in its place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// `./.codehaven/workflows/x.yml` or `haven_owner/repo/.codehaven/workflows/x.yml@ref`.
    pub uses: String,
    pub with: BTreeMap<String, String>,
    pub secrets: CallSecrets,
    pub line: u32,
}

/// Branch, tag and path filters. `None` means the key was not written, which is different from
/// an empty list: a push filter with only `tags` does not fire for branch pushes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushTrigger {
    pub branches: Option<Vec<String>>,
    pub branches_ignore: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub tags_ignore: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub paths_ignore: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequestTrigger {
    /// Empty means the default: opened, synchronize, reopened.
    pub types: Vec<String>,
    pub branches: Option<Vec<String>>,
    pub branches_ignore: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub paths_ignore: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReleaseTrigger {
    pub types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleTrigger {
    pub cron: String,
    pub line: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Dispatch {
    pub inputs: Vec<DispatchInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    String,
    Boolean,
    Choice,
    Number,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchInput {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub kind: InputType,
    pub default: Option<Json>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Defaults {
    pub shell: Option<String>,
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Concurrency {
    /// May hold `${{ }}`.
    pub group: String,
    /// `true`, `false` or an expression.
    pub cancel_in_progress: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub image: String,
    pub env: BTreeMap<String, String>,
    pub options: Option<String>,
    pub line: u32,
}

/// A service container beside the job (ch-ctk.7): it shares the job container's network, so the
/// job reaches it at `localhost:<port>` and by its id as a host name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Service {
    /// May hold `${{ }}`.
    pub image: String,
    pub env: BTreeMap<String, String>,
    /// The container ports the job waits for before its first step, from `ports:` (`5432` or
    /// `5432:5432`; the host side means nothing here).
    pub ports: Vec<u16>,
    pub options: Option<String>,
    pub line: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Matrix {
    /// Axes in file order. Each value is a JSON scalar, array or object.
    pub axes: Vec<(String, Vec<Json>)>,
    pub include: Vec<BTreeMap<String, Json>>,
    pub exclude: Vec<BTreeMap<String, Json>>,
    /// `matrix: ${{ fromJSON(...) }}`, evaluated when the run is planned.
    pub expression: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strategy {
    pub matrix: Option<Matrix>,
    pub fail_fast: bool,
    pub max_parallel: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub key: String,
    pub line: u32,
    pub name: Option<String>,
    /// Labels, possibly with `${{ }}`.
    pub runs_on: Vec<String>,
    pub container: Option<Container>,
    pub strategy: Option<Strategy>,
    pub needs: Vec<String>,
    pub condition: Option<String>,
    /// Raw, may be an expression. Default 360 as in Actions.
    pub timeout_minutes: Option<String>,
    pub continue_on_error: Option<String>,
    pub env: BTreeMap<String, String>,
    pub outputs: BTreeMap<String, String>,
    pub defaults: Defaults,
    pub concurrency: Option<Concurrency>,
    /// `permissions:` for this job's token. Replaces the workflow's.
    #[serde(default)]
    pub permissions: Option<crate::permissions::Declared>,
    #[serde(default)]
    pub services: BTreeMap<String, Service>,
    /// Set when this job calls a reusable workflow. Such a job has no `runs_on` and no steps.
    #[serde(default)]
    pub call: Option<Call>,
    /// Set on the two jobs the server makes in place of a call (`reusable::expand`): they have no
    /// steps and no runner, and the server finishes them itself.
    #[serde(default)]
    pub call_role: Option<CallRole>,
    /// Set on a job that came from a called workflow (`reusable::expand`). The caller's
    /// workflow-level `env` and `defaults` do not reach it.
    #[serde(default)]
    pub called: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    Run { run: String, shell: Option<String>, working_directory: Option<String> },
    Uses { uses: String, with: BTreeMap<String, String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// 1-based position in the job.
    pub number: u32,
    pub line: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub condition: Option<String>,
    pub kind: StepKind,
    pub env: BTreeMap<String, String>,
    pub continue_on_error: Option<String>,
    pub timeout_minutes: Option<String>,
}

impl Step {
    /// What the UI calls the step: its `name`, else the command's first line, else the action.
    pub fn display_name(&self) -> String {
        if let Some(n) = self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            return n.trim().to_string();
        }
        match &self.kind {
            StepKind::Run { run, .. } => {
                let first = run.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("run");
                format!("Run {first}")
            }
            StepKind::Uses { uses, .. } => format!("Run {uses}"),
        }
    }
}
