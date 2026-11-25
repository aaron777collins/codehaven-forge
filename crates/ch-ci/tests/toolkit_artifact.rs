//! The artifact service of the Actions toolkit (ch-ctk.6): Twirp calls with the runtime token, an
//! Azure-style upload to a signed link, a signed download, and what each refuses. The client in
//! this file sends what `@actions/artifact` sends.

#![allow(clippy::unwrap_used)]

mod support;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use serde_json::{Value, json};
use sha2::Digest;
use sqlx::PgPool;
use support::*;

const ONE_JOB: &str = "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n";
const SVC: &str = "/twirp/github.actions.results.api.v1.ArtifactService";

/// What `haven-runner` gives an action as `ACTIONS_RUNTIME_TOKEN`.
fn runtime_token(job_token: &str, run: i64, job: i64, exp: i64) -> String {
    use base64::Engine;
    use hmac::Mac;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({"sub": format!("job:{job}"), "scp": format!("Actions.Results:{run}:{job} Actions.UploadArtifacts"), "exp": exp});
    let input = format!("{}.{}", b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#), b64.encode(claims.to_string()));
    let key = sha2::Sha256::digest(job_token.as_bytes());
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    mac.update(input.as_bytes());
    format!("{input}.{}", b64.encode(mac.finalize().into_bytes()))
}

struct Client<'a> {
    w: &'a World,
    rt: String,
    run: i64,
    job: i64,
}

impl Client<'_> {
    /// A Twirp call, with the ids the toolkit reads out of its token.
    async fn call(&self, method: &str, mut body: Value) -> (StatusCode, Value) {
        body["workflowRunBackendId"] = json!(self.run.to_string());
        body["workflowJobRunBackendId"] = json!(self.job.to_string());
        self.call_as(&self.rt, method, body).await
    }

    async fn call_as(&self, token: &str, method: &str, body: Value) -> (StatusCode, Value) {
        let mut b = Request::builder().method(Method::POST).uri(format!("{SVC}/{method}")).header(header::CONTENT_TYPE, "application/json");
        if !token.is_empty() {
            b = b.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let (s, bytes) = self.w.raw(b.body(Body::from(body.to_string())).unwrap()).await;
        (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The path and query of a link, which the toolkit reaches with no credential.
    async fn blob(&self, method: Method, link: &str, query: &str, body: Vec<u8>) -> (StatusCode, axum::body::Bytes) {
        let path = link.split("/api/").nth(1).map(|p| format!("/api/{p}")).unwrap();
        let uri = if query.is_empty() { path } else { format!("{path}?{query}") };
        self.w.raw(Request::builder().method(method).uri(uri).body(Body::from(body)).unwrap()).await
    }

    async fn create(&self, name: &str) -> String {
        let (s, v) = self.call("CreateArtifact", json!({"name": name, "version": 4})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["ok"], true);
        v["signedUploadUrl"].as_str().unwrap().to_string()
    }
}

async fn job_world(pool: PgPool) -> (World, FakeRunner, ch_api_types::runner::JobPayload) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    (w, r, job)
}

fn block_list(ids: &[&str]) -> Vec<u8> {
    let items: String = ids.iter().map(|i| format!("<Latest>{i}</Latest>")).collect();
    format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>{items}</BlockList>").into_bytes()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_toolkit_uploads_an_artifact_in_blocks_and_downloads_it_by_a_signed_link(pool: PgPool) {
    let (w, _r, job) = job_world(pool).await;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let c = Client { w: &w, rt, run: job.run_id, job: job.job_id };

    let link = c.create("dist").await;
    assert!(link.contains("/api/runner/v1/toolkit/blobs/u."), "{link}");
    // Two blocks, sent out of order, with ids that need escaping in a query (base64 has + / =).
    for (id, part) in [("AA%2BB%2FCC%3D%3D", b"world".to_vec()), ("AAAA", b"hello ".to_vec())] {
        let (s, _) = c.blob(Method::PUT, &link, &format!("comp=block&blockid={id}"), part).await;
        assert_eq!(s, StatusCode::CREATED, "{id}");
    }
    let (s, _) = c.blob(Method::PUT, &link, "comp=blocklist", block_list(&["AAAA", "AA+B/CC=="])).await;
    assert_eq!(s, StatusCode::CREATED);
    let want = b"hello world";
    let hash = format!("sha256:{}", hex::encode(sha2::Sha256::digest(want)));
    let (s, v) = c.call("FinalizeArtifact", json!({"name": "dist", "size": want.len().to_string(), "hash": hash})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id: i64 = v["artifactId"].as_str().unwrap().parse().unwrap();

    // It is listed, by name and by id, and the built-in endpoints see the same artifact.
    let (_, v) = c.call("ListArtifacts", json!({})).await;
    let a = &v["artifacts"][0];
    assert_eq!(
        (a["name"].as_str(), a["size"].as_str(), a["databaseId"].as_str()),
        (Some("dist"), Some("11"), Some(id.to_string().as_str()))
    );
    assert!(a["createdAt"].as_str().unwrap().ends_with('Z'));
    let (_, v) = c.call("ListArtifacts", json!({"nameFilter": "other"})).await;
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);
    let (_, v) = c.call("ListArtifacts", json!({"nameFilter": "dist", "idFilter": id.to_string()})).await;
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 1);
    let (_, v) = c.call("ListArtifacts", json!({"idFilter": (id + 1000).to_string()})).await;
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);
    let rows = ch_ci::store::artifacts_for_run(&w.ctx, job.run_id).await.unwrap();
    assert_eq!((rows.len(), rows[0].size), (1, 11));

    // The link opens with no credential and gives the bytes back; one character off is a 404.
    let (s, v) = c.call("GetSignedArtifactURL", json!({"name": "dist"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let down = v["signedUrl"].as_str().unwrap().to_string();
    let (s, body) = c.blob(Method::GET, &down, "", vec![]).await;
    assert_eq!((s, body.as_ref()), (StatusCode::OK, want.as_ref()));
    let mut bad = down.clone();
    let last = bad.pop().unwrap();
    bad.push(if last == '0' { '1' } else { '0' });
    assert_eq!(c.blob(Method::GET, &bad, "", vec![]).await.0, StatusCode::NOT_FOUND);
    // An upload link does not download, and a download link does not upload.
    assert_eq!(c.blob(Method::GET, &link, "", vec![]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(c.blob(Method::PUT, &down, "comp=block&blockid=AAAA", b"x".to_vec()).await.0, StatusCode::NOT_FOUND);
    let (s, _) = c.call("GetSignedArtifactURL", json!({"name": "nope"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A name is written once per run: a second create is a conflict until the first is deleted.
    let (s, v) = c.call("CreateArtifact", json!({"name": "dist", "version": 4})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "already_exists");
    let (s, v) = c.call("DeleteArtifact", json!({"name": "dist"})).await;
    assert_eq!((s, v["artifactId"].as_str()), (StatusCode::OK, Some(id.to_string().as_str())));
    assert_eq!(c.blob(Method::GET, &down, "", vec![]).await.0, StatusCode::NOT_FOUND, "a deleted artifact's link is dead");
    c.create("dist").await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_single_put_uploads_a_whole_artifact_too(pool: PgPool) {
    let (w, _r, job) = job_world(pool).await;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let c = Client { w: &w, rt, run: job.run_id, job: job.job_id };
    let link = c.create("whole").await;
    let (s, _) = c.blob(Method::PUT, &link, "", b"all at once".to_vec()).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = c.call("FinalizeArtifact", json!({"name": "whole", "size": "11"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_bad_upload_is_refused_and_leaves_nothing(pool: PgPool) {
    let (w, _r, job) = job_world(pool).await;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let c = Client { w: &w, rt, run: job.run_id, job: job.job_id };

    // Names the server cannot keep, and ids that are not this job's.
    for name in ["", "a/b", "..", ".hidden", &"x".repeat(101)] {
        let (s, _) = c.call("CreateArtifact", json!({"name": name})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name:?}");
    }
    let (s, _) = c
        .call_as(
            &c.rt,
            "CreateArtifact",
            json!({"name": "x", "workflowRunBackendId": (job.run_id + 1).to_string(), "workflowJobRunBackendId": job.job_id.to_string()}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Nothing to finalize before bytes arrive, and a list that names a block never sent.
    let link = c.create("a").await;
    let (s, _) = c.call("FinalizeArtifact", json!({"name": "a"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c.blob(Method::PUT, &link, "comp=blocklist", block_list(&["NOPE"])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    for bad in [&b"<BlockList><Other>x</Other></BlockList>"[..], b"not xml", b""] {
        assert_eq!(c.blob(Method::PUT, &link, "comp=blocklist", bad.to_vec()).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(c.blob(Method::PUT, &link, "comp=block", b"x".to_vec()).await.0, StatusCode::BAD_REQUEST, "a block needs its id");
    assert_eq!(c.blob(Method::PUT, &link, "comp=metadata", b"x".to_vec()).await.0, StatusCode::BAD_REQUEST);
    // A size or a hash that is not the bytes' is refused, and the upload is gone after it.
    c.blob(Method::PUT, &link, "comp=block&blockid=AAAA", b"abc".to_vec()).await;
    c.blob(Method::PUT, &link, "comp=blocklist", block_list(&["AAAA"])).await;
    let (s, _) = c.call("FinalizeArtifact", json!({"name": "a", "size": "4"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let link = c.create("a").await;
    c.blob(Method::PUT, &link, "comp=block&blockid=AAAA", b"abc".to_vec()).await;
    c.blob(Method::PUT, &link, "comp=blocklist", block_list(&["AAAA"])).await;
    let (s, _) = c.call("FinalizeArtifact", json!({"name": "a", "size": "3", "hash": format!("sha256:{}", "0".repeat(64))})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(c.call("FinalizeArtifact", json!({"name": "a", "size": "3"})).await.0, StatusCode::NOT_FOUND);
    assert!(ch_ci::store::artifacts_for_run(&w.ctx, job.run_id).await.unwrap().is_empty());
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM toolkit_uploads").fetch_one(w.pool()).await.unwrap();
    assert_eq!(held, 0);
    let dir = ch_ci::toolkit::tmp_dir(&w.ctx);
    assert_eq!(std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0, "no files are left");

    // A forged link, an upload that was never made, and no credential at all.
    for forged in [
        "/api/runner/v1/toolkit/blobs/u.1.9999999999.00".to_string(),
        format!("/api/runner/v1/toolkit/blobs/u.1.9999999999.{}", "0".repeat(64)),
        "/api/runner/v1/toolkit/blobs/a.1.9999999999.00".to_string(),
    ] {
        let (s, _) = w.raw(Request::builder().method(Method::PUT).uri(&forged).body(Body::from("x")).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{forged}");
    }
    let (s, _) = c.call_as("", "ListArtifacts", json!({})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) =
        c.call_as(&runtime_token("chj_not-this-jobs-token", job.run_id, job.job_id, i64::MAX / 2), "ListArtifacts", json!({})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = c.call("NoSuchMethod", json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_fork_reads_artifacts_but_never_writes_and_a_finished_job_cannot_upload(pool: PgPool) {
    let (w, r, job) = job_world(pool).await;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let c = Client { w: &w, rt, run: job.run_id, job: job.job_id };
    let link = c.create("early").await;
    c.blob(Method::PUT, &link, "comp=block&blockid=AAAA", b"abc".to_vec()).await;

    // The run becomes a fork's: nothing it holds opens a write.
    sqlx::query("UPDATE workflow_runs SET event_payload = jsonb_set(COALESCE(event_payload, '{}'::jsonb), '{meta}', '{\"fork\": true}') WHERE id = $1")
        .bind(job.run_id)
        .execute(w.pool())
        .await
        .unwrap();
    for (m, body) in
        [("CreateArtifact", json!({"name": "f"})), ("FinalizeArtifact", json!({"name": "early"})), ("DeleteArtifact", json!({"name": "x"}))]
    {
        let (s, _) = c.call(m, body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m}");
    }
    assert_eq!(c.call("ListArtifacts", json!({})).await.0, StatusCode::OK, "a fork may list");
    sqlx::query("UPDATE workflow_runs SET event_payload = event_payload #- '{meta,fork}' WHERE id = $1")
        .bind(job.run_id)
        .execute(w.pool())
        .await
        .unwrap();

    // The job ends: its link and its token stop working, and the janitor takes what it left.
    r.complete(&w, job.job_id, "success", json!({})).await;
    assert_eq!(c.blob(Method::PUT, &link, "comp=block&blockid=BBBB", b"abc".to_vec()).await.0, StatusCode::NOT_FOUND);
    assert_eq!(c.call("ListArtifacts", json!({})).await.0, StatusCode::UNAUTHORIZED);
    ch_ci::workers::janitor_pass(&w.ctx).await.unwrap();
    let dir = ch_ci::toolkit::tmp_dir(&w.ctx);
    assert_eq!(std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0, "block files go with the upload");
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM toolkit_uploads").fetch_one(w.pool()).await.unwrap();
    assert_eq!(held, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_upload_cannot_pass_the_size_limit_in_blocks(pool: PgPool) {
    let (w, _r, job) = job_world(pool).await;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let c = Client { w: &w, rt, run: job.run_id, job: job.job_id };
    let link = c.create("big").await;
    // A block that says it is over the limit is refused before a byte is written.
    let mut r = Request::builder()
        .method(Method::PUT)
        .uri(format!("{}?comp=block&blockid=AAAA", link.split("/api/").nth(1).map(|p| format!("/api/{p}")).unwrap()))
        .header(header::CONTENT_LENGTH, (64u64 * 1024 * 1024 + 1).to_string())
        .body(Body::empty())
        .unwrap();
    *r.body_mut() = Body::from(vec![0u8; 16]);
    let (s, _) = w.raw(r).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    // Blocks already held count toward the artifact's limit (a sparse file costs no disk).
    let dir = ch_ci::toolkit::tmp_dir(&w.ctx);
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM toolkit_uploads LIMIT 1").fetch_one(w.pool()).await.unwrap();
    let big = std::fs::File::create(dir.join(format!("art-{id}-held.blk"))).unwrap();
    big.set_len(ch_ci::store::MAX_ARTIFACT_BYTES - 2).unwrap();
    let (s, _) = c.blob(Method::PUT, &link, "comp=block&blockid=AAAA", b"abc".to_vec()).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn joining_blocks_counts_the_joined_file_against_the_server_cap(pool: PgPool) {
    let (w, _r, job) = job_world(pool).await;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let c = Client { w: &w, rt, run: job.run_id, job: job.job_id };
    let link = c.create("dist").await;
    // Other uploads hold all but a few bytes of what the server allows (a sparse file costs no disk).
    let dir = ch_ci::toolkit::tmp_dir(&w.ctx);
    std::fs::create_dir_all(&dir).unwrap();
    let big = std::fs::File::create(dir.join("cache-998.part")).unwrap();
    big.set_len(ch_ci::toolkit::MAX_INFLIGHT_BYTES - 4).unwrap();
    let (s, _) = c.blob(Method::PUT, &link, "comp=block&blockid=AAAA", b"abc".to_vec()).await;
    assert_eq!(s, StatusCode::CREATED, "the block itself fits");
    // The joined copy would sit beside the blocks, and together they pass the cap: refused.
    let (s, _) = c.blob(Method::PUT, &link, "comp=blocklist", block_list(&["AAAA"])).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let joining = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().ends_with(".joining"));
    assert!(!joining, "no half-made join is left");
    // Room again, and the same list joins.
    std::fs::remove_file(dir.join("cache-998.part")).unwrap();
    let (s, _) = c.blob(Method::PUT, &link, "comp=blocklist", block_list(&["AAAA"])).await;
    assert_eq!(s, StatusCode::CREATED);
}
