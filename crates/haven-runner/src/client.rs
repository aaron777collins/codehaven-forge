//! The runner's side of `/api/runner/v1` (ARCHITECTURE.md section 9.4).
//!
//! Every call that must not be lost retries with exponential backoff and jitter: network
//! errors, `5xx` and `429` (honouring `Retry-After`). A `4xx` is the server's answer and is
//! returned at once. The calls are safe to repeat: log batches carry a `seq` the server dedupes
//! on, and step updates and completion set a value rather than add one.

use ch_api_types::common::ApiError as ApiErrorBody;
use ch_api_types::runner::{
    ArtifactUploaded, CacheLookupRequest, CacheLookupResponse, CompleteRequest, HeartbeatRequest, HeartbeatResponse, JobPayload,
    JobRequest, JobResponse, LogAck, LogBatch, RegisterRequest, RegisterResponse, StepUpdate,
};
use futures::StreamExt;
use reqwest::{Method, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The credential is not accepted. The runner was removed or the credential replaced.
    #[error("the server does not accept this runner's credential (HTTP 401). It was probably removed; register it again")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    /// Any other 4xx: the server understood and said no. Not retried.
    #[error("the server refused the request (HTTP {status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("server error (HTTP {status})")]
    Server { status: u16, retry_after: Option<u64> },
    #[error("cannot reach the server: {0}")]
    Network(String),
    #[error("local file problem: {0}")]
    Io(String),
}

impl ApiError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ApiError::Server { .. } | ApiError::Network(_))
    }

    /// The job or lease is gone on the server's side.
    pub fn is_lease_lost(&self) -> bool {
        matches!(self, ApiError::NotFound | ApiError::Conflict(_))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::Io(e.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub base: Duration,
    pub max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { base: Duration::from_millis(500), max: Duration::from_secs(30) }
    }
}

impl RetryPolicy {
    /// The wait before retry number `attempt` (0 for the first retry): `base * 2^attempt`, capped
    /// at `max`, then spread by up to a quarter either way so a crowd of runners does not retry
    /// in step.
    pub fn delay(&self, attempt: u32, jitter: f64) -> Duration {
        let exp = self.base.as_millis().saturating_mul(1u128 << attempt.min(20));
        let capped = exp.min(self.max.as_millis()) as f64;
        let j = 1.0 + (jitter.clamp(0.0, 1.0) - 0.5) * 0.5;
        Duration::from_millis((capped * j).max(1.0) as u64)
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: url::Url,
    credential: Option<String>,
    policy: RetryPolicy,
}

impl Client {
    pub fn new(server_url: &str, credential: Option<String>) -> anyhow::Result<Client> {
        Client::with_policy(server_url, credential, RetryPolicy::default())
    }

    pub fn with_policy(server_url: &str, credential: Option<String>, policy: RetryPolicy) -> anyhow::Result<Client> {
        let base = url::Url::parse(server_url.trim_end_matches('/'))?;
        if !matches!(base.scheme(), "http" | "https") {
            anyhow::bail!("the server URL must be http or https");
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("haven-runner/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Client { http, base, credential, policy })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    fn url(&self, segments: &[&str]) -> url::Url {
        let mut u = self.base.clone();
        if let Ok(mut s) = u.path_segments_mut() {
            s.pop_if_empty().extend(["api", "runner", "v1"]).extend(segments);
        }
        u
    }

    fn request(&self, method: Method, url: url::Url, job_token: Option<&str>) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, url).header(header::ACCEPT, "application/json");
        if let Some(t) = job_token.or(self.credential.as_deref()) {
            r = r.bearer_auth(t);
        }
        r
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let retry_after = resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();
        let message =
            serde_json::from_str::<ApiErrorBody>(&text).map(|b| b.message).unwrap_or_else(|_| text.trim().chars().take(300).collect());
        Err(match status {
            StatusCode::UNAUTHORIZED => ApiError::Unauthorized,
            StatusCode::NOT_FOUND | StatusCode::GONE => ApiError::NotFound,
            StatusCode::CONFLICT => ApiError::Conflict(message),
            // Retried like a server error, waiting at least as long as `Retry-After` says.
            StatusCode::TOO_MANY_REQUESTS => ApiError::Server { status: 429, retry_after },
            s if s.is_server_error() => ApiError::Server { status: s.as_u16(), retry_after },
            s => ApiError::Rejected { status: s.as_u16(), message },
        })
    }

    /// Runs `op` up to `attempts` times, sleeping between retryable failures.
    pub async fn retrying<T, F, Fut>(&self, attempts: u32, mut op: F) -> Result<T, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        let mut n = 0;
        loop {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) if e.is_retryable() && n + 1 < attempts => {
                    let mut wait = self.policy.delay(n, rand::random::<f64>());
                    if let ApiError::Server { retry_after: Some(secs), .. } = &e {
                        wait = wait.max(Duration::from_secs((*secs).min(60)));
                    }
                    tokio::time::sleep(wait).await;
                    n += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn send_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        url: url::Url,
        body: &B,
        job_token: Option<&str>,
    ) -> Result<T, ApiError> {
        let resp =
            self.request(method, url, job_token).json(body).send().await.map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        let resp = Self::check(resp).await?;
        let bytes = resp.bytes().await.map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        if bytes.is_empty() {
            // A bodyless `204` stands for `{}` for the types that can be empty.
            return serde_json::from_str("{}").map_err(|e| ApiError::Rejected { status: 200, message: format!("empty answer: {e}") });
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Rejected { status: 200, message: format!("the server's answer could not be read: {e}") })
    }

    /// Is the server there at all? A plain GET of `/healthz`, which needs no credential.
    pub async fn ping(&self) -> Result<u16, ApiError> {
        let mut u = self.base.clone();
        u.set_path("/healthz");
        let resp =
            self.http.get(u).timeout(Duration::from_secs(10)).send().await.map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        Ok(resp.status().as_u16())
    }

    // ---- registration and polling ----

    pub async fn register(&self, req: &RegisterRequest) -> Result<RegisterResponse, ApiError> {
        // No credential yet: `request` adds none because `credential` is None on this client.
        let url = self.url(&["register"]);
        self.retrying(5, || self.send_json(Method::POST, url.clone(), req, None)).await
    }

    /// One long poll. Not retried here: the run loop decides how to back off, and a lost
    /// response can mean a job was assigned, which the lease then recovers.
    pub async fn request_job(&self, req: &JobRequest) -> Result<Option<JobPayload>, ApiError> {
        let wait = u64::from(req.wait_seconds.unwrap_or(30).min(30));
        let resp = self
            .request(Method::POST, self.url(&["jobs", "request"]), None)
            .timeout(Duration::from_secs(wait + 20))
            .json(req)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        let resp = Self::check(resp).await?;
        let bytes = resp.bytes().await.map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let r: JobResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Rejected { status: 200, message: format!("the job could not be read: {e}") })?;
        Ok(r.job)
    }

    // ---- while a job runs ----

    pub async fn heartbeat(&self, job_id: i64) -> Result<HeartbeatResponse, ApiError> {
        let url = self.url(&["jobs", &job_id.to_string(), "heartbeat"]);
        self.retrying(2, || self.send_json(Method::POST, url.clone(), &HeartbeatRequest {}, None)).await
    }

    pub async fn send_logs(&self, job_id: i64, batch: &LogBatch) -> Result<LogAck, ApiError> {
        let url = self.url(&["jobs", &job_id.to_string(), "logs"]);
        self.retrying(8, || self.send_json(Method::POST, url.clone(), batch, None)).await
    }

    pub async fn update_step(&self, job_id: i64, number: i32, update: &StepUpdate) -> Result<(), ApiError> {
        let url = self.url(&["jobs", &job_id.to_string(), "steps", &number.to_string()]);
        let _: serde_json::Value = self.retrying(6, || self.send_json(Method::POST, url.clone(), update, None)).await?;
        Ok(())
    }

    /// Losing this loses the job's result, so it is tried for several minutes.
    pub async fn complete(&self, job_id: i64, req: &CompleteRequest) -> Result<(), ApiError> {
        let url = self.url(&["jobs", &job_id.to_string(), "complete"]);
        let _: serde_json::Value = self.retrying(14, || self.send_json(Method::POST, url.clone(), req, None)).await?;
        Ok(())
    }

    // ---- artifacts and cache (a job's own token) ----

    pub async fn upload_artifact(
        &self,
        job_token: &str,
        job_id: i64,
        name: &str,
        file: &Path,
        retention_days: Option<u32>,
    ) -> Result<ArtifactUploaded, ApiError> {
        let id = job_id.to_string();
        self.retrying(4, || async {
            let len = tokio::fs::metadata(file).await?.len();
            let f = tokio::fs::File::open(file).await?;
            let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
            let mut url = self.url(&["jobs", &id, "artifacts", name]);
            if let Some(d) = retention_days {
                url.query_pairs_mut().append_pair("retention_days", &d.to_string());
            }
            let resp = self
                .request(Method::PUT, url, Some(job_token))
                .header(header::CONTENT_TYPE, "application/zip")
                .header(header::CONTENT_LENGTH, len)
                .timeout(Duration::from_secs(3600))
                .body(body)
                .send()
                .await
                .map_err(|e| ApiError::Network(e.without_url().to_string()))?;
            let resp = Self::check(resp).await?;
            resp.json::<ArtifactUploaded>()
                .await
                .map_err(|e| ApiError::Rejected { status: 200, message: format!("unreadable answer: {e}") })
        })
        .await
    }

    pub async fn download_artifact(&self, job_token: &str, job_id: i64, name: &str, dest: &Path) -> Result<(), ApiError> {
        let id = job_id.to_string();
        self.retrying(4, || self.download_to(self.url(&["jobs", &id, "artifacts", name]), job_token, dest)).await
    }

    /// Which commit each action resolves to, pinned to the run by the server (ch-ctk.5).
    pub async fn resolve_actions(
        &self,
        job_token: &str,
        job_id: i64,
        actions: &[String],
    ) -> Result<ch_api_types::runner::ActionResolveResponse, ApiError> {
        let id = job_id.to_string();
        let url = self.url(&["jobs", &id, "actions", "resolve"]);
        let body = ch_api_types::runner::ActionResolveRequest { actions: actions.to_vec() };
        self.retrying(4, || self.send_json(Method::POST, url.clone(), &body, Some(job_token))).await
    }

    pub async fn download_action(&self, job_token: &str, job_id: i64, archive: &str, dest: &Path) -> Result<(), ApiError> {
        let id = job_id.to_string();
        self.retrying(4, || self.download_to(self.url(&["jobs", &id, "actions", "archive", archive]), job_token, dest)).await
    }

    pub async fn cache_lookup(&self, job_token: &str, req: &CacheLookupRequest) -> Result<CacheLookupResponse, ApiError> {
        let url = self.url(&["cache", "lookup"]);
        self.retrying(4, || self.send_json(Method::POST, url.clone(), req, Some(job_token))).await
    }

    /// Saves an archive under `key`. A `409` means the key already exists, which is fine:
    /// caches are write-once.
    pub async fn cache_save(&self, job_token: &str, key: &str, version: &str, file: &Path) -> Result<bool, ApiError> {
        let r = self
            .retrying(4, || async {
                let len = tokio::fs::metadata(file).await?.len();
                let f = tokio::fs::File::open(file).await?;
                let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
                let mut url = self.url(&["cache", key]);
                url.query_pairs_mut().append_pair("version", version);
                let resp = self
                    .request(Method::PUT, url, Some(job_token))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, len)
                    .timeout(Duration::from_secs(3600))
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| ApiError::Network(e.without_url().to_string()))?;
                Self::check(resp).await.map(|_| ())
            })
            .await;
        match r {
            Ok(()) => Ok(true),
            Err(ApiError::Conflict(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn cache_download(&self, job_token: &str, id: i64, dest: &Path) -> Result<(), ApiError> {
        let id = id.to_string();
        self.retrying(4, || self.download_to(self.url(&["cache", &id]), job_token, dest)).await
    }

    async fn download_to(&self, url: url::Url, token: &str, dest: &Path) -> Result<(), ApiError> {
        let resp = self
            .request(Method::GET, url, Some(token))
            .timeout(Duration::from_secs(3600))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        let resp = Self::check(resp).await?;
        let mut f = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::Network(e.without_url().to_string()))?;
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy { base: Duration::from_millis(100), max: Duration::from_secs(2) };
        assert_eq!(p.delay(0, 0.5), Duration::from_millis(100));
        assert_eq!(p.delay(1, 0.5), Duration::from_millis(200));
        assert_eq!(p.delay(3, 0.5), Duration::from_millis(800));
        assert_eq!(p.delay(10, 0.5), Duration::from_secs(2));
        assert_eq!(p.delay(200, 0.5), Duration::from_secs(2), "no overflow on a huge attempt count");
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        let p = RetryPolicy { base: Duration::from_millis(1000), max: Duration::from_secs(60) };
        assert_eq!(p.delay(0, 0.0), Duration::from_millis(750));
        assert_eq!(p.delay(0, 1.0), Duration::from_millis(1250));
        assert_eq!(p.delay(0, 9.0), Duration::from_millis(1250), "out of range jitter is clamped");
    }

    #[test]
    fn urls_are_built_under_the_runner_api() {
        let c = Client::new("https://h.example/", None).unwrap();
        assert_eq!(c.url(&["jobs", "request"]).as_str(), "https://h.example/api/runner/v1/jobs/request");
        assert_eq!(
            c.url(&["jobs", "7", "artifacts", "my report/v1"]).as_str(),
            "https://h.example/api/runner/v1/jobs/7/artifacts/my%20report%2Fv1"
        );
        assert_eq!(c.url(&["cache", "cargo-abc/def"]).as_str(), "https://h.example/api/runner/v1/cache/cargo-abc%2Fdef");
    }

    #[test]
    fn bad_server_urls_are_refused() {
        assert!(Client::new("ftp://x", None).is_err());
        assert!(Client::new("not a url", None).is_err());
    }

    #[test]
    fn error_classes() {
        assert!(ApiError::Network("x".into()).is_retryable());
        assert!(ApiError::Server { status: 503, retry_after: None }.is_retryable());
        assert!(!ApiError::Rejected { status: 400, message: String::new() }.is_retryable());
        assert!(!ApiError::Unauthorized.is_retryable());
        assert!(ApiError::NotFound.is_lease_lost());
        assert!(ApiError::Conflict("x".into()).is_lease_lost());
        assert!(!ApiError::Server { status: 500, retry_after: None }.is_lease_lost());
    }
}
