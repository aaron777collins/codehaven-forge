//! The job engine end to end: a real runner client against the in-memory server, with the host
//! standing in for the container. These cover what the engine decides; `docker.rs` covers what
//! Docker does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use haven_runner::job::JobControl;
use haven_runner::testing::TestJob;
use std::time::{Duration, Instant};

#[tokio::test]
async fn a_simple_job_runs_reports_and_cleans_up() {
    let h = Harness::new().await;
    let job = TestJob::new(1).run("echo hello from codehaven; echo \"repo=$GITHUB_REPOSITORY ci=$CI actor=$CODEHAVEN_ACTOR\"").build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success");
    let log = h.server.step_log(1, 1);
    assert!(log.contains("hello from codehaven"), "{log}");
    assert!(log.contains("repo=alice/demo ci=true actor=alice"), "{log}");
    assert_eq!(h.server.step_result(1, 1).as_deref(), Some("success"));
    let ups = h.server.step_updates(1);
    assert_eq!(ups[&1][0].status, "in_progress");
    assert_eq!(ups[&1][1].status, "completed");
    assert!(h.leftovers().is_empty(), "work dir not cleaned: {:?}", h.leftovers());
    assert_eq!(h.backend.live_containers(), 0, "container not removed");
    assert_eq!(h.backend.removed().len(), 1);
}

#[tokio::test]
async fn the_container_is_limited_labelled_and_mounts_only_the_job_directory() {
    let h = Harness::new().await;
    h.run(TestJob::new(2).run("true").build()).await;
    let specs = h.backend.created();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert_eq!(s.cpus, 4.0);
    assert_eq!(s.memory, 8 << 30);
    assert_eq!(s.pids, 4096);
    assert_eq!(s.labels["org.codehaven.job"], "2");
    assert_eq!(s.binds.len(), 1);
    let (host, ctr) = s.binds[0].split_once(':').unwrap();
    assert_eq!(host, ctr, "the same path on both sides");
    assert!(host.contains("/job-2-"), "{host}");
    assert_eq!(s.image, "codehaven-job:trixie");
}

#[tokio::test]
async fn a_failing_step_fails_the_job_and_skips_the_rest() {
    let h = Harness::new().await;
    let job = TestJob::new(3)
        .run("echo before")
        .run("echo about to fail; exit 3")
        .run("echo SHOULD-NOT-RUN")
        .run("echo cleanup ran")
        .condition("always()")
        .run("echo failure handler ran")
        .condition("failure()")
        .run("echo success handler")
        .condition("success()")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure");
    assert_eq!(h.server.step_result(3, 1).as_deref(), Some("success"));
    assert_eq!(h.server.step_result(3, 2).as_deref(), Some("failure"));
    assert_eq!(h.server.step_result(3, 3).as_deref(), Some("skipped"));
    assert_eq!(h.server.step_result(3, 4).as_deref(), Some("success"));
    assert_eq!(h.server.step_result(3, 5).as_deref(), Some("success"));
    assert_eq!(h.server.step_result(3, 6).as_deref(), Some("skipped"));
    assert!(h.server.step_log(3, 2).contains("Process completed with exit code 3."));
    assert!(!h.server.output_text(3).contains("SHOULD-NOT-RUN"));
    assert!(h.server.step_log(3, 4).contains("cleanup ran"));
}

#[tokio::test]
async fn bash_is_strict_about_errors_in_the_middle_of_a_script() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(4).run_shell("bash", "false\necho STILL-RUNNING").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(!h.server.output_text(4).contains("STILL-RUNNING"), "bash -e must stop at the first failure");
}

#[tokio::test]
async fn continue_on_error_keeps_the_job_green_but_records_the_outcome() {
    let h = Harness::new().await;
    let job = TestJob::new(5)
        .run_id("flaky", "exit 1")
        .continue_on_error()
        .run("echo outcome=${{ steps.flaky.outcome }} conclusion=${{ steps.flaky.conclusion }}")
        .run("echo only-after-failure")
        .condition("steps.flaky.outcome == 'failure'")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success");
    assert_eq!(h.server.step_result(5, 1).as_deref(), Some("success"), "the step's reported conclusion is success");
    assert!(h.server.step_log(5, 2).contains("outcome=failure conclusion=success"), "{}", h.server.step_log(5, 2));
    assert!(h.server.step_log(5, 3).contains("only-after-failure"));
}

#[tokio::test]
async fn step_outputs_flow_to_later_steps_and_to_the_server() {
    let h = Harness::new().await;
    let job = TestJob::new(6)
        .run_id("v", "echo \"ver=1.2\" >> \"$GITHUB_OUTPUT\"\necho 'notes<<EOF' >> \"$GITHUB_OUTPUT\"\necho line1 >> \"$GITHUB_OUTPUT\"\necho line2 >> \"$GITHUB_OUTPUT\"\necho EOF >> \"$GITHUB_OUTPUT\"")
        .run("echo got-${{ steps.v.outputs.ver }}")
        .output("ver", "${{ steps.v.outputs.ver }}")
        .output("combined", "v${{ steps.v.outputs.ver }}-${{ github.run_number }}")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success");
    assert!(h.server.step_log(6, 2).contains("got-1.2"));
    let outs = h.server.step_outputs(6, 1);
    assert_eq!(outs["ver"], "1.2");
    assert_eq!(outs["notes"], "line1\nline2");
    assert_eq!(r.outputs["ver"], "1.2");
    assert_eq!(r.outputs["combined"], "v1.2-1");
}

#[tokio::test]
async fn old_style_set_output_still_works() {
    let h = Harness::new().await;
    let job = TestJob::new(7).run_id("a", "echo '::set-output name=color::blue'").run("echo c=${{ steps.a.outputs.color }}").build();
    h.run(job).await;
    assert!(h.server.step_log(7, 2).contains("c=blue"));
    assert!(!h.server.step_output(7, 1).contains("set-output"), "the command line is not shown");
}

#[tokio::test]
async fn github_env_and_github_path_carry_to_later_steps() {
    let h = Harness::new().await;
    let job = TestJob::new(8)
        .run("echo \"MY_FLAG=on\" >> \"$GITHUB_ENV\"\nmkdir -p \"$RUNNER_TEMP/bin\"\nprintf '#!/bin/sh\\necho tool-ran\\n' > \"$RUNNER_TEMP/bin/mytool\"\nchmod +x \"$RUNNER_TEMP/bin/mytool\"\necho \"$RUNNER_TEMP/bin\" >> \"$GITHUB_PATH\"")
        .run("echo flag=$MY_FLAG; mytool")
        .run("echo override=$MY_FLAG").step_env("MY_FLAG", "step-wins")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(8));
    let l = h.server.step_log(8, 2);
    assert!(l.contains("flag=on"), "{l}");
    assert!(l.contains("tool-ran"), "{l}");
    assert!(h.server.step_log(8, 3).contains("override=step-wins"));
}

#[tokio::test]
async fn the_runner_owns_github_variable_names() {
    let h = Harness::new().await;
    let job = TestJob::new(9)
        .run("echo 'GITHUB_SHA=evil' >> \"$GITHUB_ENV\"; echo 'OK_VAR=fine' >> \"$GITHUB_ENV\"")
        .run("echo sha=$GITHUB_SHA ok=$OK_VAR")
        .build();
    h.run(job).await;
    assert!(h.server.step_log(9, 1).contains("cannot set GITHUB_SHA"));
    let l = h.server.step_log(9, 2);
    assert!(l.contains(&format!("sha={}", "a".repeat(40))), "{l}");
    assert!(l.contains("ok=fine"));
}

#[tokio::test]
async fn a_malformed_output_file_fails_the_step_with_a_reason() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(10).run("echo 'no equals sign' >> \"$GITHUB_OUTPUT\"").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(10, 1).contains("$GITHUB_OUTPUT: invalid format"));
}

#[tokio::test]
async fn step_summaries_reach_the_log() {
    let h = Harness::new().await;
    h.run(TestJob::new(11).run("echo '## Results' >> \"$GITHUB_STEP_SUMMARY\"; echo 'all good' >> \"$GITHUB_STEP_SUMMARY\"").build()).await;
    let l = h.server.step_log(11, 1);
    assert!(l.contains("##[group]Step summary") && l.contains("## Results") && l.contains("all good"), "{l}");
}

#[tokio::test]
async fn secrets_are_masked_everywhere_in_the_log() {
    let h = Harness::new().await;
    let pem = "-----BEGIN KEY-----\nQUJDREVGR0g=\nSElKS0xNTk8=\n-----END KEY-----";
    let job = TestJob::new(12)
        .secret("API_KEY", "s3cr3t-value-123")
        .secret("PEM", pem)
        .run("echo \"key=$K\"; echo \"again ${{ secrets.API_KEY }} plain\"; echo \"$P\"; echo token=${{ github.token }}")
        .step_env("K", "${{ secrets.API_KEY }}")
        .step_env("P", "${{ secrets.PEM }}")
        .run("v=dynamic-secret-$((90+9)); echo \"::add-mask::$v\"; echo \"leak $v now\"")
        .run("echo \"line: QUJDREVGR0g= and SElKS0xNTk8=\" >&2")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success");
    let log = h.server.log_text(12);
    for secret in ["s3cr3t-value-123", "QUJDREVGR0g=", "SElKS0xNTk8=", "dynamic-secret-99", "BEGIN KEY", "job_token_12"] {
        assert!(!log.contains(secret), "{secret:?} leaked into the log:\n{log}");
    }
    assert!(log.contains("key=***"), "{log}");
    assert!(log.contains("again *** plain"));
    assert!(log.contains("leak *** now"));
    assert!(!log.contains(&h.credential), "the runner's own credential must never be logged");
}

#[tokio::test]
async fn job_outputs_that_contain_a_secret_are_not_sent() {
    let h = Harness::new().await;
    let job = TestJob::new(13).secret("S", "topsecretvalue").output("leaky", "x-${{ secrets.S }}").output("fine", "ok").build();
    let r = h.run(job).await;
    assert_eq!(r.outputs.get("fine").map(String::as_str), Some("ok"));
    assert!(!r.outputs.contains_key("leaky"));
    assert!(h.server.log_text(13).contains("Skipped job output"));
}

#[tokio::test]
async fn conditions_use_the_workflow_expression_language() {
    let h = Harness::new().await;
    let job = TestJob::new(14)
        .run("echo push-only")
        .condition("github.event_name == 'push'")
        .run("echo pr-only")
        .condition("github.event_name == 'pull_request'")
        .run("echo main-branch")
        .condition("${{ github.ref == 'refs/heads/main' && !contains(github.actor, 'bot') }}")
        .run("echo matrix-linux")
        .condition("matrix.os == 'linux'")
        .run("echo var-set")
        .condition("vars.FEATURE == 'on'")
        .matrix(serde_json::json!({"os": "Linux"}))
        .var("FEATURE", "on")
        .build();
    h.run(job).await;
    let res: Vec<_> = (1..=5).map(|n| h.server.step_result(14, n).unwrap()).collect();
    assert_eq!(res, ["success", "skipped", "success", "success", "success"], "matrix.os compares case-insensitively");
}

#[tokio::test]
async fn a_broken_condition_fails_the_step_and_says_why() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(15).run("echo x").condition("github.event_name ==").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(15, 1).contains("Cannot evaluate the condition"));
}

#[tokio::test]
async fn expressions_in_run_env_and_job_env_are_evaluated() {
    let h = Harness::new().await;
    let job = TestJob::new(16)
        .env("FROM_JOB", "run-${{ github.run_id }}")
        .run("echo \"a=$FROM_JOB b=$STEP_VAR c=${{ format('{0}-{1}', matrix.n, runner.os) }} d=${{ join(fromJSON('[1,2,3]'), '+') }}\"")
        .step_env("STEP_VAR", "${{ github.repository_owner }}/${{ env.FROM_JOB }}")
        .matrix(serde_json::json!({"n": 7}))
        .build();
    h.run(job).await;
    let l = h.server.step_log(16, 1);
    assert!(l.contains("a=run-116 b=alice/run-116 c=7-Linux d=1+2+3"), "{l}");
}

#[tokio::test]
async fn an_expression_error_in_run_fails_the_step_without_running_it() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(17).run("echo ${{ nope() }}").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(17, 1).contains("unknown function nope()"));
}

#[tokio::test]
async fn working_directory_and_shells() {
    let h = Harness::new().await;
    let job = TestJob::new(18)
        .run("mkdir -p sub/dir && echo hi > sub/dir/f.txt")
        .run("pwd; cat f.txt")
        .with(|s| {
            if let ch_api_types::runner::StepKind::Run { working_directory, .. } = &mut s.kind {
                *working_directory = Some("sub/dir".into());
            }
        })
        .run_shell("sh", "echo posix-sh; test -n \"$BASH_VERSION\" && echo HAS-BASH || echo no-bash")
        .run_shell("bash -x {0}", "echo traced")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(18));
    let l = h.server.step_log(18, 2);
    assert!(l.contains("sub/dir") && l.contains("hi"), "{l}");
    assert!(h.server.step_log(18, 3).contains("posix-sh"));
    assert!(h.server.step_log(18, 4).contains("+ echo traced"), "{}", h.server.step_log(18, 4));
}

#[tokio::test]
async fn workflow_commands_become_annotations() {
    let h = Harness::new().await;
    h.run(TestJob::new(19).run("echo '::group::Build'; echo '::warning file=a.rs,line=3::careful'; echo '::error::boom'; echo '::endgroup::'; echo '::debug::hidden'").build()).await;
    let l = h.server.step_log(19, 1);
    for want in ["##[group]Build", "##[warning]a.rs:3: careful", "##[error]boom", "##[endgroup]"] {
        assert!(l.contains(want), "missing {want:?} in {l}");
    }
    assert!(!h.server.step_output(19, 1).contains("hidden") && !l.contains("##[debug]"));
}

#[tokio::test]
async fn hash_files_sees_the_workspace() {
    use sha2::{Digest, Sha256};
    let h = Harness::new().await;
    let job = TestJob::new(20)
        .run("printf 'lockfile-body' > Cargo.lock")
        .run("echo key=${{ hashFiles('**/Cargo.lock') }}")
        .run("echo none=[${{ hashFiles('nothing/**') }}]")
        .build();
    h.run(job).await;
    let expected = hex::encode(Sha256::digest(Sha256::digest(b"lockfile-body")));
    assert!(h.server.step_log(20, 2).contains(&format!("key={expected}")), "{}", h.server.step_log(20, 2));
    assert!(h.server.step_log(20, 3).contains("none=[]"));
}

#[tokio::test]
async fn actions_the_server_will_not_give_fail_with_a_clear_message() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(21).uses("actions/setup-node@v4", &[("node-version", "20")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(21, 1).contains("not allowed on this instance"), "{}", h.server.step_log(21, 1));
}

#[tokio::test]
async fn docker_steps_run_the_image_with_args() {
    let h = Harness::new().await;
    let job =
        TestJob::new(22).uses("docker://alpine:3.20", &[("args", "echo 'hello from the image'"), ("entrypoint", "/bin/sh -c")]).build();
    // LocalBackend runs entrypoint + cmd on the host.
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(22));
    assert!(h.server.step_log(22, 1).contains("hello from the image"), "{}", h.server.step_log(22, 1));
    let created = h.backend.created();
    assert_eq!(created.len(), 2, "the job container and the step container");
    assert_eq!(created[1].image, "alpine:3.20");
    assert!(h.backend.removed().len() == 2, "both containers are removed");
}

// ---- container.options and images --------------------------------------------------------------

#[tokio::test]
async fn forbidden_container_options_stop_the_job_before_anything_starts() {
    let h = Harness::new().await;
    for (i, opt) in ["--privileged", "-v /:/host", "--network host", "--cap-add SYS_ADMIN", "--device /dev/kvm"].iter().enumerate() {
        let id = 30 + i as i64;
        let r = h.run(TestJob::new(id).container("rust:1", Some(opt)).run("echo should-not-run").build()).await;
        assert_eq!(r.conclusion, "failure", "{opt}");
        assert!(h.server.log_text(id).contains("is not allowed"), "{opt}: {}", h.server.log_text(id));
        assert!(!h.server.log_text(id).contains("should-not-run"));
    }
    assert!(h.backend.created().is_empty(), "no container may be created for a refused job");
    assert!(h.leftovers().is_empty());
}

#[tokio::test]
async fn container_options_apply_within_the_runner_limits() {
    let h = Harness::new().await;
    h.run(
        TestJob::new(40)
            .container("rust:1-trixie", Some("--cpus 16 --memory 1g -e FOO=bar --user 1000 --workdir /src"))
            .run("true")
            .build(),
    )
    .await;
    let s = &h.backend.created()[0];
    assert_eq!(s.image, "rust:1-trixie");
    assert_eq!(s.cpus, 4.0, "capped to the runner's limit");
    assert_eq!(s.memory, 1 << 30);
    assert_eq!(s.env["FOO"], "bar");
    assert_eq!(s.user.as_deref(), Some("1000"));
    assert!(h.server.log_text(40).contains("--cpus 16 is capped to 4"));
}

#[tokio::test]
async fn labels_choose_the_image() {
    let h = Harness::with_config(|c| {
        c.labels.insert("rust".into(), "rust:1-trixie".into());
    })
    .await;
    let mut job = TestJob::new(41).run("true").build();
    job.runs_on = vec!["self-hosted".into(), "rust".into()];
    h.run(job).await;
    assert_eq!(h.backend.created()[0].image, "rust:1-trixie");
}

#[tokio::test]
async fn an_image_that_cannot_be_pulled_fails_the_job_cleanly() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(42).container("missing/image:nope", None).run("echo x").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.log_text(42).contains("pulling missing/image:nope failed"));
    assert!(h.backend.created().is_empty());
    assert!(h.leftovers().is_empty());
}

#[tokio::test]
async fn a_container_that_cannot_start_fails_the_job_cleanly() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(43).container("broken:1", None).run("echo x").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.log_text(43).contains("cannot start a container"));
    assert!(h.leftovers().is_empty());
}

// ---- cancel and timeouts -------------------------------------------------------------------------

#[tokio::test]
async fn a_server_cancel_stops_the_running_step_and_runs_cleanup_steps() {
    let h = Harness::new().await;
    let mut job = TestJob::new(50)
        .run("echo started; sleep 31701; echo NEVER")
        .run("echo skipped-by-default")
        .run("echo cleanup-ran")
        .condition("always()")
        .run("echo cancelled-handler")
        .condition("cancelled()")
        .build();
    job.lease_seconds = 3; // heartbeat every second
    let server = &h.server;
    let t0 = Instant::now();
    let canceller = async {
        server.wait_log(50, "started", Duration::from_secs(10)).await;
        server.cancel_job(50);
    };
    let (r, _) = tokio::join!(h.run(job), canceller);
    assert_eq!(r.conclusion, "cancelled");
    assert!(t0.elapsed() < Duration::from_secs(15), "took {:?}", t0.elapsed());
    assert_eq!(h.server.step_result(50, 1).as_deref(), Some("cancelled"));
    assert_eq!(h.server.step_result(50, 2).as_deref(), Some("skipped"));
    assert_eq!(h.server.step_result(50, 3).as_deref(), Some("success"));
    assert_eq!(h.server.step_result(50, 4).as_deref(), Some("success"));
    assert!(!h.server.output_text(50).contains("NEVER"));
    assert!(!process_running("sleep 31701"), "the cancelled step's process must be dead");
    assert!(h.leftovers().is_empty());
}

#[tokio::test]
async fn cancelling_kills_the_whole_process_tree_of_a_step() {
    let h = Harness::new().await;
    // A shell that starts background children and a subshell chain, then waits.
    let job = TestJob::new(51).run("(sleep 31702 &) ; sleep 31703 & ( sleep 31704; echo NEVER ) & echo started; wait").build();
    let ctl = JobControl::default();
    let c2 = ctl.cancel.clone();
    let server = &h.server;
    let canceller = async {
        server.wait_log(51, "started", Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(process_running("sleep 31703"), "children should be running before the cancel");
        c2.cancel();
    };
    let (r, _) = tokio::join!(h.run_with(job, ctl), canceller);
    assert_eq!(r.conclusion, "cancelled");
    tokio::time::sleep(Duration::from_millis(300)).await;
    for n in ["sleep 31703", "sleep 31704"] {
        assert!(!process_running(n), "{n} survived the cancel");
    }
    // 31702 was double-forked and so is not in the step's tree. In a container it dies with the
    // container; here the host stands in for it.
    let _ = std::process::Command::new("pkill").args(["-f", "sleep 31702"]).status();
}

#[tokio::test]
async fn a_step_timeout_fails_that_step_only() {
    let h = Harness::new().await;
    let job = TestJob::new(52).run("echo before; sleep 31705").step_timeout(1).run("echo after-timeout").condition("always()").build();
    let t0 = Instant::now();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure", "a step timeout is a failure, not a job timeout");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(h.server.step_log(52, 1).contains("exceeded its time limit of 1 minutes"), "{}", h.server.step_log(52, 1));
    assert!(h.server.step_log(52, 2).contains("after-timeout"));
    assert!(!process_running("sleep 31705"));
}

#[tokio::test]
async fn a_job_timeout_ends_the_job_as_timed_out_and_still_runs_cleanup() {
    let h = Harness::new().await;
    let job = TestJob::new(53)
        .run("sleep 31706")
        .run("echo not-run")
        .run("echo cleanup-after-timeout")
        .condition("always()")
        .timeout_minutes(1)
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "timed_out");
    assert!(h.server.step_log(53, 1).contains("exceeded its time limit of 1 minutes"));
    assert_eq!(h.server.step_result(53, 2).as_deref(), Some("skipped"));
    assert!(h.server.step_log(53, 3).contains("cleanup-after-timeout"));
    assert!(!process_running("sleep 31706"));
}

#[tokio::test]
async fn the_runners_own_cap_beats_a_longer_request() {
    let h = Harness::with_config(|c| c.limits.max_job_minutes = 1).await;
    let r = h.run(TestJob::new(54).run("sleep 31707").timeout_minutes(500).build()).await;
    assert_eq!(r.conclusion, "timed_out");
}

#[tokio::test]
async fn a_background_process_does_not_hold_a_finished_step_open() {
    let h = Harness::new().await;
    let t0 = Instant::now();
    let r = h.run(TestJob::new(56).run("(sleep 31710 >/dev/null 2>&1 &); echo main-done").run("echo second").build()).await;
    assert_eq!(r.conclusion, "success");
    assert!(t0.elapsed() < Duration::from_secs(10), "the step waited for its background child: {:?}", t0.elapsed());
    assert!(h.server.step_log(56, 1).contains("main-done"));
    assert!(h.server.step_log(56, 2).contains("second"));
    // The leftover belongs to the container, which dies with the job. Here the host stands in
    // for it, so clean up after ourselves.
    let _ = std::process::Command::new("pkill").args(["-f", "sleep 31710"]).status();
}

#[tokio::test]
async fn a_hard_stop_ends_the_job_without_running_cleanup() {
    let h = Harness::new().await;
    let job = TestJob::new(55).run("echo started; sleep 31708").run("echo cleanup").condition("always()").build();
    let ctl = JobControl::default();
    let (c, hs) = (ctl.cancel.clone(), ctl.hard_stop.clone());
    let server = &h.server;
    let stopper = async {
        server.wait_log(55, "started", Duration::from_secs(10)).await;
        c.cancel();
        hs.cancel();
    };
    let (r, _) = tokio::join!(h.run_with(job, ctl), stopper);
    assert_eq!(r.conclusion, "cancelled");
    assert!(!h.server.output_text(55).contains("cleanup"), "hard stop skips cleanup steps");
    assert!(!h.server.step_updates(55).contains_key(&2), "the cleanup step never started");
    assert!(h.leftovers().is_empty());
}

// ---- talking to the server --------------------------------------------------------------------

#[tokio::test]
async fn logs_survive_server_hiccups() {
    let h = Harness::new().await;
    h.server.fail_next("logs", &[503, 503, 500]);
    h.server.fail_next("steps", &[502]);
    h.server.fail_next("complete", &[503, 503]);
    let r = h.run(TestJob::new(60).run("for i in 1 2 3 4 5; do echo line-$i; done").build()).await;
    assert_eq!(r.conclusion, "success");
    let l = h.server.step_log(60, 1);
    for i in 1..=5 {
        assert!(l.contains(&format!("line-{i}")), "{l}");
    }
    assert_eq!(h.server.step_result(60, 1).as_deref(), Some("success"));
}

#[tokio::test]
async fn log_lines_arrive_in_order_across_many_batches() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(61).run("for i in $(seq 1 3000); do echo row-$i; sleep 0.0005; done").build()).await;
    assert_eq!(r.conclusion, "success");
    let lines: Vec<String> = h.server.log_lines(61).into_iter().map(|(_, l)| l).filter(|l| l.starts_with("row-")).collect();
    assert_eq!(lines.len(), 3000);
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(l, &format!("row-{}", i + 1), "out of order at {i}");
    }
    assert!(h.server.log_posts() > 1, "a long log goes in several batches");
}

#[tokio::test]
async fn a_server_that_forgets_the_job_stops_it_and_reports_nothing() {
    let h = Harness::new().await;
    let mut job = TestJob::new(62).run("echo started; sleep 31709").build();
    job.lease_seconds = 3;
    h.server.register_job(&job);
    let server = &h.server;
    let t0 = Instant::now();
    let dropper = async {
        server.wait_log(62, "started", Duration::from_secs(10)).await;
        server.drop_job(62);
    };
    let run = haven_runner::runner::execute(&h.deps, &h.client, job, JobControl::default());
    tokio::join!(run, dropper);
    assert!(t0.elapsed() < Duration::from_secs(15));
    assert!(h.server.completed(62).is_none(), "nothing to report to a server that dropped the job");
    assert!(!process_running("sleep 31709"));
    assert!(h.leftovers().is_empty());
}

#[tokio::test]
async fn heartbeats_are_sent_while_a_job_runs() {
    let h = Harness::new().await;
    let mut job = TestJob::new(63).run("sleep 3.5").build();
    job.lease_seconds = 3;
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success");
    assert!(h.server.heartbeats(63) >= 2, "got {}", h.server.heartbeats(63));
}

#[tokio::test]
async fn very_long_lines_and_binary_output_do_not_break_the_log() {
    let h = Harness::new().await;
    let r = h
        .run(TestJob::new(64).run("head -c 200000 /dev/zero | tr '\\0' 'x'; echo; head -c 3000 /dev/urandom; echo done-marker").build())
        .await;
    assert_eq!(r.conclusion, "success");
    let lines = h.server.log_lines(64);
    assert!(lines.iter().all(|(_, l)| l.len() <= 70_000), "no line over the cap");
    assert!(lines.iter().any(|(_, l)| l.contains("done-marker")));
}

#[tokio::test]
async fn the_log_has_a_size_limit() {
    let h = Harness::with_config(|c| c.limits.max_log_megabytes = 1).await;
    let r = h.run(TestJob::new(65).run("i=0; while [ $i -lt 100000 ]; do echo \"line $i that is rather long and repeated many times over to fill the log\"; i=$((i+1)); done; echo finished").build()).await;
    assert_eq!(r.conclusion, "success", "hitting the log limit does not fail the job");
    let text = h.server.log_text(65);
    assert!(text.contains("log limit of 1 MB was reached"));
    assert!(text.len() < 2 * 1024 * 1024);
}
