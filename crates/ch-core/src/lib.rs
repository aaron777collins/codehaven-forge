//! Contracts shared by every Codehaven crate: ids, errors, config, the caller's identity,
//! the public route table, domain events, service traits and `AppState`.
//!
//! The foundation stream owns this crate. After M0 a change here is a small pull request,
//! announced to every stream (ROADMAP.md section 1).

pub mod access;
pub mod actor;
pub mod apps;
pub mod audit;
pub mod collab;
pub mod config;
pub mod disposition;
pub mod error;
pub mod events;
pub mod identity;
pub mod ids;
pub mod live;
pub mod model;
pub mod net;
pub mod noreply;
pub mod not_built;
pub mod pages;
pub mod services;
pub mod state;
pub mod text;
pub mod web_contract;

pub use actor::{Actor, AppGrant, AppOrigin, AuthVia, JobPerms, Permission, RepoSet, Scopes};
pub use config::Config;
pub use error::{Error, FieldError, Result};
pub use events::DomainEvent;
pub use ids::*;
pub use state::{AppState, AppStateBuilder, Db, Extensions, Install, Worker};
