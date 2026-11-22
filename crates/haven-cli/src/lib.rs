//! `haven`, the Codehaven command line client (ARCHITECTURE.md section 13).
//!
//! The binary is a thin `main`. Everything runs through [`run`], which takes its environment
//! (config directory, working directory, standard streams) as a value so the tests can drive
//! the whole CLI against a fake server and read what it printed.

pub mod api;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod fmt;
pub mod repoctx;

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use api::ApiClient;
pub use cli::Cli;
use config::{Hosts, TokenStore};

/// Everything the CLI reads from outside itself.
pub struct Env {
    pub config_dir: PathBuf,
    pub cwd: PathBuf,
    /// `HAVEN_TOKEN`: beats the stored token.
    pub token: Option<String>,
    pub store: Arc<dyn TokenStore>,
    pub out: Arc<Mutex<Box<dyn Write + Send>>>,
    pub err: Arc<Mutex<Box<dyn Write + Send>>>,
    pub stdin: Arc<Mutex<Box<dyn Read + Send>>>,
    /// True when standard input is a terminal, so prompting for a token makes sense.
    pub interactive: bool,
}

impl Env {
    /// The real process environment.
    pub fn from_process() -> Result<Env> {
        use std::io::IsTerminal;
        let config_dir = config::default_config_dir()?;
        Ok(Env {
            store: Arc::new(config::KeyringStore::new(&config_dir)),
            config_dir,
            cwd: std::env::current_dir()?,
            token: std::env::var("HAVEN_TOKEN").ok().filter(|t| !t.trim().is_empty()),
            out: Arc::new(Mutex::new(Box::new(std::io::stdout()))),
            err: Arc::new(Mutex::new(Box::new(std::io::stderr()))),
            stdin: Arc::new(Mutex::new(Box::new(std::io::stdin()))),
            interactive: std::io::stdin().is_terminal(),
        })
    }
}

/// What a command gets: the environment plus the global flags.
pub struct Ctx {
    pub env: Env,
    pub host_flag: Option<String>,
    pub repo_flag: Option<String>,
}

impl Ctx {
    pub fn println(&self, s: impl AsRef<str>) {
        if let Ok(mut o) = self.env.out.lock() {
            let _ = writeln!(o, "{}", s.as_ref());
        }
    }

    pub fn print(&self, s: impl AsRef<str>) {
        if let Ok(mut o) = self.env.out.lock() {
            let _ = write!(o, "{}", s.as_ref());
            let _ = o.flush();
        }
    }

    pub fn eprintln(&self, s: impl AsRef<str>) {
        if let Ok(mut o) = self.env.err.lock() {
            let _ = writeln!(o, "{}", s.as_ref());
        }
    }

    pub fn print_json<T: serde::Serialize>(&self, v: &T) -> Result<()> {
        self.println(serde_json::to_string_pretty(v)?);
        Ok(())
    }

    pub fn read_stdin(&self) -> Result<String> {
        let mut s = String::new();
        self.env.stdin.lock().map_err(|_| anyhow!("standard input is unavailable"))?.read_to_string(&mut s)?;
        Ok(s)
    }

    pub fn hosts(&self) -> Result<Hosts> {
        Hosts::load(&self.env.config_dir)
    }

    /// The base URL: `--host` or `HAVEN_HOST`, else the host you last signed in to.
    pub fn host(&self) -> Result<String> {
        if let Some(h) = self.host_flag.as_deref().filter(|h| !h.trim().is_empty()) {
            return config::normalize_host(h);
        }
        match self.hosts()?.default {
            Some(h) => Ok(h),
            None => bail!("no host is set. Run `haven auth login --host <host>` or pass --host."),
        }
    }

    pub fn token_for(&self, host: &str) -> Result<Option<String>> {
        if let Some(t) = &self.env.token {
            return Ok(Some(t.clone()));
        }
        self.env.store.get(host)
    }

    /// A client for the current host, signed in.
    pub fn api(&self) -> Result<ApiClient> {
        let host = self.host()?;
        let Some(token) = self.token_for(&host)? else {
            bail!("you are not signed in to {host}. Run `haven auth login --host {host}`.");
        };
        ApiClient::new(&host, Some(token))
    }

    /// `owner` and `name` of the repository the command is about.
    pub fn repo(&self) -> Result<(String, String)> {
        if let Some(r) = &self.repo_flag {
            return repoctx::parse_owner_name(r);
        }
        let host = self.host().ok();
        repoctx::from_git_remote(&self.env.cwd, host.as_deref())
    }

    /// The first positional repo argument if given, else the flag, else the git remote.
    pub fn repo_or(&self, arg: Option<&str>) -> Result<(String, String)> {
        match arg {
            Some(a) => repoctx::parse_owner_name(a),
            None => self.repo(),
        }
    }
}

/// Parses `argv` (including the program name) and runs the command. The process exit code is
/// the return value; errors are printed to the error stream.
pub async fn run(argv: Vec<String>, env: Env) -> i32 {
    let cli = match Cli::try_parse_from(&argv) {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            let code = if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) { 0 } else { 2 };
            let text = e.render().to_string();
            let target = if code == 0 { &env.out } else { &env.err };
            if let Ok(mut o) = target.lock() {
                let _ = write!(o, "{text}");
            }
            return code;
        }
    };
    let ctx = Ctx { env, host_flag: cli.host.clone(), repo_flag: cli.repo.clone() };
    match cmd::dispatch(&ctx, cli.command).await {
        Ok(code) => code,
        Err(e) => {
            ctx.eprintln(format!("haven: {e:#}"));
            1
        }
    }
}
