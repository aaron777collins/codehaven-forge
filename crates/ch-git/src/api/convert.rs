//! Domain models to REST v3 types, with the `url` and `html_url` fields the models leave out.

use super::Api;
use base64::Engine;
use ch_api_types::common::{Permissions, ShaRef, SimpleUser};
use ch_api_types::repos::*;
use ch_core::model::*;
use ch_core::{Permission, Result, UserId};
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use std::collections::HashMap;

pub fn api_url(base: &str) -> String {
    format!("{base}/api/v3")
}

#[derive(Debug, Clone, FromRow)]
pub struct UserRef {
    pub id: i64,
    pub login: String,
    pub kind: String,
    pub is_admin: bool,
}

pub fn simple_user(base: &str, u: &UserRef) -> SimpleUser {
    SimpleUser {
        login: u.login.clone(),
        id: u.id,
        avatar_url: format!("{base}/avatars/{}.svg", u.id),
        url: format!("{}/users/{}", api_url(base), u.login),
        html_url: format!("{base}/{}", u.login),
        kind: match u.kind.as_str() {
            "org" => "Organization",
            "bot" => "Bot",
            _ => "User",
        }
        .into(),
        // Site admin needs a session with two-factor, which a listing cannot know, so it is
        // never claimed here.
        site_admin: false,
    }
}

pub async fn user_ref(api: &Api, id: UserId) -> Result<UserRef> {
    let u: UserRef = sqlx::query_as("SELECT id, username::text AS login, kind, is_admin FROM users WHERE id = $1")
        .bind(id.0)
        .fetch_one(&api.state.db)
        .await?;
    Ok(u)
}

pub async fn user_by_name(api: &Api, name: &str) -> Result<UserRef> {
    let u: Option<UserRef> = sqlx::query_as("SELECT id, username::text AS login, kind, is_admin FROM users WHERE username = $1::citext")
        .bind(name)
        .fetch_optional(&api.state.db)
        .await?;
    u.ok_or(ch_core::Error::NotFound)
}

/// Accounts by commit email, so authors link to their profiles. An email links to an account
/// only when it is verified, or when it is that account's noreply address.
pub async fn users_by_email(api: &Api, emails: &[String]) -> Result<HashMap<String, UserRef>> {
    if emails.is_empty() {
        return Ok(HashMap::new());
    }
    #[derive(FromRow)]
    struct Row {
        email: String,
        id: i64,
        login: String,
        kind: String,
        is_admin: bool,
    }
    let lowered: Vec<String> = emails.iter().map(|e| e.to_lowercase()).collect();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT lower(e.email::text) AS email, u.id, u.username::text AS login, u.kind, u.is_admin \
         FROM user_emails e JOIN users u ON u.id = e.user_id WHERE lower(e.email::text) = ANY($1) AND e.verified_at IS NOT NULL",
    )
    .bind(&lowered)
    .fetch_all(&api.state.db)
    .await?;
    let mut out: HashMap<String, UserRef> =
        rows.into_iter().map(|r| (r.email, UserRef { id: r.id, login: r.login, kind: r.kind, is_admin: r.is_admin })).collect();
    #[derive(FromRow)]
    struct ById {
        id: i64,
        login: String,
        kind: String,
        is_admin: bool,
    }
    for email in &lowered {
        let Some((id, name)) = ch_core::noreply::parse_noreply(email, &api.state.config.server.external_url) else { continue };
        let u: Option<ById> = sqlx::query_as("SELECT u.id, u.username::text AS login, u.kind, u.is_admin FROM users u WHERE u.id = $1")
            .bind(id)
            .fetch_optional(&api.state.db)
            .await?;
        if let Some(u) = u
            && u.login.to_lowercase() == name
        {
            out.insert(email.clone(), UserRef { id: u.id, login: u.login, kind: u.kind, is_admin: u.is_admin });
        }
    }
    Ok(out)
}

pub fn permissions(p: Permission) -> Permissions {
    Permissions {
        admin: p >= Permission::Admin,
        maintain: p >= Permission::Maintain,
        push: p >= Permission::Write,
        triage: p >= Permission::Triage,
        pull: p >= Permission::View,
    }
}

#[derive(FromRow)]
struct Extras {
    has_issues: bool,
    allow_squash_merge: bool,
    allow_merge_commit: bool,
    allow_rebase_merge: bool,
    delete_branch_on_merge: bool,
    size_kb: i64,
    updated_at: DateTime<Utc>,
    owner_kind: String,
    stars: i64,
    watchers: i64,
    forks: i64,
    open_issues: i64,
}

fn repo_path(r: &Repo) -> String {
    format!("{}/{}", r.owner_name, r.name)
}

pub async fn repository(api: &Api, repo: &Repo) -> Result<Repository> {
    let mut r = repository_inner(api, repo).await?;
    if let Some(parent_id) = repo.fork_parent_id
        && let Ok(parent) = api.core.do_get(&api.actor, parent_id).await
    {
        r.parent = Some(Box::new(repository_inner(api, &parent).await?));
    }
    Ok(r)
}

async fn repository_inner(api: &Api, repo: &Repo) -> Result<Repository> {
    let e: Extras = sqlx::query_as(
        "SELECT r.has_issues, r.allow_squash_merge, r.allow_merge_commit, r.allow_rebase_merge, r.delete_branch_on_merge, \
         r.size_kb, r.updated_at, u.kind AS owner_kind, \
         (SELECT count(*) FROM stars s WHERE s.repo_id = r.id) AS stars, \
         (SELECT count(*) FROM watches w WHERE w.repo_id = r.id AND w.level <> 'ignore') AS watchers, \
         (SELECT count(*) FROM repositories f WHERE f.fork_parent_id = r.id \
            AND (f.access_tier = 'public' OR (NOT $2 AND (f.access_tier = 'internal' OR f.owner_id = $3)))) AS forks, \
         (SELECT count(*) FROM issues i WHERE i.repo_id = r.id AND i.open AND NOT i.is_pull) AS open_issues \
         FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(repo.id.0)
    // Counts only forks the caller could see: public ones signed out, and internal or their own
    // signed in.
    .bind(api.actor.is_anonymous())
    .bind(api.actor.author_id().map(|u| u.0).unwrap_or(0))
    .fetch_one(&api.state.db)
    .await?;
    let base = api.base();
    let owner = simple_user(base, &UserRef { id: repo.owner_id.0, login: repo.owner_name.clone(), kind: e.owner_kind, is_admin: false });
    let perm = api.state.authz.repo_permission(&api.actor, repo.id).await.unwrap_or(Permission::None);
    let full = repo_path(repo);
    // GitHub's flag: true unless anyone may read it. Internal is private to the internet.
    let private = repo.visibility.is_private_flag();
    Ok(Repository {
        id: repo.id.0,
        name: repo.name.clone(),
        full_name: full.clone(),
        owner,
        private,
        html_url: format!("{base}/{full}"),
        description: if repo.description.is_empty() { None } else { Some(repo.description.clone()) },
        fork: repo.fork_parent_id.is_some(),
        url: format!("{}/repos/{full}", api_url(base)),
        clone_url: format!("{base}/{full}.git"),
        ssh_url: format!("ssh://git@{}/{full}.git", api.state.config.ssh.external),
        default_branch: repo.default_branch.clone(),
        visibility: repo.visibility.as_str().into(),
        contribution_policy: repo.contribution_policy.as_str().into(),
        archived: repo.archived,
        disabled: false,
        topics: repo.topics.clone(),
        has_issues: e.has_issues,
        created_at: repo.created_at,
        updated_at: e.updated_at,
        pushed_at: repo.pushed_at,
        size: e.size_kb,
        stargazers_count: e.stars,
        watchers_count: e.watchers,
        forks_count: e.forks,
        open_issues_count: e.open_issues,
        permissions: Some(permissions(perm)),
        parent: None,
        empty: repo.is_empty,
        allow_squash_merge: e.allow_squash_merge,
        allow_merge_commit: e.allow_merge_commit,
        allow_rebase_merge: e.allow_rebase_merge,
        delete_branch_on_merge: e.delete_branch_on_merge,
    })
}

pub async fn repositories(api: &Api, repos: &[Repo]) -> Result<Vec<Repository>> {
    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        out.push(repository_inner(api, r).await?);
    }
    Ok(out)
}

fn identity(s: &Signature) -> GitIdentity {
    GitIdentity { name: s.name.clone(), email: s.email.clone(), date: s.at }
}

/// A unified diff for one file, rebuilt from its hunks, for the `patch` field.
pub fn patch_text(f: &FileDiff) -> String {
    let mut out = String::new();
    for h in &f.hunks {
        out.push_str(&h.header);
        out.push('\n');
        for l in &h.lines {
            out.push(match l.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&l.text);
            out.push('\n');
        }
    }
    out
}

pub fn commit_files(diff: &Diff) -> Vec<CommitFile> {
    diff.files
        .iter()
        .map(|f| {
            let name = f.new_path.clone().or_else(|| f.old_path.clone()).unwrap_or_default();
            CommitFile {
                filename: name,
                status: match f.status {
                    FileStatus::Added => "added",
                    FileStatus::Modified => "modified",
                    FileStatus::Removed => "removed",
                    FileStatus::Renamed => "renamed",
                    FileStatus::Copied => "copied",
                }
                .into(),
                additions: f.additions as i64,
                deletions: f.deletions as i64,
                changes: (f.additions + f.deletions) as i64,
                patch: if f.binary || f.collapsed { None } else { Some(patch_text(f)) },
                previous_filename: if f.status == FileStatus::Renamed { f.old_path.clone() } else { None },
                sha: String::new(),
            }
        })
        .collect()
}

/// A commit as the API shows it. `diff` adds `stats` and `files`.
pub async fn commit(api: &Api, repo: &Repo, c: &CommitInfo, diff: Option<&Diff>) -> Result<Commit> {
    let mut v = commits(api, repo, std::slice::from_ref(c)).await?;
    let mut out = v.remove(0);
    if let Some(d) = diff {
        let files = commit_files(d);
        let additions: i64 = files.iter().map(|f| f.additions).sum();
        let deletions: i64 = files.iter().map(|f| f.deletions).sum();
        out.stats = Some(CommitStats { additions, deletions, total: additions + deletions });
        out.files = files;
    }
    Ok(out)
}

pub async fn commits(api: &Api, repo: &Repo, list: &[CommitInfo]) -> Result<Vec<Commit>> {
    let base = api.base();
    let full = repo_path(repo);
    let emails: Vec<String> = list.iter().flat_map(|c| [c.author.email.clone(), c.committer.email.clone()]).collect();
    let users = users_by_email(api, &emails).await?;
    let lookup = |email: &str| users.get(&email.to_lowercase()).map(|u| simple_user(base, u));
    Ok(list
        .iter()
        .map(|c| Commit {
            sha: c.sha.clone(),
            url: format!("{}/repos/{full}/commits/{}", api_url(base), c.sha),
            html_url: format!("{base}/{full}/commit/{}", c.sha),
            commit: CommitDetail {
                author: identity(&c.author),
                committer: identity(&c.committer),
                message: c.message.clone(),
                tree: ShaRef { sha: c.tree.clone(), url: Some(format!("{}/repos/{full}/git/trees/{}", api_url(base), c.tree)) },
                comment_count: 0,
            },
            author: lookup(&c.author.email),
            committer: lookup(&c.committer.email),
            parents: c
                .parents
                .iter()
                .map(|p| ShaRef { sha: p.clone(), url: Some(format!("{}/repos/{full}/commits/{p}", api_url(base))) })
                .collect(),
            stats: None,
            files: Vec::new(),
        })
        .collect())
}

pub fn compare_status(ahead: u32, behind: u32) -> CompareStatus {
    match (ahead, behind) {
        (0, 0) => CompareStatus::Identical,
        (_, 0) => CompareStatus::Ahead,
        (0, _) => CompareStatus::Behind,
        _ => CompareStatus::Diverged,
    }
}

pub fn content_kind(k: EntryKind) -> ContentKind {
    match k {
        EntryKind::Blob => ContentKind::File,
        EntryKind::Tree => ContentKind::Dir,
        EntryKind::Symlink => ContentKind::Symlink,
        EntryKind::Commit => ContentKind::Submodule,
    }
}

pub fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    // Clients wrap base64 at 60 or 76 columns; ignore the line breaks.
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(cleaned).ok()
}

/// One entry of a contents listing, or a file with `content` when `blob` is given.
pub fn content_entry(base: &str, repo: &Repo, git_ref: &str, e: &TreeEntry, blob: Option<&Blob>) -> ContentEntry {
    let full = repo_path(repo);
    let kind = content_kind(e.kind);
    let enc = |s: &str| ch_core::access::encode_query_value(s);
    ContentEntry {
        name: e.name.clone(),
        path: e.path.clone(),
        sha: e.sha.clone(),
        size: e.size.unwrap_or(0) as i64,
        kind,
        url: format!("{}/repos/{full}/contents/{}?ref={}", api_url(base), e.path, enc(git_ref)),
        html_url: format!("{base}/{full}/{}/{}/{}", if kind == ContentKind::Dir { "tree" } else { "blob" }, git_ref, e.path),
        download_url: if kind == ContentKind::File { Some(format!("{base}/{full}/raw/{git_ref}/{}", e.path)) } else { None },
        encoding: blob.map(|_| "base64".to_string()),
        content: blob.map(|b| encode_base64(&b.content)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_follow_the_level() {
        let p = permissions(Permission::Write);
        assert!(p.pull && p.triage && p.push && !p.maintain && !p.admin);
        let p = permissions(Permission::None);
        assert!(!p.pull);
        assert!(permissions(Permission::Admin).admin);
    }

    #[test]
    fn compare_status_cases() {
        assert_eq!(compare_status(0, 0), CompareStatus::Identical);
        assert_eq!(compare_status(3, 0), CompareStatus::Ahead);
        assert_eq!(compare_status(0, 2), CompareStatus::Behind);
        assert_eq!(compare_status(1, 1), CompareStatus::Diverged);
    }

    #[test]
    fn base64_ignores_line_breaks() {
        let enc = encode_base64(b"hello world");
        let wrapped = format!("{}\n{}", &enc[..4], &enc[4..]);
        assert_eq!(decode_base64(&wrapped).unwrap(), b"hello world");
        assert!(decode_base64("!!!").is_none());
    }

    #[test]
    fn user_kinds_map_to_api_names() {
        let u = |k: &str| simple_user("http://x", &UserRef { id: 1, login: "a".into(), kind: k.into(), is_admin: true });
        assert_eq!(u("org").kind, "Organization");
        assert_eq!(u("bot").kind, "Bot");
        assert_eq!(u("user").kind, "User");
        assert!(!u("user").site_admin);
    }

    #[test]
    fn patches_round_trip_the_marker_characters() {
        let f = FileDiff {
            status: FileStatus::Modified,
            old_path: Some("a".into()),
            new_path: Some("a".into()),
            binary: false,
            collapsed: false,
            additions: 1,
            deletions: 1,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                lines: vec![
                    DiffLine { kind: LineKind::Context, old_no: Some(1), new_no: Some(1), text: "same".into() },
                    DiffLine { kind: LineKind::Removed, old_no: Some(2), new_no: None, text: "old".into() },
                    DiffLine { kind: LineKind::Added, old_no: None, new_no: Some(2), text: "new".into() },
                ],
            }],
        };
        assert_eq!(patch_text(&f), "@@ -1,2 +1,2 @@\n same\n-old\n+new\n");
    }
}
