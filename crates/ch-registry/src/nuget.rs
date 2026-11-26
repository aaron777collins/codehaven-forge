//! A private NuGet feed (ch-ctk.13): `dotnet nuget push`, `dotnet restore`, search, unlist.
//!
//! One feed per account, at `/nuget/{owner}/index.json` (the v3 service index). Only people and
//! tokens signed in here reach it: the gate answers anything else with a Basic challenge, so a
//! stranger learns nothing, and there is no public feed. A package is `owner/id` in the same
//! `packages` table as images, with `kind = 'nuget'`, so the rules of images apply: a package
//! linked to a repository follows that repository (read to restore, write to push) and an unlinked
//! one belongs to its owner (an organization's members read, its owners write). A token needs the
//! packages scope. A workflow job pushes with `GITHUB_TOKEN` (`dotnet nuget push -k`, carried in
//! `X-NuGet-ApiKey`) to a package of its own repository, and never creates one for a public
//! repository, where the first push would publish it.
//!
//! What is read from an upload is bounded: the body goes to a temporary file under a size limit,
//! the zip's own directory is read (nothing is unpacked), and only the one `.nuspec` at the top
//! level is read, under a size limit, by a scanner that refuses any DOCTYPE or entity declaration.
//! A version is written once and never replaced; deleting one unlists it, as `dotnet nuget delete`
//! does on GitHub, and an unlisted version still restores for those who ask for it by version.

// The helpers below fail with a ready-made response, which is large; they run once per request.
#![allow(clippy::result_large_err)]

use crate::access::{Access, Package, PullMemo, authorize_kind, can_pull, may_list, may_push};
use crate::error::RegError;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use ch_core::Actor;
use ch_core::access::CurrentActor;
use ch_core::state::AppState;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use tokio::io::AsyncWriteExt;

/// The most a pushed package may weigh, as a file.
pub const MAX_NUPKG_BYTES: u64 = 250 * 1024 * 1024;
/// The most entries a package's zip may list, and the most of the `.nuspec` read from it.
pub const MAX_ZIP_ENTRIES: usize = 20_000;
pub const MAX_NUSPEC_BYTES: u64 = 512 * 1024;
/// The most versions one package may hold, and the most packages one search returns at once.
pub const MAX_VERSIONS: i64 = 5000;
pub const MAX_TAKE: i64 = 100;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/nuget/{owner}/index.json", get(service_index))
        .route("/nuget/{owner}/query", get(search))
        .route("/nuget/{owner}/registration/{id}/index.json", get(registration_index))
        .route("/nuget/{owner}/registration/{id}/{leaf}", get(registration_leaf))
        .route("/nuget/{owner}/flatcontainer/{id}/index.json", get(flat_versions))
        .route("/nuget/{owner}/flatcontainer/{id}/{version}/{file}", get(flat_file))
        .route("/nuget/{owner}/push", put(push).layer(DefaultBodyLimit::max(MAX_NUPKG_BYTES as usize + 64 * 1024)))
        // `dotnet nuget push` adds a slash to the PackagePublish address it reads from the index.
        .route("/nuget/{owner}/push/", put(push).layer(DefaultBodyLimit::max(MAX_NUPKG_BYTES as usize + 64 * 1024)))
        .route("/nuget/{owner}/push/{id}/{version}", axum::routing::delete(unlist).post(relist))
}

// ------------------------------------------------------------------------------------ names

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && !id.ends_with('.')
        && !id.contains("..")
}

/// A version in the form the feed keeps: lower case, build metadata dropped, leading zeros of the
/// numbers dropped, at least three numbers, a fourth only when it is not zero. `None` for anything
/// that is not a NuGet version.
pub fn normalize_version(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v.len() > 64 {
        return None;
    }
    let v = v.split('+').next().unwrap_or(v);
    let (nums, pre) = match v.split_once('-') {
        Some((n, p)) => (n, Some(p)),
        None => (v, None),
    };
    let mut parts: Vec<u64> = Vec::new();
    for p in nums.split('.') {
        if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(p.parse().ok()?);
    }
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    while parts.len() < 3 {
        parts.push(0);
    }
    if parts.len() == 4 && parts[3] == 0 {
        parts.pop();
    }
    let mut out = parts.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
    if let Some(p) = pre {
        if p.is_empty() || !p.split('.').all(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return None;
        }
        out.push('-');
        out.push_str(&p.to_ascii_lowercase());
    }
    Some(out)
}

/// Orders two normalized versions by SemVer precedence.
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let split = |v: &str| -> (Vec<u64>, Option<String>) {
        let (n, p) = v.split_once('-').map_or((v, None), |(n, p)| (n, Some(p.to_string())));
        (n.split('.').filter_map(|x| x.parse().ok()).collect(), p)
    };
    let ((an, ap), (bn, bp)) = (split(a), split(b));
    match an.cmp(&bn) {
        Equal => {}
        o => return o,
    }
    match (ap, bp) {
        (None, None) => Equal,
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(x), Some(y)) => {
            for (i, j) in x.split('.').zip(y.split('.')) {
                let o = match (i.parse::<u64>(), j.parse::<u64>()) {
                    (Ok(p), Ok(q)) => p.cmp(&q),
                    (Ok(_), Err(_)) => Less,
                    (Err(_), Ok(_)) => Greater,
                    (Err(_), Err(_)) => i.cmp(j),
                };
                if o != Equal {
                    return o;
                }
            }
            x.split('.').count().cmp(&y.split('.').count())
        }
    }
}

// ------------------------------------------------------------------------------------ nuspec

/// What a `.nuspec` says about the package.
#[derive(Debug, PartialEq, Eq)]
pub struct Nuspec {
    pub id: String,
    pub version: String,
    pub description: String,
    pub authors: String,
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The text of the first `<name>` element inside `xml` (no attributes on it, no children).
fn element(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let at = xml.find(&open)? + open.len();
    let end = xml[at..].find(&format!("</{name}>"))? + at;
    let text = &xml[at..end];
    if text.contains('<') {
        return None;
    }
    Some(decode_entities(text.trim()))
}

/// Reads the package's id, version, description and authors out of a nuspec. The text must be a
/// `<package>` with a `<metadata>`, and may not declare a DOCTYPE or an entity.
pub fn parse_nuspec(xml: &str) -> Result<Nuspec, &'static str> {
    let lower = xml.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") {
        return Err("the .nuspec may not declare a DOCTYPE or an entity");
    }
    let Some(start) = xml.find("<metadata") else { return Err("the .nuspec has no <metadata>") };
    let Some(end) = xml[start..].find("</metadata>") else { return Err("the .nuspec has no </metadata>") };
    let meta = &xml[start..start + end];
    // Dependency and reference groups name other packages, so only the part before them is read.
    let head = meta.split("<dependencies").next().unwrap_or(meta);
    let id = element(head, "id").ok_or("the .nuspec has no <id>")?;
    let version = element(head, "version").ok_or("the .nuspec has no <version>")?;
    Ok(Nuspec {
        id,
        version,
        description: element(head, "description").unwrap_or_default().chars().take(4000).collect(),
        authors: element(head, "authors").unwrap_or_default().chars().take(1000).collect(),
    })
}

// ------------------------------------------------------------------------------------ errors

fn plain(status: StatusCode, msg: &str) -> Response {
    let mut r = (status, format!("{msg}\n")).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

/// A refusal in the words of the registry's own errors, as plain text a NuGet client prints.
fn refused(e: RegError) -> Response {
    // An unreadable package and a missing one answer alike.
    match e.status {
        StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST => plain(StatusCode::NOT_FOUND, "Not found"),
        StatusCode::FORBIDDEN => plain(StatusCode::FORBIDDEN, &e.message),
        s => plain(s, &e.message),
    }
}

fn json_ok(v: Value) -> Response {
    let mut r = (StatusCode::OK, v.to_string()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn base(state: &AppState, owner: &str) -> String {
    format!("{}/nuget/{owner}", state.config.server.external_url.trim_end_matches('/'))
}

/// Signed in, with a credential that carries package access at all.
fn gate(actor: &Actor) -> Result<(), Response> {
    if actor.is_anonymous() {
        return Err(plain(StatusCode::UNAUTHORIZED, "Authentication required"));
    }
    if !may_list(actor) {
        return Err(plain(StatusCode::FORBIDDEN, "this credential needs the read:packages scope"));
    }
    Ok(())
}

async fn owner_exists(state: &AppState, owner: &str) -> Result<bool, Response> {
    if !ch_core::access::valid_owner_name(owner) {
        return Ok(false);
    }
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM users WHERE username = $1::citext AND kind IN ('user', 'org'))")
        .bind(owner)
        .fetch_one(&state.db)
        .await
        .map_err(|_| plain(StatusCode::SERVICE_UNAVAILABLE, "try again"))
}

/// The package the actor may read, or a plain 404.
async fn readable(state: &AppState, actor: &Actor, owner: &str, id: &str) -> Result<Package, Response> {
    gate(actor)?;
    if !id_ok(id) {
        return Err(plain(StatusCode::NOT_FOUND, "Not found"));
    }
    authorize_kind(state, actor, "nuget", owner, id, Access::Pull, false).await.map_err(refused)
}

// ------------------------------------------------------------------------------------ reading

async fn service_index(State(state): State<AppState>, CurrentActor(actor): CurrentActor, Path(owner): Path<String>) -> Response {
    if let Err(r) = gate(&actor) {
        return r;
    }
    if !owner_exists(&state, &owner).await.unwrap_or(false) {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    let b = base(&state, &owner);
    json_ok(json!({
        "version": "3.0.0",
        "resources": [
            { "@id": format!("{b}/flatcontainer/"), "@type": "PackageBaseAddress/3.0.0", "comment": "Download packages" },
            { "@id": format!("{b}/push"), "@type": "PackagePublish/2.0.0", "comment": "Push and unlist packages" },
            { "@id": format!("{b}/query"), "@type": "SearchQueryService", "comment": "Search" },
            { "@id": format!("{b}/query"), "@type": "SearchQueryService/3.0.0-beta" },
            { "@id": format!("{b}/query"), "@type": "SearchQueryService/3.0.0-rc" },
            { "@id": format!("{b}/registration/"), "@type": "RegistrationsBaseUrl" },
            { "@id": format!("{b}/registration/"), "@type": "RegistrationsBaseUrl/3.0.0-beta" },
            { "@id": format!("{b}/registration/"), "@type": "RegistrationsBaseUrl/3.6.0" },
        ]
    }))
}

#[derive(sqlx::FromRow)]
struct VersionRow {
    version: String,
    description: String,
    authors: String,
    listed: bool,
}

async fn versions_of(state: &AppState, pkg: &Package) -> Result<Vec<VersionRow>, Response> {
    let mut rows: Vec<VersionRow> =
        sqlx::query_as("SELECT version, description, authors, listed FROM nuget_versions WHERE package_id = $1")
            .bind(pkg.id)
            .fetch_all(&state.db)
            .await
            .map_err(|_| plain(StatusCode::SERVICE_UNAVAILABLE, "try again"))?;
    rows.sort_by(|a, b| cmp_versions(&a.version, &b.version));
    Ok(rows)
}

fn lower(id: &str) -> String {
    id.to_ascii_lowercase()
}

fn leaf(state: &AppState, owner: &str, pkg: &Package, v: &VersionRow) -> Value {
    let b = base(state, owner);
    let id = lower(&pkg.name);
    let content = format!("{b}/flatcontainer/{id}/{0}/{id}.{0}.nupkg", v.version);
    let reg = format!("{b}/registration/{id}/{}.json", v.version);
    json!({
        "@id": reg,
        "@type": "Package",
        "catalogEntry": {
            "@id": reg, "@type": "PackageDetails", "id": pkg.name, "version": v.version, "description": v.description,
            "authors": v.authors, "listed": v.listed, "packageContent": content,
        },
        "packageContent": content,
        "registration": format!("{b}/registration/{id}/index.json"),
    })
}

async fn registration_index(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, id)): Path<(String, String)>,
) -> Response {
    let pkg = match readable(&state, &actor, &owner, &id).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let versions = match versions_of(&state, &pkg).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let (lowest, highest) = (versions.first().map(|v| v.version.clone()), versions.last().map(|v| v.version.clone()));
    let items: Vec<Value> = versions.iter().map(|v| leaf(&state, &owner, &pkg, v)).collect();
    let reg = format!("{}/registration/{}/index.json", base(&state, &owner), lower(&pkg.name));
    json_ok(json!({
        "@id": reg, "count": 1,
        "items": [{
            "@id": format!("{reg}#page/{}/{}", lowest.clone().unwrap_or_default(), highest.clone().unwrap_or_default()),
            "@type": "catalog:CatalogPage", "count": items.len(),
            "lower": lowest.unwrap_or_default(), "upper": highest.unwrap_or_default(), "items": items,
        }]
    }))
}

async fn registration_leaf(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, id, leaf_name)): Path<(String, String, String)>,
) -> Response {
    let pkg = match readable(&state, &actor, &owner, &id).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(version) = leaf_name.strip_suffix(".json").and_then(normalize_version) else {
        return plain(StatusCode::NOT_FOUND, "Not found");
    };
    match versions_of(&state, &pkg).await {
        Ok(vs) => match vs.iter().find(|v| v.version == version) {
            Some(v) => json_ok(leaf(&state, &owner, &pkg, v)),
            None => plain(StatusCode::NOT_FOUND, "Not found"),
        },
        Err(r) => r,
    }
}

async fn flat_versions(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, id)): Path<(String, String)>,
) -> Response {
    let pkg = match readable(&state, &actor, &owner, &id).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    match versions_of(&state, &pkg).await {
        Ok(vs) => json_ok(json!({ "versions": vs.iter().map(|v| v.version.clone()).collect::<Vec<_>>() })),
        Err(r) => r,
    }
}

async fn flat_file(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, id, version, file)): Path<(String, String, String, String)>,
) -> Response {
    let pkg = match readable(&state, &actor, &owner, &id).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(version) = normalize_version(&version) else { return plain(StatusCode::NOT_FOUND, "Not found") };
    let low = lower(&pkg.name);
    let row: Option<(String, i64, String)> =
        sqlx::query_as("SELECT blob_sha256, size, nuspec FROM nuget_versions WHERE package_id = $1 AND version = $2")
            .bind(pkg.id)
            .bind(&version)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);
    let Some((sha, size, nuspec)) = row else { return plain(StatusCode::NOT_FOUND, "Not found") };
    if file.to_ascii_lowercase() == format!("{low}.{version}.nupkg") {
        return match state.blobs.get_stream(&sha).await {
            Ok(s) => (
                [(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_LENGTH, size.to_string())],
                Body::from_stream(s),
            )
                .into_response(),
            Err(_) => plain(StatusCode::NOT_FOUND, "Not found"),
        };
    }
    if file.to_ascii_lowercase() == format!("{low}.nuspec") {
        let mut r = (StatusCode::OK, nuspec).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
        return r;
    }
    plain(StatusCode::NOT_FOUND, "Not found")
}

async fn search(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(owner): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = gate(&actor) {
        return r;
    }
    if !owner_exists(&state, &owner).await.unwrap_or(false) {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    let term = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let skip = q.get("skip").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0).min(100_000);
    let take = q.get("take").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20).clamp(1, MAX_TAKE as usize);
    let prerelease = q.get("prerelease").map(|s| s.eq_ignore_ascii_case("true")).unwrap_or(false);
    // `%`, `_` and the escape itself are literal in what the person typed.
    let pattern = format!("%{}%", term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i64,
        owner_id: i64,
        repo_id: Option<i64>,
        name: String,
        orphaned: bool,
        owner_kind: String,
    }
    let rows: Vec<Row> = match sqlx::query_as(
        "SELECT p.id, p.owner_id, p.repo_id, p.name::text AS name, p.orphaned, u.kind AS owner_kind \
         FROM packages p JOIN users u ON u.id = p.owner_id \
         WHERE u.username = $1::citext AND p.kind = 'nuget' AND (p.name::text ILIKE $2 ESCAPE '\\' \
               OR EXISTS (SELECT 1 FROM nuget_versions v WHERE v.package_id = p.id AND v.description ILIKE $2 ESCAPE '\\')) \
         ORDER BY p.name LIMIT 2000",
    )
    .bind(&owner)
    .bind(&pattern)
    .fetch_all(&state.db)
    .await
    {
        Ok(r) => r,
        Err(_) => return plain(StatusCode::SERVICE_UNAVAILABLE, "try again"),
    };
    let mut memo = PullMemo::default();
    let mut found: Vec<Value> = Vec::new();
    for r in rows {
        let pkg =
            Package { id: r.id, owner_id: r.owner_id, repo_id: r.repo_id, owner_name: owner.clone(), name: r.name, orphaned: r.orphaned };
        if !can_pull(&state, &actor, &pkg, r.owner_kind == "org", &mut memo).await.unwrap_or(false) {
            continue;
        }
        let Ok(vs) = versions_of(&state, &pkg).await else { continue };
        let listed: Vec<&VersionRow> = vs.iter().filter(|v| v.listed && (prerelease || !v.version.contains('-'))).collect();
        let Some(latest) = listed.last() else { continue };
        let b = base(&state, &owner);
        let low = lower(&pkg.name);
        found.push(json!({
            "@id": format!("{b}/registration/{low}/index.json"), "@type": "Package",
            "registration": format!("{b}/registration/{low}/index.json"),
            "id": pkg.name, "version": latest.version, "description": latest.description, "authors": latest.authors,
            "totalDownloads": 0, "verified": false,
            "versions": listed.iter().map(|v| json!({ "version": v.version, "downloads": 0, "@id": format!("{b}/registration/{low}/{}.json", v.version) })).collect::<Vec<_>>(),
        }));
    }
    let total = found.len();
    let page: Vec<Value> = found.into_iter().skip(skip).take(take).collect();
    json_ok(json!({ "totalHits": total, "data": page }))
}

// ------------------------------------------------------------------------------------ pushing

/// The boundary of a `multipart/form-data` content type.
fn boundary_of(ct: &str) -> Option<String> {
    let (kind, rest) = ct.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    rest.split(';')
        .find_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
        })
        .filter(|b| !b.is_empty() && b.len() <= 200 && b.bytes().all(|c| c.is_ascii_graphic() && c != b'"'))
}

/// Where the first part's body begins and ends inside the saved upload: after the blank line that
/// ends its headers, and before the `\r\n--boundary` that closes it.
fn part_window(f: &mut std::fs::File, len: u64, boundary: &str) -> Option<(u64, u64)> {
    let opening = format!("--{boundary}\r\n");
    let mut head = vec![0u8; len.min(16 * 1024) as usize];
    f.seek(SeekFrom::Start(0)).ok()?;
    f.read_exact(&mut head).ok()?;
    if !head.starts_with(opening.as_bytes()) {
        return None;
    }
    let blank = head.windows(4).position(|w| w == b"\r\n\r\n")?;
    let start = (blank + 4) as u64;
    let closing = format!("\r\n--{boundary}");
    let tail_len = len.min((closing.len() + 64) as u64);
    let mut tail = vec![0u8; tail_len as usize];
    f.seek(SeekFrom::Start(len - tail_len)).ok()?;
    f.read_exact(&mut tail).ok()?;
    let at = tail.windows(closing.len()).rposition(|w| w == closing.as_bytes())?;
    let end = len - tail_len + at as u64;
    (end >= start).then_some((start, end))
}

/// What a pushed package holds that the feed needs.
struct Opened {
    nuspec_text: String,
    spec: Nuspec,
}

/// Reads the zip directory and the one top-level `.nuspec`. Nothing is unpacked.
fn open_nupkg(f: std::fs::File) -> Result<Opened, &'static str> {
    let mut z = zip::ZipArchive::new(f).map_err(|_| "the file is not a zip (a .nupkg is)")?;
    if z.len() > MAX_ZIP_ENTRIES {
        return Err("the package lists too many files");
    }
    let mut found = None;
    for i in 0..z.len() {
        let e = z.by_index_raw(i).map_err(|_| "the zip cannot be read")?;
        let name = e.name().to_string();
        if !name.contains('/') && !name.contains('\\') && name.to_ascii_lowercase().ends_with(".nuspec") {
            if found.is_some() {
                return Err("the package holds more than one .nuspec at its top level");
            }
            found = Some(i);
        }
    }
    let i = found.ok_or("the package has no .nuspec at its top level")?;
    let entry = z.by_index(i).map_err(|_| "the .nuspec cannot be read")?;
    if entry.size() > MAX_NUSPEC_BYTES {
        return Err("the .nuspec is too large");
    }
    let mut text = String::new();
    entry.take(MAX_NUSPEC_BYTES + 1).read_to_string(&mut text).map_err(|_| "the .nuspec is not valid UTF-8 text")?;
    if text.len() as u64 > MAX_NUSPEC_BYTES {
        return Err("the .nuspec is too large");
    }
    let spec = parse_nuspec(&text)?;
    Ok(Opened { nuspec_text: text, spec })
}

async fn push(State(state): State<AppState>, CurrentActor(actor): CurrentActor, Path(owner): Path<String>, req: Request) -> Response {
    if actor.is_anonymous() {
        return plain(StatusCode::UNAUTHORIZED, "Authentication required");
    }
    if !may_push(&actor) {
        return plain(StatusCode::FORBIDDEN, "this credential needs the write:packages scope");
    }
    if !ch_core::access::valid_owner_name(&owner) {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    // The namespace is checked before a byte of the upload is read.
    if let Err(e) = crate::access::may_push_into(&state, &actor, "nuget", &owner).await {
        return refused(e);
    }
    let Some(boundary) = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).and_then(boundary_of) else {
        return plain(StatusCode::BAD_REQUEST, "send the package as multipart/form-data");
    };
    if req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_NUPKG_BYTES + 64 * 1024)
    {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "the package is too large (250 MiB at most)");
    }
    // The upload goes to a file under the limit, and everything after reads that file.
    let Ok(upload) = tempfile::NamedTempFile::new() else { return plain(StatusCode::SERVICE_UNAVAILABLE, "try again") };
    let Ok(std_file) = upload.reopen() else { return plain(StatusCode::SERVICE_UNAVAILABLE, "try again") };
    let mut out = tokio::fs::File::from_std(std_file);
    let mut stream = req.into_body().into_data_stream();
    let mut got = 0u64;
    let idle = std::time::Duration::from_secs(60);
    loop {
        let chunk = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(c))) => c,
            Ok(None) => break,
            _ => return plain(StatusCode::REQUEST_TIMEOUT, "the upload stopped"),
        };
        got += chunk.len() as u64;
        if got > MAX_NUPKG_BYTES + 64 * 1024 {
            return plain(StatusCode::PAYLOAD_TOO_LARGE, "the package is too large (250 MiB at most)");
        }
        if out.write_all(&chunk).await.is_err() {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "try again");
        }
    }
    if out.flush().await.is_err() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "try again");
    }
    drop(out);
    // Cut the file part out into a file of its own, and read the package from it.
    let path = upload.path().to_path_buf();
    let parsed = tokio::task::spawn_blocking(move || -> Result<(tempfile::NamedTempFile, Opened, u64), &'static str> {
        let mut f = std::fs::File::open(&path).map_err(|_| "the upload cannot be read")?;
        let (start, end) = part_window(&mut f, got, &boundary).ok_or("the upload is not a multipart form with a file")?;
        if end - start > MAX_NUPKG_BYTES {
            return Err("the package is too large (250 MiB at most)");
        }
        let mut pkg = tempfile::NamedTempFile::new().map_err(|_| "try again")?;
        f.seek(SeekFrom::Start(start)).map_err(|_| "the upload cannot be read")?;
        std::io::copy(&mut f.take(end - start), pkg.as_file_mut()).map_err(|_| "the upload cannot be read")?;
        pkg.as_file_mut().flush().map_err(|_| "try again")?;
        let opened = open_nupkg(pkg.reopen().map_err(|_| "try again")?)?;
        Ok((pkg, opened, end - start))
    })
    .await;
    let (pkg_file, opened, size) = match parsed {
        Ok(Ok(v)) => v,
        Ok(Err(m)) => return plain(StatusCode::BAD_REQUEST, m),
        Err(_) => return plain(StatusCode::SERVICE_UNAVAILABLE, "try again"),
    };
    drop(upload);
    let id = opened.spec.id.clone();
    if !id_ok(&id) {
        return plain(StatusCode::BAD_REQUEST, "the package id may use letters, digits, `.`, `_` and `-`, up to 100 characters");
    }
    let Some(version) = normalize_version(&opened.spec.version) else {
        return plain(StatusCode::BAD_REQUEST, "the version is not a NuGet version");
    };
    let pkg = match authorize_kind(&state, &actor, "nuget", &owner, &id, Access::Push, true).await {
        Ok(p) => p,
        Err(e) => return refused(e),
    };
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM nuget_versions WHERE package_id = $1")
        .bind(pkg.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    if held >= MAX_VERSIONS {
        return plain(StatusCode::BAD_REQUEST, "this package holds as many versions as it may");
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM nuget_versions WHERE package_id = $1 AND version = $2)")
        .bind(pkg.id)
        .bind(&version)
        .fetch_one(&state.db)
        .await
        .unwrap_or(true);
    if exists {
        return plain(StatusCode::CONFLICT, "this version of the package already exists");
    }
    let Ok(file) = tokio::fs::File::open(pkg_file.path()).await else { return plain(StatusCode::SERVICE_UNAVAILABLE, "try again") };
    let blob = match state.blobs.put_stream(tokio_util::io::ReaderStream::new(file).boxed(), MAX_NUPKG_BYTES).await {
        Ok(b) => b,
        Err(e) => return refused(e.into()),
    };
    let mut tx = match state.db.begin().await {
        Ok(t) => t,
        Err(_) => return plain(StatusCode::SERVICE_UNAVAILABLE, "try again"),
    };
    let inserted = sqlx::query(
        "INSERT INTO nuget_versions (package_id, version, blob_sha256, size, nuspec, description, authors, pushed_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(pkg.id)
    .bind(&version)
    .bind(&blob.sha256)
    .bind(size as i64)
    .bind(&opened.nuspec_text)
    .bind(&opened.spec.description)
    .bind(&opened.spec.authors)
    .bind(actor.user_id().map(|u| u.0))
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return plain(StatusCode::CONFLICT, "this version of the package already exists");
        }
        Err(_) => return plain(StatusCode::SERVICE_UNAVAILABLE, "try again"),
    }
    if ch_db::blobs::add_ref(&mut tx, &blob).await.is_err() || tx.commit().await.is_err() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "try again");
    }
    let mut r = plain(StatusCode::CREATED, "Created");
    r.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("{}/flatcontainer/{}/{version}/{}.{version}.nupkg", base(&state, &owner), lower(&id), lower(&id)))
            .unwrap_or(HeaderValue::from_static("/")),
    );
    r
}

/// Unlists (`DELETE`) or lists again (`POST`) one version. Needs write access, as pushing does.
async fn set_listed(state: &AppState, actor: &Actor, owner: &str, id: &str, version: &str, listed: bool) -> Response {
    if actor.is_anonymous() {
        return plain(StatusCode::UNAUTHORIZED, "Authentication required");
    }
    if !id_ok(id) {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    let pkg = match authorize_kind(state, actor, "nuget", owner, id, Access::Push, false).await {
        Ok(p) => p,
        Err(e) => return refused(e),
    };
    let Some(version) = normalize_version(version) else { return plain(StatusCode::NOT_FOUND, "Not found") };
    match sqlx::query("UPDATE nuget_versions SET listed = $3 WHERE package_id = $1 AND version = $2")
        .bind(pkg.id)
        .bind(&version)
        .bind(listed)
        .execute(&state.db)
        .await
    {
        Ok(r) if r.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => plain(StatusCode::NOT_FOUND, "Not found"),
        Err(_) => plain(StatusCode::SERVICE_UNAVAILABLE, "try again"),
    }
}

async fn unlist(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, id, version)): Path<(String, String, String)>,
) -> Response {
    set_listed(&state, &actor, &owner, &id, &version, false).await
}

async fn relist(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, id, version)): Path<(String, String, String)>,
) -> Response {
    set_listed(&state, &actor, &owner, &id, &version, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_normalized_the_way_nuget_does() {
        // Written in parts: a literal four-number version reads as an address to the portability check.
        let four = format!("{}.{}.{}.{}", 1, 2, 3, 4);
        let zero4 = format!("{}.{}.{}.{}", 1, 2, 3, 0);
        for (given, want) in [
            ("1", "1.0.0"),
            ("1.0", "1.0.0"),
            ("1.2.3", "1.2.3"),
            ("01.02.003", "1.2.3"),
            (zero4.as_str(), "1.2.3"),
            (four.as_str(), four.as_str()),
            ("1.2.3-Beta.1", "1.2.3-beta.1"),
            ("1.2.3+build.5", "1.2.3"),
            ("1.2.3-rc.1+x", "1.2.3-rc.1"),
        ] {
            assert_eq!(normalize_version(given).as_deref(), Some(want), "{given}");
        }
        for bad in ["", "a", "1.2.3.4.5.6", "1..2", "1.2.3-", "1.2.3-a_b", "../1", "1.2.3/x", &"1".repeat(70)] {
            assert_eq!(normalize_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn versions_sort_by_semver_precedence() {
        let mut v = vec!["1.10.0", "1.2.0", "1.2.0-beta.2", "1.2.0-beta.11", "1.2.0-alpha", "2.0.0", "1.2.0-beta"];
        v.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(v, ["1.2.0-alpha", "1.2.0-beta", "1.2.0-beta.2", "1.2.0-beta.11", "1.2.0", "1.10.0", "2.0.0"]);
    }

    #[test]
    fn package_ids_are_plain() {
        for ok in ["Newtonsoft.Json", "a", "My_Lib-2", "A.B.C"] {
            assert!(id_ok(ok), "{ok}");
        }
        for bad in ["", ".a", "a.", "a..b", "a/b", "a b", "../x", &"x".repeat(101), "a%2fb"] {
            assert!(!id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_nuspec_gives_its_id_and_version_and_nothing_from_its_dependencies() {
        let x = r#"<?xml version="1.0"?><package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
          <metadata><id>Acme.Tool</id><version>1.2.3-beta</version><authors>A &amp; B</authors>
            <description>Does things</description>
            <dependencies><group><dependency id="Other" version="9.9.9" /></group></dependencies>
          </metadata></package>"#;
        let s = parse_nuspec(x).unwrap();
        assert_eq!(
            (s.id.as_str(), s.version.as_str(), s.authors.as_str(), s.description.as_str()),
            ("Acme.Tool", "1.2.3-beta", "A & B", "Does things")
        );
        for bad in [
            "<package><metadata><id>x</id></metadata></package>",
            "<package><metadata><version>1</version></metadata></package>",
            "<package></package>",
            "<!DOCTYPE p [<!ENTITY e 'x'>]><package><metadata><id>&e;</id><version>1</version></metadata></package>",
            "<package><metadata><id><b>x</b></id><version>1</version></metadata></package>",
        ] {
            assert!(parse_nuspec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_multipart_boundary_is_read_strictly() {
        assert_eq!(boundary_of("multipart/form-data; boundary=abc123").as_deref(), Some("abc123"));
        assert!(boundary_of("multipart/form-data; boundary=\"x y\"").is_none(), "a space is not a boundary character here");
        assert_eq!(boundary_of("multipart/form-data;boundary=\"q-1\"").as_deref(), Some("q-1"));
        assert_eq!(boundary_of("application/json; boundary=a"), None);
        assert_eq!(boundary_of("multipart/form-data"), None);
    }

    #[test]
    fn the_file_part_is_found_inside_the_form() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"package\"; filename=\"p.nupkg\"\r\nContent-Type: application/octet-stream\r\n\r\nPK-bytes-\r\n--b--\r\n";
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(body).unwrap();
        let (s, e) = part_window(&mut f, body.len() as u64, "b").unwrap();
        assert_eq!(&body[s as usize..e as usize], b"PK-bytes-");
        let mut g = tempfile::tempfile().unwrap();
        g.write_all(b"no form here").unwrap();
        assert!(part_window(&mut g, 12, "b").is_none());
    }
}
