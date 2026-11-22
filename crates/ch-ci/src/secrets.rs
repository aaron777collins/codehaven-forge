//! Secrets and variables, at repository or owner scope.
//!
//! A secret's value is sealed with the master key (AES-256-GCM) and tied to its scope, owner and
//! name, so a sealed value copied to another row does not open. Values are write-only: nothing
//! here returns one except [`resolve_for_job`], which is called only when a runner takes a job.

use crate::ctx::Ctx;
use ch_core::{Error, FieldError, Result, UserId};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SECRET_BYTES: usize = 64 << 10;
pub const MAX_VARIABLE_BYTES: usize = 16 << 10;
pub const MAX_PER_SCOPE: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Repo(i64),
    /// The owning account's secrets, for every repository it owns. `org_id` is a `users` id.
    Org(i64),
}

impl Scope {
    fn parts(self) -> (&'static str, Option<i64>, Option<i64>) {
        match self {
            Scope::Repo(r) => ("repo", Some(r), None),
            Scope::Org(o) => ("org", None, Some(o)),
        }
    }
    fn owner(self) -> i64 {
        match self {
            Scope::Repo(r) => r,
            Scope::Org(o) => o,
        }
    }
}

pub fn aad(scope: Scope, name: &str) -> Vec<u8> {
    format!("ci-secret:{}:{}:{}", scope.parts().0, scope.owner(), name).into_bytes()
}

/// Names are upper-case letters, digits and underscores, and may not start with a digit or
/// with the prefixes the runner sets itself.
pub fn validate_name(name: &str) -> Result<()> {
    let bad = |m: &str| Err(Error::Validation(vec![FieldError::custom("Secret", "name", m)]));
    if name.is_empty() || name.len() > 100 {
        return bad("Names must be between 1 and 100 characters.");
    }
    let mut it = name.bytes();
    let first = it.next().unwrap_or(b'0');
    if !(first.is_ascii_uppercase() || first == b'_') || !it.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
        return bad("Names may only contain upper-case letters, digits and underscores, and may not start with a digit.");
    }
    if name.starts_with("GITHUB_") || name.starts_with("CODEHAVEN_") {
        return bad("Names may not start with GITHUB_ or CODEHAVEN_.");
    }
    Ok(())
}

pub async fn put_secret(ctx: &Ctx, scope: Scope, name: &str, value: &str, by: Option<UserId>) -> Result<bool> {
    validate_name(name)?;
    if value.len() > MAX_SECRET_BYTES {
        return Err(Error::invalid("Secret", "value", "A secret may hold at most 64 KiB."));
    }
    let (kind, repo, org) = scope.parts();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM secrets WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 AND name <> $4")
        .bind(kind)
        .bind(repo)
        .bind(org)
        .bind(name)
        .fetch_one(&ctx.db)
        .await?;
    if count >= MAX_PER_SCOPE {
        return Err(Error::invalid("Secret", "name", format!("At most {MAX_PER_SCOPE} secrets are allowed here.")));
    }
    let sealed = ctx.key()?.seal(value.as_bytes(), &aad(scope, name))?;
    let existed = sqlx::query_scalar::<_, bool>(
        "INSERT INTO secrets (scope, repo_id, org_id, name, value_sealed, created_by) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT DO NOTHING RETURNING true",
    )
    .bind(kind)
    .bind(repo)
    .bind(org)
    .bind(name)
    .bind(&sealed)
    .bind(by.map(|u| u.0))
    .fetch_optional(&ctx.db)
    .await?
    .is_none();
    if existed {
        sqlx::query("UPDATE secrets SET value_sealed = $4, updated_at = now() WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 AND name = $5")
            .bind(kind)
            .bind(repo)
            .bind(org)
            .bind(&sealed)
            .bind(name)
            .execute(&ctx.db)
            .await?;
    }
    Ok(!existed)
}

pub async fn delete_secret(ctx: &Ctx, scope: Scope, name: &str) -> Result<bool> {
    let (kind, repo, org) = scope.parts();
    let r = sqlx::query(
        "DELETE FROM secrets WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 AND name = $4",
    )
    .bind(kind)
    .bind(repo)
    .bind(org)
    .bind(name)
    .execute(&ctx.db)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Names and times only. The value is never read back.
pub async fn list_secrets(ctx: &Ctx, scope: Scope) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>> {
    let (kind, repo, org) = scope.parts();
    Ok(sqlx::query_as("SELECT name, updated_at FROM secrets WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 ORDER BY name")
        .bind(kind)
        .bind(repo)
        .bind(org)
        .fetch_all(&ctx.db)
        .await?)
}

pub async fn put_variable(ctx: &Ctx, scope: Scope, name: &str, value: &str) -> Result<bool> {
    validate_name(name).map_err(|e| match e {
        Error::Validation(v) => Error::Validation(v.into_iter().map(|f| FieldError { resource: "Variable".into(), ..f }).collect()),
        o => o,
    })?;
    if value.len() > MAX_VARIABLE_BYTES {
        return Err(Error::invalid("Variable", "value", "A variable may hold at most 16 KiB."));
    }
    let (kind, repo, org) = scope.parts();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM variables WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 AND name <> $4")
        .bind(kind)
        .bind(repo)
        .bind(org)
        .bind(name)
        .fetch_one(&ctx.db)
        .await?;
    if count >= MAX_PER_SCOPE {
        return Err(Error::invalid("Variable", "name", format!("At most {MAX_PER_SCOPE} variables are allowed here.")));
    }
    let created = sqlx::query_scalar::<_, bool>(
        "INSERT INTO variables (scope, repo_id, org_id, name, value) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING RETURNING true",
    )
    .bind(kind)
    .bind(repo)
    .bind(org)
    .bind(name)
    .bind(value)
    .fetch_optional(&ctx.db)
    .await?
    .is_some();
    if !created {
        sqlx::query("UPDATE variables SET value = $4, updated_at = now() WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 AND name = $5")
            .bind(kind)
            .bind(repo)
            .bind(org)
            .bind(value)
            .bind(name)
            .execute(&ctx.db)
            .await?;
    }
    Ok(created)
}

pub async fn delete_variable(ctx: &Ctx, scope: Scope, name: &str) -> Result<bool> {
    let (kind, repo, org) = scope.parts();
    let r = sqlx::query(
        "DELETE FROM variables WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 AND name = $4",
    )
    .bind(kind)
    .bind(repo)
    .bind(org)
    .bind(name)
    .execute(&ctx.db)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_variables(ctx: &Ctx, scope: Scope) -> Result<Vec<(String, String, chrono::DateTime<chrono::Utc>)>> {
    let (kind, repo, org) = scope.parts();
    Ok(sqlx::query_as("SELECT name, value, updated_at FROM variables WHERE scope = $1 AND repo_id IS NOT DISTINCT FROM $2 AND org_id IS NOT DISTINCT FROM $3 ORDER BY name")
        .bind(kind)
        .bind(repo)
        .bind(org)
        .fetch_all(&ctx.db)
        .await?)
}

/// The secrets a job may see, decrypted. Only names the job references are returned (every
/// secret when it reads the whole `secrets` object), repository secrets win over the owner's,
/// and a fork pull request job gets none.
pub async fn resolve_for_job(ctx: &Ctx, repo: i64, names: &BTreeSet<String>, all: bool, fork: bool) -> Result<BTreeMap<String, String>> {
    if fork || (!all && names.is_empty()) {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, String, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT s.name, s.scope, COALESCE(s.repo_id, s.org_id), s.value_sealed FROM secrets s
         WHERE (s.scope = 'repo' AND s.repo_id = $1) OR (s.scope = 'org' AND s.org_id = (SELECT owner_id FROM repositories WHERE id = $1))",
    )
    .bind(repo)
    .fetch_all(&ctx.db)
    .await?;
    let mut out: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for (name, scope, owner, sealed) in rows {
        if !all && !names.contains(&name) {
            continue;
        }
        let sc = if scope == "repo" { Scope::Repo(owner) } else { Scope::Org(owner) };
        let plain = ctx.key()?.open(&sealed, &aad(sc, &name))?;
        let value = String::from_utf8(plain).map_err(|_| Error::internal(anyhow::anyhow!("secret {name} is not text")))?;
        let is_repo = scope == "repo";
        match out.get(&name) {
            Some((true, _)) if !is_repo => {}
            _ => {
                out.insert(name, (is_repo, value));
            }
        }
    }
    Ok(out.into_iter().map(|(k, (_, v))| (k, v)).collect())
}

/// The variables a job may see: those it references, or all of them when it reads `vars`.
pub async fn resolve_vars_for_job(ctx: &Ctx, repo: i64, names: &BTreeSet<String>, all: bool) -> Result<BTreeMap<String, String>> {
    let vars = crate::runs::load_vars(ctx, repo).await?;
    Ok(vars.into_iter().filter(|(k, _)| all || names.contains(k)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["TOKEN", "_X", "A1_B2", "DEPLOY_KEY"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "token", "1A", "A-B", "A B", "GITHUB_TOKEN", "CODEHAVEN_X", "É", &"A".repeat(101)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_associated_data_binds_scope_owner_and_name() {
        assert_ne!(aad(Scope::Repo(1), "A"), aad(Scope::Repo(2), "A"));
        assert_ne!(aad(Scope::Repo(1), "A"), aad(Scope::Org(1), "A"));
        assert_ne!(aad(Scope::Repo(1), "A"), aad(Scope::Repo(1), "B"));
    }
}
