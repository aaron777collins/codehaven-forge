//! The cache service of the Actions toolkit (ch-ctk.6): reserve, upload in ranges, commit, look
//! up and download by a signed link, with the runtime token the runner gives actions.

#![allow(clippy::unwrap_used)]

mod support;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use support::*;

const ONE_JOB: &str = "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n";

/// What `haven-runner` gives an action as `ACTIONS_RUNTIME_TOKEN`.
fn runtime_token(job_token: &str, run: i64, job: i64, exp: i64) -> String {
    use base64::Engine;
    use hmac::Mac;
    use sha2::Digest;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({"sub": format!("job:{job}"), "scp": format!("Actions.Results:{run}:{job} Actions.UploadArtifacts"), "exp": exp});
    let input = format!("{}.{}", b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#), b64.encode(claims.to_string()));
    let key = sha2::Sha256::digest(job_token.as_bytes());
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    mac.update(input.as_bytes());
    format!("{input}.{}", b64.encode(mac.finalize().into_bytes()))
}

fn req(method: Method, uri: &str, token: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Request {
    let mut b = Request::builder().method(method).uri(uri).header(header::AUTHORIZATION, format!("Bearer {token}"));
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(body)).unwrap()
}

const BASE: &str = "/api/runner/v1/toolkit/_apis/artifactcache";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_toolkit_saves_a_cache_in_ranges_and_reads_it_back_by_a_signed_link(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let exp = chrono::Utc::now().timestamp() + 600;
    let rt = runtime_token(&job.token, job.run_id, job.job_id, exp);

    // Nothing yet: 204, as the toolkit expects.
    let (s, _) = w.raw(req(Method::GET, &format!("{BASE}/cache?keys=node-1,node-&version=v1"), &rt, &[], vec![])).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Reserve, send two ranges out of order, commit.
    let (s, b) = w
        .raw(req(
            Method::POST,
            &format!("{BASE}/caches"),
            &rt,
            &[("content-type", "application/json")],
            json!({"key": "node-1", "version": "v1", "cacheSize": 10}).to_string().into_bytes(),
        ))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<Value>(&b).unwrap()["cacheId"].as_i64().unwrap();
    for (range, part) in [("bytes 5-9/*", b"world".to_vec()), ("bytes 0-4/*", b"hello".to_vec())] {
        let (s, _) = w
            .raw(req(
                Method::PATCH,
                &format!("{BASE}/caches/{id}"),
                &rt,
                &[("content-range", range), ("content-type", "application/octet-stream")],
                part,
            ))
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{range}");
    }
    // A range that does not match its body is refused. Bytes are written as they arrive, so the
    // toolkit sends that range again, which is what it does when a chunk fails.
    let (s, _) =
        w.raw(req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 0-9/*")], b"short".to_vec())).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) =
        w.raw(req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 0-9/*")], b"helloworld".to_vec())).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // A body that says it is another length than its range is refused before a byte is written.
    let (s, _) = w
        .raw(req(
            Method::PATCH,
            &format!("{BASE}/caches/{id}"),
            &rt,
            &[("content-range", "bytes 0-9/*"), ("content-length", "5")],
            b"XXXXX".to_vec(),
        ))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = w
        .raw(req(
            Method::POST,
            &format!("{BASE}/caches/{id}"),
            &rt,
            &[("content-type", "application/json")],
            json!({"size": 10}).to_string().into_bytes(),
        ))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // A second save of the same key is a conflict the toolkit reads as "already saved".
    let (s, _) = w
        .raw(req(
            Method::POST,
            &format!("{BASE}/caches"),
            &rt,
            &[("content-type", "application/json")],
            json!({"key": "node-1", "version": "v1"}).to_string().into_bytes(),
        ))
        .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // A restore key finds it; the link opens with no credential at all.
    let (s, b) = w.raw(req(Method::GET, &format!("{BASE}/cache?keys=other,node-&version=v1"), &rt, &[], vec![])).await;
    assert_eq!(s, StatusCode::OK);
    let hit: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(hit["cacheKey"], "node-1");
    assert_eq!(hit["scope"], "refs/heads/main");
    let link = hit["archiveLocation"].as_str().unwrap().split("/api/").nth(1).map(|p| format!("/api/{p}")).unwrap();
    let (s, body) = w.send_raw(Method::GET, &link, &Who::Anon, None).await;
    assert_eq!((s, body.as_ref()), (StatusCode::OK, b"helloworld".as_ref()));
    // A link with one character changed is a plain 404.
    let mut bad = link.clone();
    let last = bad.pop().unwrap();
    bad.push(if last == '0' { '1' } else { '0' });
    assert_eq!(w.send_raw(Method::GET, &bad, &Who::Anon, None).await.0, StatusCode::NOT_FOUND);

    // No token, a token signed with another key, or an expired one: not signed in.
    let forged = runtime_token("chj_not-this-jobs-token", job.run_id, job.job_id, exp);
    let expired = runtime_token(&job.token, job.run_id, job.job_id, exp - 3600);
    for t in [forged.as_str(), expired.as_str()] {
        let (s, _) = w.raw(req(Method::GET, &format!("{BASE}/cache?keys=node-1&version=v1"), t, &[], vec![])).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    // Once the job ends its runtime token is dead too.
    r.complete(&w, job.job_id, "success", json!({})).await;
    let (s, _) = w.raw(req(Method::GET, &format!("{BASE}/cache?keys=node-1&version=v1"), &rt, &[], vec![])).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// Reserves a cache and sends five bytes of it, without ever committing. Returns its id.
async fn start_upload(w: &World, rt: &str) -> i64 {
    let (s, b) = w
        .raw(req(
            Method::POST,
            &format!("{BASE}/caches"),
            rt,
            &[("content-type", "application/json")],
            json!({"key": format!("k{}", rand_suffix()), "version": "v1"}).to_string().into_bytes(),
        ))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<Value>(&b).unwrap()["cacheId"].as_i64().unwrap();
    let (s, _) =
        w.raw(req(Method::PATCH, &format!("{BASE}/caches/{id}"), rt, &[("content-range", "bytes 0-4/*")], b"hello".to_vec())).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    id
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_upload_nobody_finishes_does_not_stay_on_the_disk(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let dir = ch_ci::toolkit::tmp_dir(&w.ctx);
    let part = |id: i64| dir.join(format!("cache-{id}.part"));

    // Still being sent: the janitor leaves it alone.
    let fresh = start_upload(&w, &rt).await;
    ch_ci::workers::janitor_pass(&w.ctx).await.unwrap();
    assert!(part(fresh).exists());

    // Abandoned for over an hour while the job runs: dropped, file and all.
    let stale = start_upload(&w, &rt).await;
    sqlx::query("UPDATE toolkit_uploads SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(stale)
        .execute(w.pool())
        .await
        .unwrap();
    ch_ci::workers::janitor_pass(&w.ctx).await.unwrap();
    assert!(!part(stale).exists() && part(fresh).exists());

    // The job ends: whatever it left unfinished goes.
    r.complete(&w, job.job_id, "success", json!({})).await;
    ch_ci::workers::janitor_pass(&w.ctx).await.unwrap();
    assert!(!part(fresh).exists());
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM toolkit_uploads").fetch_one(w.pool()).await.unwrap();
    assert_eq!(left, 0);

    // A file with no reservation, once it is old enough to be sure nobody is writing it.
    std::fs::write(dir.join("cache-999.part"), b"x").unwrap();
    ch_ci::workers::janitor_pass(&w.ctx).await.unwrap();
    assert!(dir.join("cache-999.part").exists(), "a new file may belong to a reservation being made");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options().write(true).open(dir.join("cache-999.part")).unwrap().set_modified(old).unwrap();
    ch_ci::workers::janitor_pass(&w.ctx).await.unwrap();
    assert!(!dir.join("cache-999.part").exists());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn unfinished_uploads_are_capped_over_the_whole_server(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let id = start_upload(&w, &rt).await;
    // Other jobs already hold the most the server allows (a sparse file costs no disk).
    let dir = ch_ci::toolkit::tmp_dir(&w.ctx);
    let big = std::fs::File::create(dir.join("cache-998.part")).unwrap();
    big.set_len(ch_ci::toolkit::MAX_INFLIGHT_BYTES).unwrap();
    let (s, _) =
        w.raw(req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 5-9/*")], b"world".to_vec())).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    std::fs::remove_file(dir.join("cache-998.part")).unwrap();
    let (s, _) =
        w.raw(req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 5-9/*")], b"world".to_vec())).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

/// A range body that sends `first` and then waits for ever, as a client that holds a request open.
fn stalled(first: &'static [u8]) -> Body {
    use futures::StreamExt;
    Body::from_stream(
        futures::stream::once(async move { Ok::<_, std::io::Error>(bytes::Bytes::from_static(first)) }).chain(futures::stream::pending()),
    )
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_range_is_written_as_it_arrives_not_held_in_memory(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let (s, b) = w
        .raw(req(
            Method::POST,
            &format!("{BASE}/caches"),
            &rt,
            &[("content-type", "application/json")],
            json!({"key": "big", "version": "v1"}).to_string().into_bytes(),
        ))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let id = serde_json::from_slice::<Value>(&b).unwrap()["cacheId"].as_i64().unwrap();
    // A 64 MiB range of which only five bytes arrive. Those five are already on the disk, so the
    // server is not holding a range's bytes in memory until the rest comes.
    let mut r = req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 0-67108863/*")], vec![]);
    *r.body_mut() = stalled(b"hello");
    let sent = tokio::time::timeout(std::time::Duration::from_millis(500), w.raw(r)).await;
    assert!(sent.is_err(), "the request was still open");
    let part = ch_ci::toolkit::tmp_dir(&w.ctx).join(format!("cache-{id}.part"));
    assert_eq!(std::fs::read(&part).unwrap(), b"hello");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_cannot_open_more_than_twenty_reservations_even_all_at_once(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let all = (0..80).map(|i| {
        w.raw(req(
            Method::POST,
            &format!("{BASE}/caches"),
            &rt,
            &[("content-type", "application/json")],
            json!({"key": format!("p{i}"), "version": "v1"}).to_string().into_bytes(),
        ))
    });
    let made = futures::future::join_all(all).await.into_iter().filter(|(s, _)| *s == StatusCode::CREATED).count();
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM toolkit_uploads").fetch_one(w.pool()).await.unwrap();
    assert_eq!((made, held), (20, 20));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_cannot_hold_more_ranges_open_than_the_server_allows(pool: PgPool) {
    use ch_ci::toolkit::MAX_JOB_RANGES;
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE_JOB)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let id = start_upload(&w, &rt).await;
    // More ranges than the job may hold, every one held open by a client that sends a little
    // and stops.
    let held = (0..MAX_JOB_RANGES).map(|_| {
        let mut r = req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 0-67108863/*")], vec![]);
        *r.body_mut() = stalled(b"hello");
        w.raw(r)
    });
    let probe = async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        w.raw(req(Method::PATCH, &format!("{BASE}/caches/{id}"), &rt, &[("content-range", "bytes 0-4/*")], b"hello".to_vec())).await
    };
    tokio::select! {
        _ = futures::future::join_all(held) => panic!("the held ranges should still be open"),
        (s, _) = probe => assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE),
    }
}
