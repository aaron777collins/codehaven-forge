//! `haven-runner`: registers with a token an admin issued, long-polls for jobs, runs steps in
//! Docker. Never gets the host's Docker socket (ARCHITECTURE.md section 2.5).

use anyhow::{Context, Result, bail};
use ch_api_types::runner::RegisterRequest;
use clap::{Parser, Subcommand};
use haven_runner::client::Client;
use haven_runner::config::Config;
use haven_runner::credential::{self, Credential};
use haven_runner::docker::DockerBackend;
use haven_runner::runner::Runner;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "haven-runner", version, about = "Codehaven CI runner")]
struct Cli {
    /// Where the runner keeps what `register` learned. Defaults to `.runner` here, or under
    /// $HAVEN_RUNNER_HOME.
    #[arg(long, global = true, env = "HAVEN_RUNNER_CREDENTIAL_FILE")]
    credential_file: Option<PathBuf>,

    /// `text` or `json`.
    #[arg(long, global = true, env = "HAVEN_RUNNER_LOG_FORMAT", default_value = "text")]
    log_format: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register with a server using an admin-issued single-use token.
    Register {
        /// The server's address, such as https://code.example.com
        #[arg(long, env = "HAVEN_RUNNER_URL")]
        url: String,
        /// The registration token. Prefer the environment variable: command lines show up in `ps`.
        #[arg(long, env = "HAVEN_RUNNER_TOKEN", hide_env_values = true)]
        token: String,
        #[arg(long, env = "HAVEN_RUNNER_NAME")]
        name: String,
        /// Labels this runner can serve. Jobs run on it when their `runs-on` labels are a subset.
        /// Left out, the labels of the settings file's `[labels]` table and the architecture.
        #[arg(long, env = "HAVEN_RUNNER_LABELS", value_delimiter = ',')]
        labels: Vec<String>,
        /// Settings file whose labels to register. Defaults as for `run`.
        #[arg(long, env = "HAVEN_RUNNER_CONFIG")]
        config: Option<PathBuf>,
        /// Replace an existing registration file.
        #[arg(long)]
        replace: bool,
    },
    /// Poll for jobs and run them.
    Run {
        /// Settings file. Defaults to `config.toml` next to the credential file; no file means
        /// the defaults.
        #[arg(long, env = "HAVEN_RUNNER_CONFIG")]
        config: Option<PathBuf>,
        /// Jobs at once. Overrides the settings file.
        #[arg(long, env = "HAVEN_RUNNER_CONCURRENCY")]
        concurrency: Option<usize>,
        /// Where checkouts live. Overrides the settings file.
        #[arg(long, env = "HAVEN_RUNNER_WORK_DIR")]
        work_dir: Option<PathBuf>,
    },
    /// Check the setup: credential, settings, Docker, work directory, disk, server.
    Check {
        #[arg(long, env = "HAVEN_RUNNER_CONFIG")]
        config: Option<PathBuf>,
    },
}

fn init_logging(format: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("HAVEN_RUNNER_LOG")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if format == "json" { b.json().init() } else { b.init() }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(&cli.log_format);
    if let Err(e) = real_main(cli).await {
        eprintln!("haven-runner: {e:#}");
        std::process::exit(1);
    }
}

fn config_path(explicit: Option<PathBuf>, cred_path: &Path) -> PathBuf {
    explicit.unwrap_or_else(|| cred_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("config.toml"))
}

async fn real_main(cli: Cli) -> Result<()> {
    let cred_path = cli.credential_file.clone().unwrap_or_else(credential::default_path);
    match cli.command {
        Command::Register { url, token, name, labels, config, replace } => {
            if cred_path.exists() && !replace {
                bail!("{} already exists. This runner is registered. Use --replace to register again", cred_path.display());
            }
            let labels = if labels.is_empty() {
                Config::load(&config_path(config, &cred_path))?.register_labels()
            } else {
                labels.into_iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
            };
            let url = url.trim_end_matches('/').to_string();
            let client = Client::new(&url, None)?;
            let resp = client
                .register(&RegisterRequest {
                    token,
                    name: name.clone(),
                    labels: labels.clone(),
                    version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    os: Some("linux".into()),
                    arch: Some(std::env::consts::ARCH.to_string()),
                })
                .await
                .context("registration failed")?;
            Credential {
                server_url: url.clone(),
                runner_id: resp.runner_id,
                name: resp.name.clone(),
                labels: labels.clone(),
                credential: resp.credential,
            }
            .save(&cred_path)?;
            println!("Registered runner {} (id {}) with {url}", resp.name, resp.runner_id);
            println!("Labels: {}", labels.join(", "));
            println!("Credential saved to {} with mode 0600. Start the runner with `haven-runner run`.", cred_path.display());
            Ok(())
        }
        Command::Run { config, concurrency, work_dir } => {
            let cred = Credential::load(&cred_path)?;
            let mut cfg = Config::load(&config_path(config, &cred_path))?;
            if let Some(c) = concurrency {
                cfg.runner.concurrency = c;
            }
            if let Some(w) = work_dir {
                cfg.runner.work_dir = w;
                cfg.disk.guard_paths = vec![cfg.runner.work_dir.clone()];
            }
            cfg.validate()?;
            let backend = Arc::new(DockerBackend::connect(&cfg.docker.host)?);
            let (shutdown, force) = (CancellationToken::new(), CancellationToken::new());
            spawn_signal_handler(shutdown.clone(), force.clone());
            let runner = Runner::new(Arc::new(cfg), cred, backend, shutdown, force)?;
            runner.run().await
        }
        Command::Check { config } => check(&cred_path, config).await,
    }
}

#[cfg(unix)]
fn spawn_signal_handler(shutdown: CancellationToken, force: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
        let mut n = 0;
        loop {
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
            n += 1;
            if n == 1 {
                tracing::info!("stop requested: taking no more jobs. Send the signal again to cancel running jobs now");
                shutdown.cancel();
            } else {
                tracing::warn!("second stop request: cancelling running jobs");
                force.cancel();
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_signal_handler(shutdown: CancellationToken, _force: CancellationToken) {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.cancel();
    });
}

async fn check(cred_path: &Path, config: Option<PathBuf>) -> Result<()> {
    let mut problems = 0;
    let mut report = |ok: bool, what: String| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            problems += 1;
        }
    };
    let cred = match Credential::load(cred_path) {
        Ok(c) => {
            report(
                true,
                format!("credential {} (runner {} id {}, labels {})", cred_path.display(), c.name, c.runner_id, c.labels.join(",")),
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(cred_path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0);
                report(mode & 0o077 == 0, format!("credential file mode is {mode:o} (it should be 600)"));
            }
            Some(c)
        }
        Err(e) => {
            report(false, format!("{e:#}"));
            None
        }
    };
    let cfg_file = config_path(config, cred_path);
    let cfg = match Config::load(&cfg_file) {
        Ok(c) => {
            report(
                true,
                format!("settings {} ({} slot(s), work dir {})", cfg_file.display(), c.runner.concurrency, c.runner.work_dir.display()),
            );
            Some(c)
        }
        Err(e) => {
            report(false, format!("{e:#}"));
            None
        }
    };
    if let Some(cfg) = &cfg {
        match DockerBackend::connect(&cfg.docker.host) {
            Ok(b) => match haven_runner::backend::Backend::ping(&b).await {
                Ok(v) => report(true, format!("Docker {v}")),
                Err(e) => report(false, format!("Docker: {e:#}")),
            },
            Err(e) => report(false, format!("Docker: {e:#}")),
        }
        let w = &cfg.runner.work_dir;
        let writable = std::fs::create_dir_all(w).and_then(|_| std::fs::write(w.join(".check"), b"x")).is_ok();
        let _ = std::fs::remove_file(w.join(".check"));
        report(writable, format!("work directory {} is writable", w.display()));
        if let Some((p, pct)) = haven_runner::disk::worst(&cfg.disk.guard_paths) {
            report(pct < cfg.disk.max_percent, format!("disk {} is {pct}% used (limit {}%)", p.display(), cfg.disk.max_percent));
        }
    }
    if let Some(c) = &cred {
        // Asking for a job would take one, so the check only proves the server answers. A
        // credential the server rejects shows up when `run` starts.
        match Client::new(&c.server_url, None) {
            Ok(client) => match client.ping().await {
                Ok(200) => report(true, format!("server {} answers", c.server_url)),
                Ok(code) => report(false, format!("server {} answered /healthz with HTTP {code}", c.server_url)),
                Err(e) => report(false, format!("server {}: {e}", c.server_url)),
            },
            Err(e) => report(false, format!("{e:#}")),
        }
    }
    if problems > 0 {
        bail!("{problems} check(s) failed");
    }
    println!("All checks passed.");
    Ok(())
}
