//! The Codehaven server binary's library: wiring, the deny-by-default gate, security
//! headers, the trusted proxy handling, the internal listener and the subcommands.
//!
//! The foundation stream owns this crate and, after M0, the integration stream does.

pub mod admin;
pub mod api_errors;
pub mod app;
pub mod cli;
pub mod doctor;
pub mod gate;
pub mod headers;
pub mod internal;
pub mod meta;
pub mod metrics;
pub mod public_gate;
pub mod reqinfo;
pub mod serve;
pub mod wiring;
