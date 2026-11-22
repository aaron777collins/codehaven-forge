//! The executor against a real Docker daemon. These need one: `make docker-test` starts a
//! throwaway daemon (`dind-test`, never the host's) and runs this file with
//! `HAVEN_REQUIRE_DOCKER=1`. Without a daemon they skip, loudly.
//!
//! The ones named `nonroot_*` run the job as uid 10001, like the runner does in its container
//! (the builder runs as root). They re-run themselves under `setpriv`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use bollard::Docker;
use bollard::query_parameters::{InspectContainerOptions, KillContainerOptions, ListContainersOptionsBuilder, ListNetworksOptionsBuilder};
use common::*;
use haven_runner::backend::{Backend, ContainerSpec, ExecSpec};
use haven_runner::docker::{DockerBackend, LABEL_JOB, LABEL_NETWORK, LABEL_RUNNER};
use haven_runner::job::JobControl;
use haven_runner::testing::TestJob;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHELL_IMAGE: &str = "debian:trixie-slim";
/// The image jobs really get: python3 and bash, which the isolation test's scripts need.
const JOB_IMAGE: &str = "codehaven-job:trixie";

/// A real daemon, or `None` (and a skip note) when there is none and none is required.
async fn daemon() -> Option<Arc<DockerBackend>> {
    let require = std::env::var("HAVEN_REQUIRE_DOCKER").is_ok_and(|v| v == "1");
    let Ok(host) = std::env::var("HAVEN_TEST_DOCKER_HOST") else {
        assert!(!require, "HAVEN_REQUIRE_DOCKER=1 but HAVEN_TEST_DOCKER_HOST is not set");
        eprintln!("SKIPPED: no HAVEN_TEST_DOCKER_HOST. Run `make docker-test`.");
        return None;
    };
    let b = DockerBackend::connect(&host).expect("connect");
    match b.ping().await {
        Ok(_) => Some(Arc::new(b)),
        Err(e) => {
            assert!(!require, "the Docker daemon at {host} is required but not reachable: {e:#}");
            eprintln!("SKIPPED: the Docker daemon at {host} is not reachable: {e:#}");
            None
        }
    }
}

fn shared_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HAVEN_TEST_WORK_DIR").expect("HAVEN_TEST_WORK_DIR is set with HAVEN_TEST_DOCKER_HOST"))
}

fn is_root() -> bool {
    rustix::process::getuid().is_root()
}

/// A harness whose jobs run in the real daemon, in a directory both sides can see.
async fn real(backend: Arc<DockerBackend>, work: &std::path::Path) -> Harness {
    let work = work.to_path_buf();
    let mut h = Harness::with_config(move |c| {
        c.runner.work_dir = work.join("work");
        c.disk.guard_paths = vec![c.runner.work_dir.clone()];
        // Real images take a moment to start; keep the "minute" long enough for that.
    })
    .await;
    h.deps.backend = backend;
    h.deps.minute = Duration::from_secs(3);
    h
}

async fn containers_for(docker: &Docker, label: &str) -> Vec<String> {
    let mut f: HashMap<String, Vec<String>> = HashMap::new();
    f.insert("label".into(), vec![label.to_string()]);
    let opts = ListContainersOptionsBuilder::default().all(true).filters(&f).build();
    docker.list_containers(Some(opts)).await.expect("list").into_iter().filter_map(|c| c.id).collect()
}

/// Runs one command in a container through the backend and returns its exit code and output.
async fn exec_out(b: &DockerBackend, id: &str, argv: &[&str]) -> (i64, String) {
    let spec = ExecSpec { argv: argv.iter().map(|s| s.to_string()).collect(), ..Default::default() };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let run = b.exec(id, &spec, tx);
    let mut out = Vec::new();
    let drain = async {
        while let Some(c) = rx.recv().await {
            out.extend_from_slice(&c.data);
        }
    };
    let (code, _) = tokio::join!(run, drain);
    (code.expect("the command ran"), String::from_utf8_lossy(&out).into_owned())
}

/// Re-runs the current test as uid 10001. Returns true in the parent (the child did the work).
fn rerun_as_runner_user(test: &str, dir: &std::path::Path) -> bool {
    if !is_root() || std::env::var("HAVEN_NONROOT_CHILD").is_ok() {
        return false;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let exe = std::env::current_exe().expect("exe");
    let out = std::process::Command::new("setpriv")
        .args(["--reuid=10001", "--regid=10001", "--clear-groups"])
        .arg(exe)
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env("HAVEN_NONROOT_CHILD", "1")
        .env("HAVEN_NONROOT_DIR", dir)
        .env("HOME", dir)
        .output()
        .expect("setpriv runs");
    assert!(
        out.status.success(),
        "the non-root run of {test} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    true
}

fn work_for_child_or_new() -> (tempfile::TempDir, std::path::PathBuf) {
    match std::env::var("HAVEN_NONROOT_DIR") {
        Ok(d) if std::env::var("HAVEN_NONROOT_CHILD").is_ok() => (tempfile::tempdir().expect("tmp"), d.into()),
        _ => {
            let t = tempfile::tempdir_in(shared_dir()).expect("tempdir in the shared dir");
            let p = t.path().to_path_buf();
            (t, p)
        }
    }
}

// ---- the basics -------------------------------------------------------------------------------

#[tokio::test]
async fn a_job_runs_in_a_real_container_and_leaves_nothing_behind() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    let job = TestJob::new(9001)
        .container(SHELL_IMAGE, None)
        .run("echo \"os=$(. /etc/os-release; echo $ID)\"; echo \"ws=$(pwd)\"; test -d \"$GITHUB_WORKSPACE\" && echo ws-exists; echo \"uid=$(id -u)\"; echo CI=$CI")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9001));
    let l = h.server.step_log(9001, 1);
    assert!(l.contains("os=debian") && l.contains("ws-exists") && l.contains("uid=0") && l.contains("CI=true"), "{l}");
    assert!(l.contains("ws=") && l.contains("/job-9001-") && l.ends_with("CI=true") || l.contains("/workspace"), "{l}");
    assert!(containers_for(b.docker(), &format!("{LABEL_JOB}=9001")).await.is_empty(), "the container must be gone");
    assert!(h.leftovers().is_empty(), "{:?}", h.leftovers());
}

#[tokio::test]
async fn the_default_job_image_has_the_tools_scripts_expect() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    // No container.image: the runner's own default, codehaven-job:trixie, built into the test daemon.
    let job = TestJob::new(9002).run("git --version && bash --version | head -1 && node --version && python3 --version && gcc --version | head -1 && curl --version | head -1 && jq --version && echo TOOLS-OK").build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9002));
    assert!(h.server.step_log(9002, 1).contains("TOOLS-OK"));
}

#[tokio::test]
async fn steps_share_files_and_environment_through_the_bind_mount() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    let job = TestJob::new(9003)
        .container(SHELL_IMAGE, None)
        .run_id("a", "echo hello > made-in-step-1.txt; echo 'ver=2.0' >> \"$GITHUB_OUTPUT\"; echo 'SHARED=yes' >> \"$GITHUB_ENV\"; mkdir -p \"$RUNNER_TEMP/bin\"; printf '#!/bin/sh\\necho tool\\n' > \"$RUNNER_TEMP/bin/t\"; chmod +x \"$RUNNER_TEMP/bin/t\"; echo \"$RUNNER_TEMP/bin\" >> \"$GITHUB_PATH\"")
        .run("cat made-in-step-1.txt; echo shared=$SHARED v=${{ steps.a.outputs.ver }}; t")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9003));
    let l = h.server.step_log(9003, 2);
    assert!(l.contains("hello") && l.contains("shared=yes v=2.0") && l.contains("tool"), "{l}");
}

#[tokio::test]
async fn exit_codes_and_both_output_streams() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    let job = TestJob::new(9004)
        .container(SHELL_IMAGE, None)
        .run("echo to-stdout; echo to-stderr >&2; exit 7")
        .run("echo after")
        .condition("always()")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure");
    let l = h.server.step_log(9004, 1);
    assert!(l.contains("to-stdout") && l.contains("to-stderr") && l.contains("exit code 7"), "{l}");
    assert!(h.server.step_log(9004, 2).contains("after"));
}

#[tokio::test]
async fn large_output_arrives_whole_and_in_order() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    let r = h.run(TestJob::new(9005).container(SHELL_IMAGE, None).run("seq 1 30000").build()).await;
    assert_eq!(r.conclusion, "success");
    let nums: Vec<u32> = h.server.log_lines(9005).into_iter().filter_map(|(_, l)| l.parse().ok()).collect();
    assert_eq!(nums.len(), 30000);
    assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "out of order");
}

#[tokio::test]
async fn a_background_process_does_not_hold_the_step_open() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    let t0 = Instant::now();
    let r = h.run(TestJob::new(9006).container(SHELL_IMAGE, None).run("(sleep 300 &); echo main-done").run("echo second").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9006));
    assert!(t0.elapsed() < Duration::from_secs(40), "{:?}", t0.elapsed());
    assert!(h.server.step_log(9006, 2).contains("second"));
}

// ---- isolation ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_container_is_limited_unprivileged_and_mounts_only_its_job_directory() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    let job = TestJob::new(9010).container(SHELL_IMAGE, Some("--cpus 1 --memory 512m --shm-size 32m")).run("echo started; sleep 6").build();
    let docker = b.docker().clone();
    let inspect = async {
        h.server.wait_log(9010, "started", Duration::from_secs(60)).await;
        let ids = containers_for(&docker, &format!("{LABEL_JOB}=9010")).await;
        assert_eq!(ids.len(), 1, "exactly one container for the job");
        docker.inspect_container(&ids[0], None::<InspectContainerOptions>).await.expect("inspect")
    };
    let (_, info) = tokio::join!(h.run(job), inspect);
    let host = info.host_config.expect("host config");
    assert_eq!(host.privileged, Some(false));
    assert_eq!(host.nano_cpus, Some(1_000_000_000));
    assert_eq!(host.memory, Some(512 << 20));
    assert_eq!(host.memory_swap, Some(512 << 20), "no swap");
    assert_eq!(host.pids_limit, Some(4096));
    assert_eq!(host.shm_size, Some(32 << 20));
    assert!(host.devices.as_ref().is_none_or(|d| d.is_empty()), "no devices");
    assert!(host.cap_add.as_ref().is_none_or(|d| d.is_empty()), "no added capabilities");
    assert!(host.cap_drop.as_ref().is_some_and(|d| d.iter().any(|c| c == "NET_RAW")), "NET_RAW must be dropped");
    // A bridge of its own, shared with no other container.
    assert!(host.network_mode.as_deref().is_some_and(|m| m.starts_with("haven-net-")), "{:?}", host.network_mode);
    assert!(host.pid_mode.as_deref().is_none_or(str::is_empty));
    let binds = host.binds.expect("binds");
    assert_eq!(binds.len(), 1, "only the job directory: {binds:?}");
    let (src, dst) = binds[0].split_once(':').unwrap();
    assert_eq!(src, dst);
    assert!(src.contains("/job-9010-"));
    assert!(!binds.iter().any(|b| b.contains("docker.sock")));
    let mounts = info.mounts.unwrap_or_default();
    assert!(mounts.iter().all(|m| m.destination.as_deref().is_some_and(|d| d.contains("/job-9010-"))), "{mounts:?}");
}

#[tokio::test]
async fn a_job_cannot_reach_the_docker_socket_or_gain_privileges() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    let job = TestJob::new(9011)
        .container(SHELL_IMAGE, None)
        .run("test ! -e /var/run/docker.sock && echo no-socket; ls /dev | grep -c '^sd' || true; mount -t tmpfs none /mnt 2>&1 | head -1 || true; echo done")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9011));
    let l = h.server.step_log(9011, 1);
    assert!(l.contains("no-socket"), "{l}");
    assert!(
        l.contains("permission denied") || l.contains("Operation not permitted") || l.contains("must be superuser"),
        "mount should be refused in an unprivileged container: {l}"
    );
}

#[tokio::test]
async fn forbidden_container_options_never_reach_docker() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    for (i, opt) in ["--privileged", "-v /var/run/docker.sock:/var/run/docker.sock", "--network host", "--pid=host"].iter().enumerate() {
        let id = 9020 + i as i64;
        let r = h.run(TestJob::new(id).container(SHELL_IMAGE, Some(opt)).run("echo no").build()).await;
        assert_eq!(r.conclusion, "failure", "{opt}");
        assert!(containers_for(b.docker(), &format!("{LABEL_JOB}={id}")).await.is_empty());
    }
}

// Two containers of two different jobs: neither can reach the other, neither has NET_RAW, and
// the networks they ran on are gone afterwards.
#[tokio::test]
async fn jobs_cannot_reach_each_other_and_have_no_raw_sockets() {
    let Some(b) = daemon().await else { return };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    b.ensure_image(JOB_IMAGE, false, &tx).await.expect("codehaven-job:trixie is built into the test daemon by docker-tests.sh");
    // A runner label of this test alone, so parallel tests never sweep each other's leftovers.
    let spec = |name: &str| ContainerSpec {
        name: name.to_string(),
        image: JOB_IMAGE.to_string(),
        labels: BTreeMap::from([(LABEL_RUNNER.to_string(), "791".to_string()), (LABEL_JOB.to_string(), "orphan".to_string())]),
        env: BTreeMap::new(),
        binds: vec![],
        workdir: None,
        user: None,
        cpus: 0.5,
        memory: 128 << 20,
        pids: 128,
        shm_size: 16 << 20,
        entrypoint: Some(vec!["tail".into()]),
        cmd: Some(vec!["-f".into(), "/dev/null".into()]),
    };
    let a = b.create_and_start(&spec("iso-a")).await.expect("create a");
    let c = b.create_and_start(&spec("iso-b")).await.expect("create b");
    // A listener in A, backgrounded so the exec returns immediately.
    let (code, out) = exec_out(&b, &a, &["sh", "-c", "python3 -m http.server 8099 --bind 0.0.0.0 >/dev/null 2>&1 &"]).await;
    assert_eq!(code, 0, "{out}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let info = b.docker().inspect_container(&a, None::<InspectContainerOptions>).await.expect("inspect a");
    let ip = info
        .network_settings
        .as_ref()
        .and_then(|s| s.networks.as_ref())
        .and_then(|n| n.values().next())
        .and_then(|n| n.ip_address.clone())
        .expect("a has an address on its network");
    // Control: the listener is up, on A's own loopback.
    let (code, out) = exec_out(&b, &a, &["bash", "-c", "timeout 3 bash -c '</dev/tcp/127.0.0.1/8099'"]).await;
    assert_eq!(code, 0, "the control connection failed, so the listener never started: {out}");
    // B, on its own bridge, must not be able to open a connection to it.
    let probe = format!("timeout 3 bash -c '</dev/tcp/{ip}/8099'");
    let (code, out) = exec_out(&b, &c, &["bash", "-c", &probe]).await;
    assert_ne!(code, 0, "job b reached job a's listener at {ip}: {out}");
    // And B holds no NET_RAW: bit 13 (0x2000) of CapEff is zero.
    let (code, out) = exec_out(&b, &c, &["sh", "-c", "grep CapEff /proc/self/status"]).await;
    assert_eq!(code, 0, "{out}");
    let cap = out.split_whitespace().last().expect("the CapEff value");
    let cap = u64::from_str_radix(cap, 16).expect("CapEff is hexadecimal");
    assert_eq!(cap & 0x2000, 0, "NET_RAW is still held: CapEff {cap:x}");
    b.remove(&a).await.unwrap();
    b.remove(&c).await.unwrap();
    // Neither container's network is left behind.
    let mut f: HashMap<String, Vec<String>> = HashMap::new();
    f.insert("label".into(), vec![format!("{LABEL_RUNNER}=791"), format!("{LABEL_NETWORK}=1")]);
    let opts = ListNetworksOptionsBuilder::default().filters(&f).build();
    let nets = b.docker().list_networks(Some(opts)).await.expect("list networks");
    assert!(nets.is_empty(), "{} network(s) of this test's jobs are left behind", nets.len());
}

// ---- cancel and timeouts in a real container -----------------------------------------------------

#[tokio::test]
async fn cancel_kills_a_steps_whole_process_tree_in_the_container() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    let job = TestJob::new(9030)
        .container(SHELL_IMAGE, None)
        .run("sleep 31720 & sleep 31721 & (sleep 31722; echo NEVER) & echo started; wait")
        .run("if grep -l 'sleep.3172' /proc/[0-9]*/cmdline >/dev/null 2>&1; then echo TREE-ALIVE; else echo tree-dead; fi")
        .condition("always()")
        .build();
    let ctl = JobControl::default();
    let c2 = ctl.cancel.clone();
    let server = &h.server;
    let canceller = async {
        server.wait_log(9030, "started", Duration::from_secs(60)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        c2.cancel();
    };
    let t0 = Instant::now();
    let (r, _) = tokio::join!(h.run_with(job, ctl), canceller);
    assert_eq!(r.conclusion, "cancelled");
    assert!(t0.elapsed() < Duration::from_secs(60), "{:?}", t0.elapsed());
    let l = h.server.step_log(9030, 2);
    assert!(l.contains("tree-dead"), "the step's children survived the cancel: {l}");
    assert!(!h.server.output_text(9030).contains("NEVER"));
    assert!(containers_for(b.docker(), &format!("{LABEL_JOB}=9030")).await.is_empty());
}

#[tokio::test]
async fn a_step_timeout_stops_the_step_in_the_container() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b, &dir).await;
    let job = TestJob::new(9031)
        .container(SHELL_IMAGE, None)
        .run("echo begin; sleep 31730")
        .step_timeout(1)
        .run("echo next")
        .condition("always()")
        .build();
    let t0 = Instant::now();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(9031, 1).contains("exceeded its time limit"), "{}", h.server.step_log(9031, 1));
    assert!(h.server.step_log(9031, 2).contains("next"));
    assert!(t0.elapsed() < Duration::from_secs(60));
}

// ---- images and docker:// -------------------------------------------------------------------------

#[tokio::test]
async fn docker_steps_run_a_second_image_over_the_same_workspace() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    let job = TestJob::new(9040)
        .container(SHELL_IMAGE, None)
        .run("echo from-job-container > shared.txt")
        .uses_id(
            "a",
            "docker://alpine:3.20",
            &[(
                "args",
                "sh -c 'cat shared.txt; echo from-alpine; echo \"rel=$(cat /etc/alpine-release | cut -d. -f1-2)\" >> \"$GITHUB_OUTPUT\"'",
            )],
        )
        .run("echo alpine=${{ steps.a.outputs.rel }}")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9040));
    let l = h.server.step_log(9040, 2);
    assert!(l.contains("from-job-container") && l.contains("from-alpine"), "{l}");
    assert!(h.server.step_log(9040, 3).contains("alpine=3.20"), "{}", h.server.step_log(9040, 3));
    assert!(containers_for(b.docker(), &format!("{LABEL_JOB}=9040")).await.is_empty(), "the step container is removed too");
}

#[tokio::test]
async fn an_image_that_does_not_exist_fails_the_job_with_the_reason() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    let r = h.run(TestJob::new(9041).container("docker.io/library/codehaven-no-such-image-xyz:1", None).run("echo x").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(
        h.server.log_text(9041).contains("pulling docker.io/library/codehaven-no-such-image-xyz:1 failed"),
        "{}",
        h.server.log_text(9041)
    );
    assert!(containers_for(b.docker(), &format!("{LABEL_JOB}=9041")).await.is_empty());
    assert!(h.leftovers().is_empty());
}

#[tokio::test]
async fn an_image_with_no_shell_fails_the_job_cleanly() {
    let Some(b) = daemon().await else { return };
    let (_keep, dir) = work_for_child_or_new();
    let h = real(b.clone(), &dir).await;
    let r = h.run(TestJob::new(9042).container("hello-world:latest", None).run("echo x").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(containers_for(b.docker(), &format!("{LABEL_JOB}=9042")).await.is_empty(), "no container left from a job that could not start");
    assert!(h.leftovers().is_empty());
}

// ---- housekeeping --------------------------------------------------------------------------------

fn idle_spec(name: &str, image: &str, runner: i64) -> ContainerSpec {
    ContainerSpec {
        name: name.to_string(),
        image: image.to_string(),
        labels: BTreeMap::from([(LABEL_RUNNER.to_string(), runner.to_string()), (LABEL_JOB.to_string(), "orphan".to_string())]),
        env: BTreeMap::new(),
        binds: vec![],
        workdir: None,
        user: None,
        cpus: 0.5,
        memory: 128 << 20,
        pids: 128,
        shm_size: 16 << 20,
        entrypoint: Some(vec!["tail".into()]),
        cmd: Some(vec!["-f".into(), "/dev/null".into()]),
    }
}

#[tokio::test]
async fn containers_left_by_a_dead_runner_are_removed_and_only_that_runners() {
    let Some(b) = daemon().await else { return };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    b.ensure_image(SHELL_IMAGE, false, &tx).await.expect("image");
    let mine = b.create_and_start(&idle_spec("orphan-mine-1", SHELL_IMAGE, 777)).await.expect("create");
    let mine2 = b.create_and_start(&idle_spec("orphan-mine-2", SHELL_IMAGE, 777)).await.expect("create");
    let other = b.create_and_start(&idle_spec("orphan-other", SHELL_IMAGE, 778)).await.expect("create");
    assert_eq!(b.cleanup_orphans(777).await.unwrap(), 2);
    let left = containers_for(b.docker(), &format!("{LABEL_RUNNER}=777")).await;
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(containers_for(b.docker(), &format!("{LABEL_RUNNER}=778")).await.len(), 1, "another runner's container is untouched");
    let _ = (mine, mine2);
    b.remove(&other).await.unwrap();
    assert_eq!(b.cleanup_orphans(777).await.unwrap(), 0);
}

#[tokio::test]
async fn removing_a_container_twice_is_fine() {
    let Some(b) = daemon().await else { return };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    b.ensure_image(SHELL_IMAGE, false, &tx).await.unwrap();
    let id = b.create_and_start(&idle_spec("remove-twice", SHELL_IMAGE, 779)).await.unwrap();
    b.remove(&id).await.unwrap();
    b.remove(&id).await.unwrap();
    b.remove("no-such-container-at-all").await.unwrap();
}

#[tokio::test]
async fn pruning_removes_unused_images_but_never_protected_or_running_ones() {
    let Some(b) = daemon().await else { return };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    for img in ["hello-world:latest", "alpine:3.19", "alpine:3.20"] {
        b.ensure_image(img, false, &tx).await.expect("pull");
    }
    // alpine:3.19 is in use by a running container; alpine:3.20 is protected; hello-world is neither.
    let running = b.create_and_start(&idle_spec("prune-keeper", "alpine:3.19", 780)).await.expect("start");
    let freed = b.prune_images(Duration::ZERO, &["alpine:3.20".to_string(), "codehaven-job:trixie".to_string()]).await.expect("prune");
    assert!(freed > 0, "hello-world should have been removed");
    let d = b.docker();
    assert!(d.inspect_image("hello-world:latest").await.is_err(), "an unused image is removed");
    assert!(d.inspect_image("alpine:3.19").await.is_ok(), "an image a running container uses is kept");
    assert!(d.inspect_image("alpine:3.20").await.is_ok(), "a protected image is kept");
    assert!(d.inspect_image("codehaven-job:trixie").await.is_ok(), "the job image is kept");
    b.remove(&running).await.unwrap();
}

#[tokio::test]
async fn recent_images_are_not_pruned() {
    let Some(b) = daemon().await else { return };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    b.ensure_image("alpine:3.20", false, &tx).await.unwrap();
    // Alpine 3.20 was built long ago, so "newer than 100 years" would remove it; "newer than a decade ago" must not.
    // What matters here is the cutoff arithmetic: nothing is removed when the cutoff is before every image.
    b.prune_images(Duration::from_secs(100 * 365 * 86_400), &[]).await.unwrap();
    assert!(b.docker().inspect_image("alpine:3.20").await.is_ok());
}

// ---- running as the runner's own user ---------------------------------------------------------------

#[tokio::test]
async fn nonroot_files_made_by_root_in_the_container_are_taken_back_and_removed() {
    let Some(b) = daemon().await else { return };
    let (keep, dir) = work_for_child_or_new();
    if rerun_as_runner_user("nonroot_files_made_by_root_in_the_container_are_taken_back_and_removed", &dir) {
        drop(keep);
        return;
    }
    let h = real(b, &dir).await;
    // Root in the container makes files and directories the runner's user cannot touch.
    let job = TestJob::new(9050)
        .container(SHELL_IMAGE, None)
        .run("mkdir -p deep/dir && echo secret > deep/dir/f && chmod 000 deep/dir/f && chmod 500 deep/dir && chmod 500 deep; echo x > \"$RUNNER_TEMP/rootfile\"; ls -ln deep | head -3")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9050));
    assert!(h.leftovers().is_empty(), "the job directory must be removed even though root made files in it: {:?}", h.leftovers());
    drop(keep);
}

#[tokio::test]
async fn nonroot_checkout_then_root_steps_then_runner_side_actions_all_work_together() {
    let Some(b) = daemon().await else { return };
    let (keep, dir) = work_for_child_or_new();
    if rerun_as_runner_user("nonroot_checkout_then_root_steps_then_runner_side_actions_all_work_together", &dir) {
        drop(keep);
        return;
    }
    // A repository the runner can clone with a file:// URL.
    let repo = dir.join("repo-src");
    let bare = dir.join("repo.git");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("hello.txt"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&dir, &["clone", "-q", "--bare", repo.to_str().unwrap(), bare.to_str().unwrap()]);
    let sha = git(&repo, &["rev-parse", "HEAD"]);

    let h = real(b, &dir).await;
    let s2 = sha.clone();
    let job = TestJob::new(9051)
        .clone_url(&format!("file://{}", bare.display()))
        .context(move |c| c.sha = s2)
        // The runner checks out (files owned by the runner's user). The container, as root, runs
        // git in that directory, which needs safe.directory, and writes files of its own.
        .uses("actions/checkout@v4", &[])
        .run("git log -1 --format=%H; echo built > out.txt; mkdir -p dist && echo bin > dist/app")
        // The runner reads root-owned files to zip them, then writes into the same tree again.
        .uses("actions/upload-artifact@v4", &[("name", "dist"), ("path", "dist\nout.txt")])
        .uses("actions/checkout@v4", &[("path", "second")])
        .run("ls second; cat out.txt")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9051));
    assert!(h.server.step_log(9051, 2).contains(&sha), "{}", h.server.step_log(9051, 2));
    assert!(h.server.artifact(9051, "dist").is_some_and(|z| z.len() > 20), "artifact uploaded");
    assert!(
        h.server.step_log(9051, 5).contains("hello.txt") && h.server.step_log(9051, 5).contains("built"),
        "{}",
        h.server.step_log(9051, 5)
    );
    assert!(h.leftovers().is_empty());
    drop(keep);
}

#[tokio::test]
async fn nonroot_a_container_user_option_can_write_the_workspace() {
    let Some(b) = daemon().await else { return };
    let (keep, dir) = work_for_child_or_new();
    if rerun_as_runner_user("nonroot_a_container_user_option_can_write_the_workspace", &dir) {
        drop(keep);
        return;
    }
    let h = real(b, &dir).await;
    let job = TestJob::new(9052)
        .container(SHELL_IMAGE, Some("--user 1000:1000"))
        .run("echo \"uid=$(id -u)\"; touch made-by-1000 && echo wrote-ok")
        .uses("actions/upload-artifact@v4", &[("name", "f"), ("path", "made-by-1000")])
        .run("touch made-again && echo wrote-again")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(9052));
    let l = h.server.step_log(9052, 1);
    assert!(l.contains("uid=1000") && l.contains("wrote-ok"), "{l}");
    assert!(
        h.server.step_log(9052, 3).contains("wrote-again"),
        "ownership must go back to the user after a runner-side action: {}",
        h.server.step_log(9052, 3)
    );
    assert!(h.leftovers().is_empty());
    drop(keep);
}

#[tokio::test]
async fn nonroot_a_dead_job_container_still_gets_its_files_reclaimed() {
    let Some(b) = daemon().await else { return };
    let (keep, dir) = work_for_child_or_new();
    if rerun_as_runner_user("nonroot_a_dead_job_container_still_gets_its_files_reclaimed", &dir) {
        drop(keep);
        return;
    }
    let h = real(b.clone(), &dir).await;
    // Something outside kills the whole container while a step is running, as an OOM kill or a
    // daemon restart would. The exec in teardown then has nothing to run in.
    let job = TestJob::new(9053)
        .container(SHELL_IMAGE, None)
        .run("mkdir -p a && echo x > a/rootfile && chmod 500 a && echo ready; sleep 60")
        .build();
    let docker = b.docker().clone();
    let server = &h.server;
    let killer = async {
        server.wait_log(9053, "ready", Duration::from_secs(60)).await;
        let ids = containers_for(&docker, &format!("{LABEL_JOB}=9053")).await;
        docker.kill_container(&ids[0], None::<KillContainerOptions>).await.expect("kill");
    };
    let (r, _) = tokio::join!(h.run(job), killer);
    assert_ne!(r.conclusion, "success", "{}", h.server.log_text(9053));
    assert!(h.leftovers().is_empty(), "a throwaway container must reclaim the files: {:?}", h.leftovers());
    assert!(containers_for(b.docker(), &format!("{LABEL_JOB}=9053")).await.is_empty());
    drop(keep);
}
