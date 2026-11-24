//! `AppState`, the builder the wiring uses, and `Worker`.
//!
//! Every crate exposes the same four entry points (ARCHITECTURE.md section 3.3):
//! `api_routes()`, `web_routes()` (ch-web), `workers(&AppState)` and
//! `build(&AppStateBuilder) -> Result<Services>`. `crates/codehaven/src/wiring.rs` calls
//! all of them, so a stream only fills in bodies.

use crate::config::Config;
use crate::error::Result;
use crate::not_built::NotBuilt;
use crate::services::*;
use crate::web_contract::{People, Pipelines, Sessions, Tracker};
use futures::future::BoxFuture;
use sqlx::PgPool;
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub type Db = PgPool;

/// A small typed map so a crate can keep its concrete service where its own routes and
/// workers can find it again, without a process-wide global. Added by the git stream:
/// `build` inserts the concrete type, `api_routes` handlers and `workers` read it back with
/// `state.ext.get::<T>()`.
#[derive(Default)]
pub struct Extensions {
    map: std::sync::RwLock<std::collections::HashMap<std::any::TypeId, Arc<dyn std::any::Any + Send + Sync>>>,
}

impl Extensions {
    pub fn insert<T: std::any::Any + Send + Sync>(&self, value: Arc<T>) {
        if let Ok(mut m) = self.map.write() {
            m.insert(std::any::TypeId::of::<T>(), value);
        }
    }
    pub fn get<T: std::any::Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let m = self.map.read().ok()?;
        m.get(&std::any::TypeId::of::<T>()).cloned().and_then(|a| a.downcast::<T>().ok())
    }
}

macro_rules! services {
    ($($field:ident : $tr:ident),+ $(,)?) => {
        /// Cheap to clone: every field is an `Arc` or a pool handle.
        #[derive(Clone)]
        pub struct AppState {
            pub config: Arc<Config>,
            pub db: Db,
            pub audit: Arc<dyn Audit>,
            pub events: Arc<dyn EventBus>,
            pub blobs: Arc<dyn BlobStore>,
            pub ext: Arc<Extensions>,
            $(pub $field: Arc<dyn $tr>,)+
        }

        /// Filled in order by the wiring. Services a stream has not built yet are `NotBuilt`,
        /// which refuses everything, so an unbuilt crate can never grant access by accident.
        pub struct AppStateBuilder {
            pub config: Arc<Config>,
            pub db: Db,
            pub audit: Arc<dyn Audit>,
            pub events: Arc<dyn EventBus>,
            pub blobs: Arc<dyn BlobStore>,
            pub ext: Arc<Extensions>,
            $(pub $field: Arc<dyn $tr>,)+
        }

        impl AppStateBuilder {
            pub fn new(config: Arc<Config>, db: Db, audit: Arc<dyn Audit>, events: Arc<dyn EventBus>, blobs: Arc<dyn BlobStore>) -> Self {
                Self { config, db, audit, events, blobs, ext: Arc::new(Extensions::default()), $($field: Arc::new(NotBuilt),)+ }
            }

            pub fn build(self) -> AppState {
                AppState {
                    config: self.config,
                    db: self.db,
                    audit: self.audit,
                    events: self.events,
                    blobs: self.blobs,
                    ext: self.ext,
                    $($field: self.$field,)+
                }
            }
        }
    };
}

services! {
    authn: Authn,
    apps: Apps,
    invites: Invites,
    authz: Authz,
    accounts: Accounts,
    credentials: Credentials,
    twofactor: TwoFactor,
    orgs: Orgs,
    directory: Directory,
    repos: Repos,
    git: GitService,
    git_transport: GitTransport,
    issues: Issues,
    pulls: Pulls,
    reviews: Reviews,
    releases: Releases,
    search: Search,
    notifications: Notifications,
    webhooks: Webhooks,
    checks: Checks,
    ci: Ci,
    sessions: Sessions,
    people: People,
    tracker: Tracker,
    pipelines: Pipelines,
    mail: Mail,
}

/// What a crate's `build` returns. `install` moves its services into the builder.
pub trait Install {
    fn install(self, builder: &mut AppStateBuilder);
}

/// A background loop. It must stop promptly when the token is cancelled.
pub struct Worker {
    pub name: &'static str,
    pub task: Box<dyn FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send>,
}

impl Worker {
    pub fn new<F, Fut>(name: &'static str, f: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Worker { name, task: Box::new(move |t| Box::pin(f(t))) }
    }

    pub fn spawn(self, token: CancellationToken) -> tokio::task::JoinHandle<()> {
        let name = self.name;
        let fut = (self.task)(token);
        tokio::spawn(async move {
            tracing::info!(worker = name, "worker started");
            fut.await;
            tracing::info!(worker = name, "worker stopped");
        })
    }
}

/// Convenience for `build` functions that have nothing to construct yet.
pub fn ok<T>(v: T) -> Result<T> {
    Ok(v)
}
