//! The steps. Numbers match the table in ROADMAP.md section 6.

use crate::ctx::Ctx;
use anyhow::{Result, bail, ensure};
use reqwest::Method;
use std::future::Future;
use std::pin::Pin;

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

pub struct Step {
    pub number: u32,
    pub name: &'static str,
    pub run: for<'a> fn(&'a Ctx) -> StepFuture<'a>,
}

pub fn all() -> Vec<Step> {
    vec![
        Step { number: 0, name: "stack up with zero accounts", run: |c| Box::pin(step0(c)) },
        Step { number: 1, name: "signed-out sweep", run: |c| Box::pin(step1(c)) },
    ]
}

/// Every header ARCHITECTURE.md section 7.6 lists.
const SECURITY_HEADERS: &[&str] = &[
    "content-security-policy",
    "x-frame-options",
    "x-content-type-options",
    "referrer-policy",
    "cross-origin-opener-policy",
    "cross-origin-resource-policy",
    "permissions-policy",
    "x-robots-tag",
];

/// Step 0: the stack is up, `/healthz` says ok, `codehaven doctor` passes and `/login` says
/// there are no accounts. (`/readyz` is on the internal listener only. The compose healthcheck
/// runs `codehaven healthcheck` inside the server container, and this harness starts only
/// after that passes.)
async fn step0(c: &Ctx) -> Result<()> {
    let r = c.get("/healthz").await?;
    ensure!(r.status() == 200, "/healthz returned {}", r.status());
    ensure!(r.text().await? == "ok", "/healthz must say ok and nothing else");

    let (ok, out, err) = c.codehaven(&["doctor"], None).await?;
    ensure!(ok, "codehaven doctor failed:\n{out}\n{err}");

    let r = c.get("/login").await?;
    ensure!(r.status() == 200, "/login returned {}", r.status());
    let body = r.text().await?;
    ensure!(body.contains("No accounts yet"), "/login should say there are no accounts yet");
    ensure!(!body.to_lowercase().contains("sign up") && !body.to_lowercase().contains("register"), "/login must not offer sign-up");
    Ok(())
}

/// Step 1: a signed-out visitor sees almost nothing, and what they see is identical whether
/// the thing exists or not.
async fn step1(c: &Ctx) -> Result<()> {
    struct Case {
        path: &'static str,
        status: u16,
        kind: &'static str,
    }
    let cases = [
        Case { path: "/", status: 303, kind: "page" },
        Case { path: "/explore", status: 303, kind: "page" },
        Case { path: "/root", status: 303, kind: "page" },
        Case { path: "/nobody/nothing", status: 303, kind: "page" },
        Case { path: "/signup", status: 303, kind: "page" },
        Case { path: "/register", status: 303, kind: "page" },
        Case { path: "/metrics", status: 303, kind: "page" },
        Case { path: "/readyz", status: 303, kind: "page" },
        Case { path: "/api/v3/user", status: 401, kind: "api" },
        Case { path: "/api/v3/meta", status: 401, kind: "api" },
        Case { path: "/api/v3/version", status: 401, kind: "api" },
        Case { path: "/api/v3/repos/x/y", status: 401, kind: "api" },
        Case { path: "/x/y.git/info/refs?service=git-upload-pack", status: 401, kind: "git" },
        Case { path: "/v2/", status: 401, kind: "registry" },
        Case { path: "/api/runner/v1/jobs/request", status: 401, kind: "runner" },
    ];
    for case in &cases {
        let r = c.get(case.path).await?;
        ensure!(r.status().as_u16() == case.status, "{} returned {}, expected {}", case.path, r.status(), case.status);
        let h = r.headers().clone();
        match case.kind {
            "page" => {
                let loc = h.get("location").and_then(|v| v.to_str().ok()).unwrap_or("");
                ensure!(loc.starts_with("/login"), "{} redirected to {loc}", case.path);
            }
            "git" => ensure!(
                h.get("www-authenticate").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("Basic")),
                "{} needs a Basic challenge",
                case.path
            ),
            "registry" => ensure!(
                h.get("www-authenticate").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("Bearer")),
                "/v2/ needs a Bearer challenge"
            ),
            _ => {}
        }
        for name in SECURITY_HEADERS {
            ensure!(h.contains_key(*name), "{} is missing the {name} header", case.path);
        }
    }

    // A page that exists and a page that does not get the same answer, apart from the echo of
    // the path itself in `next`.
    let a = c.get("/login-nothing-here").await?;
    let b = c.get("/explore").await?;
    ensure!(a.status() == b.status(), "existing and missing pages differ");

    // The public pages carry every security header too.
    let r = c.get("/login").await?;
    for name in SECURITY_HEADERS {
        ensure!(r.headers().contains_key(*name), "/login is missing the {name} header");
    }
    ensure!(r.headers().get("server").is_none(), "the Server header must not be set");

    // No sign-up route exists, whatever the method.
    for m in [Method::GET, Method::POST, Method::PUT] {
        let r = c.send(m.clone(), "/signup", None).await?;
        ensure!(r.status() == 303, "{m} /signup returned {}", r.status());
    }

    // An identity header from an auth proxy must change nothing.
    let r = c.http.get(c.url("/api/v3/user")).header("Remote-User", "root").header("Remote-Groups", "admins").send().await?;
    if r.status() != 401 {
        bail!("Remote-User must not sign anyone in, got {}", r.status());
    }
    Ok(())
}
