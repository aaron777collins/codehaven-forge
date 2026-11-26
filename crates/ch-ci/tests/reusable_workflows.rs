//! Reusable workflows (ch-ctk.7): a job that says `uses:` is replaced, when the run is made, by
//! the called workflow's jobs, with a server-side start and finish around them.

#![allow(clippy::unwrap_used)]

mod support;
use ch_core::RunId;
use serde_json::json;
use sqlx::PgPool;
use support::*;

const CALLER: &str = "name: ci
on: push
jobs:
  prep:
    runs-on: x
    steps:
      - run: echo prep
  build:
    needs: prep
    uses: ./.codehaven/workflows/build.yml
    with:
      version: '1.2'
    secrets:
      token: ${{ secrets.DEPLOY }}
  after:
    needs: build
    runs-on: x
    steps:
      - run: echo ${{ needs.build.outputs.artifact }}
";

const CALLEE: &str = "name: build
on:
  workflow_call:
    inputs:
      version: {type: string, required: true}
    secrets:
      token: {required: true}
    outputs:
      artifact:
        value: ${{ jobs.pack.outputs.name }}
jobs:
  pack:
    runs-on: x
    outputs:
      name: ${{ steps.p.outputs.name }}
    steps:
      - id: p
        run: echo ${{ inputs.version }} ${{ secrets.token }}
";

async fn keys(w: &World, run: i64) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT job_key, status, conclusion FROM workflow_jobs WHERE run_id = $1 ORDER BY id")
        .bind(run)
        .fetch_all(w.pool())
        .await
        .unwrap()
}

fn row(k: &str, s: &str, c: Option<&str>) -> (String, String, Option<String>) {
    (k.into(), s.into(), c.map(String::from))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_called_workflow_runs_inside_the_run_and_its_outputs_reach_the_jobs_after_it(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, CALLER), (".codehaven/workflows/build.yml", CALLEE)]).await;
    let runs = w.push(repo, &git, "main", None).await;
    // The called file has no push trigger of its own, so only the caller starts a run.
    assert_eq!(runs.len(), 1);
    let run = runs[0];
    assert_eq!(
        keys(&w, run).await,
        [
            row("prep", "queued", None),
            row("build__entry", "waiting", None),
            row("build__pack", "waiting", None),
            row("build", "waiting", None),
            row("after", "waiting", None),
        ]
    );
    let r = w.runner("r", &["x"]).await;
    let prep = r.request(&w, 0).await.unwrap();
    assert_eq!(prep.job_key, "prep");
    r.complete(&w, prep.job_id, "success", json!({})).await;
    // The entry finished by itself, so the called job is the next thing a runner is offered, with
    // the input pasted in, and the secret the caller passed under the name the caller used.
    let pack = r.request(&w, 0).await.unwrap();
    assert_eq!((pack.job_key.as_str(), pack.name.as_str()), ("build__pack", "build / pack"));
    let StepRun(cmd) = StepRun::of(&pack);
    assert_eq!(cmd, "echo ${{ '1.2' }} ${{ (secrets.DEPLOY) }}");
    assert_eq!(keys(&w, run).await[1], row("build__entry", "completed", Some("success")));
    r.complete(&w, pack.job_id, "success", json!({"name": "dist.zip"})).await;
    // The exit finished by itself too, with the caller's key and the called workflow's outputs.
    assert_eq!(keys(&w, run).await[3], row("build", "completed", Some("success")));
    let after = r.request(&w, 0).await.unwrap();
    assert_eq!(after.job_key, "after");
    assert_eq!(after.needs["build"].outputs["artifact"], "dist.zip");
    assert_eq!(after.needs["build"].result, "success");
    r.complete(&w, after.job_id, "success", json!({})).await;
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("success".into())));
    // The server's own jobs have no check run of their own.
    let checks: Vec<String> = sqlx::query_scalar("SELECT name FROM check_runs ORDER BY id").fetch_all(w.pool()).await.unwrap();
    assert_eq!(checks, ["ci / prep", "ci / build / pack", "ci / after"]);
}

/// The one `run:` of a job, as the runner is told it.
struct StepRun(String);

impl StepRun {
    fn of(p: &ch_api_types::runner::JobPayload) -> StepRun {
        let s = serde_json::to_value(&p.steps[0]).unwrap();
        StepRun(s["run"].as_str().unwrap_or_default().to_string())
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_call_the_caller_would_skip_runs_nothing_and_a_failed_one_fails_what_needs_it(pool: PgPool) {
    let w = World::new(pool).await;
    // `if: false` on the call: nothing of the called workflow runs, not even a job that says always().
    let skipped = CALLER.replace("    needs: prep\n    uses:", "    needs: prep\n    if: ${{ false }}\n    uses:");
    let callee = format!("{CALLEE}  tidy:\n    needs: pack\n    if: always()\n    runs-on: x\n    steps:\n      - run: echo tidy\n");
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &skipped), (".codehaven/workflows/build.yml", &callee)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r", &["x"]).await;
    let prep = r.request(&w, 0).await.unwrap();
    r.complete(&w, prep.job_id, "success", json!({})).await;
    assert!(r.request(&w, 0).await.is_none(), "nothing of the skipped call is offered");
    let all = keys(&w, run).await;
    assert_eq!(all[1], row("build__entry", "completed", Some("skipped")));
    assert_eq!(all[2], row("build__pack", "completed", Some("skipped")));
    assert_eq!(all[3], row("build__tidy", "completed", Some("skipped")), "always() does not get past a skipped call");
    assert_eq!(all[4], row("build", "completed", Some("skipped")));
    assert_eq!(all[5], row("after", "completed", Some("skipped")));
    // `prep` succeeded, so the run did.
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("success".into())));

    // A called job that fails fails the call, and what needs the call is skipped. The tidy job
    // that says always() still runs.
    let w2 = {
        let pool2 = w.pool().clone();
        sqlx::query("DELETE FROM workflow_runs").execute(&pool2).await.unwrap();
        w
    };
    let (repo, git) = w2.repo_with(w2.alice, "demo2", &[(CI_YML, CALLER), (".codehaven/workflows/build.yml", &callee)]).await;
    let run = w2.push(repo, &git, "main", None).await[0];
    let r = w2.runner("r2", &["x"]).await;
    let prep = r.request(&w2, 0).await.unwrap();
    r.complete(&w2, prep.job_id, "success", json!({})).await;
    let pack = r.request(&w2, 0).await.unwrap();
    r.complete(&w2, pack.job_id, "failure", json!({})).await;
    let tidy = r.request(&w2, 0).await.unwrap();
    assert_eq!(tidy.job_key, "build__tidy");
    r.complete(&w2, tidy.job_id, "success", json!({})).await;
    let all = keys(&w2, run).await;
    assert_eq!(all[4], row("build", "completed", Some("failure")));
    assert_eq!(all[5], row("after", "completed", Some("skipped")));
    assert_eq!(w2.run_status(run).await, ("completed".into(), Some("failure".into())));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_call_that_cannot_be_made_is_a_failed_run_that_says_why(pool: PgPool) {
    let w = World::new(pool).await;
    let no_input = CALLER.replace("    with:\n      version: '1.2'\n", "");
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &no_input), (".codehaven/workflows/build.yml", CALLEE)]).await;
    let runs = w.push(repo, &git, "main", None).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(w.run_status(runs[0]).await, ("completed".into(), Some("failure".into())));
    assert!(w.jobs(runs[0]).await.is_empty());
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    let why = w.state.ci.run_problem(&alice, repo, RunId(runs[0])).await.unwrap().unwrap();
    assert!(why.contains("needs the input `version`"), "{why}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_workflow_in_another_repository_is_used_only_where_the_run_may_use_it(pool: PgPool) {
    let w = World::new(pool).await;
    let tools_wf = ".codehaven/workflows/tool.yml";
    let callee = "on:\n  workflow_call:\njobs:\n  t:\n    runs-on: x\n    steps:\n      - run: echo tool\n";
    let (mine, _g1) = w.repo_with(w.alice, "tools", &[(tools_wf, callee)]).await;
    let (theirs, _g2) = w.repo_with(w.bob, "theirs", &[(tools_wf, callee)]).await;
    let _ = (mine, theirs);
    let caller = |target: &str| format!("on: push\njobs:\n  a:\n    uses: {target}\n");
    let (demo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &caller("haven_alice/tools/.codehaven/workflows/tool.yml@main"))]).await;
    let sha = git.git(&["rev-parse", "main"]);
    let alice_run = |wf: String| {
        let (w, sha) = (&w, sha.clone());
        async move {
            let parsed = ch_workflow::parse(&wf).unwrap();
            ch_ci::reusable::inline_calls(&w.ctx, demo, &sha, CI_YML, false, Some(w.alice), &parsed).await
        }
    };
    // The same owner's repository, which she can read, at a branch.
    let out = alice_run(caller("haven_alice/tools/.codehaven/workflows/tool.yml@main")).await.unwrap();
    assert_eq!(out.jobs.iter().map(|j| j.key.as_str()).collect::<Vec<_>>(), ["a__entry", "a__t", "a"]);
    // Another owner's private repository looks like one that is not there.
    for (target, want) in [
        ("haven_bob/theirs/.codehaven/workflows/tool.yml@main", "was not found on this instance"),
        ("haven_alice/nothing/.codehaven/workflows/tool.yml@main", "was not found on this instance"),
        ("haven_alice/tools/.codehaven/workflows/tool.yml@nope", "no tag or branch named nope"),
        ("haven_alice/tools/.codehaven/workflows/other.yml@main", "was not found"),
    ] {
        let e = alice_run(caller(target)).await.unwrap_err();
        assert!(format!("{e:?}").contains(want), "{target}: {e:?}");
    }
    // A scheduled run (no person) and a pull request from a fork use the repository's own files
    // and public ones only, so even her own private sibling is out of reach.
    let parsed = ch_workflow::parse(&caller("haven_alice/tools/.codehaven/workflows/tool.yml@main")).unwrap();
    for (fork, actor) in [(false, None), (true, Some(w.alice))] {
        let e = ch_ci::reusable::inline_calls(&w.ctx, demo, &sha, CI_YML, fork, actor, &parsed).await.unwrap_err();
        assert!(format!("{e:?}").contains("was not found on this instance"), "fork={fork}: {e:?}");
    }
    // The repository's own files are always in reach.
    let local = ch_workflow::parse(&caller("./.codehaven/workflows/ci.yml")).unwrap();
    let e = ch_ci::reusable::inline_calls(&w.ctx, demo, &sha, CI_YML, true, None, &local).await.unwrap_err();
    assert!(format!("{e:?}").contains("calls itself"), "{e:?}");
}

/// A `with:` value may read the outputs of the jobs the call needs. The called job waits for that
/// job too and is told its outputs, so the runner can read `needs.prep.outputs.v`.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_with_value_reads_the_outputs_of_the_job_the_call_needs(pool: PgPool) {
    let w = World::new(pool).await;
    let caller = CALLER
        .replace(
            "      - run: echo prep",
            "    outputs:\n      v: ${{ steps.s.outputs.v }}\n    steps:\n      - id: s\n        run: echo v=9 >> $GITHUB_OUTPUT",
        )
        .replace("    runs-on: x\n    steps:\n    outputs:", "    runs-on: x\n    outputs:")
        .replace("version: '1.2'", "version: v${{ needs.prep.outputs.v }}");
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &caller), (".codehaven/workflows/build.yml", CALLEE)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r", &["x"]).await;
    let prep = r.request(&w, 0).await.unwrap();
    assert_eq!(prep.job_key, "prep");
    r.complete(&w, prep.job_id, "success", json!({"v": "9"})).await;
    let pack = r.request(&w, 0).await.unwrap();
    assert_eq!(pack.job_key, "build__pack");
    assert_eq!(pack.needs["prep"].outputs["v"], "9", "the called job is told the caller's job's outputs");
    let StepRun(cmd) = StepRun::of(&pack);
    assert_eq!(cmd, "echo ${{ format('v{0}', needs.prep.outputs.v) }} ${{ (secrets.DEPLOY) }}");
    r.complete(&w, pack.job_id, "success", json!({"name": "dist.zip"})).await;
    assert_eq!(keys(&w, run).await.iter().find(|k| k.0 == "build").unwrap().1, "completed");
    // A `needs` the call does not list is refused when the run is made.
    let bad = CALLER.replace("version: '1.2'", "version: ${{ needs.after.outputs.v }}");
    let (repo2, git2) = w.repo_with(w.alice, "demo2", &[(CI_YML, &bad), (".codehaven/workflows/build.yml", CALLEE)]).await;
    let runs = w.push(repo2, &git2, "main", None).await;
    let last = *runs.last().unwrap();
    assert_eq!(w.run_status(last).await.1.as_deref(), Some("failure"), "a call that cannot be made is a failed run");
}
