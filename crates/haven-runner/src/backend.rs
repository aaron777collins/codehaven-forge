//! What the job engine needs from "somewhere to run commands". `docker::DockerBackend` is the
//! real one. The tests also use `testing::LocalBackend`, which runs the commands on the host,
//! so the engine's logic can be tested without a Docker daemon.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

/// A line source for messages the backend itself produces (image pull progress).
pub type LineSink = mpsc::UnboundedSender<String>;

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    /// `host_path:container_path` pairs. Only the job's own directory is ever bound.
    pub binds: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub cpus: f64,
    pub memory: u64,
    pub pids: i64,
    pub shm_size: u64,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    /// A service container (ch-ctk.7) joins this container's network instead of getting one of
    /// its own, so the job reaches it on `localhost`.
    pub network_of: Option<String>,
    /// `name:address` lines for the container's `/etc/hosts`: a service's id points at the
    /// job's own loopback, which it shares.
    pub extra_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// A piece of output, not necessarily a whole line.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub stream: StreamKind,
    pub data: Vec<u8>,
}

#[async_trait]
pub trait Backend: Send + Sync {
    /// Is the daemon reachable? Returns its version for the log.
    async fn ping(&self) -> anyhow::Result<String>;

    /// Makes the image available, pulling it when it is missing or `always` is set. Progress
    /// goes to `log`.
    async fn ensure_image(&self, image: &str, always: bool, log: &LineSink) -> anyhow::Result<()>;

    /// Creates and starts a container, returning its id.
    async fn create_and_start(&self, spec: &ContainerSpec) -> anyhow::Result<String>;

    /// Runs a command in a running container and returns its exit code. Output arrives on
    /// `out` as it is written. Dropping the future abandons the command (see `shell::KILL_TREE`
    /// for stopping it).
    async fn exec(&self, id: &str, spec: &ExecSpec, out: mpsc::Sender<Chunk>) -> anyhow::Result<i64>;

    /// Follows a container's output until it exits, then returns its exit code.
    async fn stream_until_exit(&self, id: &str, out: mpsc::Sender<Chunk>) -> anyhow::Result<i64>;

    /// Stops and removes a container and its anonymous volumes. A container that is already
    /// gone is not an error.
    async fn remove(&self, id: &str) -> anyhow::Result<()>;

    /// Removes containers an earlier run of this runner left behind. Returns how many.
    async fn cleanup_orphans(&self, runner_id: i64) -> anyhow::Result<usize>;

    /// Removes unused images created more than `older_than` ago, never one in `protect`.
    /// Returns the bytes freed.
    async fn prune_images(&self, older_than: Duration, protect: &[String]) -> anyhow::Result<u64>;
}
