//! The registry over real HTTP, speaking the same calls `docker push` and `docker pull` make.
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
use sha2::{Digest, Sha256};
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

struct Resp {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

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

    /// The whole upload dance for one blob. Returns its digest.
    async fn push_blob(&self, name: &str, secret: &str, data: &[u8]) -> String {
        let digest = sha(data);
        let start = self.req(reqwest::Method::POST, &format!("/v2/{name}/blobs/uploads/"), secret, vec![], None).await;
        assert_eq!(start.status, 202, "{}", String::from_utf8_lossy(&start.body));
        let loc = start.h("location");
        assert!(loc.starts_with(&format!("/v2/{name}/blobs/uploads/")), "{loc}");
        let mid = data.len() / 2;
        let p1 = self.req(reqwest::Method::PATCH, &loc, secret, data[..mid].to_vec(), Some("application/octet-stream")).await;
        assert_eq!(p1.status, 202);
        assert_eq!(p1.h("range"), format!("0-{}", mid.max(1) - 1));
        let put = self
            .req(reqwest::Method::PUT, &format!("{loc}?digest={digest}"), secret, data[mid..].to_vec(), Some("application/octet-stream"))
            .await;
        assert_eq!(put.status, 201, "{}", String::from_utf8_lossy(&put.body));
        assert_eq!(put.h("docker-content-digest"), digest);
        digest
    }

    async fn push_image(&self, name: &str, tag: &str, secret: &str, layers: &[&[u8]]) -> (String, Value) {
        let config = br#"{"architecture":"amd64","os":"linux"}"#;
        let cfg = self.push_blob(name, secret, config).await;
        let mut layer_json = Vec::new();
        for l in layers {
            let d = self.push_blob(name, secret, l).await;
            layer_json.push(json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": d, "size": l.len()}));
        }
        let manifest = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cfg, "size": config.len()}, "layers": layer_json});
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let put = self
            .req(
                reqwest::Method::PUT,
                &format!("/v2/{name}/manifests/{tag}"),
                secret,
                bytes.clone(),
                Some("application/vnd.oci.image.manifest.v1+json"),
            )
            .await;
        assert_eq!(put.status, 201, "{}", String::from_utf8_lossy(&put.body));
        (sha(&bytes), manifest)
    }
}

fn sha(data: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(data)))
}

async fn world(pool: PgPool) -> (Reg, UserId) {
    let r = Reg::new(pool).await;
    let alice = r.env.user("alice").await;
    (r, alice)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn version_check_and_the_token_exchange(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let v = r.get("/v2/", "pw-alice").await;
    assert_eq!(v.status, 200);
    assert_eq!(v.h("docker-distribution-api-version"), "registry/2.0");
    // No credentials: the stand-in gate challenges, like the real one.
    let anon = r.c.get(format!("{}/v2/", r.base)).send().await.unwrap();
    assert_eq!(anon.status(), 401);
    assert!(anon.headers()[header::WWW_AUTHENTICATE].to_str().unwrap().starts_with("Bearer realm="));
    // docker login: Basic to /v2/token, then the answer as a Bearer token.
    let t =
        r.c.get(format!("{}/v2/token?service=x&scope=repository:alice/demo:push,pull", r.base))
            .basic_auth("alice", Some("pw-alice"))
            .send()
            .await
            .unwrap();
    assert_eq!(t.status(), 200);
    assert_eq!(t.headers()[header::CACHE_CONTROL], "no-store");
    let body: Value = t.json().await.unwrap();
    let token = body["token"].as_str().unwrap();
    assert_eq!(body["access_token"], body["token"]);
    assert!(body["expires_in"].as_i64().unwrap() > 0);
    assert_eq!(r.get("/v2/", token).await.status, 200);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn push_and_pull_an_image_the_way_docker_does(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let layer1 = vec![7u8; 200_000];
    let layer2 = b"second layer".to_vec();
    let (digest, manifest) = r.push_image("alice/demo", "latest", "pw-alice", &[&layer1, &layer2]).await;

    // The tag and the digest give the same bytes and the same headers.
    let by_tag = r.get("/v2/alice/demo/manifests/latest", "pw-alice").await;
    let by_digest = r.get(&format!("/v2/alice/demo/manifests/{digest}"), "pw-alice").await;
    assert_eq!(by_tag.status, 200);
    assert_eq!(by_tag.body, by_digest.body);
    assert_eq!(by_tag.h("docker-content-digest"), digest);
    assert_eq!(by_tag.h("content-type"), "application/vnd.oci.image.manifest.v1+json");
    assert_eq!(sha(&by_tag.body), digest, "the bytes are returned exactly as pushed");
    assert_eq!(serde_json::from_slice::<Value>(&by_tag.body).unwrap(), manifest);

    // Blobs: HEAD tells the size, GET returns the bytes.
    let layer_digest = sha(&layer1);
    let head = r.req(reqwest::Method::HEAD, &format!("/v2/alice/demo/blobs/{layer_digest}"), "pw-alice", vec![], None).await;
    assert_eq!(head.status, 200);
    assert_eq!(head.h("content-length"), "200000");
    assert_eq!(head.h("docker-content-digest"), layer_digest);
    let blob = r.get(&format!("/v2/alice/demo/blobs/{layer_digest}"), "pw-alice").await;
    assert_eq!(blob.body, layer1);
    let head_m = r.req(reqwest::Method::HEAD, "/v2/alice/demo/manifests/latest", "pw-alice", vec![], None).await;
    assert_eq!((head_m.status, head_m.h("docker-content-digest")), (200, digest.clone()));
    assert!(head_m.body.is_empty());

    // Tags.
    r.push_image("alice/demo", "v1", "pw-alice", &[&layer1, &layer2]).await;
    let tags = r.get("/v2/alice/demo/tags/list", "pw-alice").await.json();
    assert_eq!(tags, json!({"name": "alice/demo", "tags": ["latest", "v1"]}));
    // Events: one per manifest.
    let n: (i64,) =
        sqlx::query_as("SELECT count(*) FROM outbox_events WHERE kind = 'package_published'").fetch_one(&r.env.pool).await.unwrap();
    assert_eq!(n.0, 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn uploads_can_be_monolithic_resumed_and_cancelled(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    // Monolithic: one POST with ?digest=.
    let data = b"all in one request".to_vec();
    let d = sha(&data);
    let one = r
        .req(
            reqwest::Method::POST,
            &format!("/v2/alice/mono/blobs/uploads/?digest={d}"),
            "pw-alice",
            data.clone(),
            Some("application/octet-stream"),
        )
        .await;
    assert_eq!(one.status, 201, "{}", String::from_utf8_lossy(&one.body));
    assert_eq!(r.get(&format!("/v2/alice/mono/blobs/{d}"), "pw-alice").await.body, data);

    // Chunked, with status checks and Content-Range.
    let start = r.req(reqwest::Method::POST, "/v2/alice/chunky/blobs/uploads/", "pw-alice", vec![], None).await;
    let loc = start.h("location");
    assert_eq!(start.h("range"), "0-0");
    let a = r.send(r.c.patch(format!("{}{loc}", r.base)).header("content-range", "0-3").body("abcd"), "pw-alice").await;
    assert_eq!((a.status, a.h("range")), (202, "0-3".to_string()));
    let st = r.get(&loc, "pw-alice").await;
    assert_eq!((st.status, st.h("range")), (204, "0-3".to_string()));
    // A chunk that does not start where the upload stands is refused, and nothing is lost.
    let bad = r.send(r.c.patch(format!("{}{loc}", r.base)).header("content-range", "9-12").body("zzzz"), "pw-alice").await;
    assert_eq!(bad.status, 416);
    let b = r.send(r.c.patch(format!("{}{loc}", r.base)).header("content-range", "4-7").body("efgh"), "pw-alice").await;
    assert_eq!(b.h("range"), "0-7");
    let done = r.req(reqwest::Method::PUT, &format!("{loc}?digest={}", sha(b"abcdefgh")), "pw-alice", vec![], None).await;
    assert_eq!(done.status, 201);

    // Cancelling.
    let s2 = r.req(reqwest::Method::POST, "/v2/alice/chunky/blobs/uploads/", "pw-alice", vec![], None).await;
    let loc2 = s2.h("location");
    assert_eq!(r.req(reqwest::Method::DELETE, &loc2, "pw-alice", vec![], None).await.status, 204);
    assert_eq!(r.get(&loc2, "pw-alice").await.status, 404);
    // Unknown ids are not found.
    assert_eq!(r.get("/v2/alice/chunky/blobs/uploads/not-a-uuid", "pw-alice").await.code(), "BLOB_UPLOAD_UNKNOWN");
    assert_eq!(
        r.get("/v2/alice/chunky/blobs/uploads/00000000-0000-0000-0000-000000000000", "pw-alice").await.code(),
        "BLOB_UPLOAD_UNKNOWN"
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_wrong_digest_stores_nothing(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let start = r.req(reqwest::Method::POST, "/v2/alice/demo/blobs/uploads/", "pw-alice", vec![], None).await;
    let loc = start.h("location");
    let wrong = sha(b"something else");
    let put = r.req(reqwest::Method::PUT, &format!("{loc}?digest={wrong}"), "pw-alice", b"the real bytes".to_vec(), None).await;
    assert_eq!((put.status, put.code()), (400, "DIGEST_INVALID".to_string()));
    assert_eq!(put.json()["errors"][0]["detail"]["computed"], sha(b"the real bytes"));
    assert_eq!(r.get(&format!("/v2/alice/demo/blobs/{wrong}"), "pw-alice").await.status, 404);
    assert_eq!(r.get(&format!("/v2/alice/demo/blobs/{}", sha(b"the real bytes")), "pw-alice").await.status, 404);
    assert_eq!(r.env.count("oci_blobs").await, 0);
    assert_eq!(r.env.count("oci_uploads").await, 0, "the failed upload is cleaned up");
    // A malformed digest is refused before anything is read.
    let start = r.req(reqwest::Method::POST, "/v2/alice/demo/blobs/uploads/", "pw-alice", vec![], None).await;
    for d in ["md5:abc", "sha256:XYZ", "", "sha256:00"] {
        let put = r.req(reqwest::Method::PUT, &format!("{}?digest={d}", start.h("location")), "pw-alice", b"x".to_vec(), None).await;
        assert_eq!(put.code(), "DIGEST_INVALID", "{d:?}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_manifest_must_point_at_what_exists(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let cfg = r.push_blob("alice/demo", "pw-alice", b"{}").await;
    let missing = sha(b"never uploaded");
    let m = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {"mediaType": "x", "digest": cfg, "size": 2}, "layers": [{"mediaType": "y", "digest": missing, "size": 3}]});
    let put = r
        .req(
            reqwest::Method::PUT,
            "/v2/alice/demo/manifests/broken",
            "pw-alice",
            serde_json::to_vec(&m).unwrap(),
            Some("application/vnd.oci.image.manifest.v1+json"),
        )
        .await;
    assert_eq!((put.status, put.code()), (400, "MANIFEST_BLOB_UNKNOWN".to_string()));
    assert_eq!(put.json()["errors"][0]["detail"]["digest"], missing);
    assert_eq!(r.get("/v2/alice/demo/manifests/broken", "pw-alice").await.code(), "MANIFEST_UNKNOWN");
    assert_eq!(r.env.count("oci_manifests").await, 0);
    // Not JSON, wrong media type, wrong digest in the URL.
    for (body, ct) in
        [("nope".as_bytes().to_vec(), "application/vnd.oci.image.manifest.v1+json"), (serde_json::to_vec(&m).unwrap(), "text/plain")]
    {
        let p = r.req(reqwest::Method::PUT, "/v2/alice/demo/manifests/x", "pw-alice", body, Some(ct)).await;
        assert_eq!((p.status, p.code()), (400, "MANIFEST_INVALID".to_string()));
    }
    let ok = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {"mediaType": "x", "digest": cfg, "size": 2}, "layers": []});
    let bytes = serde_json::to_vec(&ok).unwrap();
    let wrong = r
        .req(
            reqwest::Method::PUT,
            &format!("/v2/alice/demo/manifests/{}", sha(b"other")),
            "pw-alice",
            bytes.clone(),
            Some("application/vnd.oci.image.manifest.v1+json"),
        )
        .await;
    assert_eq!(wrong.code(), "DIGEST_INVALID");
    let bad_ref = r
        .req(reqwest::Method::PUT, "/v2/alice/demo/manifests/-no", "pw-alice", bytes, Some("application/vnd.oci.image.manifest.v1+json"))
        .await;
    assert_eq!(bad_ref.code(), "MANIFEST_INVALID");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_index_needs_its_children(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let (child, _) = r.push_image("alice/multi", "amd64", "pw-alice", &[b"layer"]).await;
    let index = |digest: &str| {
        serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest, "size": 1, "platform": {"architecture": "amd64", "os": "linux"}}]})).unwrap()
    };
    let missing = r
        .req(
            reqwest::Method::PUT,
            "/v2/alice/multi/manifests/latest",
            "pw-alice",
            index(&sha(b"ghost")),
            Some("application/vnd.oci.image.index.v1+json"),
        )
        .await;
    assert_eq!(missing.code(), "MANIFEST_BLOB_UNKNOWN");
    let ok = r
        .req(
            reqwest::Method::PUT,
            "/v2/alice/multi/manifests/latest",
            "pw-alice",
            index(&child),
            Some("application/vnd.oci.image.index.v1+json"),
        )
        .await;
    assert_eq!(ok.status, 201);
    let got = r.get("/v2/alice/multi/manifests/latest", "pw-alice").await;
    assert_eq!(got.h("content-type"), "application/vnd.oci.image.index.v1+json");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn identical_layers_are_stored_once_and_can_be_mounted(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let layer = vec![42u8; 5000];
    r.push_image("alice/one", "latest", "pw-alice", &[&layer]).await;
    r.push_image("alice/two", "latest", "pw-alice", &[&layer]).await;
    let files = std::fs::read_dir(r.state.config.blobs_dir().join("sha256")).unwrap().count();
    let (stored,): (i64,) = sqlx::query_as("SELECT count(*) FROM blobs WHERE sha256 = $1")
        .bind(hex::encode(Sha256::digest(&layer)))
        .fetch_one(&r.env.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1, "one blob row for the shared layer");
    let (refs,): (i32,) = sqlx::query_as("SELECT refcount FROM blobs WHERE sha256 = $1")
        .bind(hex::encode(Sha256::digest(&layer)))
        .fetch_one(&r.env.pool)
        .await
        .unwrap();
    assert_eq!(refs, 2, "two images hold it");
    assert!(files >= 1);

    // Mount into a third image with no bytes sent.
    let d = sha(&layer);
    let m =
        r.req(reqwest::Method::POST, &format!("/v2/alice/three/blobs/uploads/?mount={d}&from=alice/one"), "pw-alice", vec![], None).await;
    assert_eq!(m.status, 201, "{}", String::from_utf8_lossy(&m.body));
    assert_eq!(m.h("location"), format!("/v2/alice/three/blobs/{d}"));
    assert_eq!(r.get(&format!("/v2/alice/three/blobs/{d}"), "pw-alice").await.body, layer);
    // Mounting from an image the caller cannot read quietly becomes a normal upload session.
    r.env.user("bob").await;
    let fallback =
        r.req(reqwest::Method::POST, &format!("/v2/bob/other/blobs/uploads/?mount={d}&from=alice/one"), "pw-bob", vec![], None).await;
    assert_eq!(fallback.status, 202);
    assert_eq!(r.get(&format!("/v2/bob/other/blobs/{d}"), "pw-bob").await.status, 404);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn who_may_pull_and_push(pool: PgPool) {
    let (r, alice) = world(pool).await;
    let bob = r.env.user("bob").await;
    r.push_image("alice/private", "latest", "pw-alice", &[b"l"]).await;
    // Bob cannot see it, and cannot tell it from one that does not exist.
    let hidden = r.get("/v2/alice/private/manifests/latest", "pw-bob").await;
    let missing = r.get("/v2/alice/nothing/manifests/latest", "pw-bob").await;
    assert_eq!((hidden.status, hidden.code()), (404, "NAME_UNKNOWN".to_string()));
    assert_eq!((missing.status, missing.code()), (404, "NAME_UNKNOWN".to_string()));
    assert_eq!(hidden.body, missing.body);
    // Nor push to Alice's namespace, whether the image exists or not.
    for name in ["alice/private", "alice/brand-new"] {
        let p = r.req(reqwest::Method::POST, &format!("/v2/{name}/blobs/uploads/"), "pw-bob", vec![], None).await;
        assert!(p.status == 403 || p.status == 404, "{name}: {}", p.status);
        assert_ne!(p.status, 202);
    }
    // Bob's own namespace is his.
    assert_eq!(r.req(reqwest::Method::POST, "/v2/bob/mine/blobs/uploads/", "pw-bob", vec![], None).await.status, 202);

    // A repository with the same name links the image to it, and its permissions apply.
    let repo = r.env.public_repo(alice, "shared").await;
    r.push_image("alice/shared", "latest", "pw-alice", &[b"s"]).await;
    let linked: (Option<i64>,) = sqlx::query_as("SELECT repo_id FROM packages WHERE name = 'shared'").fetch_one(&r.env.pool).await.unwrap();
    assert_eq!(linked.0, Some(repo.0));
    assert_eq!(r.get("/v2/alice/shared/manifests/latest", "pw-bob").await.status, 200, "public repository: any account may pull");
    let denied = r.req(reqwest::Method::POST, "/v2/alice/shared/blobs/uploads/", "pw-bob", vec![], None).await;
    assert_eq!(denied.status, 403, "read access is not push access");
    r.env.collaborator(repo, bob, "write").await;
    assert_eq!(
        r.req(reqwest::Method::POST, "/v2/alice/shared/blobs/uploads/", "pw-bob", vec![], None).await.status,
        202,
        "a writer may push"
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn tokens_need_the_packages_scope(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    r.push_image("alice/demo", "latest", "pkg-alice", &[b"l"]).await;
    // A read-only package token pulls but cannot push.
    assert_eq!(r.get("/v2/alice/demo/manifests/latest", "pull-alice").await.status, 200);
    let p = r.req(reqwest::Method::POST, "/v2/alice/demo/blobs/uploads/", "pull-alice", vec![], None).await;
    assert_eq!((p.status, p.code()), (403, "DENIED".to_string()));
    // A token made for repositories is not a package token at all.
    assert_eq!(r.get("/v2/alice/demo/manifests/latest", "repo-alice").await.status, 403);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_person_cannot_finish_anothers_upload(pool: PgPool) {
    let (r, alice) = world(pool).await;
    let bob = r.env.user("bob").await;
    let repo = r.env.public_repo(alice, "team").await;
    r.env.collaborator(repo, bob, "write").await;
    r.push_blob("alice/team", "pw-alice", b"seed").await;
    let start = r.req(reqwest::Method::POST, "/v2/alice/team/blobs/uploads/", "pw-alice", vec![], None).await;
    let loc = start.h("location");
    // Bob may push to this image, but this upload is Alice's.
    for method in [reqwest::Method::PATCH, reqwest::Method::GET, reqwest::Method::DELETE] {
        let t = r.req(method, &loc, "pw-bob", b"x".to_vec(), None).await;
        assert_eq!((t.status, t.code()), (404, "BLOB_UPLOAD_UNKNOWN".to_string()));
    }
    let put = r.req(reqwest::Method::PUT, &format!("{loc}?digest={}", sha(b"x")), "pw-bob", b"x".to_vec(), None).await;
    assert_eq!(put.status, 404);
    // And an upload id from one image does not work under another.
    r.push_blob("alice/other", "pw-alice", b"seed").await;
    assert_eq!(r.get(&loc.replace("alice/team", "alice/other"), "pw-alice").await.status, 404);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn names_are_validated(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    for name in ["alice/Demo", "alice", "alice/a/b", "alice/-x", "alice/x_", "-alice/x"] {
        let g = r.get(&format!("/v2/{name}/tags/list"), "pw-alice").await;
        assert!(g.status == 400 || g.status == 404, "{name}: {}", g.status);
        assert!(g.status != 200 && g.status != 202);
    }
    assert_eq!(r.get("/v2/alice/Demo/tags/list", "pw-alice").await.code(), "NAME_INVALID");
    assert_eq!(r.get("/v2/nobody/demo/tags/list", "pw-alice").await.code(), "NAME_UNKNOWN");
    assert_eq!(r.get("/v2/alice/demo/tags/list", "pw-alice").await.code(), "NAME_UNKNOWN", "no image yet");
    assert_eq!(r.get("/v2/alice/demo/unknown/thing", "pw-alice").await.status, 404);
    // Nothing was created by looking.
    assert_eq!(r.env.count("packages").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn tags_page_in_byte_order(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let (_, manifest) = r.push_image("alice/demo", "a", "pw-alice", &[b"l"]).await;
    let bytes = serde_json::to_vec(&manifest).unwrap();
    for t in ["b", "c", "B", "v1.0", "v1.10", "v1.2"] {
        let p = r
            .req(
                reqwest::Method::PUT,
                &format!("/v2/alice/demo/manifests/{t}"),
                "pw-alice",
                bytes.clone(),
                Some("application/vnd.oci.image.manifest.v1+json"),
            )
            .await;
        assert_eq!(p.status, 201);
    }
    let first = r.get("/v2/alice/demo/tags/list?n=3", "pw-alice").await;
    assert_eq!(first.json()["tags"], json!(["B", "a", "b"]), "uppercase sorts before lowercase in byte order");
    let link = first.h("link");
    assert!(link.contains("last=b") && link.contains("rel=\"next\""), "{link}");
    let second = r.get("/v2/alice/demo/tags/list?n=3&last=b", "pw-alice").await;
    assert_eq!(second.json()["tags"], json!(["c", "v1.0", "v1.10"]));
    let third = r.get("/v2/alice/demo/tags/list?n=3&last=v1.10", "pw-alice").await;
    assert_eq!(third.json()["tags"], json!(["v1.2"]));
    assert_eq!(third.h("link"), "");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_manifests_tags_and_blobs(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let (digest, _) = r.push_image("alice/demo", "latest", "pw-alice", &[b"layer"]).await;
    // A tag goes without the manifest.
    assert_eq!(r.req(reqwest::Method::DELETE, "/v2/alice/demo/manifests/latest", "pw-alice", vec![], None).await.status, 202);
    assert_eq!(r.get("/v2/alice/demo/manifests/latest", "pw-alice").await.code(), "MANIFEST_UNKNOWN");
    assert_eq!(r.get(&format!("/v2/alice/demo/manifests/{digest}"), "pw-alice").await.status, 200, "still there by digest");
    // Deleting by digest removes it, and the tags that named it.
    r.push_image("alice/demo", "again", "pw-alice", &[b"layer"]).await;
    assert_eq!(r.req(reqwest::Method::DELETE, &format!("/v2/alice/demo/manifests/{digest}"), "pw-alice", vec![], None).await.status, 202);
    assert_eq!(r.get("/v2/alice/demo/tags/list", "pw-alice").await.json()["tags"], json!([]));
    assert_eq!(r.req(reqwest::Method::DELETE, &format!("/v2/alice/demo/manifests/{digest}"), "pw-alice", vec![], None).await.status, 404);
    // Blobs.
    let layer = sha(b"layer");
    assert_eq!(r.req(reqwest::Method::DELETE, &format!("/v2/alice/demo/blobs/{layer}"), "pw-alice", vec![], None).await.status, 202);
    assert_eq!(r.get(&format!("/v2/alice/demo/blobs/{layer}"), "pw-alice").await.status, 404);
    assert_eq!(r.req(reqwest::Method::DELETE, &format!("/v2/alice/demo/blobs/{layer}"), "pw-alice", vec![], None).await.status, 404);
    // Wrong methods.
    assert_eq!(r.req(reqwest::Method::POST, &format!("/v2/alice/demo/manifests/{digest}"), "pw-alice", vec![], None).await.status, 405);
    assert_eq!(r.req(reqwest::Method::PUT, "/v2/alice/demo/tags/list", "pw-alice", vec![], None).await.status, 405);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn referrers_lists_manifests_naming_a_subject(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let (subject, _) = r.push_image("alice/demo", "latest", "pw-alice", &[b"l"]).await;
    let cfg = r.push_blob("alice/demo", "pw-alice", b"{\"sig\":1}").await;
    let sig = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "artifactType": "application/example.sig", "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": cfg, "size": 9}, "layers": [], "subject": {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": subject, "size": 1}});
    let put = r
        .req(
            reqwest::Method::PUT,
            "/v2/alice/demo/manifests/sig",
            "pw-alice",
            serde_json::to_vec(&sig).unwrap(),
            Some("application/vnd.oci.image.manifest.v1+json"),
        )
        .await;
    assert_eq!(put.status, 201);
    assert_eq!(put.h("oci-subject"), subject);
    let refs = r.get(&format!("/v2/alice/demo/referrers/{subject}"), "pw-alice").await.json();
    assert_eq!(refs["manifests"].as_array().unwrap().len(), 1);
    assert_eq!(refs["manifests"][0]["artifactType"], "application/example.sig");
    assert_eq!(r.get(&format!("/v2/alice/demo/referrers/{}", sha(b"nothing")), "pw-alice").await.json()["manifests"], json!([]));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn garbage_collection_keeps_what_is_used_and_respects_grace(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    r.push_image("alice/demo", "keep", "pw-alice", &[b"kept layer"]).await;
    let (orphan, _) = r.push_image("alice/demo", "temp", "pw-alice", &[b"orphan layer"]).await;
    r.req(reqwest::Method::DELETE, "/v2/alice/demo/manifests/temp", "pw-alice", vec![], None).await;
    // An abandoned upload.
    let start = r.req(reqwest::Method::POST, "/v2/alice/demo/blobs/uploads/", "pw-alice", b"".to_vec(), None).await;
    let tmp = start.h("location");
    // Everything is fresh, so nothing goes.
    let s = ch_registry::gc::sweep(&r.state).await.unwrap();
    assert_eq!(s, ch_registry::gc::Swept::default(), "inside the grace period nothing is collected");
    assert_eq!(r.get(&tmp, "pw-alice").await.status, 204);

    // Age everything past every grace period.
    sqlx::query("UPDATE oci_uploads SET updated_at = now() - interval '2 days'").execute(&r.env.pool).await.unwrap();
    sqlx::query("UPDATE oci_manifests SET created_at = now() - interval '30 days'").execute(&r.env.pool).await.unwrap();
    sqlx::query("UPDATE oci_blobs SET created_at = now() - interval '3 days'").execute(&r.env.pool).await.unwrap();
    let s = ch_registry::gc::sweep(&r.state).await.unwrap();
    assert_eq!(s.uploads, 1);
    assert_eq!(s.manifests, 1, "the untagged manifest");
    assert_eq!(s.blobs, 1, "the orphan layer; the config is shared with the kept image and stays");
    assert_eq!(r.get(&tmp, "pw-alice").await.status, 404);
    assert_eq!(r.get(&format!("/v2/alice/demo/manifests/{orphan}"), "pw-alice").await.status, 404);
    assert_eq!(r.get(&format!("/v2/alice/demo/blobs/{}", sha(b"orphan layer")), "pw-alice").await.status, 404);
    // What the tagged image uses is untouched.
    assert_eq!(r.get("/v2/alice/demo/manifests/keep", "pw-alice").await.status, 200);
    assert_eq!(r.get(&format!("/v2/alice/demo/blobs/{}", sha(b"kept layer")), "pw-alice").await.status, 200);

    // The stored file goes once the blob store's own grace passes.
    let hex = hex::encode(Sha256::digest(b"orphan layer"));
    let path = r.state.config.blobs_dir().join("sha256").join(&hex[..2]).join(&hex[2..4]).join(&hex);
    assert!(path.exists());
    sqlx::query("UPDATE blobs SET unreferenced_since = now() - interval '3 days' WHERE refcount = 0").execute(&r.env.pool).await.unwrap();
    let s = ch_registry::gc::sweep(&r.state).await.unwrap();
    assert!(s.stored_files >= 1);
    assert!(!path.exists(), "the file was removed");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_handler_reached_without_a_caller_fails_closed(pool: PgPool) {
    let env = TestEnv::new(pool.clone()).await;
    let b = env.builder().await;
    let state = b.build();
    // No gate at all: the extractor sees `Anonymous`.
    let app = Router::new().merge(ch_registry::registry_routes()).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let c = reqwest::Client::new();
    for (m, p) in [("GET", "/v2/a/b/manifests/x"), ("POST", "/v2/a/b/blobs/uploads/"), ("PUT", "/v2/a/b/manifests/x"), ("GET", "/v2/token")]
    {
        let r = c.request(m.parse().unwrap(), format!("http://{addr}{p}")).send().await.unwrap();
        assert_eq!(r.status(), 403, "{m} {p}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn package_tokens_work_on_linked_images_and_deleting_the_repository_narrows(pool: PgPool) {
    // ch-ctk.12: a packages scope gives package access on the repositories the person may use,
    // with no `repo` scope. Deleting the repository leaves the image to its owner alone.
    let (r, alice) = world(pool).await;
    let bob = r.env.user("bob").await;
    let carol = r.env.user("carol").await;
    let org = r.env.org("crew").await;
    for (u, role) in [(alice, "owner"), (carol, "member")] {
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org.0)
            .bind(u.0)
            .bind(role)
            .execute(&r.env.pool)
            .await
            .unwrap();
    }
    let repo = r.env.repo(org, "app").await;
    r.env.collaborator(repo, alice, "admin").await;
    r.env.collaborator(repo, bob, "write").await;
    r.push_image("crew/app", "latest", "pkg-alice", &[b"a"]).await;
    let linked: (Option<i64>,) = sqlx::query_as("SELECT repo_id FROM packages WHERE name = 'app'").fetch_one(&r.env.pool).await.unwrap();
    assert_eq!(linked.0, Some(repo.0), "linked on first push by a writer with a packages token");
    r.push_image("crew/app", "v2", "pkg-bob", &[b"b"]).await;
    assert_eq!(r.get("/v2/crew/app/manifests/latest", "pull-bob").await.status, 200, "read:packages pulls a linked private image");
    let p = r.req(reqwest::Method::POST, "/v2/crew/app/blobs/uploads/", "pull-bob", vec![], None).await;
    assert_eq!(p.status, 403, "read:packages never pushes");
    // Carol is in the organization but has no access to the private repository.
    assert_eq!(r.get("/v2/crew/app/manifests/latest", "pw-carol").await.code(), "NAME_UNKNOWN");
    sqlx::query("DELETE FROM repositories WHERE id = $1").bind(repo.0).execute(&r.env.pool).await.unwrap();
    assert_eq!(
        r.get("/v2/crew/app/manifests/latest", "pw-carol").await.code(),
        "NAME_UNKNOWN",
        "an orphan does not open up to every member"
    );
    assert_eq!(r.get("/v2/crew/app/manifests/latest", "pull-bob").await.code(), "NAME_UNKNOWN");
    assert_eq!(r.get("/v2/crew/app/manifests/latest", "pw-alice").await.status, 200, "the organization's owner keeps it");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_to_someone_elses_account_answers_like_a_hidden_image(pool: PgPool) {
    // ch-x0b: an image you cannot read and one that does not exist give one answer to a push.
    let (r, _alice) = world(pool).await;
    r.env.user("bob").await;
    r.push_image("alice/private", "latest", "pw-alice", &[b"l"]).await;
    let hidden = r.req(reqwest::Method::POST, "/v2/alice/private/blobs/uploads/", "pw-bob", vec![], None).await;
    let missing = r.req(reqwest::Method::POST, "/v2/alice/brand-new/blobs/uploads/", "pw-bob", vec![], None).await;
    let no_owner = r.req(reqwest::Method::POST, "/v2/nobody/brand-new/blobs/uploads/", "pw-bob", vec![], None).await;
    assert_eq!((hidden.status, hidden.code()), (404, "NAME_UNKNOWN".to_string()));
    assert_eq!((missing.status, missing.body.clone()), (hidden.status, hidden.body.clone()));
    assert_eq!((no_owner.status, no_owner.body), (hidden.status, hidden.body));
}

// ---- Range requests on blobs -------------------------------------------------------------

async fn ranged(r: &Reg, path: &str, range: &str) -> Resp {
    r.send(r.c.get(format!("{}{path}", r.base)).header("range", range), "pw-alice").await
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_single_range_returns_206_with_just_those_bytes(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    // Large enough to arrive in several chunks, with bytes that tell their position apart.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (_, _) = r.push_image("alice/demo", "latest", "pw-alice", &[&data]).await;
    let d = sha(&data);
    let path = format!("/v2/alice/demo/blobs/{d}");

    let whole = r.get(&path, "pw-alice").await;
    assert_eq!((whole.status, whole.h("accept-ranges")), (200, "bytes".to_string()));

    let mid = ranged(&r, &path, "bytes=100000-100099").await;
    assert_eq!(mid.status, 206);
    assert_eq!(mid.body, &data[100_000..100_100]);
    assert_eq!(mid.h("content-range"), "bytes 100000-100099/300000");
    assert_eq!(mid.h("content-length"), "100");
    assert_eq!(mid.h("docker-content-digest"), d, "the digest header names the whole blob");

    let open_end = ranged(&r, &path, "bytes=299990-").await;
    assert_eq!((open_end.status, open_end.body.as_slice()), (206, &data[299_990..]));
    assert_eq!(open_end.h("content-range"), "bytes 299990-299999/300000");

    let suffix = ranged(&r, &path, "bytes=-5").await;
    assert_eq!((suffix.status, suffix.body.as_slice()), (206, &data[299_995..]));

    let clamped = ranged(&r, &path, "bytes=299998-999999").await;
    assert_eq!((clamped.status, clamped.h("content-range")), (206, "bytes 299998-299999/300000".to_string()));

    let first = ranged(&r, &path, "bytes=0-0").await;
    assert_eq!((first.status, first.body.as_slice()), (206, &data[..1]));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_range_past_the_end_is_416_and_bad_syntax_is_ignored(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let data = vec![9u8; 1000];
    r.push_image("alice/demo", "latest", "pw-alice", &[&data]).await;
    let d = sha(&data);
    let path = format!("/v2/alice/demo/blobs/{d}");

    for h in ["bytes=1000-", "bytes=5000-6000", "bytes=-0"] {
        let t = ranged(&r, &path, h).await;
        assert_eq!(t.status, 416, "{h}");
        assert_eq!(t.h("content-range"), "bytes */1000", "{h}");
        assert_eq!(t.h("docker-content-digest"), d);
    }
    // Not one well formed range: ignored, the whole blob comes back with 200.
    for h in ["bytes=5-2", "bytes=0-9,20-29", "items=0-9", "bytes=a-b", "garbage", "bytes=-"] {
        let t = ranged(&r, &path, h).await;
        assert_eq!((t.status, t.body.len()), (200, 1000), "{h}");
        assert!(t.h("content-range").is_empty(), "{h}");
    }
    // HEAD ignores Range and reports the whole size.
    let head = r.send(r.c.head(format!("{}{path}", r.base)).header("range", "bytes=0-9"), "pw-alice").await;
    assert_eq!((head.status, head.h("content-length")), (200, "1000".to_string()));
    // A range does not get around access: a stranger still sees nothing.
    r.env.user("bob").await;
    let hidden = r.send(r.c.get(format!("{}{path}", r.base)).header("range", "bytes=0-9"), "pw-bob").await;
    assert_eq!(hidden.status, 404);
}

// ---- Catalog -----------------------------------------------------------------------------

async fn catalog(r: &Reg, query: &str, secret: &str) -> Resp {
    r.get(&format!("/v2/_catalog{query}"), secret).await
}

fn repos(resp: &Resp) -> Vec<String> {
    resp.json()["repositories"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_catalog_lists_only_what_the_caller_can_read(pool: PgPool) {
    let (r, alice) = world(pool).await;
    let bob = r.env.user("bob").await;
    r.env.user("carol").await;
    // Alice: one private image, one linked to a public repository, one linked to a private
    // repository Bob collaborates on.
    r.push_image("alice/private", "latest", "pw-alice", &[b"p"]).await;
    r.env.public_repo(alice, "open").await;
    r.push_image("alice/open", "latest", "pw-alice", &[b"o"]).await;
    let team = r.env.repo(alice, "team").await;
    r.push_image("alice/team", "latest", "pw-alice", &[b"t"]).await;
    r.env.collaborator(team, bob, "read").await;
    r.push_image("bob/mine", "latest", "pw-bob", &[b"b"]).await;
    // An organization: alice owns, bob is a member, carol is outside.
    let acme = r.env.org("acme").await;
    for (who, role) in [(alice, "owner"), (bob, "member")] {
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(acme.0)
            .bind(who.0)
            .bind(role)
            .execute(&r.env.pool)
            .await
            .unwrap();
    }
    r.push_image("acme/tool", "latest", "pw-alice", &[b"a"]).await;

    let a = catalog(&r, "", "pw-alice").await;
    assert_eq!(a.status, 200);
    assert_eq!(a.h("content-type"), "application/json");
    assert_eq!(repos(&a), ["acme/tool", "alice/open", "alice/private", "alice/team"]);
    assert_eq!(repos(&catalog(&r, "", "pw-bob").await), ["acme/tool", "alice/open", "alice/team", "bob/mine"]);
    assert_eq!(repos(&catalog(&r, "", "pw-carol").await), ["alice/open"], "only the public repository's image");
    // A read-only package token sees what the account sees.
    assert_eq!(repos(&catalog(&r, "", "pull-bob").await), ["acme/tool", "alice/open", "alice/team", "bob/mine"]);
    // A token without a packages scope lists nothing and is told why.
    let no_scope = catalog(&r, "", "repo-alice").await;
    assert_eq!((no_scope.status, no_scope.code()), (403, "DENIED".to_string()));
    // Signed out: the gate answers 401 before the handler runs.
    let anon = r.c.get(format!("{}/v2/_catalog", r.base)).send().await.unwrap();
    assert_eq!(anon.status(), 401);
    assert!(anon.headers().contains_key(header::WWW_AUTHENTICATE));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_catalog_agrees_with_what_a_pull_allows(pool: PgPool) {
    let (r, alice) = world(pool).await;
    let bob = r.env.user("bob").await;
    r.push_image("alice/a-private", "latest", "pw-alice", &[b"1"]).await;
    r.env.public_repo(alice, "b-open").await;
    r.push_image("alice/b-open", "latest", "pw-alice", &[b"2"]).await;
    let c = r.env.repo(alice, "c-shared").await;
    r.push_image("alice/c-shared", "latest", "pw-alice", &[b"3"]).await;
    r.env.collaborator(c, bob, "write").await;
    // A repository that was linked and then made private again: Bob must not see it listed.
    let d = r.env.public_repo(alice, "d-flipped").await;
    r.push_image("alice/d-flipped", "latest", "pw-alice", &[b"4"]).await;
    sqlx::query("UPDATE repositories SET visibility = 'private' WHERE id = $1").bind(d.0).execute(&r.env.pool).await.unwrap();

    let listed = repos(&catalog(&r, "", "pw-bob").await);
    for name in ["alice/a-private", "alice/b-open", "alice/c-shared", "alice/d-flipped"] {
        let pull = r.get(&format!("/v2/{name}/tags/list"), "pw-bob").await.status == 200;
        assert_eq!(listed.contains(&name.to_string()), pull, "{name}: catalog and pull disagree");
    }
    assert_eq!(listed, ["alice/b-open", "alice/c-shared"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_catalog_pages_with_n_and_last(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    for name in ["alice/e", "alice/b", "alice/d", "alice/a", "alice/c"] {
        r.push_image(name, "latest", "pw-alice", &[name.as_bytes()]).await;
    }
    let p1 = catalog(&r, "?n=2", "pw-alice").await;
    assert_eq!(repos(&p1), ["alice/a", "alice/b"]);
    assert_eq!(p1.h("link"), "</v2/_catalog?n=2&last=alice/b>; rel=\"next\"");
    let p2 = catalog(&r, "?n=2&last=alice/b", "pw-alice").await;
    assert_eq!(repos(&p2), ["alice/c", "alice/d"]);
    let p3 = catalog(&r, "?n=2&last=alice/d", "pw-alice").await;
    assert_eq!(repos(&p3), ["alice/e"]);
    assert_eq!(p3.h("link"), "", "the last page has no next link");
    // A page that fits exactly has no next link either.
    assert_eq!(catalog(&r, "?n=5", "pw-alice").await.h("link"), "");
    // n=0 is an empty list; a bad n is a 400, not a silent default.
    let zero = catalog(&r, "?n=0", "pw-alice").await;
    assert_eq!((zero.status, repos(&zero).len(), zero.h("link")), (200, 0, String::new()));
    assert_eq!(catalog(&r, "?n=x", "pw-alice").await.status, 400);
    assert_eq!(catalog(&r, "?n=-1", "pw-alice").await.status, 400);
    // After the last name: empty.
    assert!(repos(&catalog(&r, "?last=alice/z", "pw-alice").await).is_empty());
    // Only GET and HEAD.
    let post = r.req(reqwest::Method::POST, "/v2/_catalog", "pw-alice", vec![], None).await;
    assert_eq!(post.status, 405);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_caller_who_sees_little_gets_a_short_page_and_a_cursor_that_names_nothing_hidden(pool: PgPool) {
    use ch_registry::catalog::{Cursor, Limits, page};
    let (r, _alice) = world(pool).await;
    let bob = r.env.user("bob").await;
    for name in ["alice/a", "alice/b", "alice/c", "alice/d"] {
        r.push_image(name, "latest", "pw-alice", &[name.as_bytes()]).await;
    }
    r.push_image("bob/z", "latest", "pw-bob", &[b"z"]).await;
    let actor = Actor::User { id: bob, is_admin: false, has_2fa: false, via: AuthVia::Basic };
    let start = Cursor::default();
    // Reading two rows at a time and giving up after three: Bob can read none of Alice's four.
    let tight = Limits { batch: 2, max_scan: 3 };
    let (names, next) = page(&r.state, &actor, 10, &start, tight).await.unwrap();
    assert!(names.is_empty());
    let next = next.expect("more to read");
    // The cursor says how far to skip, never the name of a row Bob cannot pull.
    assert_eq!(next, Cursor { last: String::new(), skip: 3 });
    assert!(!next.last.contains("alice"), "a cursor must not carry a hidden name");
    let (names, next) = page(&r.state, &actor, 10, &Cursor { last: String::new(), skip: 3 }, tight).await.unwrap();
    assert_eq!((names, next), (vec!["bob/z".to_string()], None));
    // With room to read, the same call finds it in one go across several batches.
    let roomy = Limits { batch: 2, max_scan: 100 };
    let (names, next) = page(&r.state, &actor, 10, &start, roomy).await.unwrap();
    assert_eq!((names, next), (vec!["bob/z".to_string()], None));
    // Budget spent after a readable name: the cursor is that readable name plus what was skipped since.
    r.push_image("bob/zz", "latest", "pw-bob", &[b"zz"]).await;
    let (names, next) = page(&r.state, &actor, 10, &start, Limits { batch: 2, max_scan: 5 }).await.unwrap();
    assert_eq!(names, ["bob/z"]);
    assert_eq!(next, Some(Cursor { last: "bob/z".into(), skip: 0 }));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_next_link_never_carries_a_hidden_name(pool: PgPool) {
    use ch_registry::catalog::{Cursor, link};
    let (r, _alice) = world(pool).await;
    let _ = r;
    assert_eq!(link(2, &Cursor { last: "alice/b".into(), skip: 0 }, "ignored"), "</v2/_catalog?n=2&last=alice/b>; rel=\"next\"");
    assert_eq!(link(2, &Cursor { last: String::new(), skip: 5000 }, "5000.abc"), "</v2/_catalog?n=2&last=&skip=5000.abc>; rel=\"next\"");
    // A `last` that is not an image name is encoded, so the link means what the server meant.
    assert_eq!(link(2, &Cursor { last: "a&b+c d".into(), skip: 0 }, "ignored"), "</v2/_catalog?n=2&last=a%26b%2Bc%20d>; rel=\"next\"");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_catalog_and_tag_cursors_are_400_not_500(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    r.push_image("alice/demo", "latest", "pw-alice", &[b"l"]).await;
    for q in [
        "?last=%00",
        "?last=a%00b",
        "?skip=-1",
        "?skip=x",
        "?skip=99999999999",
        "?skip=%00",
        "?skip=1",
        "?skip=00",
        "?skip=01.abc",
        "?skip=3.",
        "?skip=3.00",
        "?skip=3.0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        assert_eq!(catalog(&r, q, "pw-alice").await.status, 400, "{q}");
    }
    // A bare zero skips nothing and is accepted.
    assert_eq!(catalog(&r, "?skip=0", "pw-alice").await.status, 200);
    assert_eq!(r.get("/v2/alice/demo/tags/list?last=%00", "pw-alice").await.status, 400);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_skip_the_server_issued_works_and_only_for_that_caller(pool: PgPool) {
    let (r, alice) = world(pool).await;
    r.env.user("bob").await;
    r.env.user("carol").await;
    // Pushing 1100 images is too slow; the rows carry no repository, so bob can read none of
    // alice's and the catalog hides every one of them from him.
    for i in 0..1100 {
        sqlx::query("INSERT INTO packages (owner_id, kind, name) VALUES ($1, 'container', $2)")
            .bind(alice.0)
            .bind(format!("a{i:04}"))
            .execute(&r.env.pool)
            .await
            .unwrap();
    }
    r.push_image("bob/z", "latest", "pw-bob", &[b"z"]).await;

    // The default scan budget runs out on alice's rows first: an empty page, but more to read.
    let c = catalog(&r, "", "pw-bob").await;
    assert_eq!(c.status, 200);
    assert!(repos(&c).is_empty(), "the scan budget runs out on alice's rows first");
    let l = c.h("link");
    assert!(!l.is_empty(), "a page that stopped early has a next link");
    // The server's link is the only place a skip comes from, and it is signed.
    assert!(l.starts_with("</v2/_catalog?n=1000&last=&skip=1000."), "{l}");
    assert!(l.ends_with(">; rel=\"next\""), "{l}");
    let url = l.strip_prefix('<').unwrap().split_once('>').unwrap().0;

    // The server's own link works, for the caller it was issued to.
    let next = r.get(url, "pw-bob").await;
    assert_eq!(next.status, 200);
    assert_eq!(repos(&next), ["bob/z"]);
    assert_eq!(next.h("link"), "", "bob has seen everything he can");
    // Issued for bob does not mean usable by carol.
    assert_eq!(r.get(url, "pw-carol").await.status, 400);
    // The signature covers the whole cursor: a changed last or a changed count refuses.
    assert_eq!(r.get(&url.replace("last=&", "last=a0005&"), "pw-bob").await.status, 400);
    assert_eq!(r.get(&url.replace("skip=1000.", "skip=999."), "pw-bob").await.status, 400);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn hidden_rows_of_one_organization_cost_one_permission_lookup_not_one_each(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    r.env.user("bob").await;
    let acme = r.env.org("acme").await;
    for i in 0..30 {
        sqlx::query("INSERT INTO packages (owner_id, kind, name) VALUES ($1, 'container', $2)")
            .bind(acme.0)
            .bind(format!("app{i:02}"))
            .execute(&r.env.pool)
            .await
            .unwrap();
    }
    let before = r.org_role_calls.load(std::sync::atomic::Ordering::SeqCst);
    let c = catalog(&r, "", "pw-bob").await;
    assert!(repos(&c).is_empty(), "Bob is not a member of acme");
    let calls = r.org_role_calls.load(std::sync::atomic::Ordering::SeqCst) - before;
    assert!(calls <= 1, "{calls} organization lookups for 30 rows of one organization");
}

// ---- Found by the OCI conformance suite --------------------------------------------------

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_reference_that_cannot_exist_is_not_found_on_read_and_delete_but_invalid_on_write(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    r.push_image("alice/demo", "latest", "pw-alice", &[b"l"]).await;
    // The conformance suite asks for ".INVALID_MANIFEST_NAME", which is not a legal tag.
    for method in [reqwest::Method::GET, reqwest::Method::HEAD, reqwest::Method::DELETE] {
        let t = r.req(method.clone(), "/v2/alice/demo/manifests/.INVALID_MANIFEST_NAME", "pw-alice", vec![], None).await;
        assert_eq!(t.status, 404, "{method}");
        if method != reqwest::Method::HEAD {
            assert_eq!(t.code(), "MANIFEST_UNKNOWN", "{method}");
        }
    }
    // Writing under it is still refused as invalid.
    let put = r
        .req(
            reqwest::Method::PUT,
            "/v2/alice/demo/manifests/.INVALID_MANIFEST_NAME",
            "pw-alice",
            b"{}".to_vec(),
            Some("application/vnd.oci.image.manifest.v1+json"),
        )
        .await;
    assert_eq!((put.status, put.code()), (400, "MANIFEST_INVALID".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn referrers_carry_annotations_and_an_artifact_type_and_can_be_filtered(pool: PgPool) {
    let (r, _alice) = world(pool).await;
    let (subject, _) = r.push_image("alice/demo", "latest", "pw-alice", &[b"l"]).await;
    let cfg = r.push_blob("alice/demo", "pw-alice", b"{\"sig\":1}").await;
    let subj = json!({"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": subject, "size": 1});
    // One names its type with artifactType; the other only through its config media type.
    let by_field = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "artifactType": "application/example.sig",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": cfg, "size": 9}, "layers": [], "subject": subj,
        "annotations": {"note": "by field"}});
    let by_config = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/example.sbom", "digest": cfg, "size": 9}, "layers": [], "subject": subj,
        "annotations": {"note": "by config"}});
    for (tag, m) in [("a", &by_field), ("b", &by_config)] {
        let put = r
            .req(
                reqwest::Method::PUT,
                &format!("/v2/alice/demo/manifests/{tag}"),
                "pw-alice",
                serde_json::to_vec(m).unwrap(),
                Some("application/vnd.oci.image.manifest.v1+json"),
            )
            .await;
        assert_eq!(put.status, 201, "{}", String::from_utf8_lossy(&put.body));
    }
    let all = r.get(&format!("/v2/alice/demo/referrers/{subject}"), "pw-alice").await;
    assert_eq!(all.h("content-type"), "application/vnd.oci.image.index.v1+json");
    assert_eq!(all.h("oci-filters-applied"), "");
    let list = all.json()["manifests"].as_array().unwrap().clone();
    assert_eq!(list.len(), 2);
    let find = |note: &str| list.iter().find(|m| m["annotations"]["note"] == note).unwrap().clone();
    assert_eq!(find("by field")["artifactType"], "application/example.sig");
    assert_eq!(find("by config")["artifactType"], "application/example.sbom", "falls back to the config media type");

    let only = r.get(&format!("/v2/alice/demo/referrers/{subject}?artifactType=application/example.sbom"), "pw-alice").await;
    assert_eq!(only.h("oci-filters-applied"), "artifactType");
    let filtered = only.json()["manifests"].as_array().unwrap().clone();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["annotations"]["note"], "by config");
    let none = r.get(&format!("/v2/alice/demo/referrers/{subject}?artifactType=application/nothing"), "pw-alice").await;
    assert_eq!((none.h("oci-filters-applied"), none.json()["manifests"].as_array().unwrap().len()), ("artifactType".to_string(), 0));
}
