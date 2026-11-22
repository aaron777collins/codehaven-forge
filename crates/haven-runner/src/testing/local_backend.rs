//! A `Backend` that runs commands on the host instead of in containers. The job engine does
//! not know the difference, so its logic (step order, `if:`, outputs, masking, cancel, timeouts)
//! can be tested without a Docker daemon. Never use it for real jobs: nothing is isolated.

use crate::backend::{Backend, Chunk, ContainerSpec, ExecSpec, LineSink, StreamKind};
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct LocalBackend {
    containers: Mutex<HashMap<String, ContainerSpec>>,
    removed: Mutex<Vec<String>>,
    created: Mutex<Vec<ContainerSpec>>,
}

impl LocalBackend {
    pub fn new() -> LocalBackend {
        LocalBackend::default()
    }

    /// Ids of containers that were removed, in order.
    pub fn removed(&self) -> Vec<String> {
        self.removed.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Every container spec that was created, in order.
    pub fn created(&self) -> Vec<ContainerSpec> {
        self.created.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn live_containers(&self) -> usize {
        self.containers.lock().map(|c| c.len()).unwrap_or(0)
    }

    async fn run(argv: &[String], env: &HashMap<String, String>, workdir: Option<&str>, out: mpsc::Sender<Chunk>) -> Result<i64> {
        let Some((prog, args)) = argv.split_first() else { bail!("empty command") };
        let mut cmd = Command::new(prog);
        cmd.args(args).env_clear().stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        if let Some(p) = std::env::var_os("PATH") {
            cmd.env("PATH", p);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(w) = workdir {
            cmd.current_dir(w);
        }
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("cannot start {prog}: {e}"))?;
        let mut so = child.stdout.take().expect("piped");
        let mut se = child.stderr.take().expect("piped");
        let (o2, e2) = (out.clone(), out);
        // Both streams are read inside this future, so dropping it drops them with it.
        let pump = async {
            let a = async {
                let mut buf = [0u8; 4096];
                loop {
                    match so.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if o2.send(Chunk { stream: StreamKind::Stdout, data: buf[..n].to_vec() }).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            };
            let b = async {
                let mut buf = [0u8; 4096];
                loop {
                    match se.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if e2.send(Chunk { stream: StreamKind::Stderr, data: buf[..n].to_vec() }).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            };
            tokio::join!(a, b);
        };
        // A background process the step left running keeps the pipes open after the step itself
        // has exited. The step is over when its own process is, so give the pipes a moment to
        // drain and move on.
        tokio::pin!(pump);
        let mut pump_done = false;
        let status = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                st = &mut wait => st?,
                _ = &mut pump => {
                    pump_done = true;
                    wait.await?
                }
            }
        };
        if !pump_done {
            let _ = tokio::time::timeout(Duration::from_millis(500), &mut pump).await;
        }
        Ok(match status.code() {
            Some(c) => i64::from(c),
            None => {
                use std::os::unix::process::ExitStatusExt;
                128 + i64::from(status.signal().unwrap_or(0))
            }
        })
    }
}

#[async_trait]
impl Backend for LocalBackend {
    async fn ping(&self) -> Result<String> {
        Ok("local".into())
    }

    async fn ensure_image(&self, image: &str, _always: bool, log: &LineSink) -> Result<()> {
        if image.contains("missing") {
            bail!("pulling {image} failed: manifest unknown");
        }
        let _ = log.send(format!("Using image {image} (local backend)"));
        Ok(())
    }

    async fn create_and_start(&self, spec: &ContainerSpec) -> Result<String> {
        if spec.image.contains("broken") {
            bail!("cannot start a container from {}", spec.image);
        }
        let id = spec.name.clone();
        self.containers.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.insert(id.clone(), spec.clone());
        self.created.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.push(spec.clone());
        Ok(id)
    }

    async fn exec(&self, id: &str, spec: &ExecSpec, out: mpsc::Sender<Chunk>) -> Result<i64> {
        let base = self.containers.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.get(id).cloned();
        let Some(base) = base else { bail!("no such container {id}") };
        let mut env: HashMap<String, String> = base.env.clone().into_iter().collect();
        env.extend(spec.env.clone());
        Self::run(&spec.argv, &env, spec.workdir.as_deref(), out).await
    }

    async fn stream_until_exit(&self, id: &str, out: mpsc::Sender<Chunk>) -> Result<i64> {
        let spec = self.containers.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.get(id).cloned();
        let Some(spec) = spec else { bail!("no such container {id}") };
        let mut argv = spec.entrypoint.clone().unwrap_or_default();
        argv.extend(spec.cmd.clone().unwrap_or_default());
        if argv.is_empty() {
            bail!("the image {} has no command", spec.image);
        }
        let env: HashMap<String, String> = spec.env.clone().into_iter().collect();
        Self::run(&argv, &env, spec.workdir.as_deref(), out).await
    }

    async fn remove(&self, id: &str) -> Result<()> {
        self.containers.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.remove(id);
        self.removed.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.push(id.to_string());
        Ok(())
    }

    async fn cleanup_orphans(&self, _runner_id: i64) -> Result<usize> {
        Ok(0)
    }

    async fn prune_images(&self, _older_than: Duration, _protect: &[String]) -> Result<u64> {
        Ok(0)
    }
}
