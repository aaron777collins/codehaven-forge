//! The run loop: wait for a free slot, ask the server for a job, run it, repeat. Also startup
//! cleanup, the disk guard, image pruning and graceful shutdown.

use crate::backend::{Backend, ContainerSpec};
use crate::client::{ApiError, Client};
use crate::config::Config;
use crate::credential::Credential;
use crate::disk;
use crate::docker::{LABEL_JOB, LABEL_RUNNER};
use crate::job::{self, JobControl, JobDeps, RunnerInfo};
use anyhow::{Result, bail};
use ch_api_types::runner::JobRequest;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// How long the loop waits after a failed poll, doubling to the cap.
const POLL_BACKOFF_START: Duration = Duration::from_secs(1);
const POLL_BACKOFF_MAX: Duration = Duration::from_secs(60);
const DISK_RECHECK: Duration = Duration::from_secs(30);

pub struct Runner {
    cfg: Arc<Config>,
    cred: Credential,
    client: Arc<Client>,
    backend: Arc<dyn Backend>,
    /// First stop signal: take no more jobs and let running ones finish within the grace period.
    shutdown: CancellationToken,
    /// Second signal: cancel everything now.
    force: CancellationToken,
    active: Arc<Mutex<HashMap<i64, JobControl>>>,
    minute: Duration,
}

impl Runner {
    pub fn new(
        cfg: Arc<Config>,
        cred: Credential,
        backend: Arc<dyn Backend>,
        shutdown: CancellationToken,
        force: CancellationToken,
    ) -> Result<Runner> {
        let client = Arc::new(Client::new(&cred.server_url, Some(cred.credential.clone()))?);
        Ok(Runner {
            cfg,
            cred,
            client,
            backend,
            shutdown,
            force,
            active: Arc::new(Mutex::new(HashMap::new())),
            minute: Duration::from_secs(60),
        })
    }

    /// For tests that want a client with fast retries.
    pub fn with_client(mut self, client: Arc<Client>) -> Runner {
        self.client = client;
        self
    }

    /// Shortens what a "minute" of `timeout-minutes` lasts. For tests.
    pub fn with_minute(mut self, minute: Duration) -> Runner {
        self.minute = minute;
        self
    }

    pub fn running_jobs(&self) -> usize {
        self.active.lock().map(|a| a.len()).unwrap_or(0)
    }

    fn deps(&self) -> Arc<JobDeps> {
        Arc::new(JobDeps {
            cfg: self.cfg.clone(),
            client: self.client.clone(),
            backend: self.backend.clone(),
            runner: RunnerInfo { id: self.cred.runner_id, name: self.cred.name.clone(), credential: self.cred.credential.clone() },
            minute: self.minute,
        })
    }

    /// Runs until stopped. Returns an error only for things that need a person: a credential
    /// the server no longer accepts, or a work directory that cannot be used.
    pub async fn run(&self) -> Result<()> {
        self.startup().await?;
        let maintenance = self.spawn_maintenance();
        let deps = self.deps();
        let sem = Arc::new(Semaphore::new(self.cfg.runner.concurrency));
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut backoff = POLL_BACKOFF_START;
        let mut fatal: Option<anyhow::Error> = None;
        let mut last_pressure_prune = std::time::Instant::now() - Duration::from_secs(3600);

        tracing::info!(runner = %self.cred.name, slots = self.cfg.runner.concurrency, server = %self.cred.server_url, "waiting for jobs");
        'main: loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            let permit = tokio::select! {
                p = sem.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
                _ = self.shutdown.cancelled() => break,
            };
            while tasks.try_join_next().is_some() {}

            // Disk guard: do not start work on a nearly full disk.
            if let Some((path, pct)) = disk::worst(&self.cfg.disk.guard_paths)
                && pct >= self.cfg.disk.max_percent
            {
                if last_pressure_prune.elapsed() > Duration::from_secs(300) {
                    last_pressure_prune = std::time::Instant::now();
                    tracing::warn!(path = %path.display(), percent = pct, "disk is nearly full, pruning unused images");
                    self.prune(Duration::from_secs(3600)).await;
                }
                if disk::worst(&self.cfg.disk.guard_paths).is_some_and(|(_, p)| p >= self.cfg.disk.max_percent) {
                    tracing::warn!(path = %path.display(), percent = pct, limit = self.cfg.disk.max_percent, "not taking jobs until disk space is back");
                    drop(permit);
                    tokio::select! {
                        _ = tokio::time::sleep(DISK_RECHECK) => {}
                        _ = self.shutdown.cancelled() => break 'main,
                    }
                    continue;
                }
            }

            let req = JobRequest { wait_seconds: Some(self.cfg.runner.poll_wait_seconds), labels: self.cred.labels.clone() };
            let polled = tokio::select! {
                r = self.client.request_job(&req) => r,
                _ = self.shutdown.cancelled() => break 'main,
            };
            match polled {
                Ok(Some(payload)) => {
                    backoff = POLL_BACKOFF_START;
                    let ctl = JobControl::default();
                    if let Ok(mut a) = self.active.lock() {
                        a.insert(payload.job_id, ctl.clone());
                    }
                    let (deps, active, client) = (deps.clone(), self.active.clone(), self.client.clone());
                    let force = self.force.clone();
                    tasks.spawn(async move {
                        let id = payload.job_id;
                        tracing::info!(job_id = id, name = %payload.name, "job started");
                        // A second stop signal cancels this job outright.
                        let watcher = {
                            let ctl = ctl.clone();
                            tokio::spawn(async move {
                                force.cancelled().await;
                                ctl.cancel.cancel();
                                ctl.hard_stop.cancel();
                            })
                        };
                        execute(&deps, &client, payload, ctl).await;
                        watcher.abort();
                        if let Ok(mut a) = active.lock() {
                            a.remove(&id);
                        }
                        drop(permit);
                    });
                }
                Ok(None) => {
                    backoff = POLL_BACKOFF_START;
                    drop(permit);
                }
                Err(ApiError::Unauthorized) => {
                    fatal = Some(anyhow::anyhow!(ApiError::Unauthorized));
                    break;
                }
                Err(e) => {
                    drop(permit);
                    tracing::warn!("cannot ask for a job: {e}. Trying again in {}s", backoff.as_secs());
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.shutdown.cancelled() => break 'main,
                    }
                    backoff = (backoff * 2).min(POLL_BACKOFF_MAX);
                }
            }
        }

        self.drain(&mut tasks).await;
        maintenance.abort();
        match fatal {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Lets running jobs finish within the grace period, then cancels them, then stops them.
    async fn drain(&self, tasks: &mut JoinSet<()>) {
        if tasks.is_empty() {
            return;
        }
        tracing::info!(jobs = tasks.len(), grace_seconds = self.cfg.runner.shutdown_grace_seconds, "waiting for running jobs");
        let grace = Duration::from_secs(self.cfg.runner.shutdown_grace_seconds);
        let finished = tokio::select! {
            _ = async { while tasks.join_next().await.is_some() {} } => true,
            _ = tokio::time::sleep(grace) => false,
            _ = self.force.cancelled() => false,
        };
        if finished {
            return;
        }
        tracing::warn!("grace period over, cancelling {} job(s)", self.running_jobs());
        for ctl in self.snapshot() {
            ctl.cancel.cancel();
        }
        let ok = tokio::select! {
            _ = async { while tasks.join_next().await.is_some() {} } => true,
            _ = tokio::time::sleep(Duration::from_secs(45)) => false,
            _ = self.force.cancelled() => false,
        };
        if ok {
            return;
        }
        for ctl in self.snapshot() {
            ctl.hard_stop.cancel();
        }
        let _ = tokio::time::timeout(Duration::from_secs(60), async { while tasks.join_next().await.is_some() {} }).await;
        tasks.abort_all();
    }

    fn snapshot(&self) -> Vec<JobControl> {
        self.active.lock().map(|a| a.values().cloned().collect()).unwrap_or_default()
    }

    // ---- startup and maintenance -----------------------------------------------------------

    async fn startup(&self) -> Result<()> {
        // The daemon may still be starting (compose brings both up together).
        let mut waited = Duration::ZERO;
        loop {
            match self.backend.ping().await {
                Ok(v) => {
                    tracing::info!(docker = %v, "Docker is reachable");
                    break;
                }
                Err(e) if waited < Duration::from_secs(120) => {
                    tracing::warn!("Docker is not ready: {e:#}");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        _ = self.shutdown.cancelled() => return Ok(()),
                    }
                    waited += Duration::from_secs(5);
                }
                Err(e) => bail!("Docker is not reachable: {e:#}"),
            }
        }
        let work = &self.cfg.runner.work_dir;
        std::fs::create_dir_all(work).map_err(|e| anyhow::anyhow!("cannot create the work directory {}: {e}", work.display()))?;
        let probe = work.join(format!(".write-test-{}", std::process::id()));
        std::fs::write(&probe, b"x").map_err(|e| anyhow::anyhow!("the work directory {} is not writable: {e}", work.display()))?;
        let _ = std::fs::remove_file(&probe);

        match self.backend.cleanup_orphans(self.cred.runner_id).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("removed {n} container(s) left by an earlier run"),
            Err(e) => tracing::warn!("could not look for leftover containers: {e:#}"),
        }
        self.clean_stale_dirs().await;
        Ok(())
    }

    /// Job directories nothing is using any more, from a runner that was killed.
    async fn clean_stale_dirs(&self) {
        let work = &self.cfg.runner.work_dir;
        let Ok(rd) = std::fs::read_dir(work) else { return };
        let mut stuck = Vec::new();
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("job-") {
                continue;
            }
            let p = e.path();
            if std::fs::remove_dir_all(&p).is_err() {
                stuck.push(p);
            }
        }
        if stuck.is_empty() {
            return;
        }
        // Files made by a container belong to root. A throwaway container can hand them back.
        tracing::info!("{} stale job director(ies) need their ownership fixed", stuck.len());
        let Some((uid, gid)) = owner_of(work) else { return };
        let image = self.cfg.image_for(&[], None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        if self.backend.ensure_image(&image, false, &tx).await.is_err() {
            tracing::warn!("cannot clean stale job directories: image {image} is not available");
            return;
        }
        for dir in stuck {
            let d = dir.to_string_lossy().into_owned();
            let spec = ContainerSpec {
                name: format!("codehaven-reclaim-{:08x}", rand::random::<u32>()),
                image: image.clone(),
                labels: BTreeMap::from([
                    (LABEL_RUNNER.to_string(), self.cred.runner_id.to_string()),
                    (LABEL_JOB.to_string(), "stale".to_string()),
                ]),
                env: BTreeMap::new(),
                binds: vec![format!("{d}:{d}")],
                workdir: None,
                user: Some("0".into()),
                cpus: 1.0,
                memory: 512 << 20,
                pids: 256,
                shm_size: 64 << 20,
                entrypoint: Some(vec!["/bin/sh".into()]),
                cmd: Some(vec!["-c".into(), format!("chown -R {uid}:{gid} '{d}'; chmod -R u+rwX '{d}'; exit 0")]),
                network_of: None,
                extra_hosts: Vec::new(),
            };
            if let Ok(id) = self.backend.create_and_start(&spec).await {
                let (tx, mut rx) = tokio::sync::mpsc::channel(8);
                let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
                let _ = tokio::time::timeout(Duration::from_secs(120), self.backend.stream_until_exit(&id, tx)).await;
                let _ = drain.await;
                let _ = self.backend.remove(&id).await;
            }
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!("cannot remove {}: {e}", dir.display());
            }
        }
    }

    fn protected_images(&self) -> Vec<String> {
        let mut v: Vec<String> = self.cfg.labels.values().cloned().collect();
        v.push(crate::config::DEFAULT_JOB_IMAGE.to_string());
        v.sort();
        v.dedup();
        v
    }

    async fn prune(&self, older_than: Duration) {
        match self.backend.prune_images(older_than, &self.protected_images()).await {
            Ok(0) => {}
            Ok(freed) => tracing::info!(freed_bytes = freed, "removed unused images"),
            Err(e) => tracing::warn!("image cleanup failed: {e:#}"),
        }
    }

    fn spawn_maintenance(&self) -> tokio::task::JoinHandle<()> {
        let backend = self.backend.clone();
        let protect = self.protected_images();
        let every = Duration::from_secs(self.cfg.docker.prune_interval_minutes.max(1) * 60);
        let older = self.cfg.prune_after();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                match backend.prune_images(older, &protect).await {
                    Ok(0) => {}
                    Ok(freed) => tracing::info!(freed_bytes = freed, "removed unused images"),
                    Err(e) => tracing::warn!("image cleanup failed: {e:#}"),
                }
            }
        })
    }
}

#[cfg(unix)]
fn owner_of(p: &std::path::Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| (m.uid(), m.gid()))
}

#[cfg(not(unix))]
fn owner_of(_p: &std::path::Path) -> Option<(u32, u32)> {
    None
}

/// One job from pickup to report, with the heartbeat running alongside.
pub async fn execute(deps: &JobDeps, client: &Arc<Client>, payload: ch_api_types::runner::JobPayload, ctl: JobControl) {
    let job_id = payload.job_id;
    let lease = payload.lease_seconds.max(1);
    let every = Duration::from_secs(u64::from((lease / 3).clamp(1, 30)));
    let lease_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hb = {
        let (client, ctl, lost) = (client.clone(), ctl.clone(), lease_lost.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                match client.heartbeat(job_id).await {
                    Ok(r) if r.cancel => {
                        tracing::info!(job_id, "the server cancelled this job");
                        ctl.cancel.cancel();
                    }
                    Ok(_) => {}
                    Err(e) if e.is_lease_lost() => {
                        tracing::warn!(job_id, "the server no longer has this job ({e}), stopping it");
                        lost.store(true, std::sync::atomic::Ordering::SeqCst);
                        ctl.cancel.cancel();
                        ctl.hard_stop.cancel();
                        return;
                    }
                    Err(e) => tracing::warn!(job_id, "heartbeat failed: {e}"),
                }
            }
        })
    };

    let result = job::run_job(deps, &payload, &ctl).await;
    hb.abort();
    if lease_lost.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::warn!(job_id, "not reporting a result for a job the server dropped");
        return;
    }
    match job::report(deps, job_id, &result).await {
        Ok(()) => tracing::info!(job_id, conclusion = %result.conclusion, "job finished"),
        Err(e) => tracing::error!(job_id, conclusion = %result.conclusion, "could not report the result: {e}"),
    }
}
