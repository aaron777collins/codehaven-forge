//! What every step shares: an HTTP client that never follows redirects, the base URL, and a
//! way to run the `codehaven` binary.

use anyhow::{Context, Result};
use reqwest::{Client, Method, Response, redirect::Policy};
use std::process::Stdio;

pub struct Ctx {
    pub base: String,
    pub http: Client,
}

impl Ctx {
    pub fn from_env() -> Result<Self> {
        let base = std::env::var("CH_E2E_URL").unwrap_or_else(|_| "http://server:3200".into());
        let http = Client::builder().redirect(Policy::none()).timeout(std::time::Duration::from_secs(20)).build()?;
        Ok(Ctx { base: base.trim_end_matches('/').to_string(), http })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// A request from one client address. The e2e stack trusts the harness's own subnet as a
    /// proxy, so `X-Forwarded-For` picks the address the server sees.
    pub async fn send(&self, method: Method, path: &str, from_ip: Option<&str>) -> Result<Response> {
        let mut req = self.http.request(method, self.url(path));
        if let Some(ip) = from_ip {
            req = req.header("x-forwarded-for", ip);
        }
        req.send().await.with_context(|| format!("request to {path}"))
    }

    pub async fn get(&self, path: &str) -> Result<Response> {
        self.send(Method::GET, path, None).await
    }

    /// Runs `codehaven <args>` and returns (success, stdout, stderr).
    pub async fn codehaven(&self, args: &[&str], stdin: Option<&str>) -> Result<(bool, String, String)> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("codehaven")
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting the codehaven binary")?;
        if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
            si.write_all(text.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into()))
    }
}
