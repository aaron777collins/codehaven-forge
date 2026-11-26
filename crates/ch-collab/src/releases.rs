//! Releases: a tag plus notes and downloadable assets.

use crate::core::Core;
use crate::util::{self, writing_user};
use bytes::Bytes;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::services::BlobRef;
use ch_core::{Actor, Error, FieldError, Permission, ReleaseId, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use serde_json::json;
use sqlx::FromRow;
use std::collections::HashSet;

/// The largest file a release can hold.
pub const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, FromRow)]
struct ReleaseRow {
    id: ReleaseId,
    repo_id: RepoId,
    tag_name: String,
    target_commitish: String,
    name: String,
    body: String,
    draft: bool,
    prerelease: bool,
    author_id: UserId,
    created_at: DateTime<Utc>,
    published_at: Option<DateTime<Utc>>,
}

const REL_COLS: &str = "id, repo_id, tag_name, target_commitish, name, body, draft, prerelease, author_id, created_at, published_at";

#[derive(Debug, Clone, FromRow)]
struct AssetRow {
    id: i64,
    release_id: ReleaseId,
    name: String,
    label: String,
    content_type: String,
    size: i64,
    blob_sha256: String,
    download_count: i64,
    uploader_id: Option<UserId>,
    created_at: DateTime<Utc>,
}

const ASSET_COLS: &str = "id, release_id, name, label, content_type, size, blob_sha256, download_count, uploader_id, created_at";

impl From<ReleaseRow> for (Release, String) {
    fn from(r: ReleaseRow) -> Self {
        (
            Release {
                id: r.id,
                repo_id: r.repo_id,
                tag_name: r.tag_name,
                name: r.name,
                body: r.body,
                draft: r.draft,
                prerelease: r.prerelease,
                author_id: r.author_id,
                created_at: r.created_at,
                published_at: r.published_at,
            },
            r.target_commitish,
        )
    }
}

/// A tag must be a valid git ref name and not look like an option or a sha.
pub fn valid_tag_name(t: &str) -> bool {
    if t.is_empty()
        || t.len() > 200
        || t.starts_with('-')
        || t.starts_with('/')
        || t.ends_with('/')
        || t.ends_with('.')
        || t.ends_with(".lock")
    {
        return false;
    }
    if t.contains("..") || t.contains("//") || t.contains("@{") || t == "@" {
        return false;
    }
    if t.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')) {
        return false;
    }
    !t.split('/').any(|seg| seg.starts_with('.') || seg.ends_with(".lock"))
}

fn valid_asset_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars().count() <= 255
        && !n.contains('/')
        && !n.contains('\\')
        && !n.chars().any(|c| c.is_control())
        && n != "."
        && n != ".."
}

/// The version in a tag as `(major, minor, patch)`: `v1.2.3`, `1.2`, `2.0.0-rc.1` (the part after
/// `-` or `+` is dropped). None when the tag does not start with a number.
pub fn version_key(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let core = t.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.');
    let mut next = || -> Option<u64> {
        match parts.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse().ok(),
            Some(_) => None,
            None => Some(0),
        }
    };
    let (major, minor, patch) = (next()?, next()?, next()?);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// One merged pull request in the notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteItem {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub labels: Vec<String>,
}

const CATEGORIES: &[(&str, &[&str])] = &[
    ("Breaking changes", &["breaking", "breaking change"]),
    ("New features", &["enhancement", "feature"]),
    ("Fixes", &["bug", "fix"]),
    ("Documentation", &["documentation", "docs"]),
];

/// Groups merged pull requests by label and writes the markdown body.
pub fn render_notes(items: &[NoteItem], compare: Option<(&str, &str, &str)>) -> String {
    let mut groups: Vec<(&str, Vec<&NoteItem>)> = CATEGORIES.iter().map(|(n, _)| (*n, Vec::new())).collect();
    let mut other: Vec<&NoteItem> = Vec::new();
    for it in items {
        let lower: Vec<String> = it.labels.iter().map(|l| l.to_lowercase()).collect();
        match CATEGORIES.iter().position(|(_, ls)| ls.iter().any(|l| lower.iter().any(|x| x == l))) {
            Some(i) => groups[i].1.push(it),
            None => other.push(it),
        }
    }
    let mut out = String::new();
    let line = |it: &NoteItem| format!("* {} (#{}) by @{}\n", it.title, it.number, it.author);
    for (name, list) in groups.iter().filter(|(_, l)| !l.is_empty()) {
        out.push_str(&format!("## {name}\n\n"));
        for it in list {
            out.push_str(&line(it));
        }
        out.push('\n');
    }
    if !other.is_empty() {
        out.push_str(if out.is_empty() { "## What changed\n\n" } else { "## Other changes\n\n" });
        for it in other {
            out.push_str(&line(it));
        }
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("No pull requests were merged since the last release.\n\n");
    }
    if let Some((url, prev, tag)) = compare {
        out.push_str(&format!("**Full changelog**: {url}/compare/{prev}...{tag}\n"));
    }
    out
}

const NOTES_SQL_BY_COMMITS: &str = "SELECT i.number, i.title, u.username::text,
        COALESCE((SELECT array_agg(l.name::text ORDER BY l.name::text) FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id), '{}')
     FROM issues i JOIN pull_requests p ON p.issue_id = i.id JOIN users u ON u.id = i.author_id
     WHERE i.repo_id = $1 AND p.merged_at IS NOT NULL AND p.merge_commit_sha = ANY($2)
     ORDER BY p.merged_at, i.number";

const NOTES_SQL_BY_BRANCH: &str = "SELECT i.number, i.title, u.username::text,
        COALESCE((SELECT array_agg(l.name::text ORDER BY l.name::text) FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id), '{}')
     FROM issues i JOIN pull_requests p ON p.issue_id = i.id JOIN users u ON u.id = i.author_id
     WHERE i.repo_id = $1 AND p.merged_at IS NOT NULL AND p.base_ref = $2
     ORDER BY p.merged_at, i.number";

/// `render_notes` for a repository whose `release.yml` may name its own sections and leave pull
/// requests out. With no sections in the file, the built-in grouping is used (the exclusions
/// still apply).
pub fn render_notes_with(items: &[NoteItem], cfg: &ch_core::release_notes::NotesConfig, compare: Option<(&str, &str, &str)>) -> String {
    let kept: Vec<NoteItem> = items.iter().filter(|it| !cfg.excludes(&it.labels, &it.author)).cloned().collect();
    if !cfg.has_categories() {
        return render_notes(&kept, compare);
    }
    let mut groups: Vec<Vec<&NoteItem>> = vec![Vec::new(); cfg.categories.len()];
    for it in &kept {
        if let Some(i) = cfg.category_of(&it.labels, &it.author) {
            groups[i].push(it);
        }
    }
    let mut out = String::new();
    for (c, list) in cfg.categories.iter().zip(&groups) {
        if list.is_empty() {
            continue;
        }
        out.push_str(&format!("## {}\n\n", c.title));
        for it in list {
            out.push_str(&format!("* {} (#{}) by @{}\n", it.title, it.number, it.author));
        }
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("No pull requests were merged since the last release.\n\n");
    }
    if let Some((url, prev, tag)) = compare {
        out.push_str(&format!("**Full changelog**: {url}/compare/{prev}...{tag}\n"));
    }
    out
}

impl Core {
    async fn release_row(&self, repo: RepoId, id: ReleaseId) -> Result<ReleaseRow> {
        let sql = format!("SELECT {REL_COLS} FROM releases WHERE id = $1 AND repo_id = $2");
        sqlx::query_as::<_, ReleaseRow>(&sql).bind(id).bind(repo).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    async fn release_by_tag_row(&self, repo: RepoId, tag: &str) -> Result<ReleaseRow> {
        let sql = format!("SELECT {REL_COLS} FROM releases WHERE repo_id = $1 AND tag_name = $2");
        sqlx::query_as::<_, ReleaseRow>(&sql).bind(repo).bind(tag).fetch_optional(self.db()).await?.ok_or(Error::NotFound)
    }

    async fn release_views_from(&self, rows: Vec<ReleaseRow>) -> Result<Vec<ReleaseView>> {
        let ids: Vec<i64> = rows.iter().map(|r| r.id.0).collect();
        let assets: Vec<AssetRow> = if ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as::<_, AssetRow>(&format!("SELECT {ASSET_COLS} FROM release_assets WHERE release_id = ANY($1) ORDER BY name"))
                .bind(&ids)
                .fetch_all(self.db())
                .await?
        };
        let mut users: HashSet<UserId> = rows.iter().map(|r| r.author_id).collect();
        users.extend(assets.iter().filter_map(|a| a.uploader_id));
        let users: Vec<UserId> = users.into_iter().collect();
        let refs = self.user_refs(&users).await?;
        let mut out = Vec::new();
        for r in rows {
            let rid = r.id;
            let author = refs.get(&r.author_id).cloned().unwrap_or_else(|| crate::core::ghost(r.author_id));
            let infos: Vec<AssetInfo> = assets.iter().filter(|a| a.release_id == rid).map(|a| asset_info(a, &refs)).collect();
            let (release, target) = r.into();
            out.push(ReleaseView { release, target_commitish: target, author, assets: infos });
        }
        Ok(out)
    }

    fn can_see_drafts(perm: Permission) -> bool {
        perm >= Permission::Write
    }

    pub async fn release_views(&self, actor: &Actor, repo: RepoId) -> Result<Vec<ReleaseView>> {
        let perm = self.need_view(actor, repo).await?;
        let sql = format!(
            "SELECT {REL_COLS} FROM releases WHERE repo_id = $1 AND ($2 OR NOT draft) ORDER BY COALESCE(published_at, created_at) DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, ReleaseRow>(&sql).bind(repo).bind(Self::can_see_drafts(perm)).fetch_all(self.db()).await?;
        self.release_views_from(rows).await
    }

    pub async fn release_view(&self, actor: &Actor, repo: RepoId, id: ReleaseId) -> Result<ReleaseView> {
        let perm = self.need_view(actor, repo).await?;
        let row = self.release_row(repo, id).await?;
        if row.draft && !Self::can_see_drafts(perm) {
            return Err(Error::NotFound);
        }
        self.release_views_from(vec![row]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    pub async fn release_view_by_tag(&self, actor: &Actor, repo: RepoId, tag: &str) -> Result<ReleaseView> {
        let perm = self.need_view(actor, repo).await?;
        let row = self.release_by_tag_row(repo, tag).await?;
        if row.draft && !Self::can_see_drafts(perm) {
            return Err(Error::NotFound);
        }
        self.release_views_from(vec![row]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    pub async fn release_latest_view(&self, actor: &Actor, repo: RepoId) -> Result<ReleaseView> {
        self.need_view(actor, repo).await?;
        // The newest mark wins, whatever the publish dates say; releases that were never marked
        // come after every marked one, newest first. A draft, a prerelease and a release that
        // asked not to be the latest never are.
        let sql = format!(
            "SELECT {REL_COLS} FROM releases WHERE repo_id = $1 AND NOT draft AND NOT prerelease AND NOT never_latest
             ORDER BY marked_latest_at DESC NULLS LAST, published_at DESC, id DESC LIMIT 1"
        );
        let row = sqlx::query_as::<_, ReleaseRow>(&sql).bind(repo).fetch_optional(self.db()).await?.ok_or(Error::NotFound)?;
        self.release_views_from(vec![row]).await?.into_iter().next().ok_or(Error::NotFound)
    }

    /// Applies what `make_latest` asked for to a published release. A prerelease is never the
    /// latest, so nothing is recorded for one.
    async fn apply_mark(
        &self,
        tx: &mut ch_core::services::DbTx<'_>,
        repo: RepoId,
        id: ReleaseId,
        tag: &str,
        mode: MakeLatest,
    ) -> Result<()> {
        let mark = match mode {
            MakeLatest::True => true,
            MakeLatest::False => {
                sqlx::query("UPDATE releases SET never_latest = true, marked_latest_at = NULL WHERE id = $1")
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
                return Ok(());
            }
            MakeLatest::Legacy => {
                let others: Vec<String> = sqlx::query_scalar(
                    "SELECT tag_name FROM releases WHERE repo_id = $1 AND id <> $2 AND NOT draft AND NOT prerelease AND NOT never_latest",
                )
                .bind(repo)
                .bind(id)
                .fetch_all(&mut **tx)
                .await?;
                // A version that cannot be read is treated as the newest, so a tag with no
                // version in it still ends up as the latest.
                match version_key(tag) {
                    None => true,
                    Some(mine) => others.iter().filter_map(|t| version_key(t)).all(|o| mine >= o),
                }
            }
        };
        if mark {
            sqlx::query("UPDATE releases SET marked_latest_at = now(), never_latest = false WHERE id = $1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    /// Creates the tag at `target` unless it exists already. A tag that exists is left alone.
    async fn ensure_tag(&self, actor: &Actor, repo: RepoId, tag: &str, target: &str, client: &ClientInfo) -> Result<()> {
        let full = format!("refs/tags/{tag}");
        if !self.d.git.refs(repo, Some(&full)).await?.iter().any(|r| r.name == full) {
            let sha = self.d.git.resolve(repo, target).await.map_err(|e| {
                if matches!(e, Error::NotFound) {
                    Error::Validation(vec![FieldError::custom(
                        "Release",
                        "target_commitish",
                        "The target does not exist in this repository",
                    )])
                } else {
                    e
                }
            })?;
            self.d.git.create_ref(actor, repo, &full, &sha, client).await?;
        }
        Ok(())
    }

    pub async fn release_create(&self, actor: &Actor, repo: RepoId, draft: ReleaseDraft, client: &ClientInfo) -> Result<ReleaseView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let tag = draft.tag_name.trim().to_string();
        if !valid_tag_name(&tag) {
            return Err(Error::invalid("Release", "tag_name", "tag_name is not a valid tag name"));
        }
        let target = draft.target_commitish.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| rrow.default_branch.clone());
        let mut body = util::body("Release", "body", &draft.body.clone().unwrap_or_default())?;
        let name = draft.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| tag.clone());
        if draft.generate_notes && body.trim().is_empty() {
            body = self.release_notes(&rrow, &tag, None, Some(&target)).await?.body;
        }
        if !draft.draft {
            self.ensure_tag(actor, repo, &tag, &target, client).await?;
        }
        let mut tx = self.db().begin().await?;
        let id: Option<ReleaseId> = sqlx::query_scalar(
            "INSERT INTO releases (repo_id, tag_name, target_commitish, name, body, draft, prerelease, author_id, published_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CASE WHEN $6 THEN NULL ELSE now() END) ON CONFLICT (repo_id, tag_name) DO NOTHING RETURNING id",
        )
        .bind(repo)
        .bind(&tag)
        .bind(&target)
        .bind(&name)
        .bind(&body)
        .bind(draft.draft)
        .bind(draft.prerelease)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Err(Error::Validation(vec![FieldError::new("Release", "tag_name", "already_exists")]));
        };
        if draft.make_latest == MakeLatest::False || (!draft.draft && !draft.prerelease) {
            self.apply_mark(&mut tx, repo, id, &tag, draft.make_latest).await?;
        }
        if !draft.draft {
            self.announce_published(&mut tx, actor, client, repo, id, &tag, user).await?;
        }
        tx.commit().await?;
        self.release_view(actor, repo, id).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn announce_published(
        &self,
        tx: &mut ch_core::services::DbTx<'_>,
        actor: &Actor,
        client: &ClientInfo,
        repo: RepoId,
        id: ReleaseId,
        tag: &str,
        user: UserId,
    ) -> Result<()> {
        self.d
            .audit
            .record(
                tx,
                AuditEvent::new(actor, client, action::RELEASE_PUBLISHED, Outcome::Success)
                    .target("release", id)
                    .details(json!({ "repo_id": repo.0, "tag": tag })),
            )
            .await?;
        self.d.events.emit(tx, DomainEvent::ReleasePublished { repo_id: repo, release_id: id, tag: tag.to_string(), actor: user }).await?;
        Ok(())
    }

    pub async fn release_update(
        &self,
        actor: &Actor,
        repo: RepoId,
        id: ReleaseId,
        edit: ReleaseEdit,
        client: &ClientInfo,
    ) -> Result<ReleaseView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        let rrow = self.repo(repo).await?;
        Self::ensure_writable(&rrow)?;
        let cur = self.release_row(repo, id).await?;
        let tag = match &edit.tag_name {
            Some(t) => {
                let t = t.trim().to_string();
                if !valid_tag_name(&t) {
                    return Err(Error::invalid("Release", "tag_name", "tag_name is not a valid tag name"));
                }
                t
            }
            None => cur.tag_name.clone(),
        };
        let target = edit.target_commitish.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| cur.target_commitish.clone());
        let draft = edit.draft.unwrap_or(cur.draft);
        let prerelease = edit.prerelease.unwrap_or(cur.prerelease);
        let name = edit.name.clone().unwrap_or_else(|| cur.name.clone());
        let mut body = match &edit.body {
            Some(b) => util::body("Release", "body", b)?,
            None => cur.body.clone(),
        };
        if edit.generate_notes == Some(true) && body.trim().is_empty() {
            body = self.release_notes(&rrow, &tag, None, Some(&target)).await?.body;
        }
        if !draft {
            self.ensure_tag(actor, repo, &tag, &target, client).await?;
        }
        let publishing = cur.draft && !draft;
        let mut tx = self.db().begin().await?;
        let res = sqlx::query(
            "UPDATE releases SET tag_name = $2, target_commitish = $3, name = $4, body = $5, draft = $6, prerelease = $7,
                    published_at = CASE WHEN $6 THEN NULL WHEN published_at IS NULL THEN now() ELSE published_at END WHERE id = $1",
        )
        .bind(id)
        .bind(&tag)
        .bind(&target)
        .bind(&name)
        .bind(&body)
        .bind(draft)
        .bind(prerelease)
        .execute(&mut *tx)
        .await;
        match res {
            Ok(_) => {}
            Err(e) if util::is_unique(&e) => return Err(Error::Validation(vec![FieldError::new("Release", "tag_name", "already_exists")])),
            Err(e) => return Err(e.into()),
        }
        if edit.make_latest == Some(MakeLatest::False) || (!draft && !prerelease && (publishing || edit.make_latest.is_some())) {
            self.apply_mark(&mut tx, repo, id, &tag, edit.make_latest.unwrap_or_default()).await?;
        }
        if publishing {
            self.announce_published(&mut tx, actor, client, repo, id, &tag, user).await?;
        }
        tx.commit().await?;
        self.release_view(actor, repo, id).await
    }

    pub async fn release_delete(&self, actor: &Actor, repo: RepoId, id: ReleaseId, client: &ClientInfo) -> Result<()> {
        self.need(actor, repo, Permission::Write).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let cur = self.release_row(repo, id).await?;
        let assets: Vec<String> =
            sqlx::query_scalar("SELECT blob_sha256 FROM release_assets WHERE release_id = $1").bind(id).fetch_all(self.db()).await?;
        let mut tx = self.db().begin().await?;
        sqlx::query("DELETE FROM releases WHERE id = $1").bind(id).execute(&mut *tx).await?;
        for sha in &assets {
            ch_db::blobs::release(&mut tx, sha).await?;
        }
        self.d
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::RELEASE_DELETED, Outcome::Success)
                    .target("release", id)
                    .details(json!({ "repo_id": repo.0, "tag": cur.tag_name })),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ------------------------------------------------------------ notes

    pub async fn release_generate_notes(
        &self,
        actor: &Actor,
        repo: RepoId,
        tag: &str,
        previous: Option<&str>,
        target: Option<&str>,
    ) -> Result<GeneratedNotes> {
        self.need(actor, repo, Permission::Write).await?;
        let rrow = self.repo(repo).await?;
        if !valid_tag_name(tag) {
            return Err(Error::invalid("Release", "tag_name", "tag_name is not a valid tag name"));
        }
        self.release_notes(&rrow, tag, previous, target).await
    }

    async fn release_notes(
        &self,
        repo: &crate::core::RepoRow,
        tag: &str,
        previous: Option<&str>,
        target: Option<&str>,
    ) -> Result<GeneratedNotes> {
        // The previous tag is the one asked for, else the newest published release's tag.
        let prev: Option<String> = match previous {
            Some(p) if !p.trim().is_empty() => Some(p.trim().to_string()),
            _ => sqlx::query_scalar("SELECT tag_name FROM releases WHERE repo_id = $1 AND NOT draft AND tag_name <> $2 ORDER BY published_at DESC, id DESC LIMIT 1")
                .bind(repo.id)
                .bind(tag)
                .fetch_optional(self.db())
                .await?,
        };
        let head = target.map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&repo.default_branch);
        let cfg = self.notes_config(repo).await;
        // The pull requests that are in this release are the ones whose merge commit sits between
        // the previous tag and the target, however long ago or on whichever branch they were
        // merged. With no previous tag, everything merged into the target branch counts.
        let rows: Vec<(i64, String, String, Vec<String>)> = match &prev {
            Some(p) => {
                let base = format!("refs/tags/{p}");
                if self.d.git.resolve(repo.id, &base).await.is_err_and(|e| matches!(e, Error::NotFound)) {
                    return Err(Error::Validation(vec![FieldError::custom(
                        "Release",
                        "previous_tag_name",
                        "The previous tag does not exist",
                    )]));
                }
                let shas = match self.d.git.range_shas(repo.id, Some(&base), head, 20_000).await {
                    Ok(s) => s,
                    Err(Error::NotFound) => {
                        return Err(Error::Validation(vec![FieldError::custom(
                            "Release",
                            "target_commitish",
                            "The target does not exist in this repository",
                        )]));
                    }
                    Err(e) => return Err(e),
                };
                sqlx::query_as(NOTES_SQL_BY_COMMITS).bind(repo.id).bind(&shas).fetch_all(self.db()).await?
            }
            None => sqlx::query_as(NOTES_SQL_BY_BRANCH).bind(repo.id).bind(head).fetch_all(self.db()).await?,
        };
        let items: Vec<NoteItem> =
            rows.into_iter().map(|(number, title, author, labels)| NoteItem { number, title, author, labels }).collect();
        let base = format!("{}/{}", self.d.external_url(), repo.full_name());
        let body = render_notes_with(&items, &cfg, prev.as_deref().map(|p| (base.as_str(), p, tag)));
        Ok(GeneratedNotes { name: tag.to_string(), body })
    }

    /// The repository's `.github/release.yml` (or `.codehaven/release.yml`) at its default branch.
    /// A file that is missing or does not parse is the default grouping, never an error.
    async fn notes_config(&self, repo: &crate::core::RepoRow) -> ch_core::release_notes::NotesConfig {
        for path in [".codehaven/release.yml", ".github/release.yml"] {
            if let Ok(b) = self.d.git.blob(repo.id, &repo.default_branch, path, ch_core::release_notes::MAX_CONFIG_BYTES as u64).await
                && !b.truncated
                && let Ok(text) = String::from_utf8(b.content)
            {
                return ch_core::release_notes::parse_config(&text).unwrap_or_default();
            }
        }
        Default::default()
    }

    // ------------------------------------------------------------ assets

    async fn asset_row(&self, repo: RepoId, asset: i64) -> Result<(AssetRow, ReleaseRow)> {
        let a = sqlx::query_as::<_, AssetRow>(&format!("SELECT {ASSET_COLS} FROM release_assets WHERE id = $1"))
            .bind(asset)
            .fetch_optional(self.db())
            .await?
            .ok_or(Error::NotFound)?;
        let r = self.release_row(repo, a.release_id).await?;
        Ok((a, r))
    }

    pub async fn release_asset(&self, actor: &Actor, repo: RepoId, asset: i64) -> Result<AssetInfo> {
        let perm = self.need_view(actor, repo).await?;
        let (a, r) = self.asset_row(repo, asset).await?;
        if r.draft && !Self::can_see_drafts(perm) {
            return Err(Error::NotFound);
        }
        let refs = self.user_refs(&a.uploader_id.into_iter().collect::<Vec<_>>()).await?;
        Ok(asset_info(&a, &refs))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn release_upload_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        release: ReleaseId,
        name: &str,
        label: Option<&str>,
        content_type: &str,
        body: BoxStream<'static, std::io::Result<Bytes>>,
        _client: &ClientInfo,
    ) -> Result<AssetInfo> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        self.release_row(repo, release).await?;
        let name = name.trim();
        if !valid_asset_name(name) {
            return Err(Error::invalid("ReleaseAsset", "name", "name must be a file name without slashes"));
        }
        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM release_assets WHERE release_id = $1 AND name = $2")
            .bind(release)
            .bind(name)
            .fetch_optional(self.db())
            .await?;
        if existing.is_some() {
            return Err(Error::Validation(vec![FieldError::new("ReleaseAsset", "name", "already_exists")]));
        }
        let ct = if content_type.trim().is_empty() || content_type.len() > 200 { "application/octet-stream" } else { content_type.trim() };
        let blob: BlobRef = self.d.blobs.put_stream(body, MAX_ASSET_BYTES).await?;
        let mut tx = self.db().begin().await?;
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO release_assets (release_id, name, label, content_type, size, blob_sha256, uploader_id) VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (release_id, name) DO NOTHING RETURNING id",
        )
        .bind(release)
        .bind(name)
        .bind(label.unwrap_or("").trim())
        .bind(ct)
        .bind(blob.size as i64)
        .bind(&blob.sha256)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            drop(tx);
            self.drop_orphan_blob(&blob.sha256).await;
            return Err(Error::Validation(vec![FieldError::new("ReleaseAsset", "name", "already_exists")]));
        };
        ch_db::blobs::add_ref(&mut tx, &blob).await?;
        tx.commit().await?;
        self.release_asset(actor, repo, id).await
    }

    /// A stored file that nothing references and the table has never seen is deleted at once.
    async fn drop_orphan_blob(&self, sha: &str) {
        let known: Option<i64> = sqlx::query_scalar("SELECT refcount::bigint FROM blobs WHERE sha256 = $1")
            .bind(sha)
            .fetch_optional(self.db())
            .await
            .ok()
            .flatten();
        if known.is_none() {
            let _ = self.d.blobs.delete(sha).await;
        }
    }

    pub async fn release_update_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        asset: i64,
        name: Option<&str>,
        label: Option<&str>,
        _client: &ClientInfo,
    ) -> Result<AssetInfo> {
        writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (cur, _) = self.asset_row(repo, asset).await?;
        let name = match name.map(str::trim) {
            Some(n) if !valid_asset_name(n) => {
                return Err(Error::invalid("ReleaseAsset", "name", "name must be a file name without slashes"));
            }
            Some(n) => n.to_string(),
            None => cur.name.clone(),
        };
        let label = match label {
            Some(l) => l.trim().chars().take(255).collect::<String>(),
            None => cur.label.clone(),
        };
        let res = sqlx::query("UPDATE release_assets SET name = $2, label = $3 WHERE id = $1")
            .bind(asset)
            .bind(&name)
            .bind(&label)
            .execute(self.db())
            .await;
        match res {
            Ok(_) => {}
            Err(e) if util::is_unique(&e) => {
                return Err(Error::Validation(vec![FieldError::new("ReleaseAsset", "name", "already_exists")]));
            }
            Err(e) => return Err(e.into()),
        }
        self.release_asset(actor, repo, asset).await
    }

    pub async fn release_delete_asset(&self, actor: &Actor, repo: RepoId, asset: i64, _client: &ClientInfo) -> Result<()> {
        self.need(actor, repo, Permission::Write).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (a, _) = self.asset_row(repo, asset).await?;
        let mut tx = self.db().begin().await?;
        sqlx::query("DELETE FROM release_assets WHERE id = $1").bind(asset).execute(&mut *tx).await?;
        ch_db::blobs::release(&mut tx, &a.blob_sha256).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn open(&self, a: AssetRow) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        let stream = self.d.blobs.get_stream(&a.blob_sha256).await?;
        sqlx::query("UPDATE release_assets SET download_count = download_count + 1 WHERE id = $1").bind(a.id).execute(self.db()).await?;
        let refs = self.user_refs(&a.uploader_id.into_iter().collect::<Vec<_>>()).await?;
        let mut info = asset_info(&a, &refs);
        info.download_count += 1;
        Ok((info, stream))
    }

    pub async fn release_open_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        tag: &str,
        name: &str,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        let perm = self.need_view(actor, repo).await?;
        let r = self.release_by_tag_row(repo, tag).await?;
        if r.draft && !Self::can_see_drafts(perm) {
            return Err(Error::NotFound);
        }
        let a = sqlx::query_as::<_, AssetRow>(&format!("SELECT {ASSET_COLS} FROM release_assets WHERE release_id = $1 AND name = $2"))
            .bind(r.id)
            .bind(name)
            .fetch_optional(self.db())
            .await?
            .ok_or(Error::NotFound)?;
        self.open(a).await
    }

    pub async fn release_open_asset_by_id(
        &self,
        actor: &Actor,
        repo: RepoId,
        asset: i64,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        let perm = self.need_view(actor, repo).await?;
        let (a, r) = self.asset_row(repo, asset).await?;
        if r.draft && !Self::can_see_drafts(perm) {
            return Err(Error::NotFound);
        }
        self.open(a).await
    }
}

fn asset_info(a: &AssetRow, refs: &std::collections::HashMap<UserId, UserRef>) -> AssetInfo {
    AssetInfo {
        id: a.id,
        release_id: a.release_id,
        name: a.name.clone(),
        label: a.label.clone(),
        content_type: a.content_type.clone(),
        size: a.size,
        sha256: a.blob_sha256.clone(),
        download_count: a.download_count,
        uploader: a.uploader_id.and_then(|u| refs.get(&u).cloned()),
        created_at: a.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_names() {
        for ok in ["v1.0.0", "release/2026-01", "1.2", "v1.0-rc.1", "café"] {
            assert!(valid_tag_name(ok), "{ok}");
        }
        for bad in [
            "", "-v1", "a b", "a..b", "a~1", "a^", "a:b", "a?", "a*", "a[0]", "a\\b", "/a", "a/", "a//b", "a.lock", ".hidden", "a/.b", "@",
            "a@{b}", "x.", "a\tb",
        ] {
            assert!(!valid_tag_name(bad), "{bad}");
        }
        assert!(!valid_tag_name(&"x".repeat(201)));
    }

    #[test]
    fn asset_names() {
        assert!(valid_asset_name("haven-0.1.0.tar.gz"));
        assert!(!valid_asset_name("a/b"));
        assert!(!valid_asset_name(""));
        assert!(!valid_asset_name(".."));
        assert!(!valid_asset_name("a\nb"));
    }

    fn item(n: i64, t: &str, labels: &[&str]) -> NoteItem {
        NoteItem { number: n, title: t.into(), author: "alice".into(), labels: labels.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn notes_group_by_label() {
        let md = render_notes(
            &[
                item(1, "Add search", &["enhancement"]),
                item(2, "Fix crash", &["bug"]),
                item(3, "Tidy", &[]),
                item(4, "Drop old flag", &["breaking", "bug"]),
                item(5, "Docs", &["Documentation"]),
            ],
            Some(("https://h.example/o/r", "v1", "v2")),
        );
        let breaking = md.find("## Breaking changes").unwrap();
        let features = md.find("## New features").unwrap();
        let fixes = md.find("## Fixes").unwrap();
        let docs = md.find("## Documentation").unwrap();
        let other = md.find("## Other changes").unwrap();
        assert!(breaking < features && features < fixes && fixes < docs && docs < other);
        assert!(md.contains("* Drop old flag (#4) by @alice"));
        assert!(md.contains("**Full changelog**: https://h.example/o/r/compare/v1...v2"));
        // A pull request with two labels appears once, under the first category.
        assert_eq!(md.matches("#4").count(), 1);
    }

    #[test]
    fn notes_with_only_uncategorised_items() {
        let md = render_notes(&[item(3, "Tidy", &[])], None);
        assert!(md.starts_with("## What changed"));
        assert!(!md.contains("Full changelog"));
    }

    #[test]
    fn empty_notes_say_so() {
        assert!(render_notes(&[], None).contains("No pull requests were merged"));
    }
}
