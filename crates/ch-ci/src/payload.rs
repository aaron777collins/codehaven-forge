//! Builds the job a runner receives (`ch_api_types::runner::JobPayload`).
//!
//! The steps go out with their expressions unevaluated, because the runner knows step outputs
//! and the server does not. What the server does decide is who sees what: only the secrets the
//! job names, none at all for a fork pull request, and a token that works for this job alone.

use crate::ctx::Ctx;
use crate::discovery::repo_ci;
use crate::runs::{JobRow, RunRow, aggregate, base_context, fetch_jobs, fetch_run, github_context};
use crate::secrets::{resolve_for_job, resolve_vars_for_job};
use crate::spec::{RunMeta, split_payload};
use ch_api_types::runner as rn;
use ch_core::Result;
use ch_workflow::expr::{self, Value};
use ch_workflow::{StepKind, Workflow, refs::job_refs};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// How long a runner may go without a heartbeat before the job fails.
pub const LEASE_SECONDS: u32 = 120;

fn eval_flag(raw: Option<&str>, cx: &expr::Context<'_>) -> bool {
    match raw {
        None => false,
        Some(s) => expr::eval_typed(s, cx).map(|v| v.truthy() && v.to_display() != "false").unwrap_or(false),
    }
}

fn eval_minutes(raw: Option<&str>, cx: &expr::Context<'_>) -> Option<u32> {
    let s = raw?;
    let v = expr::eval_typed(s, cx).ok()?;
    let n: f64 = v.to_display().trim().parse().ok()?;
    (n.is_finite() && n >= 1.0).then(|| (n as u32).min(ch_workflow::plan::MAX_TIMEOUT_MINUTES))
}

/// The workflow-level `env` and `defaults` a job starts from.
///
/// A job from a called workflow (ch-ctk.7) starts from none: the caller's `env` may hold a secret
/// the caller did not pass on, and may belong to a workflow in another repository. The called
/// workflow's own `env` and `defaults` were put under its jobs when the call was inlined.
fn workflow_level(meta: &RunMeta, called: bool) -> Workflow {
    let (env, defaults) = if called { Default::default() } else { (meta.workflow_env.clone(), meta.workflow_defaults.clone()) };
    Workflow {
        name: Some(meta.workflow_name.clone()),
        run_name: None,
        triggers: Default::default(),
        env,
        defaults,
        concurrency: None,
        permissions: None,
        jobs: vec![],
    }
}

pub async fn build_payload(ctx: &Ctx, job: &JobRow, token: String) -> Result<rn::JobPayload> {
    let run: RunRow = fetch_run(ctx, job.run_id).await?;
    let (event, meta) = split_payload(&run.event_payload);
    let spec = job.spec()?;
    let info = repo_ci(ctx, ch_core::RepoId(job.repo_id)).await?;
    let base = ctx.base_url();

    // Which secrets and variables does this job read?
    let shell = workflow_level(&meta, spec.job.called);
    let refs = job_refs(&shell, &spec.job);
    let secrets = resolve_for_job(ctx, job.repo_id, &refs.secrets, refs.all_secrets, meta.fork).await?;
    let vars = resolve_vars_for_job(ctx, job.repo_id, &refs.vars, refs.all_vars, meta.fork).await?;

    // What the jobs this one needs reported.
    let siblings = fetch_jobs(ctx, job.run_id).await?;
    let mut needs = BTreeMap::new();
    for k in &spec.job.needs {
        let (result, outputs) = aggregate(&siblings, k);
        needs.insert(k.clone(), rn::NeedsResult { result: result.into(), outputs });
    }

    let github = github_context(&base, &run, &meta, &event);
    let all_vars = crate::runs::load_vars(ctx, job.repo_id, meta.fork).await?;
    let mut cx = base_context(&github, &meta.inputs, &all_vars);
    cx.set("matrix", Value::from_json(&json!(spec.matrix.clone().unwrap_or_default())));

    let steps = spec
        .job
        .steps
        .iter()
        .map(|s| rn::StepSpec {
            number: s.number as i32,
            id: s.id.clone(),
            name: s.name.clone(),
            kind: match &s.kind {
                StepKind::Run { run, shell, working_directory } => {
                    rn::StepKind::Run { run: run.clone(), shell: shell.clone(), working_directory: working_directory.clone() }
                }
                StepKind::Uses { uses, with } => rn::StepKind::Uses { uses: uses.clone(), inputs: with.clone() },
            },
            env: s.env.clone(),
            condition: s.condition.clone(),
            continue_on_error: eval_flag(s.continue_on_error.as_deref(), &cx),
            timeout_minutes: eval_minutes(s.timeout_minutes.as_deref(), &cx),
        })
        .collect();

    let mut env = shell.env.clone();
    env.extend(spec.job.env.clone());

    let container =
        spec.job.container.as_ref().map(|c| rn::ContainerSpec { image: c.image.clone(), env: c.env.clone(), options: c.options.clone() });
    let _ = BTreeSet::<String>::new();

    Ok(rn::JobPayload {
        job_id: job.id,
        run_id: job.run_id,
        repo: rn::RepoInfo {
            id: job.repo_id,
            full_name: info.full_name(),
            clone_url: format!("{base}/{}.git", info.full_name()),
            default_branch: info.default_branch.clone(),
        },
        job_key: job.job_key.clone(),
        name: job.name.clone(),
        runs_on: spec.runs_on.clone(),
        container,
        env,
        matrix: spec.matrix.as_ref().map(|m| json!(m)).unwrap_or(serde_json::Value::Null),
        needs,
        steps,
        outputs: spec.job.outputs.clone(),
        secrets,
        vars,
        context: rn::RunContext {
            event_name: run.event.clone(),
            git_ref: run.git_ref.clone().unwrap_or_default(),
            sha: run.head_sha.clone(),
            actor: meta.actor_login.clone(),
            repository: info.full_name(),
            run_id: run.id,
            run_number: run.run_number,
            run_attempt: run.attempt,
            workflow: meta.workflow_name.clone(),
            server_url: base.clone(),
            api_url: format!("{base}/api/v3"),
            event,
        },
        default_shell: spec.job.defaults.shell.clone().or(shell.defaults.shell.clone()),
        default_working_directory: spec.job.defaults.working_directory.clone().or(shell.defaults.working_directory.clone()),
        timeout_minutes: spec.timeout_minutes,
        token,
        lease_seconds: LEASE_SECONDS,
        services: spec
            .job
            .services
            .iter()
            .map(|(id, s)| rn::ServiceSpec {
                id: id.clone(),
                image: s.image.clone(),
                env: s.env.clone(),
                ports: s.ports.clone(),
                options: s.options.clone(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_workflow::Defaults;

    fn meta() -> RunMeta {
        RunMeta {
            workflow_env: BTreeMap::from([("CALLER_ENV".to_string(), "${{ secrets.TOPSECRET }}".to_string())]),
            workflow_defaults: Defaults { shell: Some("bash".into()), working_directory: Some("sub".into()) },
            ..Default::default()
        }
    }

    #[test]
    fn a_job_of_the_caller_inherits_the_workflow_env_and_defaults() {
        let w = workflow_level(&meta(), false);
        assert_eq!(w.env["CALLER_ENV"], "${{ secrets.TOPSECRET }}");
        assert_eq!(w.defaults.shell.as_deref(), Some("bash"));
    }

    #[test]
    fn a_called_job_inherits_nothing_from_the_caller_workflow() {
        // The caller's `env` may hold a secret the caller did not pass on (ch-ctk.7).
        let w = workflow_level(&meta(), true);
        assert!(w.env.is_empty());
        assert_eq!(w.defaults, Defaults::default());
    }
}
