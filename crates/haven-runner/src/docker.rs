//! The Docker backend, on `bollard`. It talks to whatever `DOCKER_HOST` names, which in the
//! compose stack is the separate `dind` daemon and never the host's socket.

use crate::backend::{Backend, Chunk, ContainerSpec, ExecSpec, LineSink, StreamKind};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder, ListImagesOptionsBuilder,
    ListNetworksOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

pub const LABEL_RUNNER: &str = "org.codehaven.runner";
pub const LABEL_JOB: &str = "org.codehaven.job";
/// On the container: the network that belongs to it alone. On that network: the value "1", so
/// the networks this runner owns can be told from everything else in the daemon.
pub const LABEL_NETWORK: &str = "org.codehaven.network";

pub struct DockerBackend {
    docker: Docker,
}

impl DockerBackend {
    /// `host` empty means `DOCKER_HOST` (and the TLS variables next to it).
    pub fn connect(host: &str) -> Result<DockerBackend> {
        let docker = if host.trim().is_empty() { Docker::connect_with_defaults() } else { Docker::connect_with_host(host.trim()) }
            .context("cannot set up the Docker connection")?;
        Ok(DockerBackend { docker })
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    /// A bridge network for this one container, with inter-container traffic off, so no two
    /// jobs (or a job and a step container) ever share a wire. The labels are the spec's own,
    /// which carry the runner label `cleanup_orphans` sweeps by, plus `org.codehaven.network=1`.
    ///
    /// Concurrency here is 2 jobs plus their helper containers, so the daemon's default address
    /// pool (15 networks of the 172.17/12 base) is comfortably enough.
    async fn create_job_network(&self, name: &str, spec: &ContainerSpec) -> Result<()> {
        let labels: HashMap<String, String> =
            spec.labels.clone().into_iter().chain([(LABEL_NETWORK.to_string(), "1".to_string())]).collect();
        let body = NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            labels: Some(labels),
            options: Some(HashMap::from([("com.docker.network.bridge.enable_icc".to_string(), "false".to_string())])),
            ..Default::default()
        };
        self.docker.create_network(body).await.with_context(|| format!("cannot create the network {name}"))?;
        Ok(())
    }

    /// Removes a container's own network. A 404 means it is already gone, and "has active
    /// endpoints" means the daemon has not released the container yet; neither is an error.
    async fn remove_job_network(&self, name: &str) {
        match self.docker.remove_network(name).await {
            Ok(()) => {}
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {}
            Err(bollard::errors::Error::DockerResponseServerError { message, .. }) if message.contains("has active endpoints") => {}
            Err(e) => tracing::warn!("cannot remove the network {name}: {e}"),
        }
    }
}

/// `alpine:3.20` into (`alpine`, `3.20`). A digest reference keeps the digest as the tag part.
pub fn split_image(image: &str) -> (String, String) {
    if let Some((name, digest)) = image.split_once('@') {
        return (name.to_string(), digest.to_string());
    }
    let slash = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    match image[slash..].rfind(':') {
        Some(i) => (image[..slash + i].to_string(), image[slash + i + 1..].to_string()),
        None => (image.to_string(), "latest".to_string()),
    }
}

fn env_list(m: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    m.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

fn create_body(spec: &ContainerSpec, network: &str) -> ContainerCreateBody {
    let host_config = HostConfig {
        binds: if spec.binds.is_empty() { None } else { Some(spec.binds.clone()) },
        nano_cpus: Some((spec.cpus * 1_000_000_000.0) as i64),
        memory: Some(spec.memory as i64),
        // Equal to memory: no swap, so a runaway job is killed instead of thrashing the host.
        memory_swap: Some(spec.memory as i64),
        pids_limit: Some(spec.pids),
        shm_size: Some(spec.shm_size as i64),
        // A tiny init reaps the zombies that build tools leave behind.
        init: Some(true),
        // A bridge of its own: no other container, this owner's or anyone else's, is ever on it.
        network_mode: Some(network.to_string()),
        // No raw sockets: a job cannot sniff or spoof its neighbours on the daemon's bridges.
        cap_drop: Some(vec!["NET_RAW".to_string()]),
        // Never privileged, no devices: the defaults, said out loud.
        privileged: Some(false),
        ..Default::default()
    };
    let mut labels = spec.labels.clone();
    // So remove() can find the network that belongs to this container alone.
    labels.insert(LABEL_NETWORK.to_string(), network.to_string());
    ContainerCreateBody {
        image: Some(spec.image.clone()),
        env: Some(env_list(&spec.env)),
        labels: Some(labels.into_iter().collect()),
        working_dir: spec.workdir.clone(),
        user: spec.user.clone(),
        entrypoint: spec.entrypoint.clone(),
        cmd: spec.cmd.clone(),
        host_config: Some(host_config),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        ..Default::default()
    }
}

fn chunk_of(o: LogOutput) -> Option<Chunk> {
    match o {
        LogOutput::StdOut { message } | LogOutput::Console { message } => {
            Some(Chunk { stream: StreamKind::Stdout, data: message.to_vec() })
        }
        LogOutput::StdErr { message } => Some(Chunk { stream: StreamKind::Stderr, data: message.to_vec() }),
        LogOutput::StdIn { .. } => None,
    }
}

#[async_trait]
impl Backend for DockerBackend {
    async fn ping(&self) -> Result<String> {
        let v = self.docker.version().await.context("the Docker daemon did not answer")?;
        Ok(v.version.unwrap_or_else(|| "unknown".into()))
    }

    async fn ensure_image(&self, image: &str, always: bool, log: &LineSink) -> Result<()> {
        if !always && self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        let _ = log.send(format!("Pulling image {image}"));
        let (name, tag) = split_image(image);
        let opts = CreateImageOptionsBuilder::default().from_image(&name).tag(&tag).build();
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(ev) = stream.next().await {
            let info = ev.with_context(|| format!("pulling {image} failed"))?;
            if let Some(e) = info.error_detail.as_ref().and_then(|d| d.message.clone()) {
                bail!("pulling {image} failed: {e}");
            }
            if let Some(status) = info.status
                && (status.starts_with("Digest:") || status.starts_with("Status:"))
            {
                let _ = log.send(status);
            }
        }
        Ok(())
    }

    async fn create_and_start(&self, spec: &ContainerSpec) -> Result<String> {
        let network = format!("haven-net-{}", spec.name);
        self.create_job_network(&network, spec).await?;
        let opts = CreateContainerOptionsBuilder::default().name(&spec.name).build();
        let created = match self.docker.create_container(Some(opts), create_body(spec, &network)).await {
            Ok(c) => c,
            Err(e) => {
                self.remove_job_network(&network).await;
                return Err(anyhow!(e)).with_context(|| format!("cannot create a container from {}", spec.image));
            }
        };
        if let Err(e) = self.docker.start_container(&created.id, None).await {
            let _ = self.remove(&created.id).await;
            self.remove_job_network(&network).await;
            return Err(anyhow!(e)).with_context(|| format!("cannot start a container from {}", spec.image));
        }
        Ok(created.id)
    }

    async fn exec(&self, id: &str, spec: &ExecSpec, out: mpsc::Sender<Chunk>) -> Result<i64> {
        let opts = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            attach_stdin: Some(false),
            tty: Some(false),
            cmd: Some(spec.argv.clone()),
            env: Some(env_list(&spec.env)),
            working_dir: spec.workdir.clone(),
            user: spec.user.clone(),
            privileged: Some(false),
            ..Default::default()
        };
        let created = self.docker.create_exec(id, opts).await.context("cannot start a command in the job container")?;
        match self.docker.start_exec(&created.id, None).await.context("cannot attach to the command")? {
            StartExecResults::Attached { mut output, .. } => {
                // The stream stays open while any process the command started holds its output,
                // such as a daemon left running in the background. The command is over when its
                // own process is, so check for that and stop reading shortly after.
                let mut check = tokio::time::interval(Duration::from_millis(400));
                check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut exited_at: Option<tokio::time::Instant> = None;
                loop {
                    let linger = async {
                        match exited_at {
                            Some(t) => tokio::time::sleep_until(t + Duration::from_millis(1500)).await,
                            None => std::future::pending::<()>().await,
                        }
                    };
                    tokio::select! {
                        item = output.next() => match item {
                            Some(Ok(chunk)) => {
                                if let Some(c) = chunk_of(chunk)
                                    && out.send(c).await.is_err() {
                                        break;
                                    }
                            }
                            Some(Err(e)) => return Err(anyhow!(e)).context("lost the command's output"),
                            None => break,
                        },
                        _ = check.tick(), if exited_at.is_none() => {
                            if let Ok(i) = self.docker.inspect_exec(&created.id).await
                                && i.running == Some(false) {
                                    exited_at = Some(tokio::time::Instant::now());
                                }
                        }
                        _ = linger => break,
                    }
                }
            }
            StartExecResults::Detached => bail!("the command ran detached"),
        }
        // The stream can end a moment before the daemon records the exit code.
        for _ in 0..50 {
            let inspect = self.docker.inspect_exec(&created.id).await.context("cannot read the command's exit code")?;
            if inspect.running != Some(true) {
                return inspect.exit_code.ok_or_else(|| anyhow!("the daemon did not report an exit code"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("the command's output closed but it is still running")
    }

    async fn stream_until_exit(&self, id: &str, out: mpsc::Sender<Chunk>) -> Result<i64> {
        let opts = LogsOptionsBuilder::default().follow(true).stdout(true).stderr(true).build();
        let mut logs = self.docker.logs(id, Some(opts));
        while let Some(item) = logs.next().await {
            let chunk = item.context("lost the container's output")?;
            if let Some(c) = chunk_of(chunk)
                && out.send(c).await.is_err()
            {
                break;
            }
        }
        for _ in 0..100 {
            let inspect = self.docker.inspect_container(id, None).await.context("cannot read the container's state")?;
            let state = inspect.state.unwrap_or_default();
            if state.running != Some(true) {
                return Ok(state.exit_code.unwrap_or(-1));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("the container's output closed but it is still running")
    }

    async fn remove(&self, id: &str) -> Result<()> {
        // The container's label says which network goes with it. Already gone: nothing to do.
        let network = match self.docker.inspect_container(id, None).await {
            Ok(info) => info.config.as_ref().and_then(|c| c.labels.as_ref()).and_then(|l| l.get(LABEL_NETWORK)).cloned(),
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => return Ok(()),
            Err(e) => {
                tracing::warn!("cannot inspect container {id} before removing it: {e}");
                None
            }
        };
        let opts = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
        match self.docker.remove_container(id, Some(opts)).await {
            Ok(()) => {}
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {}
            // 409 "removal already in progress" is also fine.
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => {}
            Err(e) => return Err(anyhow!(e)).with_context(|| format!("cannot remove container {id}")),
        }
        if let Some(name) = network {
            self.remove_job_network(&name).await;
        }
        Ok(())
    }

    async fn cleanup_orphans(&self, runner_id: i64) -> Result<usize> {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("label".into(), vec![format!("{LABEL_RUNNER}={runner_id}")]);
        let opts = ListContainersOptionsBuilder::default().all(true).filters(&filters).build();
        let list = self.docker.list_containers(Some(opts)).await.context("cannot list containers")?;
        let mut n = 0;
        for c in list {
            if let Some(id) = c.id
                && self.remove(&id).await.is_ok()
            {
                n += 1;
            }
        }
        // The same crashed run leaves the per-job networks behind, and they carry the same label.
        let opts = ListNetworksOptionsBuilder::default().filters(&filters).build();
        match self.docker.list_networks(Some(opts)).await {
            Ok(nets) => {
                for net in nets {
                    let Some(name) = net.name.clone().or_else(|| net.id.clone()) else { continue };
                    if let Err(e) = self.docker.remove_network(&name).await {
                        tracing::warn!("cannot remove the orphan network {name}: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("cannot list the networks of runner {runner_id}: {e}"),
        }
        Ok(n)
    }

    async fn prune_images(&self, older_than: Duration, protect: &[String]) -> Result<u64> {
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - older_than.as_secs() as i64;
        let images =
            self.docker.list_images(Some(ListImagesOptionsBuilder::default().all(false).build())).await.context("cannot list images")?;
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptionsBuilder::default().all(true).build()))
            .await
            .context("cannot list containers")?;
        let in_use: std::collections::HashSet<String> = containers.iter().filter_map(|c| c.image_id.clone()).collect();
        let mut freed = 0u64;
        for img in images {
            if img.created > cutoff || in_use.contains(&img.id) {
                continue;
            }
            // Images the config maps labels to may exist only here (loaded, not pulled), so
            // removing one would be permanent.
            if img.repo_tags.iter().any(|t| protect.iter().any(|p| tag_matches(p, t))) {
                continue;
            }
            let opts = RemoveImageOptionsBuilder::default().force(false).build();
            if self.docker.remove_image(&img.id, Some(opts), None).await.is_ok() {
                freed += img.size.max(0) as u64;
            }
        }
        Ok(freed)
    }
}

/// `codehaven-job:trixie` matches a tag of `codehaven-job:trixie` and `docker.io/library/...`
/// forms of the same name.
fn tag_matches(wanted: &str, tag: &str) -> bool {
    let norm = |s: &str| {
        let (n, t) = split_image(s);
        let n = n
            .strip_prefix("docker.io/")
            .unwrap_or(&n)
            .strip_prefix("library/")
            .unwrap_or_else(|| n.strip_prefix("docker.io/").unwrap_or(&n))
            .to_string();
        format!("{n}:{t}")
    };
    norm(wanted) == norm(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names_split() {
        assert_eq!(split_image("alpine"), ("alpine".into(), "latest".into()));
        assert_eq!(split_image("alpine:3.20"), ("alpine".into(), "3.20".into()));
        assert_eq!(split_image("localhost:5000/team/app"), ("localhost:5000/team/app".into(), "latest".into()));
        assert_eq!(split_image("localhost:5000/team/app:v2"), ("localhost:5000/team/app".into(), "v2".into()));
        assert_eq!(split_image("a/b@sha256:abc"), ("a/b".into(), "sha256:abc".into()));
    }

    #[test]
    fn protected_tags_match_across_spellings() {
        assert!(tag_matches("codehaven-job:trixie", "codehaven-job:trixie"));
        assert!(tag_matches("alpine:3.20", "docker.io/library/alpine:3.20"));
        assert!(tag_matches("alpine", "alpine:latest"));
        assert!(!tag_matches("alpine:3.20", "alpine:3.19"));
    }

    #[test]
    fn the_container_is_never_privileged_and_always_limited() {
        let spec = ContainerSpec {
            name: "n".into(),
            image: "i".into(),
            labels: Default::default(),
            env: [("A".to_string(), "1".to_string())].into(),
            binds: vec!["/work/j:/work/j".into()],
            workdir: Some("/work/j/workspace".into()),
            user: None,
            cpus: 2.5,
            memory: 1 << 30,
            pids: 4096,
            shm_size: 64 << 20,
            entrypoint: Some(vec!["tail".into()]),
            cmd: Some(vec!["-f".into(), "/dev/null".into()]),
        };
        let b = create_body(&spec, "haven-net-n");
        let h = b.host_config.unwrap();
        assert_eq!(h.privileged, Some(false));
        assert_eq!(h.nano_cpus, Some(2_500_000_000));
        assert_eq!(h.memory, Some(1 << 30));
        assert_eq!(h.memory_swap, Some(1 << 30));
        assert_eq!(h.pids_limit, Some(4096));
        assert_eq!(h.binds, Some(vec!["/work/j:/work/j".to_string()]));
        // A network of its own, with no raw socket to abuse it or anyone else's with.
        assert_eq!(h.network_mode.as_deref(), Some("haven-net-n"));
        assert_eq!(h.cap_drop, Some(vec!["NET_RAW".to_string()]));
        assert!(h.devices.is_none() && h.cap_add.is_none() && h.pid_mode.is_none());
        assert_eq!(b.env, Some(vec!["A=1".to_string()]));
        assert_eq!(b.labels.as_ref().and_then(|l| l.get(LABEL_NETWORK).map(String::as_str)), Some("haven-net-n"));
    }
}
