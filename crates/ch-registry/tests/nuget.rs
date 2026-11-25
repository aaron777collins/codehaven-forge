//! The private NuGet feed over real HTTP, speaking the calls `dotnet nuget push` and `dotnet restore` make (ch-ctk.13).
#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use ch_core::actor::{AuthVia, Scopes};
use ch_core::model::{ClientInfo, SshIdentity};
use ch_core::services::{Authn, Authz, OrgRole};
use ch_core::{Actor, Error, OrgId, Permission, RepoId, Result, TokenId, UserId};
use ch_testkit::TestEnv;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct FakeAuthz {
    pool: PgPool,
    org_role_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Authz for FakeAuthz {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        let (uid, cap) = match actor {
            Actor::User { id, .. } => (*id, Permission::Admin),
            // As the real Authz does: a token is capped by its repository scopes, so a
            // packages-only token has no repository permission at all.
            Actor::Token { user_id, scopes, .. } => (*user_id, scopes.repo_cap()),
            _ => return Ok(Permission::None),
        };
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT owner_id, visibility FROM repositories WHERE id = $1").bind(repo.0).fetch_optional(&self.pool).await?;
        let Some((owner, vis)) = row else { return Ok(Permission::None) };
        let mut perm = if owner == uid.0 { Permission::Admin } else { Permission::None };
        let c: Option<(String,)> = sqlx::query_as("SELECT permission FROM repo_collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(repo.0)
            .bind(uid.0)
            .fetch_optional(&self.pool)
            .await?;
        if let Some((p,)) = c {
            perm = perm.max(Permission::parse(&p).unwrap());
        }
        if vis == "public" {
            perm = perm.max(Permission::Read);
        }
        Ok(perm.min(cap))
    }
    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        self.org_role_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Some(u) = actor.user_id() else { return Ok(None) };
        let r: Option<(String,)> = sqlx::query_as("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org.0)
            .bind(u.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.map(|r| if r.0 == "owner" { OrgRole::Owner } else { OrgRole::Member }))
    }
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let p = self.repo_permission(actor, repo).await?;
        if p == Permission::None {
            return Err(Error::NotFound);
        }
        if !p.allows(needed) {
            return Err(Error::forbidden("insufficient access"));
        }
        Ok(())
    }
}

/// Passwords `pw-<user>`; tokens `pkg-<user>` (write:packages), `pull-<user>` (read:packages),
/// `repo-<user>` (repo only). The secret works as Basic password or as Bearer token.
struct FakeAuthn {
    pool: PgPool,
}

impl FakeAuthn {
    async fn resolve(&self, user: Option<&str>, secret: &str) -> Option<Actor> {
        let (name, kind) = if let Some(n) = secret.strip_prefix("pw-") {
            (n, "pw")
        } else if let Some(n) = secret.strip_prefix("pkg-") {
            (n, "pkg")
        } else if let Some(n) = secret.strip_prefix("pull-") {
            (n, "pull")
        } else {
            (secret.strip_prefix("repo-")?, "repo")
        };
        if user.is_some_and(|u| u != name) {
            return None;
        }
        let id: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM users WHERE username = $1::citext").bind(name).fetch_optional(&self.pool).await.ok()?;
        let id = UserId(id?.0);
        Some(match kind {
            "pw" => Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Basic },
            "pkg" => Actor::Token { user_id: id, token_id: TokenId(1), scopes: Scopes::new(["write:packages"]) },
            "pull" => Actor::Token { user_id: id, token_id: TokenId(2), scopes: Scopes::new(["read:packages"]) },
            _ => Actor::Token { user_id: id, token_id: TokenId(3), scopes: Scopes::new(["repo"]) },
        })
    }
}

#[async_trait]
impl Authn for FakeAuthn {
    async fn from_session(&self, _c: &str, _i: &ClientInfo) -> Result<Actor> {
        Ok(Actor::Anonymous)
    }
    async fn from_basic(&self, u: &str, s: &str, _i: &ClientInfo) -> Result<Actor> {
        self.resolve(Some(u), s).await.ok_or_else(Error::unauthorized)
    }
    async fn from_bearer(&self, t: &str, _i: &ClientInfo) -> Result<Actor> {
        self.resolve(None, t).await.ok_or_else(Error::unauthorized)
    }
    async fn from_ssh_key(&self, _f: &str) -> Result<Option<SshIdentity>> {
        Ok(None)
    }
}

async fn fake_gate(State(authn): State<Arc<FakeAuthn>>, mut req: Request, next: Next) -> Response {
    use base64::Engine;
    let client = ClientInfo::local();
    let header_val = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_string).unwrap_or_default();
    let actor = if let Some(b) = header_val.strip_prefix("Basic ") {
        let raw = base64::engine::general_purpose::STANDARD.decode(b).ok().and_then(|b| String::from_utf8(b).ok());
        match raw.and_then(|r| r.split_once(':').map(|(u, p)| (u.to_string(), p.to_string()))) {
            Some((u, p)) => authn.from_basic(&u, &p, &client).await.ok(),
            None => None,
        }
    } else if let Some(t) = header_val.strip_prefix("Bearer ") {
        authn.from_bearer(t, &client).await.ok()
    } else {
        None
    };
    let Some(actor) = actor else {
        let mut r = (StatusCode::UNAUTHORIZED, "{\"errors\":[{\"code\":\"UNAUTHORIZED\"}]}").into_response();
        r.headers_mut().insert(header::WWW_AUTHENTICATE, "Bearer realm=\"http://x/v2/token\",service=\"x\"".parse().unwrap());
        return r;
    };
    req.extensions_mut().insert(actor);
    next.run(req).await
}

#[allow(dead_code)]
struct Reg {
    env: TestEnv,
    state: ch_core::AppState,
    base: String,
    c: reqwest::Client,
    cancel: CancellationToken,
    org_role_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for Reg {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[allow(dead_code)]
struct Resp {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

#[allow(dead_code)]
impl Resp {
    fn h(&self, name: &str) -> String {
        self.headers.get(name).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn code(&self) -> String {
        self.json()["errors"][0]["code"].as_str().unwrap_or("").to_string()
    }
}

impl Reg {
    async fn new(pool: PgPool) -> Reg {
        let env = TestEnv::new(pool.clone()).await;
        let mut b = env.builder().await;
        let org_role_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        b.authz = Arc::new(FakeAuthz { pool: pool.clone(), org_role_calls: org_role_calls.clone() });
        let authn = Arc::new(FakeAuthn { pool: pool.clone() });
        b.authn = authn.clone();
        let state = b.build();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().merge(ch_registry::registry_routes()).with_state(state.clone()).layer(from_fn_with_state(authn, fake_gate));
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).with_graceful_shutdown(async move { c2.cancelled().await }).await;
        });
        Reg {
            env,
            state,
            base: format!("http://{addr}"),
            c: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
            cancel,
            org_role_calls,
        }
    }

    async fn send(&self, rb: reqwest::RequestBuilder, secret: &str) -> Resp {
        let r = rb.bearer_auth(secret).send().await.unwrap();
        Resp { status: r.status().as_u16(), headers: r.headers().clone(), body: r.bytes().await.unwrap().to_vec() }
    }

    async fn req(&self, method: reqwest::Method, path: &str, secret: &str, body: Vec<u8>, ct: Option<&str>) -> Resp {
        let mut rb = self.c.request(method, format!("{}{path}", self.base)).body(body);
        if let Some(ct) = ct {
            rb = rb.header("content-type", ct);
        }
        self.send(rb, secret).await
    }

    async fn get(&self, path: &str, secret: &str) -> Resp {
        self.req(reqwest::Method::GET, path, secret, vec![], None).await
    }
}

fn nupkg(spec: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut z = zip::ZipWriter::new(&mut buf);
    let o = zip::write::SimpleFileOptions::default();
    for (name, data) in extra {
        z.start_file(*name, o).unwrap();
        z.write_all(data).unwrap();
    }
    z.start_file("pkg.nuspec", o).unwrap();
    z.write_all(spec.as_bytes()).unwrap();
    z.finish().unwrap();
    buf.into_inner()
}

fn spec(id: &str, version: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><package xmlns=\"http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd\"><metadata><id>{id}</id><version>{version}</version><authors>Ann</authors><description>A {id} library</description></metadata></package>"
    )
}

fn form(file: &[u8]) -> (Vec<u8>, String) {
    let b = "----boundary1234";
    let mut body = format!("--{b}\r\nContent-Disposition: form-data; name=\"package\"; filename=\"package.nupkg\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={b}"))
}

impl Reg {
    async fn push_pkg(&self, owner: &str, secret: &str, file: &[u8]) -> Resp {
        let (body, ct) = form(file);
        self.req(reqwest::Method::PUT, &format!("/nuget/{owner}/push"), secret, body, Some(&ct)).await
    }
}

async fn user(r: &Reg, name: &str) -> UserId {
    r.env.user(name).await
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_package_is_pushed_found_restored_unlisted_and_listed_again(pool: PgPool) {
    let r = Reg::new(pool).await;
    user(&r, "alice").await;
    let pkg = nupkg(&spec("Acme.Tool", "1.2.0"), &[("lib/net8.0/Acme.Tool.dll", b"MZ-not-really")]);
    let put = r.push_pkg("alice", "pkg-alice", &pkg).await;
    assert_eq!(put.status, 201, "{}", String::from_utf8_lossy(&put.body));
    // The service index names the resources a client uses.
    let idx = r.get("/nuget/alice/index.json", "pull-alice").await;
    assert_eq!(idx.status, 200);
    let types: Vec<String> = idx.json()["resources"].as_array().unwrap().iter().map(|x| x["@type"].as_str().unwrap().to_string()).collect();
    for want in ["PackageBaseAddress/3.0.0", "PackagePublish/2.0.0", "SearchQueryService", "RegistrationsBaseUrl/3.6.0"] {
        assert!(types.iter().any(|t| t == want), "{want} in {types:?}");
    }
    // Restore: the versions, the file and the nuspec, by lower-case id.
    let v = r.get("/nuget/alice/flatcontainer/acme.tool/index.json", "pull-alice").await;
    assert_eq!(v.json()["versions"], json!(["1.2.0"]));
    let f = r.get("/nuget/alice/flatcontainer/acme.tool/1.2.0/acme.tool.1.2.0.nupkg", "pull-alice").await;
    assert_eq!((f.status, f.body.clone()), (200, pkg.clone()));
    let n = r.get("/nuget/alice/flatcontainer/acme.tool/1.2.0/acme.tool.nuspec", "pull-alice").await;
    assert!(String::from_utf8_lossy(&n.body).contains("<id>Acme.Tool</id>"));
    let reg = r.get("/nuget/alice/registration/Acme.Tool/index.json", "pull-alice").await;
    assert_eq!(reg.json()["items"][0]["items"][0]["catalogEntry"]["version"], "1.2.0");
    let leaf = r.get("/nuget/alice/registration/acme.tool/1.2.json", "pull-alice").await;
    assert_eq!(leaf.status, 200, "a short version names the same leaf");
    // Search finds it by id and by words of its description.
    for q in ["acme", "library", ""] {
        let s = r.get(&format!("/nuget/alice/query?q={q}&take=10"), "pull-alice").await;
        assert_eq!(s.json()["totalHits"], 1, "{q}");
        assert_eq!(s.json()["data"][0]["id"], "Acme.Tool");
    }
    assert_eq!(r.get("/nuget/alice/query?q=nothing", "pull-alice").await.json()["totalHits"], 0);
    // Unlisted: out of search, still restorable. Listed again: back.
    let d = r.req(reqwest::Method::DELETE, "/nuget/alice/push/Acme.Tool/1.2", "pkg-alice", vec![], None).await;
    assert_eq!(d.status, 204);
    assert_eq!(r.get("/nuget/alice/query?q=acme", "pull-alice").await.json()["totalHits"], 0);
    assert_eq!(r.get("/nuget/alice/flatcontainer/acme.tool/index.json", "pull-alice").await.json()["versions"], json!(["1.2.0"]));
    let p = r.req(reqwest::Method::POST, "/nuget/alice/push/Acme.Tool/1.2.0", "pkg-alice", vec![], None).await;
    assert_eq!(p.status, 204);
    assert_eq!(r.get("/nuget/alice/query?q=acme", "pull-alice").await.json()["totalHits"], 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_version_is_written_once_and_a_bad_package_is_refused(pool: PgPool) {
    let r = Reg::new(pool).await;
    user(&r, "alice").await;
    let ok = nupkg(&spec("Acme.Tool", "1.0.0"), &[]);
    assert_eq!(r.push_pkg("alice", "pkg-alice", &ok).await.status, 201);
    // The same version, written another way, is the same version.
    let again = nupkg(&spec("acme.tool", "1.0"), &[("x.txt", b"other")]);
    assert_eq!(r.push_pkg("alice", "pkg-alice", &again).await.status, 409);
    // Not a form, not a zip, no nuspec, a DOCTYPE, a bad id, a bad version.
    let bad_form = r.req(reqwest::Method::PUT, "/nuget/alice/push", "pkg-alice", ok.clone(), Some("application/octet-stream")).await;
    assert_eq!(bad_form.status, 400);
    assert_eq!(r.push_pkg("alice", "pkg-alice", b"not a zip at all").await.status, 400);
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        use std::io::Write;
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"hi").unwrap();
        z.finish().unwrap();
    }
    assert_eq!(r.push_pkg("alice", "pkg-alice", &buf.into_inner()).await.status, 400);
    let doctype = format!("<!DOCTYPE p [<!ENTITY e 'x'>]>{}", spec("Evil", "1.0.0"));
    assert_eq!(r.push_pkg("alice", "pkg-alice", &nupkg(&doctype, &[])).await.status, 400);
    assert_eq!(r.push_pkg("alice", "pkg-alice", &nupkg(&spec("a/b", "1.0.0"), &[])).await.status, 400);
    assert_eq!(r.push_pkg("alice", "pkg-alice", &nupkg(&spec("Fine", "not-a-version"), &[])).await.status, 400);
    // Nothing of those was kept.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM nuget_versions").fetch_one(&r.env.pool).await.unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn who_may_read_and_who_may_push(pool: PgPool) {
    let r = Reg::new(pool).await;
    let alice = user(&r, "alice").await;
    let bob = user(&r, "bob").await;
    let pkg = nupkg(&spec("Mine", "1.0.0"), &[]);
    // Credentials that cannot push: read-only packages, repo-only.
    assert_eq!(r.push_pkg("alice", "pull-alice", &pkg).await.status, 403);
    assert_eq!(r.push_pkg("alice", "repo-alice", &pkg).await.status, 403);
    // Bob cannot push to Alice's feed, and learns nothing about it.
    assert!([403, 404].contains(&r.push_pkg("alice", "pkg-bob", &pkg).await.status));
    assert_eq!(r.push_pkg("alice", "pkg-alice", &pkg).await.status, 201);
    // Bob cannot read it, whatever he asks; a token without the scope cannot even ask.
    for path in [
        "/nuget/alice/flatcontainer/mine/index.json",
        "/nuget/alice/flatcontainer/mine/1.0.0/mine.1.0.0.nupkg",
        "/nuget/alice/registration/mine/index.json",
    ] {
        assert_eq!(r.get(path, "pull-bob").await.status, 404, "{path}");
    }
    assert_eq!(r.get("/nuget/alice/query?q=mine", "pull-bob").await.json()["totalHits"], 0, "search shows only what he may read");
    assert_eq!(r.get("/nuget/alice/index.json", "repo-bob").await.status, 403);
    assert_eq!(r.get("/nuget/nobody/index.json", "pull-bob").await.status, 404);
    assert_eq!(r.req(reqwest::Method::DELETE, "/nuget/alice/push/Mine/1.0.0", "pkg-bob", vec![], None).await.status, 404);
    let still: bool = sqlx::query_scalar("SELECT listed FROM nuget_versions LIMIT 1").fetch_one(&r.env.pool).await.unwrap();
    assert!(still);
    // Linked to a repository, the repository decides: a collaborator with read restores it.
    let repo = r.env.repo(alice, "lib").await;
    sqlx::query("UPDATE packages SET repo_id = $1 WHERE name = 'Mine'").bind(repo.0).execute(&r.env.pool).await.unwrap();
    assert_eq!(r.get("/nuget/alice/flatcontainer/mine/index.json", "pull-bob").await.status, 404);
    sqlx::query("INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, 'read')")
        .bind(repo.0)
        .bind(bob.0)
        .execute(&r.env.pool)
        .await
        .unwrap();
    assert_eq!(r.get("/nuget/alice/flatcontainer/mine/index.json", "pull-bob").await.status, 200);
    assert_eq!(
        r.req(reqwest::Method::DELETE, "/nuget/alice/push/Mine/1.0.0", "pkg-bob", vec![], None).await.status,
        403,
        "read is not write"
    );
    // Deleting the account's package takes its versions and gives the blob back.
    sqlx::query("DELETE FROM packages WHERE name = 'Mine'").execute(&r.env.pool).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM nuget_versions").fetch_one(&r.env.pool).await.unwrap();
    assert_eq!(left, 0);
    let refs: i64 = sqlx::query_scalar("SELECT COALESCE(sum(refcount), 0)::bigint FROM blobs").fetch_one(&r.env.pool).await.unwrap();
    assert_eq!(refs, 0, "the package's hold on its file is gone with it");
}
