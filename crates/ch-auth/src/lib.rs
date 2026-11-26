//! Identity: accounts, invites, sessions, two-factor, lockout, personal access tokens, SSH
//! keys, authorization, orgs and the user, org and site admin REST endpoints.
//!
//! The instance is invite-only. There is no sign-up route, no setup page and no proxy-header
//! login. The first admin is made with `codehaven admin create-user` on the server, and every
//! other account comes from an invite (ARCHITECTURE.md sections 7.3 to 7.6).
//!
//! What lives where:
//! - `authn`: session, Basic, bearer, job token, runner credential and SSH key lookup
//! - `authz`: the permission model, as a pure function plus its queries
//! - `accounts`, `directory`, `orgs`: the account lifecycle
//! - `invites`: single-use invite links
//! - `credentials`, `twofactor`: the interactive sign-in flows the web layer drives
//! - `password`, `totp`, `lockout`, `ratelimit`: the primitives
//! - `guard`: CSRF, the Origin check and rate limits, as one middleware
//! - `rest`: the API routes

pub mod accounts;
pub mod apps;
pub mod authn;
pub mod authz;
pub mod core;
pub mod credentials;
pub mod directory;
pub mod guard;
pub mod invites;
pub mod lockout;
pub mod orgs;
pub mod passkeys;
pub mod password;
pub mod ratelimit;
pub mod rest;
pub mod sshkeys;
pub mod sweep;
pub mod totp;
pub mod twofactor;
pub mod util;
pub mod web;

use axum::Router;
use ch_core::Result;
use ch_core::services::{Accounts, Apps, Authn, Authz, Credentials, Directory, Invites, Orgs, TwoFactor};
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use ch_core::web_contract::{People, Sessions};
use std::sync::Arc;

pub use crate::core::{AuthOptions, Core};
pub use crate::guard::Guard;

/// Mounted under `/api/v3`.
pub fn api_routes() -> Router<AppState> {
    rest::routes()
}

/// The five signed-out OAuth routes. Empty while MCP is off.
pub fn oauth_routes(config: &ch_core::Config) -> Router<AppState> {
    apps::routes(config)
}

/// Background loops: the session, link, lockout, invite and audit sweep.
pub fn workers(state: &AppState) -> Vec<Worker> {
    vec![sweep::worker(state)]
}

pub struct AuthServices {
    pub authn: Arc<dyn Authn>,
    pub apps: Arc<dyn Apps>,
    pub invites: Arc<dyn Invites>,
    pub authz: Arc<dyn Authz>,
    pub accounts: Arc<dyn Accounts>,
    pub credentials: Arc<dyn Credentials>,
    pub twofactor: Arc<dyn TwoFactor>,
    pub orgs: Arc<dyn Orgs>,
    pub directory: Arc<dyn Directory>,
    /// What the web pages call (`ch_core::web_contract`).
    pub sessions: Arc<dyn Sessions>,
    pub people: Arc<dyn People>,
}

impl Install for AuthServices {
    fn install(self, b: &mut AppStateBuilder) {
        b.authn = self.authn;
        b.apps = self.apps;
        b.invites = self.invites;
        b.authz = self.authz;
        b.accounts = self.accounts;
        b.credentials = self.credentials;
        b.twofactor = self.twofactor;
        b.orgs = self.orgs;
        b.directory = self.directory;
        b.sessions = self.sessions;
        b.people = self.people;
    }
}

/// Built first: git, collab and the web layer all call `Authn` and `Authz`.
pub fn build(b: &AppStateBuilder) -> Result<AuthServices> {
    build_with(b, AuthOptions::default())
}

/// The same with options. Tests use cheap password hashing and a fixed master key.
pub fn build_with(b: &AppStateBuilder, opts: AuthOptions) -> Result<AuthServices> {
    let mut opts = opts;
    let client_fetcher = opts.client_fetcher.take();
    let core = Arc::new(Core::new(b.db.clone(), b.audit.clone(), b.events.clone(), b.config.clone(), opts)?);
    let authz = Arc::new(authz::AuthzService::new(core.clone()));
    let fetcher = client_fetcher.unwrap_or_else(|| Arc::new(apps::cimd::HttpFetcher));
    b.ext.insert(Arc::new(apps::OAuthLimits::new()));
    b.ext.insert(Arc::new(guard::SignedOutRejections::new(&b.config)));
    let accounts: Arc<dyn Accounts> = Arc::new(accounts::AccountsService::new(core.clone()));
    let credentials: Arc<dyn Credentials> = Arc::new(credentials::CredentialsService::new(core.clone()));
    let twofactor: Arc<dyn TwoFactor> = Arc::new(twofactor::TwoFactorService::new(core.clone()));
    let orgs: Arc<dyn Orgs> = Arc::new(orgs::OrgsService::new(core.clone(), authz.clone()));
    let directory: Arc<dyn Directory> = Arc::new(directory::DirectoryService::new(core.clone()));
    Ok(AuthServices {
        authn: Arc::new(authn::AuthnService::new(core.clone())),
        apps: Arc::new(apps::AppsService::new(core.clone(), authz.clone(), fetcher)),
        invites: Arc::new(invites::InvitesService::new(core.clone())),
        authz,
        sessions: Arc::new(web::SessionsSvc {
            core: core.clone(),
            credentials: credentials.clone(),
            twofactor: twofactor.clone(),
            directory: directory.clone(),
            accounts: accounts.clone(),
        }),
        people: Arc::new(web::PeopleSvc { core, orgs: orgs.clone() }),
        accounts,
        credentials,
        twofactor,
        orgs,
        directory,
    })
}
