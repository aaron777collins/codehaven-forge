//! The run loop: registration, polling, concurrency, routing by label, and shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use ch_api_types::runner::RegisterRequest;
use common::*;
use haven_runner::backend::Backend;
use haven_runner::client::Client;
use haven_runner::config::Config;
use haven_runner::credential::Credential;
use haven_runner::runner::Runner;
use haven_runner::testing::{FakeServer, LocalBackend, TestJob};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

struct Rig {
    server: FakeServer,
    backend: Arc<LocalBackend>,
    cfg: Config,
    cred: Credential,
    work: tempfile::TempDir,
}

async fn rig(labels: &[&str], f: impl FnOnce(&mut Config)) -> Rig {
    let server = FakeServer::start().await;
    server.add_registration_token("reg");
    let work = tempfile::tempdir().unwrap();
    // Register through the real call, the way `haven-runner register` does.
    let client = Client::with_policy(&server.url, None, fast_policy()).unwrap();
    let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
    let resp = client
        .register(&RegisterRequest {
            token: "reg".into(),
            name: "loop-runner".into(),
            labels: labels.clone(),
            version: None,
            os: None,
            arch: None,
        })
        .await
        .unwrap();
    let cred =
        Credential { server_url: server.url.clone(), runner_id: resp.runner_id, name: resp.name, labels, credential: resp.credential };
    let mut cfg = Config::default();
    cfg.runner.work_dir = work.path().join("work");
    cfg.disk.guard_paths = vec![cfg.runner.work_dir.clone()];
    cfg.runner.poll_wait_seconds = 1;
    cfg.runner.shutdown_grace_seconds = 3;
    f(&mut cfg);
    Rig { server, backend: Arc::new(LocalBackend::new()), cfg, cred, work }
}

impl Rig {
    fn runner(&self) -> (Arc<Runner>, CancellationToken, CancellationToken) {
        let (shutdown, force) = (CancellationToken::new(), CancellationToken::new());
        let backend: Arc<dyn Backend> = self.backend.clone();
        let client = Arc::new(Client::with_policy(&self.server.url, Some(self.cred.credential.clone()), fast_policy()).unwrap());
        let r = Runner::new(Arc::new(self.cfg.clone()), self.cred.clone(), backend, shutdown.clone(), force.clone())
            .unwrap()
            .with_client(client)
            .with_minute(Duration::from_secs(1));
        (Arc::new(r), shutdown, force)
    }
}

/// Polls `cond` every 20 ms until it is true or `limit` passes. Panics with `what` on timeout.
async fn wait_until(limit: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < limit, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// When the job's only step started and ended, as nanosecond timestamps read from its log.
fn span(server: &FakeServer, id: i64) -> (u128, u128) {
    let log = server.step_log(id, 1);
    let stamp = |prefix: &str| {
        log.lines()
            .find(|l| l.starts_with(prefix))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|f| f.parse::<u128>().ok())
            .unwrap_or_else(|| panic!("job {id}: no {:?} timestamp in the step log:\n{log}", prefix.trim()))
    };
    (stamp(&format!("start-{id} ")), stamp(&format!("end-{id} ")))
}

#[tokio::test]
async fn the_runner_picks_up_queued_jobs_and_reports_them() {
    let rig = rig(&["ubuntu-latest", "linux"], |_| {}).await;
    let (runner, shutdown, _force) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    for id in 1..=3 {
        rig.server.enqueue(TestJob::new(id).run(&format!("echo job-{id}-ran")).build());
    }
    for id in 1..=3 {
        let c = rig.server.wait_complete(id, Duration::from_secs(20)).await;
        assert_eq!(c.conclusion, "success");
        assert!(rig.server.step_log(id, 1).contains(&format!("job-{id}-ran")));
    }
    shutdown.cancel();
    let r = tokio::time::timeout(Duration::from_secs(10), task).await.expect("stops promptly").unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(rig.backend.live_containers(), 0);
    assert!(
        !rig.work.path().join("work").read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("job-")),
        "no job directories left"
    );
}

#[tokio::test]
async fn jobs_run_in_parallel_up_to_the_concurrency_limit() {
    let rig = rig(&["linux"], |c| c.runner.concurrency = 2).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    // Each job holds its slot for about a second and records when it started and ended.
    for id in 1..=4 {
        let mut j = TestJob::new(id).run(&format!("echo \"start-{id} $(date +%s%N)\"; sleep 1; echo \"end-{id} $(date +%s%N)\"")).build();
        j.runs_on = vec!["linux".into()];
        rig.server.enqueue(j);
    }
    for id in 1..=4 {
        rig.server.wait_complete(id, Duration::from_secs(30)).await;
    }
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
    // Two slots means some pair of jobs shared a moment, and never three at once.
    let spans: Vec<(u128, u128)> = (1..=4).map(|id| span(&rig.server, id)).collect();
    assert!(
        spans.iter().enumerate().any(|(i, a)| { spans.iter().enumerate().any(|(j, b)| i != j && a.0 < b.1 && b.0 < a.1) }),
        "two slots should run jobs in parallel, spans: {spans:?}"
    );
    for &(start, _) in &spans {
        let running = spans.iter().filter(|&&(s, e)| s <= start && start < e).count();
        assert!(running <= 2, "never three jobs at once, but {running} were running at {start}, spans: {spans:?}");
    }
}

#[tokio::test]
async fn one_slot_runs_jobs_one_at_a_time() {
    let rig = rig(&["linux"], |c| c.runner.concurrency = 1).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    for id in 1..=2 {
        let mut j = TestJob::new(id).run(&format!("echo \"start-{id} $(date +%s%N)\"; sleep 1; echo \"end-{id} $(date +%s%N)\"")).build();
        j.runs_on = vec!["linux".into()];
        rig.server.enqueue(j);
    }
    rig.server.wait_complete(1, Duration::from_secs(20)).await;
    rig.server.wait_complete(2, Duration::from_secs(20)).await;
    let (s1, e1) = span(&rig.server, 1);
    let (s2, e2) = span(&rig.server, 2);
    assert!(e1 <= s2 || e2 <= s1, "one slot must run the jobs one at a time, spans: ({s1}, {e1}), ({s2}, {e2})");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}

#[tokio::test]
async fn a_runner_only_takes_jobs_its_labels_cover() {
    let rig = rig(&["linux"], |_| {}).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    let mut gpu = TestJob::new(1).run("echo gpu").build();
    gpu.runs_on = vec!["gpu".into()];
    let mut ok = TestJob::new(2).run("echo cpu").build();
    ok.runs_on = vec!["linux".into()];
    rig.server.enqueue(gpu);
    rig.server.enqueue(ok);
    rig.server.wait_complete(2, Duration::from_secs(20)).await;
    let polls = rig.server.requests_to("jobs/request");
    wait_until(Duration::from_secs(20), "two more polls after the cpu job", || rig.server.requests_to("jobs/request") >= polls + 2).await;
    assert!(rig.server.completed(1).is_none(), "the gpu job is not this runner's");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}

#[tokio::test]
async fn an_idle_runner_stops_at_once_when_asked() {
    let rig = rig(&["linux"], |c| c.runner.poll_wait_seconds = 30).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t0 = Instant::now();
    shutdown.cancel();
    let r = tokio::time::timeout(Duration::from_secs(5), task).await.expect("it must not wait out the 30 s poll").unwrap();
    assert!(r.is_ok());
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn shutdown_lets_a_running_job_finish_within_the_grace_period() {
    let rig = rig(&["linux"], |c| c.runner.shutdown_grace_seconds = 10).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    let mut j = TestJob::new(1).run("echo started; sleep 2; echo finished-normally").build();
    j.runs_on = vec!["linux".into()];
    rig.server.enqueue(j);
    rig.server.wait_log(1, "started", Duration::from_secs(10)).await;
    shutdown.cancel();
    let r = tokio::time::timeout(Duration::from_secs(15), task).await.expect("stops after the job").unwrap();
    assert!(r.is_ok());
    let c = rig.server.completed(1).expect("the job was reported before exit");
    assert_eq!(c.conclusion, "success");
    assert!(rig.server.step_log(1, 1).contains("finished-normally"));
}

#[tokio::test]
async fn shutdown_cancels_a_job_that_outlasts_the_grace_period() {
    let rig = rig(&["linux"], |c| c.runner.shutdown_grace_seconds = 1).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    let mut j = TestJob::new(1).run("echo started; sleep 31711").run("echo cleanup-ran").condition("always()").build();
    j.runs_on = vec!["linux".into()];
    rig.server.enqueue(j);
    rig.server.wait_log(1, "started", Duration::from_secs(10)).await;
    let t0 = Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), task).await.expect("stops").unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    let c = rig.server.completed(1).expect("reported");
    assert_eq!(c.conclusion, "cancelled");
    assert!(rig.server.step_log(1, 2).contains("cleanup-ran"), "cleanup steps still run after a graceful cancel");
    assert!(!process_running("sleep 31711"));
    assert_eq!(rig.backend.live_containers(), 0);
}

#[tokio::test]
async fn a_second_stop_signal_cancels_everything_now() {
    let rig = rig(&["linux"], |c| c.runner.shutdown_grace_seconds = 120).await;
    let (runner, shutdown, force) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    let mut j = TestJob::new(1).run("echo started; sleep 31712").run("echo cleanup").condition("always()").build();
    j.runs_on = vec!["linux".into()];
    rig.server.enqueue(j);
    rig.server.wait_log(1, "started", Duration::from_secs(10)).await;
    shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    force.cancel();
    tokio::time::timeout(Duration::from_secs(20), task).await.expect("stops").unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    assert_eq!(rig.server.completed(1).expect("reported").conclusion, "cancelled");
    assert!(!process_running("sleep 31712"));
}

#[tokio::test]
async fn a_credential_the_server_rejects_ends_the_runner_with_an_error() {
    let mut rig = rig(&["linux"], |_| {}).await;
    rig.cred.credential = "chr_revoked".into();
    let (runner, _s, _f) = rig.runner();
    let r = tokio::time::timeout(Duration::from_secs(10), runner.run()).await.expect("must not retry a 401 forever");
    let e = r.unwrap_err().to_string();
    assert!(e.contains("credential"), "{e}");
}

#[tokio::test]
async fn polling_errors_back_off_and_recover() {
    let rig = rig(&["linux"], |_| {}).await;
    rig.server.fail_next("jobs/request", &[503, 500]);
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    let mut j = TestJob::new(1).run("echo ok").build();
    j.runs_on = vec!["linux".into()];
    rig.server.enqueue(j);
    // Two failed polls cost about 1 s and 2 s of backoff before the job is found.
    let c = rig.server.wait_complete(1, Duration::from_secs(30)).await;
    assert_eq!(c.conclusion, "success");
    assert!(rig.server.requests_to("jobs/request") >= 3);
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}

#[tokio::test]
async fn a_full_disk_stops_new_jobs_until_space_is_back() {
    // A limit of 1 percent is always exceeded on a real file system.
    let rig = rig(&["linux"], |c| c.disk.max_percent = 1).await;
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    let mut j = TestJob::new(1).run("echo should-not-start").build();
    j.runs_on = vec!["linux".into()];
    rig.server.enqueue(j);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(rig.server.completed(1).is_none(), "no job may start while the disk is over its limit");
    assert_eq!(rig.server.requests_to("jobs/request"), 0, "it does not even ask for work");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}

#[tokio::test]
async fn leftover_job_directories_are_removed_at_startup() {
    let rig = rig(&["linux"], |_| {}).await;
    let stale = rig.cfg.runner.work_dir.join("job-99-12345");
    std::fs::create_dir_all(stale.join("workspace")).unwrap();
    std::fs::write(stale.join("workspace/file"), "x").unwrap();
    let keep = rig.cfg.runner.work_dir.join("not-a-job");
    std::fs::create_dir_all(&keep).unwrap();
    let (runner, shutdown, _) = rig.runner();
    let task = tokio::spawn({
        let r = runner.clone();
        async move { r.run().await }
    });
    wait_until(Duration::from_secs(20), "the stale job directory to be removed", || !stale.exists()).await;
    assert!(!stale.exists(), "a stale job directory is removed");
    assert!(keep.exists(), "only job-* directories are touched");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}

#[tokio::test]
async fn an_unwritable_work_directory_is_reported_not_ignored() {
    let rig = rig(&["linux"], |c| c.runner.work_dir = "/proc/haven-nope".into()).await;
    let (runner, _s, _f) = rig.runner();
    let e = runner.run().await.unwrap_err().to_string();
    assert!(e.contains("work directory"), "{e}");
}

#[tokio::test]
async fn the_credential_file_round_trips_through_register() {
    let server = FakeServer::start().await;
    server.add_registration_token("t");
    let client = Client::new(&server.url, None).unwrap();
    let resp = client
        .register(&RegisterRequest {
            token: "t".into(),
            name: "n".into(),
            labels: vec!["linux".into()],
            version: None,
            os: None,
            arch: None,
        })
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".runner");
    Credential {
        server_url: server.url.clone(),
        runner_id: resp.runner_id,
        name: resp.name.clone(),
        labels: vec!["linux".into()],
        credential: resp.credential.clone(),
    }
    .save(&path)
    .unwrap();
    let back = Credential::load(&path).unwrap();
    assert_eq!(back.credential, resp.credential);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
