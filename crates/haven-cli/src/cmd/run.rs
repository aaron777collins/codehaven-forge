//! `haven run`: list, view, watch, logs, rerun and cancel workflow runs.

use crate::cli::{RunCmd, RunIdArgs, RunListArgs, RunLogsArgs, RunRerunArgs, RunWatchArgs};
use crate::{Ctx, fmt};
use anyhow::{Result, bail};
use ch_api_types::ci::{WorkflowJob, WorkflowJobsResponse, WorkflowRun, WorkflowRunsResponse};
use chrono::Utc;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub async fn run(ctx: &Ctx, cmd: RunCmd) -> Result<i32> {
    match cmd {
        RunCmd::List(a) => list(ctx, a).await,
        RunCmd::View(a) => view(ctx, a).await,
        RunCmd::Watch(a) => watch(ctx, a).await,
        RunCmd::Logs(a) => logs(ctx, a).await,
        RunCmd::Rerun(a) => rerun(ctx, a).await,
        RunCmd::Cancel(a) => cancel(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, a: RunListArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let path = match &a.workflow {
        Some(w) => format!("/repos/{owner}/{repo}/actions/workflows/{w}/runs"),
        None => format!("/repos/{owner}/{repo}/actions/runs"),
    };
    let mut query: Vec<(&str, String)> = vec![];
    if let Some(b) = &a.branch {
        query.push(("branch", b.clone()));
    }
    if let Some(s) = &a.status {
        query.push(("status", s.clone()));
    }
    let runs: Vec<WorkflowRun> =
        ctx.api()?.get_paginated_wrapped(&path, &query, a.limit, |p: WorkflowRunsResponse| p.workflow_runs).await?;
    if a.json {
        ctx.print_json(&runs)?;
        return Ok(0);
    }
    if runs.is_empty() {
        ctx.println("No runs.");
        return Ok(0);
    }
    let now = Utc::now();
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            vec![
                r.id.to_string(),
                fmt::state_word(r.status.as_deref(), r.conclusion.as_deref()),
                fmt::truncate(&r.display_title, 50),
                r.head_branch.clone().unwrap_or_default(),
                r.event.clone(),
                fmt::ago(r.created_at, now),
            ]
        })
        .collect();
    ctx.print(fmt::table(&rows));
    Ok(0)
}

async fn jobs_of(ctx: &Ctx, owner: &str, repo: &str, run_id: i64) -> Result<Vec<WorkflowJob>> {
    ctx.api()?
        .get_paginated_wrapped(&format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs"), &[], 0, |p: WorkflowJobsResponse| p.jobs)
        .await
}

async fn view(ctx: &Ctx, a: RunIdArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let run: WorkflowRun = ctx.api()?.get(&format!("/repos/{owner}/{repo}/actions/runs/{}", a.run_id)).await?;
    let jobs = jobs_of(ctx, &owner, &repo, a.run_id).await?;
    if a.json {
        ctx.print_json(&serde_json::json!({ "run": run, "jobs": jobs }))?;
        return Ok(0);
    }
    let now = Utc::now();
    ctx.println(format!("Run {} {}", run.id, run.display_title));
    ctx.println(format!(
        "{} on {} ({}), started {}",
        fmt::state_word(run.status.as_deref(), run.conclusion.as_deref()),
        run.head_branch.as_deref().unwrap_or("?"),
        run.event,
        fmt::ago(run.created_at, now)
    ));
    for j in &jobs {
        ctx.println("");
        ctx.println(format!("{}  {}", j.name, fmt::state_word(Some(&j.status), j.conclusion.as_deref())));
        for s in &j.steps {
            ctx.println(format!("  {}. {}  {}", s.number, s.name, fmt::state_word(Some(&s.status), s.conclusion.as_deref())));
        }
    }
    ctx.println("");
    ctx.println(&run.html_url);
    Ok(0)
}

/// Job states by name, for noticing what changed between two polls.
fn job_states(jobs: &[WorkflowJob]) -> BTreeMap<String, String> {
    jobs.iter().map(|j| (j.name.clone(), fmt::state_word(Some(&j.status), j.conclusion.as_deref()))).collect()
}

/// The lines to print when the job states move from `before` to `after`.
fn changes(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    after.iter().filter(|(k, v)| before.get(*k) != Some(*v)).map(|(k, v)| format!("{k}: {v}")).collect()
}

async fn watch(ctx: &Ctx, a: RunWatchArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    let started = Instant::now();
    let mut seen = BTreeMap::new();
    loop {
        let run: WorkflowRun = api.get(&format!("/repos/{owner}/{repo}/actions/runs/{}", a.run_id)).await?;
        let now = job_states(&jobs_of(ctx, &owner, &repo, a.run_id).await?);
        for l in changes(&seen, &now) {
            ctx.println(l);
        }
        seen = now;
        if run.status.as_deref() == Some("completed") {
            let c = run.conclusion.clone().unwrap_or_else(|| "unknown".into());
            ctx.println(format!("Run {} finished: {c}", run.id));
            return Ok(if c == "success" { 0 } else { 1 });
        }
        if started.elapsed() > Duration::from_secs(6 * 3600) {
            bail!("gave up watching after 6 hours. The run is still going.");
        }
        tokio::time::sleep(Duration::from_secs(a.interval.max(1))).await;
    }
}

/// Picks jobs by id or by part of the name, case-insensitively.
fn pick_jobs<'a>(jobs: &'a [WorkflowJob], want: Option<&str>) -> Result<Vec<&'a WorkflowJob>> {
    let Some(w) = want else { return Ok(jobs.iter().collect()) };
    let wl = w.to_lowercase();
    let picked: Vec<&WorkflowJob> = jobs.iter().filter(|j| j.id.to_string() == w || j.name.to_lowercase().contains(&wl)).collect();
    if picked.is_empty() {
        bail!(
            "no job matches {w:?}. This run has: {}",
            jobs.iter().map(|j| format!("{} ({})", j.name, j.id)).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(picked)
}

async fn logs(ctx: &Ctx, a: RunLogsArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let jobs = jobs_of(ctx, &owner, &repo, a.run_id).await?;
    let picked = pick_jobs(&jobs, a.job.as_deref())?;
    let api = ctx.api()?;
    for j in &picked {
        if picked.len() > 1 {
            ctx.println(format!("== {} ==", j.name));
        }
        let text = api.get_text(&format!("/repos/{owner}/{repo}/actions/jobs/{}/logs", j.id), Some("text/plain")).await?;
        ctx.print(&text);
        if !text.ends_with('\n') {
            ctx.println("");
        }
    }
    Ok(0)
}

async fn rerun(ctx: &Ctx, a: RunRerunArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let path = if a.failed {
        format!("/repos/{owner}/{repo}/actions/runs/{}/rerun-failed-jobs", a.run_id)
    } else {
        format!("/repos/{owner}/{repo}/actions/runs/{}/rerun", a.run_id)
    };
    ctx.api()?.post_empty(&path, &serde_json::json!({})).await?;
    ctx.println(format!("Re-running run {}", a.run_id));
    Ok(0)
}

async fn cancel(ctx: &Ctx, a: RunIdArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    ctx.api()?.post_empty(&format!("/repos/{owner}/{repo}/actions/runs/{}/cancel", a.run_id), &serde_json::json!({})).await?;
    ctx.println(format!("Cancelling run {}", a.run_id));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: i64, name: &str) -> WorkflowJob {
        serde_json::from_value(serde_json::json!({
            "id": id, "run_id": 1, "run_attempt": 1, "name": name, "status": "completed", "conclusion": "success",
            "html_url": "", "url": "", "check_run_url": ""
        }))
        .unwrap()
    }

    #[test]
    fn jobs_are_picked_by_id_or_name() {
        let jobs = vec![job(10, "ci / lint"), job(11, "ci / test (linux)"), job(12, "ci / test (mac)")];
        assert_eq!(pick_jobs(&jobs, None).unwrap().len(), 3);
        assert_eq!(pick_jobs(&jobs, Some("11")).unwrap()[0].name, "ci / test (linux)");
        assert_eq!(pick_jobs(&jobs, Some("TEST")).unwrap().len(), 2);
        let e = pick_jobs(&jobs, Some("deploy")).unwrap_err().to_string();
        assert!(e.contains("ci / lint (10)"), "{e}");
    }

    #[test]
    fn watch_prints_only_changes() {
        let a: BTreeMap<String, String> = [("lint".to_string(), "queued".to_string())].into();
        let b: BTreeMap<String, String> =
            [("lint".to_string(), "in_progress".to_string()), ("test".to_string(), "queued".to_string())].into();
        assert_eq!(changes(&BTreeMap::new(), &a), ["lint: queued"]);
        assert_eq!(changes(&a, &b), ["lint: in_progress", "test: queued"]);
        assert!(changes(&b, &b).is_empty());
    }
}
