//! Manifests and tags (OCI Distribution Spec, "Pushing manifests" and "Listing tags").
//!
//! A manifest is accepted only when everything it points at is already in this image: the
//! config and every layer for an image manifest, every child manifest for an index. That is
//! what makes a pull of any tag complete.

use crate::access::Package;
use crate::error::{RegError, RegResult};
use crate::names::{Reference, is_digest, parse_reference};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use ch_core::state::AppState;
use ch_core::{DomainEvent, PackageId, UserId};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Registries are expected to take at least 4 MiB; nothing real is near it.
pub const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;

pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

pub fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Index,
}

/// What a manifest points at, after it has been checked to be well formed.
#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub kind: Kind,
    /// Blob digests: the config and the layers. Empty for an index.
    pub blobs: Vec<String>,
    /// Child manifest digests of an index. Empty for an image manifest.
    pub children: Vec<String>,
    pub media_type: String,
}

/// Checks the JSON against the structure the spec requires and lists what it references.
pub fn parse_manifest(content_type: &str, bytes: &[u8]) -> RegResult<Parsed> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(RegError::manifest_invalid("the manifest is larger than 4 MiB"));
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|e| RegError::manifest_invalid(format!("not valid JSON: {e}")))?;
    if v.get("schemaVersion").and_then(Value::as_i64) != Some(2) {
        return Err(RegError::manifest_invalid("schemaVersion must be 2"));
    }
    let declared = v.get("mediaType").and_then(Value::as_str);
    let ct = content_type.split(';').next().unwrap_or("").trim();
    if let Some(d) = declared
        && !ct.is_empty()
        && d != ct
    {
        return Err(RegError::manifest_invalid("the mediaType in the manifest does not match its Content-Type"));
    }
    let media_type = declared.unwrap_or(ct).to_string();
    let digests = |field: &str| -> RegResult<Vec<String>> {
        let Some(arr) = v.get(field).and_then(Value::as_array) else { return Ok(Vec::new()) };
        arr.iter()
            .map(|d| {
                let dg = d
                    .get("digest")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RegError::manifest_invalid(format!("an entry in {field} has no digest")))?;
                if !is_digest(dg) {
                    return Err(RegError::manifest_invalid(format!("{dg} is not a sha256 digest")));
                }
                Ok(dg.to_string())
            })
            .collect()
    };
    if v.get("manifests").is_some() {
        if !matches!(media_type.as_str(), OCI_INDEX | DOCKER_LIST) {
            return Err(RegError::manifest_invalid("an index needs an index media type"));
        }
        return Ok(Parsed { kind: Kind::Index, blobs: Vec::new(), children: digests("manifests")?, media_type });
    }
    if v.get("config").is_some() && v.get("layers").is_some() {
        if !matches!(media_type.as_str(), OCI_MANIFEST | DOCKER_MANIFEST) {
            return Err(RegError::manifest_invalid("an image manifest needs an image manifest media type"));
        }
        let cfg =
            v["config"].get("digest").and_then(Value::as_str).ok_or_else(|| RegError::manifest_invalid("the config has no digest"))?;
        if !is_digest(cfg) {
            return Err(RegError::manifest_invalid("the config digest is not a sha256 digest"));
        }
        let mut blobs = vec![cfg.to_string()];
        blobs.extend(digests("layers")?);
        return Ok(Parsed { kind: Kind::Image, blobs, children: Vec::new(), media_type });
    }
    Err(RegError::manifest_invalid("neither an image manifest (config and layers) nor an index (manifests)"))
}

fn set(resp: &mut Response, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}

fn base(status: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    set(&mut r, "docker-distribution-api-version", "registry/2.0");
    r
}

#[derive(sqlx::FromRow)]
struct Manifest {
    digest: String,
    media_type: String,
    content: Vec<u8>,
}

async fn find_manifest(state: &AppState, pkg: &Package, reference: &Reference) -> RegResult<Manifest> {
    let row: Option<Manifest> = match reference {
        Reference::Digest(d) => {
            sqlx::query_as("SELECT digest, media_type, content FROM oci_manifests WHERE package_id = $1 AND digest = $2")
                .bind(pkg.id)
                .bind(d)
                .fetch_optional(&state.db)
                .await?
        }
        Reference::Tag(t) => {
            sqlx::query_as(
                "SELECT m.digest, m.media_type, m.content FROM oci_tags t JOIN oci_manifests m ON m.id = t.manifest_id \
                 WHERE t.package_id = $1 AND t.name = $2",
            )
            .bind(pkg.id)
            .bind(t)
            .fetch_optional(&state.db)
            .await?
        }
    };
    row.ok_or_else(RegError::manifest_unknown)
}

/// A reference to write under: anything that is not a tag or a sha256 digest is invalid.
fn reference(s: &str) -> RegResult<Reference> {
    parse_reference(s).ok_or_else(|| RegError::manifest_invalid("the reference must be a tag or a sha256 digest"))
}

/// A reference to read or delete: one that could never name a manifest is simply not found,
/// which is what clients (and the conformance suite) expect for a made-up name.
fn existing_reference(s: &str) -> RegResult<Reference> {
    parse_reference(s).ok_or_else(RegError::manifest_unknown)
}

/// `GET` and `HEAD /v2/<name>/manifests/<reference>`.
pub async fn get(state: &AppState, pkg: &Package, reference_str: &str, head: bool) -> RegResult<Response> {
    let m = find_manifest(state, pkg, &existing_reference(reference_str)?).await?;
    let len = m.content.len();
    let mut r = if head { base(StatusCode::OK) } else { Response::new(Body::from(m.content)) };
    set(&mut r, "docker-distribution-api-version", "registry/2.0");
    set(&mut r, "content-type", &m.media_type);
    set(&mut r, "content-length", &len.to_string());
    set(&mut r, "docker-content-digest", &m.digest);
    set(&mut r, "etag", &format!("\"{}\"", m.digest));
    Ok(r)
}

/// `PUT /v2/<name>/manifests/<reference>`.
pub async fn put(state: &AppState, pkg: &Package, reference_str: &str, content_type: &str, body: Body) -> RegResult<Response> {
    let reference = reference(reference_str)?;
    let bytes = axum::body::to_bytes(body, MAX_MANIFEST_BYTES + 1)
        .await
        .map_err(|_| RegError::manifest_invalid("the manifest is larger than 4 MiB"))?;
    let parsed = parse_manifest(content_type, &bytes)?;
    let digest = digest_of(&bytes);
    if let Reference::Digest(d) = &reference
        && *d != digest
    {
        return Err(RegError::digest_invalid("the manifest does not match the digest in the URL"));
    }

    // Everything it points at must already be here.
    for b in &parsed.blobs {
        let have: Option<(i64,)> = sqlx::query_as("SELECT 1::bigint FROM oci_blobs WHERE package_id = $1 AND digest = $2")
            .bind(pkg.id)
            .bind(b)
            .fetch_optional(&state.db)
            .await?;
        if have.is_none() {
            return Err(RegError::manifest_blob_unknown(b));
        }
    }
    for c in &parsed.children {
        let have: Option<(i64,)> = sqlx::query_as("SELECT 1::bigint FROM oci_manifests WHERE package_id = $1 AND digest = $2")
            .bind(pkg.id)
            .bind(c)
            .fetch_optional(&state.db)
            .await?;
        if have.is_none() {
            return Err(RegError::manifest_blob_unknown(c));
        }
    }

    let mut tx = state.db.begin().await?;
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO oci_manifests (package_id, digest, media_type, size, content) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (package_id, digest) DO UPDATE SET media_type = EXCLUDED.media_type RETURNING id",
    )
    .bind(pkg.id)
    .bind(&digest)
    .bind(&parsed.media_type)
    .bind(bytes.len() as i64)
    .bind(bytes.as_ref())
    .fetch_one(&mut *tx)
    .await?;
    let tag = match &reference {
        Reference::Tag(t) => {
            sqlx::query(
                "INSERT INTO oci_tags (package_id, name, manifest_id) VALUES ($1, $2, $3) \
                 ON CONFLICT (package_id, name) DO UPDATE SET manifest_id = EXCLUDED.manifest_id, updated_at = now()",
            )
            .bind(pkg.id)
            .bind(t)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            Some(t.clone())
        }
        Reference::Digest(_) => None,
    };
    state
        .events
        .emit(&mut tx, DomainEvent::PackagePublished { package_id: PackageId(pkg.id), owner_id: UserId(pkg.owner_id), tag })
        .await?;
    tx.commit().await?;

    let mut r = base(StatusCode::CREATED);
    set(&mut r, "location", &format!("/v2/{}/manifests/{digest}", pkg.full_name()));
    set(&mut r, "docker-content-digest", &digest);
    // A manifest with a subject answers with it, so clients know the referrers API applies.
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes)
        && let Some(s) = v.get("subject").and_then(|s| s.get("digest")).and_then(Value::as_str)
    {
        set(&mut r, "oci-subject", s);
    }
    Ok(r)
}

/// `DELETE /v2/<name>/manifests/<reference>`: a digest removes the manifest and its tags, a
/// tag removes just the tag.
pub async fn delete(state: &AppState, pkg: &Package, reference_str: &str) -> RegResult<Response> {
    match existing_reference(reference_str)? {
        Reference::Digest(d) => {
            let gone = sqlx::query("DELETE FROM oci_manifests WHERE package_id = $1 AND digest = $2")
                .bind(pkg.id)
                .bind(&d)
                .execute(&state.db)
                .await?;
            if gone.rows_affected() == 0 {
                return Err(RegError::manifest_unknown());
            }
        }
        Reference::Tag(t) => {
            let gone =
                sqlx::query("DELETE FROM oci_tags WHERE package_id = $1 AND name = $2").bind(pkg.id).bind(&t).execute(&state.db).await?;
            if gone.rows_affected() == 0 {
                return Err(RegError::manifest_unknown());
            }
        }
    }
    Ok(base(StatusCode::ACCEPTED))
}

/// `GET /v2/<name>/tags/list?n=&last=`.
pub async fn tags(state: &AppState, pkg: &Package, q: &std::collections::HashMap<String, String>) -> RegResult<Response> {
    let n: i64 = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(1000).clamp(1, 1000);
    let last = q.get("last").cloned().unwrap_or_default();
    if last.contains('\0') {
        return Err(RegError::unsupported(StatusCode::BAD_REQUEST, "last must not contain a NUL byte"));
    }
    // Byte order, which is what clients that page through the list expect.
    let mut names: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM oci_tags WHERE package_id = $1 AND name COLLATE \"C\" > $2 ORDER BY name COLLATE \"C\" LIMIT $3")
            .bind(pkg.id)
            .bind(&last)
            .bind(n + 1)
            .fetch_all(&state.db)
            .await?;
    let more = names.len() as i64 > n;
    names.truncate(n as usize);
    let tags: Vec<String> = names.into_iter().map(|t| t.0).collect();
    let body = serde_json::json!({ "name": pkg.full_name(), "tags": tags }).to_string();
    let mut r = Response::new(Body::from(body));
    set(&mut r, "content-type", "application/json");
    set(&mut r, "docker-distribution-api-version", "registry/2.0");
    if more && let Some(last_tag) = tags.last() {
        set(&mut r, "link", &format!("</v2/{}/tags/list?n={n}&last={last_tag}>; rel=\"next\"", pkg.full_name()));
    }
    Ok(r)
}

/// `GET /v2/<name>/referrers/<digest>`: manifests that name this one as their subject, as an
/// image index of descriptors. Each descriptor carries the manifest's annotations and its
/// artifact type (the `artifactType` field, or for an image manifest without one, its config's
/// media type). `?artifactType=` filters, and `OCI-Filters-Applied` says so.
pub async fn referrers(state: &AppState, pkg: &Package, digest: &str, artifact_type: Option<&str>) -> RegResult<Response> {
    if !is_digest(digest) {
        return Err(RegError::digest_invalid("the digest must be sha256: and 64 lowercase hex characters"));
    }
    let rows: Vec<(String, String, i64, Vec<u8>)> =
        sqlx::query_as("SELECT digest, media_type, size, content FROM oci_manifests WHERE package_id = $1 ORDER BY id")
            .bind(pkg.id)
            .fetch_all(&state.db)
            .await?;
    let mut manifests = Vec::new();
    for (d, media_type, size, content) in rows {
        let Ok(v) = serde_json::from_slice::<Value>(&content) else { continue };
        if v.get("subject").and_then(|s| s.get("digest")).and_then(Value::as_str) != Some(digest) {
            continue;
        }
        let kind = v
            .get("artifactType")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
            .or_else(|| v.get("config").and_then(|c| c.get("mediaType")).and_then(Value::as_str));
        if let Some(want) = artifact_type
            && kind != Some(want)
        {
            continue;
        }
        let mut entry = serde_json::json!({ "mediaType": media_type, "digest": d, "size": size });
        if let Some(k) = kind {
            entry["artifactType"] = Value::String(k.to_string());
        }
        if let Some(a) = v.get("annotations").filter(|a| a.is_object()) {
            entry["annotations"] = a.clone();
        }
        manifests.push(entry);
    }
    let body = serde_json::json!({ "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests }).to_string();
    let mut r = Response::new(Body::from(body));
    set(&mut r, "content-type", OCI_INDEX);
    set(&mut r, "docker-distribution-api-version", "registry/2.0");
    if artifact_type.is_some() {
        set(&mut r, "oci-filters-applied", "artifactType");
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn image(media: &str) -> String {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{media}","config":{{"mediaType":"x","digest":"{D1}","size":2}},"layers":[{{"mediaType":"y","digest":"{D2}","size":3}}]}}"#
        )
    }

    #[test]
    fn image_manifests_list_their_blobs() {
        for media in [OCI_MANIFEST, DOCKER_MANIFEST] {
            let p = parse_manifest(media, image(media).as_bytes()).unwrap();
            assert_eq!(p, Parsed { kind: Kind::Image, blobs: vec![D1.into(), D2.into()], children: vec![], media_type: media.into() });
        }
    }

    #[test]
    fn indexes_list_their_children() {
        let idx = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{{"mediaType":"{OCI_MANIFEST}","digest":"{D1}","size":1}}]}}"#
        );
        let p = parse_manifest(OCI_INDEX, idx.as_bytes()).unwrap();
        assert_eq!((p.kind, p.children), (Kind::Index, vec![D1.to_string()]));
    }

    #[test]
    fn a_missing_media_type_falls_back_to_the_content_type() {
        let m = format!(r#"{{"schemaVersion":2,"config":{{"digest":"{D1}"}},"layers":[]}}"#);
        assert_eq!(parse_manifest(OCI_MANIFEST, m.as_bytes()).unwrap().media_type, OCI_MANIFEST);
        assert_eq!(parse_manifest(&format!("{OCI_MANIFEST}; charset=utf-8"), m.as_bytes()).unwrap().media_type, OCI_MANIFEST);
    }

    #[test]
    fn bad_manifests_are_refused_with_manifest_invalid() {
        let cases: Vec<(&str, String)> = vec![
            (OCI_MANIFEST, "not json".into()),
            (OCI_MANIFEST, r#"{"schemaVersion":1}"#.into()),
            (OCI_MANIFEST, r#"{"schemaVersion":2}"#.into()),
            (OCI_MANIFEST, image(DOCKER_MANIFEST)),
            (OCI_INDEX, image(OCI_MANIFEST)),
            (OCI_MANIFEST, r#"{"schemaVersion":2,"config":{"digest":"sha512:abc"},"layers":[]}"#.to_string()),
            (OCI_MANIFEST, format!(r#"{{"schemaVersion":2,"config":{{"digest":"{D1}"}},"layers":[{{"size":1}}]}}"#)),
            (
                "application/vnd.docker.distribution.manifest.v1+json",
                format!(r#"{{"schemaVersion":2,"config":{{"digest":"{D1}"}},"layers":[]}}"#),
            ),
        ];
        for (ct, body) in cases {
            let e = parse_manifest(ct, body.as_bytes()).unwrap_err();
            assert_eq!(e.code, "MANIFEST_INVALID", "{body}");
        }
        assert!(parse_manifest(OCI_MANIFEST, &vec![b' '; MAX_MANIFEST_BYTES + 1]).is_err());
    }

    #[test]
    fn digests_are_sha256_of_the_exact_bytes() {
        assert_eq!(digest_of(b"abc"), "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    proptest! {
        #[test]
        fn parsing_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..500)) {
            let _ = parse_manifest(OCI_MANIFEST, &bytes);
        }
    }
}
