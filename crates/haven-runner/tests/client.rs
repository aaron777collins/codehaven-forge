//! The runner's protocol client against the in-memory server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use ch_api_types::runner::{JobRequest, LogBatch, RegisterRequest, StepUpdate};
use common::fast_policy;
use haven_runner::client::{ApiError, Client};
use haven_runner::testing::{FakeServer, TestJob};
use std::collections::BTreeMap;

fn reg(token: &str) -> RegisterRequest {
    RegisterRequest {
        token: token.into(),
        name: "r1".into(),
        labels: vec!["linux".into()],
        version: Some("0".into()),
        os: None,
        arch: None,
    }
}

#[tokio::test]
async fn registration_is_single_use() {
    let s = FakeServer::start().await;
    s.add_registration_token("regtok");
    let c = Client::with_policy(&s.url, None, fast_policy()).unwrap();
    let ok = c.register(&reg("regtok")).await.unwrap();
    assert!(ok.credential.starts_with("chr_"));
    assert_eq!(ok.name, "r1");
    let again = c.register(&reg("regtok")).await.unwrap_err();
    assert!(matches!(again, ApiError::Unauthorized), "{again:?}");
    let made_up = c.register(&reg("made-up")).await.unwrap_err();
    assert!(matches!(made_up, ApiError::Unauthorized));
    assert_eq!(s.runner_count(), 1);
}

#[tokio::test]
async fn registration_retries_server_errors_but_not_refusals() {
    let s = FakeServer::start().await;
    s.add_registration_token("t");
    s.fail_next("register", &[503, 502]);
    let c = Client::with_policy(&s.url, None, fast_policy()).unwrap();
    assert!(c.register(&reg("t")).await.is_ok());
    assert_eq!(s.requests_to("register"), 3);
    s.fail_next("register", &[401]);
    let before = s.requests_to("register");
    assert!(matches!(c.register(&reg("nope")).await, Err(ApiError::Unauthorized)));
    assert_eq!(s.requests_to("register"), before + 1, "a 401 is not retried");
}

#[tokio::test]
async fn jobs_are_matched_by_label_subset_and_the_wait_returns_empty() {
    let s = FakeServer::start().await;
    let (_, cred) = s.add_runner("r", &["linux", "x64"]);
    let c = Client::with_policy(&s.url, Some(cred), fast_policy()).unwrap();
    let mut gpu = TestJob::new(1).run("true").build();
    gpu.runs_on = vec!["gpu".into()];
    let mut plain = TestJob::new(2).run("true").build();
    plain.runs_on = vec!["linux".into()];
    s.enqueue(gpu);
    s.enqueue(plain);
    let req = JobRequest { wait_seconds: Some(1), labels: vec!["linux".into(), "x64".into()] };
    let got = c.request_job(&req).await.unwrap().expect("the plain job");
    assert_eq!(got.job_id, 2, "the gpu job needs a label this runner lacks");
    assert!(c.request_job(&JobRequest { wait_seconds: Some(0), labels: req.labels.clone() }).await.unwrap().is_none());
}

#[tokio::test]
async fn a_wrong_credential_is_unauthorized_everywhere() {
    let s = FakeServer::start().await;
    let c = Client::with_policy(&s.url, Some("nope".into()), fast_policy()).unwrap();
    s.enqueue(TestJob::new(1).run("true").build());
    assert!(matches!(c.request_job(&JobRequest::default()).await, Err(ApiError::Unauthorized)));
    assert!(matches!(c.heartbeat(1).await, Err(ApiError::Unauthorized)));
    assert!(matches!(c.send_logs(1, &LogBatch { step: 1, seq: 0, lines: vec![] }).await, Err(ApiError::Unauthorized)));
}

#[tokio::test]
async fn log_batches_are_idempotent_on_seq() {
    let s = FakeServer::start().await;
    let (_, cred) = s.add_runner("r", &["linux"]);
    s.enqueue(TestJob::new(5).run("true").build());
    let c = Client::with_policy(&s.url, Some(cred), fast_policy()).unwrap();
    let b = LogBatch { step: 1, seq: 0, lines: vec!["a".into(), "b".into()] };
    assert_eq!(c.send_logs(5, &b).await.unwrap().next_seq, 1);
    assert_eq!(c.send_logs(5, &b).await.unwrap().next_seq, 1, "a retry of the same batch");
    c.send_logs(5, &LogBatch { step: 1, seq: 1, lines: vec!["c".into()] }).await.unwrap();
    let lines: Vec<String> = s.log_lines(5).into_iter().map(|(_, l)| l).collect();
    assert_eq!(lines, ["a", "b", "c"], "the repeat was dropped");
}

#[tokio::test]
async fn a_job_the_server_forgot_is_a_lost_lease_not_a_retry() {
    let s = FakeServer::start().await;
    let (_, cred) = s.add_runner("r", &["linux"]);
    let c = Client::with_policy(&s.url, Some(cred), fast_policy()).unwrap();
    let e = c.heartbeat(999).await.unwrap_err();
    assert!(e.is_lease_lost(), "{e:?}");
    assert_eq!(s.requests_to("heartbeat"), 1, "not retried");
    let u = StepUpdate { status: "in_progress".into(), conclusion: None, outputs: BTreeMap::new() };
    assert!(c.update_step(999, 1, &u).await.unwrap_err().is_lease_lost());
}

#[tokio::test]
async fn persistent_server_errors_give_up_after_the_attempts() {
    let s = FakeServer::start().await;
    let (_, cred) = s.add_runner("r", &["linux"]);
    s.enqueue(TestJob::new(5).run("true").build());
    let c = Client::with_policy(&s.url, Some(cred), fast_policy()).unwrap();
    s.fail_next("heartbeat", &[500; 10]);
    let e = c.heartbeat(5).await.unwrap_err();
    assert!(matches!(e, ApiError::Server { .. }), "{e:?}");
    assert_eq!(s.requests_to("heartbeat"), 2, "heartbeat tries twice, the loop tries again in 30 s");
}

#[tokio::test]
async fn an_unreachable_server_is_a_network_error_that_retries() {
    // Nothing listens on this port.
    let c = Client::with_policy("http://127.0.0.1:9", Some("x".into()), fast_policy()).unwrap();
    let start = std::time::Instant::now();
    let e = c.send_logs(1, &LogBatch { step: 1, seq: 0, lines: vec![] }).await.unwrap_err();
    assert!(matches!(e, ApiError::Network(_)), "{e:?}");
    assert!(start.elapsed() >= std::time::Duration::from_millis(30), "it backed off between attempts: {:?}", start.elapsed());
    assert!(!e.to_string().contains("127.0.0.1:9/api"), "error text should not carry the URL: {e}");
}

#[tokio::test]
async fn ping_reaches_healthz() {
    let s = FakeServer::start().await;
    let c = Client::with_policy(&s.url, None, fast_policy()).unwrap();
    assert_eq!(c.ping().await.unwrap(), 200);
}
