//! REST v3 data types, webhook payloads and the runner protocol.
//!
//! The server, the `haven` CLI and the runner all use these, so a change to the API
//! contract breaks the build of whoever depends on it. Every type round-trips through
//! serde and has an `insta` snapshot of its JSON.

// The webhook tests build whole repository payloads with `json!`.
#![recursion_limit = "1024"]

pub mod ci;
pub mod common;
pub mod hooks;
pub mod issues;
pub mod pulls;
pub mod releases;
pub mod repos;
pub mod runner;
pub mod search;
pub mod users;
pub mod webhook;

/// Test helper: the value must survive JSON and match its committed snapshot.
#[cfg(test)]
pub(crate) fn roundtrip<T>(name: &str, v: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_value(v).expect("serialize");
    let back: T = serde_json::from_value(json.clone()).expect("deserialize");
    assert_eq!(&back, v, "{name} did not round-trip");
    insta::assert_json_snapshot!(name, json);
}

/// A fixed timestamp for tests, so snapshots never change.
#[cfg(test)]
pub(crate) fn t0() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z").expect("fixed time").with_timezone(&chrono::Utc)
}

#[cfg(test)]
pub(crate) fn user(login: &str, id: i64) -> common::SimpleUser {
    common::SimpleUser {
        login: login.into(),
        id,
        avatar_url: format!("https://code.example.com/avatars/{id}.svg"),
        url: format!("https://code.example.com/api/v3/users/{login}"),
        html_url: format!("https://code.example.com/{login}"),
        kind: "User".into(),
        site_admin: false,
    }
}
