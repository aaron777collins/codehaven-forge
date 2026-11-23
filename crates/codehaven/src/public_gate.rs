//! The signed-out door to public repositories (epic ch-4pn, spec section 5).
//!
//! Two locks. The gate admits a signed-out request only when its method and path are on
//! `ch_core::access::PUBLIC_REPO_ROUTES`, the instance allows public repositories, and the
//! repository (or the owner) it names is public right now. A layer inside the router then
//! checks the router picked that same route. After that, `Authz` still has to answer `View`.
//!
//! Anything else gets exactly the rejection a signed-out request got before public
//! repositories existed, so private, internal and missing look the same.

use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ch_auth::ratelimit::Limiter;
use ch_core::access::{self, Cost, PublicMatch, PublicRead, PublicTarget};
use ch_core::config::PublicAccessConfig;
use ch_core::state::AppState;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The signed-out caps, one set per process.
pub struct PublicLimits {
    lookups: Arc<Semaphore>,
    instance: HashMap<Cost, Arc<Semaphore>>,
    per_address: HashMap<Cost, usize>,
    held: Arc<Mutex<HashMap<(Cost, String), usize>>>,
    git_rate: Limiter,
    archive_rate: Limiter,
    instance_rate: Limiter,
    /// Set when a lookup failed: until then the gate refuses without asking the database.
    backoff: Mutex<Option<std::time::Instant>>,
}

/// Held for as long as the response body is being sent. Dropping it gives the capacity back.
pub struct CostPermit {
    _instance: Option<OwnedSemaphorePermit>,
    held: Arc<Mutex<HashMap<(Cost, String), usize>>>,
    key: Option<(Cost, String)>,
}

impl Drop for CostPermit {
    fn drop(&mut self) {
        if let Some(k) = self.key.take()
            && let Ok(mut m) = self.held.lock()
            && let Some(n) = m.get_mut(&k)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&k);
            }
        }
    }
}

impl PublicLimits {
    pub fn new(c: &PublicAccessConfig) -> Self {
        let n = |v: u32| v.max(1) as usize;
        let mut instance = HashMap::new();
        let mut per_address = HashMap::new();
        for (cost, inst, addr) in [
            (Cost::Heavy, c.heavy_instance, c.heavy_per_address),
            (Cost::Git, c.git_instance, c.git_per_address),
            (Cost::Archive, c.archive_instance, c.archive_per_address),
            (Cost::Raw, c.raw_instance, c.raw_per_address),
        ] {
            instance.insert(cost, Arc::new(Semaphore::new(n(inst))));
            per_address.insert(cost, n(addr));
        }
        Self {
            lookups: Arc::new(Semaphore::new(n(c.lookups_at_once))),
            instance,
            per_address,
            held: Arc::new(Mutex::new(HashMap::new())),
            git_rate: Limiter::new(c.git_per_address_per_minute.max(1), c.git_per_address_per_minute.max(1)),
            archive_rate: Limiter::new(c.archive_per_address_per_minute.max(1), c.archive_per_address_per_minute.max(1)),
            instance_rate: Limiter::new(c.instance_requests_per_minute.max(1), c.instance_requests_per_minute.max(1)),
            backoff: Mutex::new(None),
        }
    }

    /// The process-wide limits, made from the first configuration that asks.
    pub fn get(c: &PublicAccessConfig) -> &'static PublicLimits {
        static L: OnceLock<PublicLimits> = OnceLock::new();
        L.get_or_init(|| PublicLimits::new(c))
    }

    /// Takes capacity for one request of this cost from this address, or says how long to wait.
    pub fn take(&self, cost: Cost, addr: &str) -> Result<CostPermit, u64> {
        if self.instance_rate.hit("instance").is_err() {
            return Err(5);
        }
        match cost {
            Cost::Git => self.git_rate.hit(addr)?,
            Cost::Archive => self.archive_rate.hit(addr)?,
            _ => {}
        }
        let Some(sem) = self.instance.get(&cost) else {
            return Ok(CostPermit { _instance: None, held: self.held.clone(), key: None });
        };
        let max = self.per_address.get(&cost).copied().unwrap_or(1);
        let key = (cost, addr.to_string());
        {
            let mut m = self.held.lock().unwrap_or_else(|e| e.into_inner());
            // A hard size, so a flood of addresses cannot grow the map without bound.
            if m.len() > 50_000 && !m.contains_key(&key) {
                return Err(5);
            }
            let n = m.entry(key.clone()).or_insert(0);
            if *n >= max {
                return Err(5);
            }
            *n += 1;
        }
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                drop(CostPermit { _instance: None, held: self.held.clone(), key: Some(key) });
                return Err(5);
            }
        };
        Ok(CostPermit { _instance: Some(permit), held: self.held.clone(), key: Some(key) })
    }
}

/// What the gate decided for a signed-out request on a public-looking route.
pub enum Admit {
    /// Let it through with this marker and the capacity it holds.
    Yes(PublicRead, CostPermit),
    /// Too many at once from this address or for the instance.
    Busy(u64),
    /// Not public, not found, switch off, or anything else: the ordinary rejection.
    No,
}

/// Is this signed-out request one the public list admits right now?
pub async fn admit(state: &AppState, m: &PublicMatch<'_>, addr: &str) -> Admit {
    let limits = PublicLimits::get(&state.config.public_access);
    // While the database is failing, refuse at once instead of queueing strangers behind it.
    if limits.backoff.lock().map(|b| b.is_some_and(|until| std::time::Instant::now() < until)).unwrap_or(false) {
        return Admit::No;
    }
    let Ok(_lookup) = limits.lookups.clone().try_acquire_owned() else { return Admit::No };
    let lookup = lookup_target(state, m);
    let ok = match tokio::time::timeout(std::time::Duration::from_secs(2), lookup).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) | Err(_) => {
            if let Ok(mut b) = limits.backoff.lock() {
                *b = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
            }
            false
        }
    };
    if !ok {
        return Admit::No;
    }
    match limits.take(m.rule.cost, addr) {
        Ok(p) => Admit::Yes(PublicRead { pattern: m.rule.pattern, cost: m.rule.cost }, p),
        Err(wait) => Admit::Busy(wait),
    }
}

/// Is the target public now? An error is the database's, never an answer.
async fn lookup_target(state: &AppState, m: &PublicMatch<'_>) -> Result<bool, sqlx::Error> {
    Ok(match m.rule.target {
        PublicTarget::Discovery => switch_on(state).await?,
        PublicTarget::Owner => match m.owner {
            Some(o) if access::valid_owner_name(o) => owner_is_public(state, o).await?,
            _ => false,
        },
        PublicTarget::Repo => match (m.owner, m.repo.and_then(access::valid_repo_name)) {
            (Some(o), Some(r)) if access::valid_owner_name(o) => repo_is_public(state, o, r).await?,
            _ => false,
        },
    })
}

async fn switch_on(state: &AppState) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, bool>("SELECT allow_public_repos FROM visibility_settings")
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(false))
}

async fn owner_is_public(state: &AppState, owner: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<bool>>(
        "SELECT EXISTS (SELECT 1 FROM users u JOIN repositories r ON r.owner_id = u.id \
         WHERE u.username = $1::citext AND r.access_tier = 'public') \
         AND (SELECT allow_public_repos FROM visibility_settings)",
    )
    .bind(owner)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false))
}

/// The same lookup the handlers make (the name, then an old name), filtered to public. An old
/// name counts only when it was recorded while the repository was public.
async fn repo_is_public(state: &AppState, owner: &str, name: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<bool>>(
        "WITH named AS (SELECT r.id, r.access_tier FROM repositories r JOIN users u ON u.id = r.owner_id \
                         WHERE u.username = $1::citext AND r.name = $2::citext), \
              moved AS (SELECT r.id, r.access_tier FROM repo_redirects x JOIN repositories r ON r.id = x.repo_id \
                         WHERE x.old_owner = $1::citext AND x.old_name = $2::citext AND x.audience = 'public' \
                         AND NOT EXISTS (SELECT 1 FROM named)) \
         SELECT EXISTS (SELECT 1 FROM named WHERE access_tier = 'public' UNION ALL SELECT 1 FROM moved WHERE access_tier = 'public') \
                AND (SELECT allow_public_repos FROM visibility_settings)",
    )
    .bind(owner)
    .bind(name)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false))
}

/// The byte cap and the whole deadline for one signed-out answer of this cost.
fn caps(cost: Cost, c: &PublicAccessConfig) -> (u64, std::time::Duration) {
    const MIB: u64 = 1024 * 1024;
    let secs = std::time::Duration::from_secs;
    match cost {
        Cost::Git => (2048 * MIB, secs(c.git_timeout_secs.max(1))),
        Cost::Archive => (500 * MIB, secs(c.archive_timeout_secs.max(1))),
        Cost::Raw => (2048 * MIB, secs(c.git_timeout_secs.max(1))),
        Cost::Heavy | Cost::Light => (64 * MIB, secs(60)),
    }
}

/// Keeps the capacity a request took until its whole body has been sent, and stops a body that
/// runs past its byte cap, its deadline, or 60 seconds without progress. Stopping aborts the
/// connection: a stranger never gets a file that only looks complete.
pub fn hold_until_sent(resp: Response, permit: CostPermit, cost: Cost, c: &PublicAccessConfig) -> Response {
    let (cap, total) = caps(cost, c);
    let stall = std::time::Duration::from_secs(60);
    let start = tokio::time::Instant::now();
    let (parts, body) = resp.into_parts();
    struct St {
        body: axum::body::BodyDataStream,
        sent: u64,
        done: bool,
        _permit: CostPermit,
    }
    let st = St { body: body.into_data_stream(), sent: 0, done: false, _permit: permit };
    let stream = futures::stream::unfold(st, move |mut st| async move {
        if st.done {
            return None;
        }
        let left = total.saturating_sub(start.elapsed());
        let fail =
            |st: St, why: &'static str| Some((Err::<bytes::Bytes, std::io::Error>(std::io::Error::other(why)), St { done: true, ..st }));
        match tokio::time::timeout(left.min(stall), st.body.next()).await {
            Ok(Some(Ok(chunk))) => {
                st.sent += chunk.len() as u64;
                if st.sent > cap {
                    return fail(st, "over the size limit for signed-out downloads");
                }
                Some((Ok(chunk), st))
            }
            Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e)), St { done: true, ..st })),
            Ok(None) => None,
            Err(_) => fail(st, "past the time limit for signed-out downloads"),
        }
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// The answer when a public read is over its caps. Only ever given for a public repository.
pub fn busy(wait: u64) -> Response {
    let mut r = (StatusCode::TOO_MANY_REQUESTS, "Too many requests. Try again shortly.\n").into_response();
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The second lock, inside the router: a request the gate admitted as signed out runs only
/// the handler of the route it was admitted for.
pub async fn route_check(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(read) = req.extensions().get::<PublicRead>().cloned() else { return next.run(req).await };
    let matched = req.extensions().get::<MatchedPath>().map(|m| m.as_str().to_owned());
    let same = matched.as_deref().is_some_and(|p| access::normalize_route(read.pattern) == access::normalize_route(p));
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let query = req.uri().query().map(str::to_owned);
    let kind = access::classify_for(&path, state.config.mcp.enabled);
    if !same {
        return crate::gate::reject(&state, kind, &method, &path, query.as_deref());
    }
    let resp = next.run(req).await;
    // A handler that refuses a signed-out caller gives the ordinary rejection, never a
    // message that says something exists.
    if matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return crate::gate::reject(&state, kind, &method, &path, query.as_deref());
    }
    resp
}

/// What every admitted signed-out answer carries: never cached anywhere, and no cookie.
pub fn finish(mut resp: Response) -> Response {
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    resp.headers_mut().remove(header::SET_COOKIE);
    resp
}
