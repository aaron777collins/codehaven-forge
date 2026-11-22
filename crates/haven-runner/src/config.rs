//! Runner settings: a TOML file plus the handful of flags and environment variables that
//! override it. Every field has a default, so a runner with no file at all works.

use crate::options::{self, Limits};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The image a label with no mapping runs in. Built from `deploy/images/job.Dockerfile`.
pub const DEFAULT_JOB_IMAGE: &str = "codehaven-job:trixie";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub runner: RunnerSection,
    pub limits: LimitsSection,
    pub docker: DockerSection,
    pub disk: DiskSection,
    /// Label to job image. A job's `runs-on` picks the first of its labels that is listed here.
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunnerSection {
    /// Where checkouts and job files live. Must be the same path inside the Docker daemon the
    /// jobs run in, because job containers bind-mount it.
    pub work_dir: PathBuf,
    /// Jobs at once.
    pub concurrency: usize,
    /// Longest a job request waits on the server, in seconds (the server caps it at 30).
    pub poll_wait_seconds: u32,
    /// On shutdown, how long running jobs get to finish before they are cancelled.
    pub shutdown_grace_seconds: u64,
    /// Log step debug lines (`::debug::`).
    pub debug: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsSection {
    pub cpus: f64,
    pub memory: String,
    pub pids: i64,
    pub shm_size: String,
    /// The longest any job may run, whatever its own `timeout-minutes` says.
    pub max_job_minutes: u32,
    /// Log bytes kept per job. Past this the log says so and stops growing.
    pub max_log_megabytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerSection {
    /// `tcp://dind:2376`, `unix:///var/run/docker.sock`. Empty means `DOCKER_HOST`.
    pub host: String,
    /// `if-not-present` or `always`.
    pub pull_policy: String,
    /// Images no job has used for this many days are removed.
    pub prune_after_days: u32,
    pub prune_interval_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiskSection {
    /// Directories whose file systems are checked before taking a job. List the work
    /// directory and, if you can mount it, the Docker data root.
    pub guard_paths: Vec<PathBuf>,
    /// Above this many percent used, the runner prunes, and if that does not help it stops
    /// taking jobs until space is back.
    pub max_percent: u8,
}

impl Default for Config {
    fn default() -> Self {
        let mut labels = BTreeMap::new();
        for l in ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04", "linux"] {
            labels.insert(l.to_string(), DEFAULT_JOB_IMAGE.to_string());
        }
        Config {
            runner: RunnerSection::default(),
            limits: LimitsSection::default(),
            docker: DockerSection::default(),
            disk: DiskSection::default(),
            labels,
        }
    }
}

impl Default for RunnerSection {
    fn default() -> Self {
        RunnerSection { work_dir: PathBuf::from("/work"), concurrency: 2, poll_wait_seconds: 30, shutdown_grace_seconds: 60, debug: false }
    }
}

impl Default for LimitsSection {
    fn default() -> Self {
        LimitsSection { cpus: 4.0, memory: "8g".into(), pids: 4096, shm_size: "1g".into(), max_job_minutes: 360, max_log_megabytes: 100 }
    }
}

impl Default for DockerSection {
    fn default() -> Self {
        DockerSection { host: String::new(), pull_policy: "if-not-present".into(), prune_after_days: 7, prune_interval_minutes: 60 }
    }
}

impl Default for DiskSection {
    fn default() -> Self {
        DiskSection { guard_paths: vec![PathBuf::from("/work")], max_percent: 85 }
    }
}

impl Config {
    /// Reads a TOML file. A missing file is the defaults; a file that is there but wrong is an
    /// error that names the file.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let mut c: Config = toml::from_str(&s).with_context(|| format!("reading {}", path.display()))?;
                // A `[labels]` table replaces the built-in mapping rather than adding to it.
                if !toml_has_labels(&s) {
                    c.labels = Config::default().labels;
                }
                c.validate().with_context(|| format!("in {}", path.display()))?;
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.runner.concurrency == 0 || self.runner.concurrency > 64 {
            bail!("runner.concurrency must be between 1 and 64");
        }
        if self.runner.poll_wait_seconds > 30 {
            bail!("runner.poll_wait_seconds is at most 30, the longest the server waits");
        }
        if !(self.limits.cpus > 0.0 && self.limits.cpus.is_finite()) {
            bail!("limits.cpus must be above 0");
        }
        if options::parse_size(&self.limits.memory).is_none() {
            bail!("limits.memory {:?} is not a size such as 8g", self.limits.memory);
        }
        if options::parse_size(&self.limits.shm_size).is_none() {
            bail!("limits.shm_size {:?} is not a size such as 1g", self.limits.shm_size);
        }
        if self.limits.pids < 64 {
            bail!("limits.pids must be at least 64");
        }
        if self.limits.max_job_minutes == 0 {
            bail!("limits.max_job_minutes must be above 0");
        }
        if !matches!(self.docker.pull_policy.as_str(), "if-not-present" | "always") {
            bail!("docker.pull_policy must be if-not-present or always");
        }
        if self.disk.max_percent == 0 || self.disk.max_percent > 100 {
            bail!("disk.max_percent must be between 1 and 100");
        }
        if !self.runner.work_dir.is_absolute() {
            bail!("runner.work_dir must be an absolute path, because job containers bind-mount it");
        }
        for (label, image) in &self.labels {
            if label.trim().is_empty() || image.trim().is_empty() {
                bail!("labels: an entry has an empty label or image");
            }
        }
        Ok(())
    }

    pub fn limits(&self) -> Limits {
        Limits {
            cpus: self.limits.cpus,
            memory: options::parse_size(&self.limits.memory).unwrap_or(8 << 30),
            pids: self.limits.pids,
            shm_size: options::parse_size(&self.limits.shm_size).unwrap_or(1 << 30),
        }
    }

    /// The image for a job: its own `container.image` wins, then the first `runs-on` label with
    /// a mapping, then the default.
    pub fn image_for(&self, runs_on: &[String], container_image: Option<&str>) -> String {
        if let Some(i) = container_image.map(str::trim).filter(|i| !i.is_empty()) {
            return i.to_string();
        }
        for l in runs_on {
            if let Some(i) = self.labels.get(l) {
                return i.clone();
            }
        }
        DEFAULT_JOB_IMAGE.to_string()
    }

    pub fn prune_after(&self) -> Duration {
        Duration::from_secs(u64::from(self.docker.prune_after_days) * 86_400)
    }

    pub fn always_pull(&self) -> bool {
        self.docker.pull_policy == "always"
    }

    /// Hard cap on a job's runtime in minutes: the job's own request, never above the runner's.
    pub fn job_minutes(&self, requested: u32) -> u32 {
        let r = if requested == 0 { self.limits.max_job_minutes } else { requested };
        r.min(self.limits.max_job_minutes)
    }
}

fn toml_has_labels(s: &str) -> bool {
    s.parse::<toml::Table>().map(|t| t.contains_key("labels")).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let c = Config::default();
        c.validate().unwrap();
        assert_eq!(c.runner.concurrency, 2);
        assert_eq!(c.limits().cpus, 4.0);
        assert_eq!(c.limits().memory, 8 << 30);
        assert_eq!(c.limits().pids, 4096);
        assert_eq!(c.disk.max_percent, 85);
    }

    #[test]
    fn the_shipped_settings_files_are_valid() {
        for (name, text) in [
            ("deploy/runner.toml", include_str!("../../../deploy/runner.toml")),
            ("deploy/runner.example.toml", include_str!("../../../deploy/runner.example.toml")),
        ] {
            let c: Config = toml::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            c.validate().unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
        // The example documents the defaults, so loading it changes nothing.
        let ex: Config = toml::from_str(include_str!("../../../deploy/runner.example.toml")).unwrap();
        assert_eq!(ex.runner, Config::default().runner);
        assert_eq!(ex.limits, Config::default().limits);
        assert_eq!(ex.docker, Config::default().docker);
        assert_eq!(ex.disk, Config::default().disk);
        assert_eq!(ex.labels["linux"], DEFAULT_JOB_IMAGE);
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "[runner]\nconcurrency = 5\n[limits]\nmemory = \"2g\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.runner.concurrency, 5);
        assert_eq!(c.limits().memory, 2 << 30);
        assert_eq!(c.limits.cpus, 4.0);
        assert_eq!(c.labels["ubuntu-latest"], DEFAULT_JOB_IMAGE);
    }

    #[test]
    fn a_labels_table_replaces_the_builtin_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "[labels]\nrust = \"rust:1-trixie\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.labels.len(), 1);
        assert_eq!(c.image_for(&["rust".into()], None), "rust:1-trixie");
        assert_eq!(c.image_for(&["ubuntu-latest".into()], None), DEFAULT_JOB_IMAGE, "unmapped labels fall back to the default");
    }

    #[test]
    fn unknown_keys_and_bad_values_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        for (body, needle) in [
            ("[runner]\nconcurency = 1\n", "concurency"),
            ("[runner]\nconcurrency = 0\n", "concurrency"),
            ("[runner]\npoll_wait_seconds = 90\n", "poll_wait_seconds"),
            ("[limits]\nmemory = \"lots\"\n", "memory"),
            ("[limits]\ncpus = 0\n", "cpus"),
            ("[docker]\npull_policy = \"sometimes\"\n", "pull_policy"),
            ("[disk]\nmax_percent = 0\n", "max_percent"),
            ("[runner]\nwork_dir = \"relative\"\n", "work_dir"),
            ("not toml at all [", "reading"),
        ] {
            std::fs::write(&p, body).unwrap();
            let e = format!("{:#}", Config::load(&p).unwrap_err());
            assert!(e.contains(needle), "{body:?} gave {e}");
        }
    }

    #[test]
    fn image_choice_order() {
        let c = Config::default();
        assert_eq!(c.image_for(&["ubuntu-latest".into()], Some("rust:1")), "rust:1", "container.image wins");
        assert_eq!(c.image_for(&["self-hosted".into(), "linux".into()], None), DEFAULT_JOB_IMAGE);
        assert_eq!(c.image_for(&[], Some("  ")), DEFAULT_JOB_IMAGE);
    }

    #[test]
    fn job_minutes_are_capped_by_the_runner() {
        let c = Config::default();
        assert_eq!(c.job_minutes(30), 30);
        assert_eq!(c.job_minutes(10_000), 360);
        assert_eq!(c.job_minutes(0), 360);
    }
}
