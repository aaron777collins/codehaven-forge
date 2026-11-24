//! `GET /v2/_catalog` (OCI Distribution Spec, "Listing repositories"): the images the caller
//! could pull, in byte order, paged with `n` and `last`.
//!
//! An image the caller cannot read is left out, exactly as `authorize` would make it look
//! absent, so the list never says more than a pull would. Signed out, only images linked to
//! public repositories are read at all. The rows are checked one by one with the same rule a
//! pull uses (`access::can_pull`), in batches, and a request reads at most `Limits::max_scan`
//! rows (the next-page cursor is then a readable name plus a count, never the name of a row the
//! caller cannot pull): a caller who can see almost nothing in a large registry gets a short
//! page and a `next` link instead of making the server walk the whole table. That count, the
//! `skip` in a next link, is signed for that caller and that `last`, and a skip the server did
//! not issue is refused.

use crate::access::{Package, PullMemo, can_pull};
use crate::error::{RegError, RegResult};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use ch_core::Actor;
use ch_core::Config;
use ch_core::state::AppState;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::OnceLock;

type HmacSha256 = Hmac<Sha256>;

static KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// The most names one page returns, and what an absent `n` means.
pub const MAX_PAGE: i64 = 1000;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Rows read from the database at a time.
    pub batch: i64,
    /// Rows read for one request, readable or not.
    pub max_scan: i64,
}

impl Default for Limits {
    fn default() -> Self {
        // At most this many permission lookups per request, fewer when rows share a repository
        // or an owner (`access::PullMemo`).
        Self { batch: 100, max_scan: 1000 }
    }
}

/// The most rows one signed skip may cover. A skip the server did not issue is refused.
pub const MAX_SKIP: i64 = 100_000;

/// Where a page starts: after the name `last`, then past `skip` more rows. `last` is only ever a
/// name the caller supplied or one the caller can pull. `skip` is how many rows were read since
/// then and not shown, which says how many there were and nothing about what they are called.
/// In a next link `skip` is signed for that caller and that `last`, and a skip the server did
/// not issue is refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cursor {
    pub last: String,
    pub skip: i64,
}

/// The signing key for catalog skip values, derived once per process.
fn key(config: &Config) -> &'static [u8; 32] {
    KEY.get_or_init(|| {
        let mut base = [0u8; 32];
        match std::fs::read(config.master_key_path()) {
            Ok(b) if b.len() == 32 => base.copy_from_slice(&b),
            _ => rand::rng().fill(&mut base),
        }
        let mut mac = HmacSha256::new_from_slice(&base).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
        mac.update(b"codehaven-registry-catalog-v1");
        let mut out = [0u8; 32];
        out.copy_from_slice(&mac.finalize().into_bytes());
        out
    })
}

/// A stable label for who is calling, so a skip is bound to the caller it was issued to.
fn caller(actor: &Actor) -> String {
    match actor {
        Actor::Anonymous => "anon".into(),
        Actor::User { id, .. } => format!("u{}", id.0),
        Actor::Token { user_id, .. } => format!("u{}", user_id.0),
        Actor::Job { job_id, .. } => format!("j{}", job_id.0),
        Actor::Runner { runner_id } => format!("r{}", runner_id.0),
        Actor::App { grant_id, .. } => format!("a{}", grant_id.0),
        Actor::System => "system".into(),
    }
}

fn hmac_hex(key: &[u8; 32], msg: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Compares without stopping at the first different byte.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The signed skip value for a next link: bound to this caller and this `last`.
pub fn sign_skip(config: &Config, actor: &Actor, last: &str, skip: i64) -> String {
    format!("{skip}.{}", hmac_hex(key(config), &format!("catalog-skip|{}|{last}|{skip}", caller(actor))))
}

/// Reads back a skip only when it is exactly the value this server issued to this caller for
/// this `last`, or a bare `0`. Anything else, a leading zero included, is `None`. A signed skip
/// does not expire and does not name `n`: it only says how many rows the caller was already
/// told it cannot see, so replaying it later discloses nothing new.
pub fn open_skip(config: &Config, actor: &Actor, last: &str, value: &str) -> Option<i64> {
    if value == "0" {
        return Some(0);
    }
    let (num, sig) = value.split_once('.')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if num.len() > 1 && num.starts_with('0') {
        return None;
    }
    let skip = num.parse::<i64>().ok()?;
    if !(0..=MAX_SKIP).contains(&skip) {
        return None;
    }
    let expected = hmac_hex(key(config), &format!("catalog-skip|{}|{last}|{skip}", caller(actor)));
    if !ct_eq(sig.as_bytes(), expected.as_bytes()) {
        return None;
    }
    Some(skip)
}

/// Percent-encodes a query value: every byte except ASCII letters, digits, `-`, `.`, `_`, `~`
/// and `/` becomes `%XX` with uppercase hex. An image name stays readable; anything else a
/// caller sent as `last` (such as `&` or `+`) cannot change the next link's meaning.
fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `Link` header value for the next page. `signed_skip` is the signed value this server
/// issued, used only when the next page starts past rows the caller cannot see.
pub fn link(n: i64, next: &Cursor, signed_skip: &str) -> String {
    let skip = if next.skip > 0 { format!("&skip={signed_skip}") } else { String::new() };
    format!("</v2/_catalog?n={n}&last={}{skip}>; rel=\"next\"", query_value(&next.last))
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    owner_id: i64,
    repo_id: Option<i64>,
    owner_name: String,
    name: String,
    owner_kind: String,
    full: String,
    orphaned: bool,
}

/// One page of the catalog: the readable names after `from` (at most `n`), and the cursor for
/// the next page when there may be more.
pub async fn page(state: &AppState, actor: &Actor, n: i64, from: &Cursor, limits: Limits) -> RegResult<(Vec<String>, Option<Cursor>)> {
    if n <= 0 {
        return Ok((Vec::new(), None));
    }
    // `after` is where the database reads from and may be a name the caller cannot pull; it
    // never leaves this function. `resume` is what may be handed back.
    let mut after = from.last.clone();
    let mut offset = from.skip;
    let mut resume = from.clone();
    let mut seen = 0i64;
    let mut names: Vec<String> = Vec::new();
    let mut memo = PullMemo::default();
    loop {
        // Signed out ($4), rows the caller could never pull are not even read, so neither an
        // offset nor the scan budget can count them. The per-row rule stays `can_pull`.
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT p.id, p.owner_id, p.repo_id, u.username::text AS owner_name, p.name::text AS name, u.kind AS owner_kind, p.orphaned, \
                    (u.username::text || '/' || p.name::text) AS \"full\" \
             FROM packages p JOIN users u ON u.id = p.owner_id \
             WHERE p.kind = 'container' AND (u.username::text || '/' || p.name::text) COLLATE \"C\" > $1 \
               AND (NOT $4 OR EXISTS (SELECT 1 FROM repositories r WHERE r.id = p.repo_id AND r.access_tier = 'public')) \
             ORDER BY (u.username::text || '/' || p.name::text) COLLATE \"C\" LIMIT $2 OFFSET $3",
        )
        .bind(&after)
        .bind(limits.batch)
        .bind(offset)
        .bind(actor.is_anonymous())
        .fetch_all(&state.db)
        .await?;
        offset = 0;
        let full_batch = rows.len() as i64 == limits.batch;
        for row in rows {
            seen += 1;
            after.clone_from(&row.full);
            let pkg = Package {
                id: row.id,
                owner_id: row.owner_id,
                repo_id: row.repo_id,
                owner_name: row.owner_name,
                name: row.name,
                orphaned: row.orphaned,
            };
            if can_pull(state, actor, &pkg, row.owner_kind == "org", &mut memo).await? {
                if names.len() as i64 == n {
                    // One readable name past the page: there is a next page, starting after the
                    // last name of this one.
                    let next = names.last().map(|l| Cursor { last: l.clone(), skip: 0 });
                    return Ok((names, next));
                }
                resume = Cursor { last: row.full.clone(), skip: 0 };
                names.push(row.full);
            } else {
                resume.skip += 1;
            }
            if seen >= limits.max_scan {
                // Out of budget: hand back what was found and let the client continue from the
                // last name it may see, plus a count of the rows read since.
                return Ok((names, Some(resume)));
            }
        }
        if !full_batch {
            return Ok((names, None));
        }
    }
}

/// `GET` and `HEAD /v2/_catalog`.
pub async fn get(state: &AppState, actor: &Actor, q: &HashMap<String, String>, head: bool) -> RegResult<Response> {
    if !crate::access::may_list(actor) {
        return Err(RegError::denied("this token needs the read:packages scope"));
    }
    let n = match q.get("n") {
        None => MAX_PAGE,
        Some(v) => v.parse::<i64>().map_err(|_| RegError::unsupported(StatusCode::BAD_REQUEST, "n must be a whole number"))?.min(MAX_PAGE),
    };
    if n < 0 {
        return Err(RegError::unsupported(StatusCode::BAD_REQUEST, "n must not be negative"));
    }
    let last = q.get("last").map(String::as_str).unwrap_or_default();
    if last.contains('\0') {
        return Err(RegError::unsupported(StatusCode::BAD_REQUEST, "last must not contain a NUL byte"));
    }
    let skip = match q.get("skip") {
        None => 0,
        Some(v) => open_skip(&state.config, actor, last, v).ok_or_else(|| {
            RegError::unsupported(StatusCode::BAD_REQUEST, "skip must be the value from the next link this server sent you")
        })?,
    };
    let (repositories, next) = page(state, actor, n, &Cursor { last: last.to_string(), skip }, Limits::default()).await?;
    let body = serde_json::json!({ "repositories": repositories }).to_string();
    let mut r = Response::new(if head { Body::empty() } else { Body::from(body) });
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert("docker-distribution-api-version", HeaderValue::from_static("registry/2.0"));
    if let Some(next) = next
        && let Ok(v) = HeaderValue::from_str(&link(n, &next, &sign_skip(&state.config, actor, &next.last, next.skip)))
    {
        h.insert(header::LINK, v);
    }
    Ok(r)
}
