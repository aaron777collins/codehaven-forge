//! The whole `haven` CLI against a fake server that speaks the REST v3 shapes. Response bodies
//! are the committed `ch-api-types` snapshots, so a drift in the DTOs breaks these tests too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::State as AxState;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use haven_cli::config::{FileStore, TokenStore};
use haven_cli::{Env, run};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

// ---- fixtures ------------------------------------------------------------------------------------

macro_rules! snap {
    ($name:literal) => {{
        let text = include_str!(concat!("../../ch-api-types/src/snapshots/ch_api_types__", $name, ".snap"));
        let body = text.splitn(3, "---\n").nth(2).expect("snapshot body");
        serde_json::from_str::<Value>(body).expect("snapshot is JSON")
    }};
}

fn with(mut v: Value, pairs: &[(&str, Value)]) -> Value {
    for (k, x) in pairs {
        v[*k] = x.clone();
    }
    v
}

fn repo_json(full: &str) -> Value {
    with(snap!("repos_repository"), &[("full_name", json!(full)), ("name", json!(full.split('/').nth(1).unwrap_or(full)))])
}

fn issue_json(n: i64, title: &str, is_pr: bool) -> Value {
    let mut v =
        with(snap!("issues_issue"), &[("number", json!(n)), ("title", json!(title)), ("state", json!("open")), ("comments", json!(1))]);
    if is_pr {
        v["pull_request"] = json!({"url": "u", "html_url": "h", "diff_url": "d", "patch_url": "p"});
    } else {
        v.as_object_mut().unwrap().remove("pull_request");
    }
    v
}

// ---- the fake server -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Rec {
    method: String,
    path: String,
    query: String,
    body: Value,
    auth: String,
}

#[derive(Default)]
struct State {
    recs: Vec<Rec>,
    run_polls: u32,
    org_repos_only: bool,
    repo_tiers: bool,
    merge_status: Option<u16>,
    release_assets: Vec<Value>,
    uploads: Vec<(String, String, usize)>,
    base: String,
}

type Shared = Arc<Mutex<State>>;

fn json_resp(status: u16, v: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn err_resp(status: u16, msg: &str) -> Response {
    json_resp(status, json!({"message": msg}))
}

async fn handle(AxState(st): AxState<Shared>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 10 << 20).await.unwrap_or_default();
    let body_json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    let auth = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let method = parts.method.clone();
    let rec = Rec {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        body: if body_json.is_null() && !bytes.is_empty() { json!({"raw_len": bytes.len()}) } else { body_json.clone() },
        auth: auth.clone(),
    };
    let base = {
        let mut g = st.lock().unwrap();
        g.recs.push(rec);
        g.base.clone()
    };
    if auth != "token tok-good" {
        return err_resp(401, "Bad credentials");
    }
    let p = path.strip_prefix("/api/v3").unwrap_or(&path).to_string();
    let seg: Vec<&str> = p.trim_matches('/').split('/').collect();
    let q = |k: &str| -> Option<String> { query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(String::from)) };
    match (method.clone(), seg.as_slice()) {
        (Method::GET, ["user"]) => json_resp(200, with(snap!("users_user"), &[("login", json!("alice"))])),
        (Method::POST, ["user", "repos"]) => json_resp(201, repo_json(&format!("alice/{}", body_json["name"].as_str().unwrap_or("x")))),
        (Method::POST, ["orgs", org, "repos"]) => {
            json_resp(201, repo_json(&format!("{org}/{}", body_json["name"].as_str().unwrap_or("x"))))
        }
        (Method::GET, ["users", _, "repos"]) if st.lock().unwrap().org_repos_only => err_resp(404, "Not Found"),
        (Method::GET, ["users", u, "repos"]) | (Method::GET, ["orgs", u, "repos"]) => {
            json_resp(200, json!([repo_json(&format!("{u}/one")), repo_json(&format!("{u}/two"))]))
        }
        (Method::GET, ["user", "repos"]) => {
            if st.lock().unwrap().repo_tiers {
                json_resp(
                    200,
                    json!([
                        with(repo_json("alice/mine"), &[("visibility", json!("private"))]),
                        with(repo_json("alice/inside"), &[("visibility", json!("internal"))]),
                        repo_json("alice/open"), // the snapshot's own "public"
                    ]),
                )
            } else {
                json_resp(200, json!([repo_json("alice/mine")]))
            }
        }
        (Method::GET, ["repos", o, r]) => json_resp(200, repo_json(&format!("{o}/{r}"))),
        (Method::POST, ["repos", o, r, "forks"]) => {
            json_resp(202, repo_json(&format!("{}/{r}", body_json["organization"].as_str().unwrap_or("alice")).replace("//", "/")).clone())
                .into_response()
                .tap(|_| {
                    let _ = o;
                })
        }

        // issues
        (Method::GET, ["repos", _, _, "issues"]) => {
            let page: u32 = q("page").and_then(|p| p.parse().ok()).unwrap_or(1);
            let per_page: u32 = q("per_page").and_then(|p| p.parse().ok()).unwrap_or(30);
            let all: Vec<Value> = (1..=5).map(|n| issue_json(n, &format!("issue {n}"), n == 2)).collect();
            let start = ((page - 1) * per_page.min(2)) as usize;
            let chunk: Vec<Value> = all.iter().skip(start).take(per_page.min(2) as usize).cloned().collect();
            let mut r = json_resp(200, Value::Array(chunk));
            if start + (per_page.min(2) as usize) < all.len() {
                let next = format!("<{base}/api/v3{p}?page={}&per_page={per_page}>; rel=\"next\"", page + 1);
                r.headers_mut().insert(header::LINK, next.parse().unwrap());
            }
            r
        }
        (Method::POST, ["repos", _, _, "issues"]) => json_resp(
            201,
            with(issue_json(9, body_json["title"].as_str().unwrap_or(""), false), &[("html_url", json!("https://h/alice/demo/issues/9"))]),
        ),
        (Method::GET, ["repos", _, _, "issues", n]) => json_resp(200, issue_json(n.parse().unwrap(), "the title", false)),
        (Method::PATCH, ["repos", _, _, "issues", n]) => {
            json_resp(200, with(issue_json(n.parse().unwrap(), "closing this", false), &[("state", json!("closed"))]))
        }
        (Method::GET, ["repos", _, _, "issues", _, "comments"]) => json_resp(200, json!([snap!("issues_issue_comment")])),
        (Method::POST, ["repos", _, _, "issues", _, "comments"]) => {
            json_resp(201, with(snap!("issues_issue_comment"), &[("html_url", json!("https://h/c/1"))]))
        }

        // pulls
        (Method::GET, ["repos", _, _, "pulls"]) => {
            let merged = with(
                snap!("pulls_pull_request"),
                &[("number", json!(1)), ("title", json!("merged one")), ("state", json!("closed")), ("merged", json!(true))],
            );
            let closed = with(
                snap!("pulls_pull_request"),
                &[("number", json!(2)), ("title", json!("closed one")), ("state", json!("closed")), ("merged", json!(false))],
            );
            let open = with(
                snap!("pulls_pull_request"),
                &[
                    ("number", json!(3)),
                    ("title", json!("open one")),
                    ("state", json!("open")),
                    ("merged", json!(false)),
                    ("draft", json!(false)),
                ],
            );
            match q("state").as_deref() {
                Some("closed") => json_resp(200, json!([merged, closed])),
                Some("open") | None => json_resp(200, json!([open])),
                _ => json_resp(200, json!([open, merged, closed])),
            }
        }
        (Method::POST, ["repos", _, _, "pulls"]) => {
            json_resp(201, with(snap!("pulls_pull_request"), &[("html_url", json!("https://h/alice/demo/pulls/4"))]))
        }
        (Method::GET, ["repos", _, _, "pulls", n]) => json_resp(
            200,
            with(
                snap!("pulls_pull_request"),
                &[
                    ("number", json!(n.parse::<i64>().unwrap())),
                    ("title", json!("view me")),
                    ("state", json!("open")),
                    ("merged", json!(false)),
                ],
            ),
        ),
        (Method::GET, ["repos", _, _, "pulls", _, "reviews"]) => json_resp(200, json!([snap!("pulls_review")])),
        (Method::POST, ["repos", _, _, "pulls", _, "reviews"]) => json_resp(200, snap!("pulls_review")),
        (Method::GET, ["repos", _, _, "pulls", _, "files"]) => json_resp(
            200,
            json!([with(
                snap!("pulls_pull_file"),
                &[("filename", json!("src/lib.rs")), ("status", json!("modified")), ("patch", json!("@@ -1 +1 @@\n-old\n+new"))]
            )]),
        ),
        (Method::PUT, ["repos", _, _, "pulls", _, "merge"]) => match st.lock().unwrap().merge_status {
            Some(s) => err_resp(s, "Required status check \"ci / test\" is expected."),
            None => json_resp(200, with(snap!("pulls_merge_result"), &[("sha", json!("0123456789abcdef"))])),
        },
        (Method::PUT, ["repos", _, _, "pulls", n, "auto-merge"]) => json_resp(
            200,
            with(
                snap!("pulls_pull_request"),
                &[
                    ("number", json!(n.parse::<i64>().unwrap_or(0))),
                    ("auto_merge", json!({"merge_method": "squash", "enabled_by": snap!("pulls_auto_merge")["enabled_by"]})),
                ],
            ),
        ),
        (Method::DELETE, ["repos", _, _, "pulls", n, "auto-merge"]) => json_resp(
            200,
            with(snap!("pulls_pull_request"), &[("number", json!(n.parse::<i64>().unwrap_or(0))), ("auto_merge", json!(null))]),
        ),
        (Method::GET, ["repos", _, _, "commits", _, "check-runs"]) => json_resp(200, snap!("ci_check_runs_response")),

        // actions
        (Method::GET, ["repos", _, _, "actions", "runs"]) | (Method::GET, ["repos", _, _, "actions", "workflows", _, "runs"]) => {
            json_resp(200, snap!("ci_workflow_runs_response"))
        }
        (Method::GET, ["repos", _, _, "actions", "runs", id]) => {
            let polls = {
                let mut g = st.lock().unwrap();
                g.run_polls += 1;
                g.run_polls
            };
            let (status, conclusion) = if polls >= 3 {
                ("completed", json!(if st.lock().unwrap().recs.iter().any(|r| r.path.ends_with("/999")) { "failure" } else { "success" }))
            } else {
                ("in_progress", Value::Null)
            };
            json_resp(
                200,
                with(
                    snap!("ci_workflow_run"),
                    &[("id", json!(id.parse::<i64>().unwrap())), ("status", json!(status)), ("conclusion", conclusion)],
                ),
            )
        }
        (Method::GET, ["repos", _, _, "actions", "runs", _, "jobs"]) => {
            let polls = st.lock().unwrap().run_polls;
            let (status, conclusion) = if polls >= 3 { ("completed", json!("success")) } else { ("in_progress", Value::Null) };
            json_resp(
                200,
                json!({"total_count": 1, "jobs": [with(snap!("ci_workflow_job"), &[("id", json!(5001)), ("name", json!("ci / test")), ("status", json!(status)), ("conclusion", conclusion)])]}),
            )
        }
        (Method::GET, ["repos", _, _, "actions", "jobs", _, "logs"]) => (StatusCode::OK, "line one\nline two\n").into_response(),
        (Method::POST, ["repos", _, _, "actions", "runs", _, "rerun" | "rerun-failed-jobs" | "cancel"]) => json_resp(201, json!({})),
        (Method::GET, ["repos", _, _, "actions", "workflows"]) => json_resp(200, snap!("ci_workflows_response")),
        (Method::POST, ["repos", _, _, "actions", "workflows", _, "dispatches"]) => json_resp(204, Value::Null),

        // releases
        (Method::GET, ["repos", _, _, "releases"]) => json_resp(200, json!([snap!("releases_release")])),
        (Method::POST, ["repos", _, _, "releases"]) => json_resp(
            201,
            with(
                snap!("releases_release"),
                &[
                    ("upload_url", json!(format!("{base}/api/v3/repos/alice/demo/releases/1/assets{{?name,label}}"))),
                    ("html_url", json!("https://h/alice/demo/releases/tag/v1")),
                    ("assets", json!([])),
                ],
            ),
        ),
        (Method::GET, ["repos", _, _, "releases", "tags", _]) => {
            let assets = st.lock().unwrap().release_assets.clone();
            json_resp(
                200,
                with(
                    snap!("releases_release"),
                    &[
                        ("upload_url", json!(format!("{base}/api/v3/repos/alice/demo/releases/1/assets{{?name,label}}"))),
                        ("assets", Value::Array(assets)),
                    ],
                ),
            )
        }
        (Method::POST, ["repos", _, _, "releases", _, "assets"]) => {
            let name = q("name").unwrap_or_default();
            let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            st.lock().unwrap().uploads.push((name.clone(), ct, bytes.len()));
            json_resp(201, with(snap!("releases_release_asset"), &[("name", json!(name))]))
        }
        (Method::DELETE, ["repos", _, _, "releases", "assets", _]) => json_resp(204, Value::Null),
        _ => err_resp(404, "Not Found"),
    }
}

trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl Tap for Response {}

struct Server {
    url: String,
    st: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server() -> Server {
    let st: Shared = Arc::new(Mutex::new(State::default()));
    let app = Router::new().fallback(handle).with_state(st.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    st.lock().unwrap().base = url.clone();
    let task = tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    Server { url, st, task }
}

impl Server {
    fn recs(&self) -> Vec<Rec> {
        self.st.lock().unwrap().recs.clone()
    }

    fn find(&self, method: &str, path_suffix: &str) -> Vec<Rec> {
        self.recs().into_iter().filter(|r| r.method == method && r.path.ends_with(path_suffix)).collect()
    }
}

// ---- the CLI harness -------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct Cli {
    dir: tempfile::TempDir,
    cwd: tempfile::TempDir,
    host: String,
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

impl Cli {
    fn new(host: &str) -> Cli {
        Cli { dir: tempfile::tempdir().unwrap(), cwd: tempfile::tempdir().unwrap(), host: host.to_string() }
    }

    /// Runs `haven <args>` with the given standard input and `HAVEN_TOKEN`.
    async fn run_with(&self, args: &[&str], stdin: &str, token: Option<&str>) -> Out {
        let (out, err) = (Buf::default(), Buf::default());
        let env = Env {
            config_dir: self.dir.path().to_path_buf(),
            cwd: self.cwd.path().to_path_buf(),
            token: token.map(String::from),
            store: Arc::new(FileStore::new(self.dir.path())),
            out: Arc::new(Mutex::new(Box::new(out.clone()))),
            err: Arc::new(Mutex::new(Box::new(err.clone()))),
            stdin: Arc::new(Mutex::new(Box::new(std::io::Cursor::new(stdin.as_bytes().to_vec())) as Box<dyn Read + Send>)),
            interactive: false,
        };
        let mut argv: Vec<String> = vec!["haven".into(), "--host".into(), self.host.clone()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let code = run(argv, env).await;
        Out { code, out: out.text(), err: err.text() }
    }

    async fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, "", Some("tok-good")).await
    }

    /// Like `run` with `-R alice/demo` already in place.
    async fn repo(&self, args: &[&str]) -> Out {
        let mut a = vec!["-R", "alice/demo"];
        a.extend_from_slice(args);
        self.run(&a).await
    }
}

fn ok(o: &Out) {
    assert_eq!(o.code, 0, "exit {}:\nstdout: {}\nstderr: {}", o.code, o.out, o.err);
}

// ---- auth ----------------------------------------------------------------------------------------

#[tokio::test]
async fn auth_login_stores_the_token_and_status_checks_it() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run_with(&["auth", "login", "--with-token"], "tok-good\n", None).await;
    ok(&o);
    assert!(o.out.contains("as alice"), "{}", o.out);
    let store = FileStore::new(c.dir.path());
    assert_eq!(store.get(&s.url).unwrap().as_deref(), Some("tok-good"));
    let hosts = std::fs::read_to_string(c.dir.path().join("hosts.toml")).unwrap();
    assert!(hosts.contains("alice") && !hosts.contains("tok-good"), "no secret in hosts.toml: {hosts}");

    // Later commands need no flag: the stored host and token are used.
    let o = c.run_with(&["auth", "status"], "", None).await;
    ok(&o);
    assert!(o.out.contains("Signed in") && o.out.contains("alice"), "{}", o.out);
    let o = c.run_with(&["auth", "token"], "", None).await;
    assert_eq!(o.out.trim(), "tok-good");

    let o = c.run_with(&["auth", "logout"], "", None).await;
    ok(&o);
    assert_eq!(store.get(&s.url).unwrap(), None);
    let o = c.run_with(&["auth", "status"], "", None).await;
    assert_eq!(o.code, 1);
}

#[tokio::test]
async fn auth_login_refuses_a_bad_token_and_stores_nothing() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run_with(&["auth", "login", "--with-token"], "wrong-token", None).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("Bad credentials") && o.err.contains("haven auth login"), "{}", o.err);
    assert_eq!(FileStore::new(c.dir.path()).get(&s.url).unwrap(), None);
    assert!(!c.dir.path().join("hosts.toml").exists());
    let empty = c.run_with(&["auth", "login", "--with-token"], "  \n", None).await;
    assert_eq!(empty.code, 1);
}

#[tokio::test]
async fn the_environment_token_beats_the_stored_one_and_commands_need_a_sign_in() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run_with(&["issue", "list", "-R", "alice/demo"], "", None).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("not signed in"), "{}", o.err);
    let o = c.run_with(&["issue", "list", "-R", "alice/demo"], "", Some("tok-good")).await;
    ok(&o);
    assert!(s.recs().iter().all(|r| r.auth == "token tok-good"));
}

#[tokio::test]
async fn the_git_credential_helper_answers_only_for_its_own_host() {
    let s = server().await;
    let c = Cli::new(&s.url);
    ok(&c.run_with(&["auth", "login", "--with-token"], "tok-good", None).await);
    let host_port = s.url.trim_start_matches("http://");
    let o = c.run_with(&["auth", "git-credential", "get"], &format!("protocol=http\nhost={host_port}\n\n"), None).await;
    ok(&o);
    assert!(o.out.contains("username=alice") && o.out.contains("password=tok-good"), "{}", o.out);
    let other = c.run_with(&["auth", "git-credential", "get"], "protocol=https\nhost=github.com\n\n", None).await;
    ok(&other);
    assert_eq!(other.out, "", "no token for a stranger");
    let ssh = c.run_with(&["auth", "git-credential", "get"], &format!("protocol=ssh\nhost={host_port}\n\n"), None).await;
    assert_eq!(ssh.out, "");
    for op in ["store", "erase"] {
        let o = c
            .run_with(&["auth", "git-credential", op], &format!("protocol=http\nhost={host_port}\nusername=x\npassword=y\n\n"), None)
            .await;
        ok(&o);
        assert_eq!(o.out, "");
    }
    assert_eq!(FileStore::new(c.dir.path()).get(&s.url).unwrap().as_deref(), Some("tok-good"), "store and erase must not touch the token");
}

// ---- issues ----------------------------------------------------------------------------------------

#[tokio::test]
async fn issue_list_pages_through_and_hides_pull_requests() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["issue", "list", "--limit", "0"]).await;
    ok(&o);
    assert!(o.out.contains("#1") && o.out.contains("issue 3") && o.out.contains("#5"), "{}", o.out);
    assert!(!o.out.contains("issue 2"), "issue 2 is a pull request");
    let pages = s.find("GET", "/issues").len();
    assert!(pages >= 3, "followed the Link header, {pages} requests");
    let limited = c.repo(&["issue", "list", "--limit", "2"]).await;
    ok(&limited);
    assert_eq!(limited.out.lines().count(), 2, "{}", limited.out);
    let bad = c.repo(&["issue", "list", "--state", "bogus"]).await;
    assert_eq!(bad.code, 1);
}

#[tokio::test]
async fn issue_list_json_is_valid_json() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["issue", "list", "--json", "--limit", "3"]).await;
    ok(&o);
    let v: Value = serde_json::from_str(&o.out).expect("valid JSON");
    assert_eq!(v.as_array().unwrap().len(), 3);
    assert_eq!(v[0]["number"], 1);
}

#[tokio::test]
async fn issue_create_close_comment_and_view() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["issue", "create", "--title", "Broken thing", "--body", "details here", "--label", "bug", "--assignee", "bob"]).await;
    ok(&o);
    assert!(o.out.contains("/issues/9"));
    let body = &s.find("POST", "/issues")[0].body;
    assert_eq!(body["title"], "Broken thing");
    assert_eq!(body["body"], "details here");
    assert_eq!(body["labels"], json!(["bug"]));
    assert_eq!(body["assignees"], json!(["bob"]));

    let o = c.repo(&["issue", "close", "7", "--comment", "done in #8"]).await;
    ok(&o);
    let patch = &s.find("PATCH", "/issues/7")[0].body;
    assert_eq!(
        patch,
        &json!({"state": "closed", "state_reason": "completed"}),
        "closing must not send labels or assignees, which would clear them"
    );
    assert_eq!(s.find("POST", "/issues/7/comments")[0].body["body"], "done in #8");
    let recs = s.recs();
    let comment_at = recs.iter().position(|r| r.path.ends_with("/issues/7/comments")).unwrap();
    let patch_at = recs.iter().position(|r| r.method == "PATCH").unwrap();
    assert!(comment_at < patch_at, "the comment goes first");

    ok(&c.repo(&["issue", "comment", "7", "--body", "thanks"]).await);
    assert_eq!(s.find("POST", "/issues/7/comments").len(), 2);
    let empty = c.repo(&["issue", "comment", "7"]).await;
    assert_eq!(empty.code, 1);

    let o = c.repo(&["issue", "view", "5", "--comments"]).await;
    ok(&o);
    assert!(o.out.contains("#5 the title") && o.out.contains("Me too"), "{}", o.out);
    let o = c.repo(&["issue", "close", "7", "--reason", "meh"]).await;
    assert_eq!(o.code, 1);
}

#[tokio::test]
async fn issue_bodies_can_come_from_a_file_or_stdin() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let f = c.cwd.path().join("body.md");
    std::fs::write(&f, "from a file\nline two").unwrap();
    ok(&c.repo(&["issue", "create", "-t", "T", "-F", f.to_str().unwrap()]).await);
    assert_eq!(s.find("POST", "/issues")[0].body["body"], "from a file\nline two");
    let o = c.run_with(&["-R", "alice/demo", "issue", "create", "-t", "T2", "-F", "-"], "piped body", Some("tok-good")).await;
    ok(&o);
    assert_eq!(s.find("POST", "/issues")[1].body["body"], "piped body");
}

// ---- repos -------------------------------------------------------------------------------------------

#[tokio::test]
async fn repo_create_picks_the_right_endpoint_and_defaults_to_private() {
    let s = server().await;
    let c = Cli::new(&s.url);
    ok(&c.run(&["repo", "create", "mine", "-d", "my repo"]).await);
    let r = &s.find("POST", "/user/repos")[0];
    assert_eq!(r.body["name"], "mine");
    assert_eq!(r.body["visibility"], "private");
    assert!(r.body.get("private").is_none() || r.body["private"].is_null());
    assert_eq!(r.body["description"], "my repo");
    ok(&c.run(&["repo", "create", "alice/also-mine"]).await);
    assert_eq!(s.find("POST", "/user/repos").len(), 2, "an owner that is you uses your own endpoint");
    ok(&c.run(&["repo", "create", "acme/team-repo", "--internal", "--add-readme"]).await);
    let org = &s.find("POST", "/orgs/acme/repos")[0];
    assert_eq!(org.body["visibility"], "internal");
    assert_eq!(org.body["auto_init"], true);
    let o = c.run(&["repo", "create", "acme/", "--json"]).await;
    assert_eq!(o.code, 1);
}

#[tokio::test]
async fn repo_create_public_refuses_and_sends_nothing() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run(&["repo", "create", "open", "--public"]).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("Make a repository public in the browser."), "{}", o.err);
    assert!(s.find("POST", "/user/repos").is_empty(), "nothing was sent");
    let o = c.run(&["repo", "create", "x", "--public", "--internal"]).await;
    assert_ne!(o.code, 0);
}

#[tokio::test]
async fn repo_list_shows_all_three_tiers() {
    let s = server().await;
    let c = Cli::new(&s.url);
    s.st.lock().unwrap().repo_tiers = true;
    let o = c.run(&["repo", "list"]).await;
    ok(&o);
    assert!(o.out.contains("private") && o.out.contains("internal") && o.out.contains("public"), "{}", o.out);
}

#[tokio::test]
async fn repo_view_list_and_fork() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run(&["repo", "view", "alice/demo"]).await;
    ok(&o);
    assert!(o.out.contains("alice/demo") && o.out.contains("Default branch: main") && o.out.contains("Clone (ssh)"), "{}", o.out);
    let o = c.run(&["repo", "view", "alice/demo", "--json"]).await;
    assert_eq!(serde_json::from_str::<Value>(&o.out).unwrap()["full_name"], "alice/demo");

    let o = c.run(&["repo", "list"]).await;
    ok(&o);
    assert!(o.out.contains("alice/mine"));
    let o = c.run(&["repo", "list", "bob"]).await;
    ok(&o);
    assert!(o.out.contains("bob/one") && o.out.contains("bob/two"));
    s.st.lock().unwrap().org_repos_only = true;
    let o = c.run(&["repo", "list", "acme"]).await;
    ok(&o);
    assert!(o.out.contains("acme/one"), "{}", o.out);
    assert_eq!(s.find("GET", "/orgs/acme/repos").len(), 1, "fell back to the organization endpoint on a 404");

    ok(&c.run(&["repo", "fork", "bob/tool", "--org", "acme"]).await);
    assert_eq!(s.find("POST", "/forks")[0].body["organization"], "acme");
}

// ---- pull requests --------------------------------------------------------------------------------------

#[tokio::test]
async fn pr_list_states() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["pr", "list"]).await;
    ok(&o);
    assert!(o.out.contains("#3") && o.out.contains("open one"), "{}", o.out);
    let o = c.repo(&["pr", "list", "--state", "merged"]).await;
    ok(&o);
    assert!(o.out.contains("merged one") && !o.out.contains("closed one"), "merged filters closed ones client side: {}", o.out);
    assert!(s.find("GET", "/pulls").iter().any(|r| r.query.contains("state=closed")), "the server is asked for closed");
    let o = c.repo(&["pr", "list", "--state", "all", "--json"]).await;
    assert_eq!(serde_json::from_str::<Value>(&o.out).unwrap().as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn pr_create_uses_the_current_branch_and_the_default_base() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(c.cwd.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q", "-b", "feature-x"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@x", "commit", "-q", "--allow-empty", "-m", "c"]);
    let o = c.repo(&["pr", "create", "--title", "My change", "--body", "why"]).await;
    ok(&o);
    assert!(o.out.contains("/pulls/4"));
    let b = &s.find("POST", "/pulls")[0].body;
    assert_eq!(b["head"], "feature-x");
    assert_eq!(b["base"], "main", "the repository's default branch");
    assert_eq!(b["title"], "My change");
    assert!(b.get("draft").is_none());
    ok(&c.repo(&["pr", "create", "-t", "Draft", "-H", "bob:topic", "-B", "release", "--draft"]).await);
    let b = &s.find("POST", "/pulls")[1].body;
    assert_eq!((b["head"].as_str(), b["base"].as_str(), b["draft"].as_bool()), (Some("bob:topic"), Some("release"), Some(true)));
}

#[tokio::test]
async fn pr_create_without_a_branch_says_what_to_do() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["pr", "create", "-t", "x"]).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("--head") || o.err.contains("repository"), "{}", o.err);
}

#[tokio::test]
async fn pr_view_review_diff_and_merge() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["pr", "view", "3", "--comments"]).await;
    ok(&o);
    assert!(o.out.contains("#3 view me") && o.out.contains("Mergeable"), "{}", o.out);
    assert!(o.out.contains("Checks:"), "{}", o.out);

    let o = c.repo(&["pr", "diff", "3"]).await;
    ok(&o);
    assert!(o.out.contains("diff --git a/src/lib.rs b/src/lib.rs") && o.out.contains("+new"), "{}", o.out);

    ok(&c.repo(&["pr", "review", "3", "--approve"]).await);
    assert_eq!(s.find("POST", "/pulls/3/reviews")[0].body["event"], "APPROVE");
    let none = c.repo(&["pr", "review", "3"]).await;
    assert_eq!(none.code, 1);
    let nobody = c.repo(&["pr", "review", "3", "--request-changes"]).await;
    assert_eq!(nobody.code, 1, "requesting changes needs a reason");
    ok(&c.repo(&["pr", "review", "3", "--request-changes", "-b", "please fix"]).await);
    assert_eq!(s.find("POST", "/pulls/3/reviews")[1].body["event"], "REQUEST_CHANGES");

    let o = c.repo(&["pr", "merge", "3", "--squash", "--subject", "Add the thing"]).await;
    ok(&o);
    assert!(o.out.contains("Merged #3") && o.out.contains("01234567"), "{}", o.out);
    let m = &s.find("PUT", "/pulls/3/merge")[0].body;
    assert_eq!((m["merge_method"].as_str(), m["commit_title"].as_str()), (Some("squash"), Some("Add the thing")));
    ok(&c.repo(&["pr", "merge", "3"]).await);
    assert_eq!(s.find("PUT", "/pulls/3/merge")[1].body["merge_method"], "merge");
    let both = c.repo(&["pr", "merge", "3", "--squash", "--rebase"]).await;
    assert_ne!(both.code, 0);
}

#[tokio::test]
async fn pr_merge_auto_turns_auto_merge_on_and_off() {
    // ch-96n: the REST route exists, so the CLI offers it.
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["pr", "merge", "3", "--auto", "--squash"]).await;
    ok(&o);
    assert!(o.out.contains("Auto-merge is on for #3") && o.out.contains("squash"), "{}", o.out);
    assert_eq!(s.find("PUT", "/pulls/3/auto-merge")[0].body["merge_method"], "squash");
    assert!(s.find("PUT", "/pulls/3/merge").is_empty(), "nothing merges now");
    let o = c.repo(&["pr", "merge", "3", "--disable-auto"]).await;
    ok(&o);
    assert!(o.out.contains("Auto-merge is off for #3"), "{}", o.out);
    assert_eq!(s.find("DELETE", "/pulls/3/auto-merge").len(), 1);
    let both = c.repo(&["pr", "merge", "3", "--auto", "--disable-auto"]).await;
    assert_ne!(both.code, 0);
}

#[tokio::test]
async fn pr_merge_shows_why_the_server_refused() {
    let s = server().await;
    s.st.lock().unwrap().merge_status = Some(405);
    let c = Cli::new(&s.url);
    let o = c.repo(&["pr", "merge", "3"]).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("Required status check") && o.err.contains("405"), "{}", o.err);
}

// ---- actions ------------------------------------------------------------------------------------------

#[tokio::test]
async fn run_list_view_logs_rerun_cancel() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["run", "list", "--branch", "main", "--status", "completed"]).await;
    ok(&o);
    assert!(!o.out.trim().is_empty());
    assert!(
        s.find("GET", "/actions/runs")[0].query.contains("branch=main")
            && s.find("GET", "/actions/runs")[0].query.contains("status=completed")
    );
    ok(&c.repo(&["run", "list", "-w", "ci.yml"]).await);
    assert!(!s.find("GET", "/actions/workflows/ci.yml/runs").is_empty());

    s.st.lock().unwrap().run_polls = 5; // finished
    let o = c.repo(&["run", "view", "1234"]).await;
    ok(&o);
    assert!(o.out.contains("Run 1234") && o.out.contains("ci / test"), "{}", o.out);

    let o = c.repo(&["run", "logs", "1234"]).await;
    ok(&o);
    assert_eq!(o.out, "line one\nline two\n");
    let o = c.repo(&["run", "logs", "1234", "--job", "nothing-like-this"]).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("ci / test"), "the error lists the jobs: {}", o.err);

    ok(&c.repo(&["run", "rerun", "1234"]).await);
    ok(&c.repo(&["run", "rerun", "1234", "--failed"]).await);
    ok(&c.repo(&["run", "cancel", "1234"]).await);
    assert_eq!(s.find("POST", "/runs/1234/rerun").len(), 1);
    assert_eq!(s.find("POST", "/runs/1234/rerun-failed-jobs").len(), 1);
    assert_eq!(s.find("POST", "/runs/1234/cancel").len(), 1);
}

#[tokio::test]
async fn run_watch_follows_a_run_and_exits_by_its_conclusion() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["run", "watch", "1234", "--interval", "1"]).await;
    assert_eq!(o.code, 0, "{}\n{}", o.out, o.err);
    assert!(o.out.contains("ci / test: in_progress") && o.out.contains("ci / test: success"), "{}", o.out);
    assert_eq!(o.out.matches("ci / test: in_progress").count(), 1, "only changes are printed: {}", o.out);
    assert!(o.out.contains("finished: success"));

    // A run that ends in failure exits non-zero. (The fake fails run 999.)
    s.st.lock().unwrap().run_polls = 0;
    let o = c.repo(&["run", "watch", "999", "--interval", "1"]).await;
    assert_eq!(o.code, 1, "{}", o.out);
    assert!(o.out.contains("finished: failure"));
}

#[tokio::test]
async fn workflow_list_and_dispatch() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["workflow", "list"]).await;
    ok(&o);
    assert!(o.out.contains(".yaml") || o.out.contains(".yml"), "{}", o.out);
    ok(&c.repo(&["workflow", "run", "ci.yml", "-f", "level=debug", "-f", "dry=true"]).await);
    let d = &s.find("POST", "/dispatches")[0];
    assert!(d.path.contains("/actions/workflows/ci.yml/dispatches"));
    assert_eq!(d.body["ref"], "main", "defaults to the repository's default branch");
    assert_eq!(d.body["inputs"], json!({"level": "debug", "dry": "true"}));
    ok(&c.repo(&["workflow", "run", "ci.yml", "--ref", "release"]).await);
    let d = &s.find("POST", "/dispatches")[1];
    assert_eq!(d.body["ref"], "release");
    assert!(d.body.get("inputs").is_none(), "no inputs when none were given");
    let bad = c.repo(&["workflow", "run", "ci.yml", "-f", "novalue"]).await;
    assert_eq!(bad.code, 1);
}

// ---- releases --------------------------------------------------------------------------------------------

#[tokio::test]
async fn release_create_uploads_files_after_creating_the_release() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let f = c.cwd.path().join("app v1.zip");
    std::fs::write(&f, b"zipbytes").unwrap();
    let o = c.repo(&["release", "create", "v1", "-t", "Version 1", "-n", "notes", "--draft", f.to_str().unwrap()]).await;
    ok(&o);
    let b = &s.find("POST", "/releases")[0].body;
    assert_eq!(
        (b["tag_name"].as_str(), b["name"].as_str(), b["body"].as_str(), b["draft"].as_bool()),
        (Some("v1"), Some("Version 1"), Some("notes"), Some(true))
    );
    assert!(b.get("generate_release_notes").is_none());
    let up = s.st.lock().unwrap().uploads.clone();
    assert_eq!(up, vec![("app%20v1.zip".to_string(), "application/zip".to_string(), 8)], "the name is sent encoded, the type guessed");
    assert!(o.out.contains("Uploaded app v1.zip") && o.out.contains("/releases/tag/v1"), "{}", o.out);

    // A file that does not exist must stop before the release is created.
    let before = s.find("POST", "/releases").len();
    let o = c.repo(&["release", "create", "v2", "/no/such/file.bin"]).await;
    assert_eq!(o.code, 1);
    assert_eq!(s.find("POST", "/releases").len(), before);
}

#[tokio::test]
async fn release_upload_checks_for_clashes_first_and_clobber_replaces() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let (a, b) = (c.cwd.path().join("a.txt"), c.cwd.path().join("b.txt"));
    std::fs::write(&a, "A").unwrap();
    std::fs::write(&b, "B").unwrap();
    s.st.lock().unwrap().release_assets = vec![with(snap!("releases_release_asset"), &[("name", json!("b.txt")), ("id", json!(77))])];
    let o = c.repo(&["release", "upload", "v1", a.to_str().unwrap(), b.to_str().unwrap()]).await;
    assert_eq!(o.code, 1);
    assert!(o.err.contains("b.txt is already on this release"), "{}", o.err);
    assert!(s.st.lock().unwrap().uploads.is_empty(), "nothing was uploaded, not even a.txt");
    let o = c.repo(&["release", "upload", "v1", a.to_str().unwrap(), b.to_str().unwrap(), "--clobber"]).await;
    ok(&o);
    assert_eq!(s.find("DELETE", "/releases/assets/77").len(), 1);
    let names: Vec<String> = s.st.lock().unwrap().uploads.iter().map(|u| u.0.clone()).collect();
    assert_eq!(names, ["a.txt", "b.txt"]);
}

#[tokio::test]
async fn release_list() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.repo(&["release", "list"]).await;
    ok(&o);
    assert!(!o.out.trim().is_empty());
    let o = c.repo(&["release", "list", "--json"]).await;
    assert!(serde_json::from_str::<Value>(&o.out).unwrap().is_array());
}

// ---- api -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn api_get_post_headers_and_exit_status() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run(&["api", "/user"]).await;
    ok(&o);
    assert_eq!(serde_json::from_str::<Value>(&o.out).unwrap()["login"], "alice");

    // -f on a GET becomes the query string.
    ok(&c.run(&["api", "/repos/alice/demo/issues", "-f", "state=closed", "-f", "q=a b"]).await);
    assert_eq!(s.find("GET", "/issues").last().unwrap().query, "q=a%20b&state=closed");

    // -f and -F on a POST become a JSON body, typed by -F.
    ok(&c
        .run(&["api", "/repos/alice/demo/issues", "-X", "POST", "-f", "title=T", "-F", "n=3", "-F", "flag=true", "-F", "nothing=null"])
        .await);
    let posts = s.find("POST", "/issues");
    assert_eq!(posts.last().unwrap().body, json!({"title": "T", "n": 3, "flag": true, "nothing": null}));

    let o = c.run(&["api", "/user", "--include"]).await;
    ok(&o);
    assert!(o.out.starts_with("HTTP 200"), "{}", o.out);

    let o = c.run(&["api", "/nope"]).await;
    assert_eq!(o.code, 1, "a 404 exits non-zero");
    assert!(o.out.contains("Not Found"));

    let o = c.run(&["api", "/user", "-H", "no colon here"]).await;
    assert_eq!(o.code, 1);
}

#[tokio::test]
async fn api_paginate_merges_every_page() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run(&["api", "/repos/alice/demo/issues", "--paginate", "-f", "per_page=2"]).await;
    ok(&o);
    let v: Value = serde_json::from_str(&o.out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 5, "all pages in one array");
}

#[tokio::test]
async fn the_token_is_never_sent_to_another_host() {
    let s = server().await;
    let other = server().await;
    let c = Cli::new(&s.url);
    // An absolute URL on a different host: the call goes out, but without the token.
    let o = c.run(&["api", &format!("{}/api/v3/user", other.url)]).await;
    assert_eq!(o.code, 1, "{}", o.out);
    assert!(other.recs().iter().all(|r| r.auth.is_empty()), "the token leaked to another host: {:?}", other.recs());
}

#[tokio::test]
async fn unknown_commands_and_help_exit_cleanly() {
    let s = server().await;
    let c = Cli::new(&s.url);
    let o = c.run(&["--help"]).await;
    assert_eq!(o.code, 0);
    assert!(
        o.out.contains("auth")
            && o.out.contains("repo")
            && o.out.contains("issue")
            && o.out.contains("pr")
            && o.out.contains("run")
            && o.out.contains("workflow")
            && o.out.contains("release")
            && o.out.contains("api")
    );
    let o = c.run(&["bogus"]).await;
    assert_eq!(o.code, 2);
    assert!(!o.err.is_empty());
    let o = c.run(&["--version"]).await;
    assert_eq!(o.code, 0);
}
