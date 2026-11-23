//! haven-runner: the Codehaven CI runner (ARCHITECTURE.md section 9.5).

pub mod actions;
pub mod backend;
pub mod client;
pub mod commands;
pub mod config;
pub mod credential;
pub mod disk;
pub mod docker;
pub mod expr;
pub mod fsglob;
pub mod job;
pub mod jobenv;
pub mod logs;
pub mod mask;
pub mod options;
pub mod runner;
pub mod safefs;
pub mod shell;

/// The first line of a script, cut to a length that reads well in a group header.
pub(crate) fn fmt_first_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() > 120 {
        let cut: String = first.chars().take(117).collect();
        format!("{cut}...")
    } else {
        first.to_string()
    }
}

#[cfg(feature = "testing")]
pub mod testing;
