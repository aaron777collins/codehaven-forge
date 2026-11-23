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
        Step { number: 2, name: "a public repository, signed out", run: |c| Box::pin(step2(c)) },
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

// ---------------------------------------------------------------- step 2

const PW: &str = "lantern harbour violet 42 kettle";

fn basic(user: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{PW}")))
}

/// The value of `name="<field>" value="..."` in a page.
fn field_value(html: &str, field: &str) -> Option<String> {
    let at = html.find(&format!("name=\"{field}\" value=\""))?;
    let rest = &html[at + field.len() + 15..];
    Some(rest[..rest.find('"')?].to_string())
}

/// `name=value` of every `Set-Cookie`, joined for a `Cookie` header.
fn cookies_of(r: &reqwest::Response, jar: &mut Vec<String>) {
    for v in r.headers().get_all("set-cookie") {
        if let Some(pair) = v.to_str().ok().and_then(|s| s.split(';').next()) {
            let name = pair.split('=').next().unwrap_or("").to_string();
            jar.retain(|c| !c.starts_with(&format!("{name}=")));
            if !pair.ends_with('=') {
                jar.push(pair.to_string());
            }
        }
    }
}

async fn git(args: &[&str]) -> Result<(bool, String)> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("HOME", "/tmp")
        .output()
        .await?;
    Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

/// Step 2 (epic ch-4pn): an owner makes a repository public in the browser, and then a stranger
/// with no account can read it and clone it, but not push, not fetch deleted history, and not
/// tell private and missing apart. Turning the switch off takes it back to internal.
async fn step2(c: &Ctx) -> Result<()> {
    let (ok, _, err) = c
        .codehaven(&["admin", "create-user", "--username", "pubowner", "--email", "pubowner@e2e.invalid", "--password-stdin"], Some(PW))
        .await?;
    ensure!(ok, "create-user: {err}");
    let (ok, out, err) = c.codehaven(&["admin", "public-repos", "on"], None).await?;
    ensure!(ok && out.contains("No repository was made public"), "public-repos on: {out}{err}");

    let auth = basic("pubowner");
    let api = |m: Method, p: &str| c.http.request(m, c.url(&format!("/api/v3{p}"))).header("authorization", &auth);
    for (name, vis) in [("open", "internal"), ("hidden", "private")] {
        let r =
            api(Method::POST, "/user/repos").json(&serde_json::json!({"name": name, "auto_init": true, "visibility": vis})).send().await?;
        ensure!(r.status() == 201, "create {name}: {}", r.status());
    }
    let r = api(Method::POST, "/user/repos").json(&serde_json::json!({"name": "x", "visibility": "public"})).send().await?;
    ensure!(r.status() == 403, "REST never creates a public repository, got {}", r.status());

    // A commit on a branch that is then deleted: still in the store until it is rebuilt.
    let main: serde_json::Value = api(Method::GET, "/repos/pubowner/open/git/ref/heads/main").send().await?.json().await?;
    let main_sha = main["object"]["sha"].as_str().unwrap_or_default().to_string();
    let r = api(Method::POST, "/repos/pubowner/open/git/refs")
        .json(&serde_json::json!({"ref": "refs/heads/tmp", "sha": main_sha}))
        .send()
        .await?;
    ensure!(r.status() == 201, "create branch: {}", r.status());
    use base64::Engine;
    let secret = base64::engine::general_purpose::STANDARD.encode("a password that was committed by mistake\n");
    let r = api(Method::PUT, "/repos/pubowner/open/contents/secret.txt")
        .json(&serde_json::json!({"message": "Oops", "content": secret, "branch": "tmp"}))
        .send()
        .await?;
    ensure!(r.status() == 201, "commit on tmp: {}", r.status());
    let put: serde_json::Value = r.json().await?;
    let secret_sha = put["commit"]["sha"].as_str().unwrap_or_default().to_string();
    ensure!(secret_sha.len() == 40, "no commit sha in {put}");
    let r = api(Method::DELETE, "/repos/pubowner/open/git/refs/heads/tmp").send().await?;
    ensure!(r.status() == 204, "delete branch: {}", r.status());

    // Sign in through the form, then publish on the confirm page.
    let mut jar: Vec<String> = Vec::new();
    let login = c.http.get(c.url("/login")).send().await?;
    cookies_of(&login, &mut jar);
    let page = login.text().await?;
    let token = field_value(&page, "_csrf").unwrap_or_default();
    let r = c
        .http
        .post(c.url("/login"))
        .header("cookie", jar.join("; "))
        .header("origin", &c.base)
        .form(&[("username", "pubowner"), ("password", PW), ("_csrf", token.as_str())])
        .send()
        .await?;
    ensure!(r.status() == 303, "sign in: {}", r.status());
    cookies_of(&r, &mut jar);
    let page = c.http.get(c.url("/pubowner/open/settings/visibility")).header("cookie", jar.join("; ")).send().await?;
    ensure!(page.status() == 200, "confirm page: {}", page.status());
    let html = page.text().await?;
    ensure!(html.contains("Anyone on the internet will be able to read and copy this repository"), "the warning is shown");
    let snapshot = field_value(&html, "snapshot").unwrap_or_default();
    let token = field_value(&html, "_csrf").unwrap_or_default();
    let publish = |confirm: &'static str| {
        let (jar, snapshot, token) = (jar.join("; "), snapshot.clone(), token.clone());
        async move {
            c.http
                .post(c.url("/pubowner/open/settings/visibility"))
                .header("cookie", jar)
                .header("origin", &c.base)
                .form(&[
                    ("value", "public"),
                    ("confirm", confirm),
                    ("checked_history", "1"),
                    ("snapshot", snapshot.as_str()),
                    ("_csrf", token.as_str()),
                ])
                .send()
                .await
        }
    };
    ensure!(publish("pubowner/wrong").await?.status() == 422, "a wrong name is refused");
    let r = publish("pubowner/open").await?;
    ensure!(r.status() == 303, "publish: {}", r.status());
    let repo: serde_json::Value = api(Method::GET, "/repos/pubowner/open").send().await?.json().await?;
    ensure!(repo["visibility"] == "public" && repo["private"] == false, "the repository is public: {repo}");

    // Signed out: pages and the API read, with no cookie and no caching.
    let r = c.get("/pubowner/open").await?;
    ensure!(r.status() == 200, "signed-out repo page: {}", r.status());
    ensure!(r.headers().get("set-cookie").is_none(), "a signed-out read sets no cookie");
    ensure!(r.headers().get("cache-control").and_then(|v| v.to_str().ok()) == Some("private, no-store"), "never cached");
    let r = c.get("/api/v3/repos/pubowner/open").await?;
    ensure!(r.status() == 200, "signed-out API: {}", r.status());
    let r = c.get("/explore").await?;
    ensure!(r.status() == 200 && r.text().await?.contains("pubowner/open"), "explore lists the public repository");

    // Real git, signed out: clone and fetch work, deleted history is gone, push is refused.
    let url = c.url("/pubowner/open.git");
    let _ = tokio::fs::remove_dir_all("/tmp/pub-clone").await;
    let (ok, out) = git(&["clone", "--quiet", &url, "/tmp/pub-clone"]).await?;
    ensure!(ok, "signed-out clone failed: {out}");
    let (ok, _) = git(&["-C", "/tmp/pub-clone", "-c", "protocol.version=2", "fetch", "--quiet", "origin", &secret_sha]).await?;
    ensure!(!ok, "a deleted commit must not be fetchable after publishing");
    let (ok, _) = git(&["-C", "/tmp/pub-clone", "-c", "protocol.version=0", "fetch", "--quiet", "origin", &secret_sha]).await?;
    ensure!(!ok, "nor under protocol v0");
    let (ok, _) = git(&["-C", "/tmp/pub-clone", "push", "--quiet", "origin", "HEAD:refs/heads/stranger"]).await?;
    ensure!(!ok, "a signed-out push must be refused");

    // Private and missing look the same.
    let a = c.get("/pubowner/hidden.git/info/refs?service=git-upload-pack").await?;
    let b = c.get("/pubowner/nothing.git/info/refs?service=git-upload-pack").await?;
    ensure!(a.status() == 401 && b.status() == 401, "private and missing clones challenge: {} {}", a.status(), b.status());
    ensure!(a.text().await? == b.text().await?, "the same body");
    let a = c.get("/api/v3/repos/pubowner/hidden").await?;
    let b = c.get("/api/v3/repos/pubowner/nothing").await?;
    ensure!(a.status() == 401 && b.status() == 401, "private and missing API: {} {}", a.status(), b.status());

    // Off takes it back to internal at once.
    let (ok, out, err) = c.codehaven(&["admin", "public-repos", "off"], None).await?;
    ensure!(ok && out.contains("1 public repository is internal now"), "public-repos off: {out}{err}");
    let r = c.get("/pubowner/open").await?;
    ensure!(r.status() == 303, "after off the page asks for sign-in, got {}", r.status());
    let (ok, _) = git(&["ls-remote", &url]).await?;
    ensure!(!ok, "after off a signed-out clone is refused");
    Ok(())
}
