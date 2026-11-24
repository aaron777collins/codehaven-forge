//! Test support for the collab crate: fake git, identity and CI services over a real Postgres,
//! and a `World` with a few people and a repository.

#![allow(dead_code, clippy::unwrap_used)]

use async_trait::async_trait;
use bytes::Bytes;
use ch_collab::{Core, Deps};
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::services::*;
use ch_core::{Actor, AuthVia, Error, Permission, RepoId, Result, TeamId, UserId};
use ch_testkit::TestEnv;
use ch_testkit::fixtures::{client, user_actor};
use chrono::{TimeZone, Utc};
use futures::stream::BoxStream;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn unavailable<T>() -> Result<T> {
    Err(Error::unavailable("not part of this test"))
}

// ---------------------------------------------------------------- authz

pub struct TestAuthz {
    pub pool: PgPool,
}

#[async_trait]
impl Authz for TestAuthz {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        if actor.is_system() || actor.is_site_admin() {
            return Ok(Permission::Admin);
        }
        let Some(user) = actor.user_id() else { return Ok(Permission::None) };
        let row: Option<(UserId, String)> =
            sqlx::query_as("SELECT owner_id, visibility FROM repositories WHERE id = $1").bind(repo).fetch_optional(&self.pool).await?;
        let Some((owner, visibility)) = row else { return Ok(Permission::None) };
        if owner == user {
            return Ok(Permission::Admin);
        }
        let perm: Option<String> = sqlx::query_scalar("SELECT permission FROM repo_collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(repo)
            .bind(user)
            .fetch_optional(&self.pool)
            .await?;
        let mut p = perm.and_then(|p| Permission::parse(&p)).unwrap_or(Permission::None);
        if p == Permission::None && visibility == "public" {
            p = Permission::Read;
        }
        if let Actor::Token { scopes, .. } = actor {
            p = p.min(scopes.repo_cap());
        }
        Ok(p)
    }
    async fn org_role(&self, _actor: &Actor, _org: ch_core::OrgId) -> Result<Option<ch_core::services::OrgRole>> {
        Ok(None)
    }
    /// Ownership or a collaborator row. A public repository's tier alone is not a grant.
    async fn has_grant(&self, actor: &Actor, repo: RepoId) -> Result<bool> {
        if actor.is_system() || actor.is_site_admin() {
            return Ok(true);
        }
        let Some(user) = actor.user_id() else { return Ok(false) };
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT r.owner_id = $2 OR EXISTS (SELECT 1 FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $2) FROM repositories r WHERE r.id = $1",
        )
        .bind(repo)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some_and(|r| r.0))
    }
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let p = self.repo_permission(actor, repo).await?;
        if p < Permission::Read {
            return Err(Error::NotFound);
        }
        if p < needed {
            return Err(Error::forbidden("insufficient permission"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- repos and accounts

pub struct FakeRepos {
    pub pool: PgPool,
    pub authz: Arc<TestAuthz>,
}

type RepoRowT = (
    RepoId,
    UserId,
    String,
    String,
    String,
    String,
    String,
    bool,
    bool,
    Option<RepoId>,
    Vec<String>,
    Option<chrono::DateTime<Utc>>,
    chrono::DateTime<Utc>,
);

impl FakeRepos {
    async fn load(&self, sql_where: &str, a: &str, b: &str, id: i64) -> Result<Repo> {
        let sql = format!(
            "SELECT r.id, r.owner_id, u.username::text, r.name::text, r.description, r.access_tier, r.default_branch, r.archived, r.is_empty, r.fork_parent_id, r.topics, r.pushed_at, r.created_at
             FROM repositories r JOIN users u ON u.id = r.owner_id WHERE {sql_where}"
        );
        let mut q = sqlx::query_as::<_, RepoRowT>(&sql);
        q = if sql_where.contains("$1 AND") { q.bind(a).bind(b) } else { q.bind(id) };
        let r = q.fetch_optional(&self.pool).await?.ok_or(Error::NotFound)?;
        Ok(Repo {
            id: r.0,
            owner_id: r.1,
            owner_name: r.2,
            name: r.3,
            description: r.4,
            visibility: Visibility::parse(&r.5).unwrap_or(Visibility::Private),
            default_branch: r.6,
            archived: r.7,
            is_empty: r.8,
            fork_parent_id: r.9,
            topics: r.10,
            pushed_at: r.11,
            created_at: r.12,
            contribution_policy: Default::default(),
        })
    }
}

#[async_trait]
impl Repos for FakeRepos {
    async fn create(&self, _a: &Actor, _n: NewRepo, _c: &ClientInfo) -> Result<Repo> {
        unavailable()
    }
    async fn get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo> {
        let r = self.load("u.username = $1 AND r.name = $2", owner, name, 0).await?;
        if self.authz.repo_permission(actor, r.id).await? < Permission::Read {
            return Err(Error::NotFound);
        }
        Ok(r)
    }
    async fn get(&self, actor: &Actor, id: RepoId) -> Result<Repo> {
        let r = self.load("r.id = $1", "", "", id.0).await?;
        if self.authz.repo_permission(actor, r.id).await? < Permission::Read {
            return Err(Error::NotFound);
        }
        Ok(r)
    }
    async fn list_for_owner(&self, _a: &Actor, _o: UserId) -> Result<Vec<Repo>> {
        unavailable()
    }
    async fn rename(&self, _a: &Actor, _i: RepoId, _n: &str, _c: &ClientInfo) -> Result<Repo> {
        unavailable()
    }
    async fn transfer(&self, _a: &Actor, _i: RepoId, _n: UserId, _c: &ClientInfo) -> Result<Repo> {
        unavailable()
    }
    async fn fork(&self, _a: &Actor, _i: RepoId, _into: UserId, _c: &ClientInfo) -> Result<Repo> {
        unavailable()
    }
    async fn delete(&self, _a: &Actor, _i: RepoId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn collaborators(&self, _a: &Actor, _id: RepoId) -> Result<Vec<Collaborator>> {
        unavailable()
    }
    async fn set_collaborator(&self, _a: &Actor, _id: RepoId, _u: UserId, _p: Permission, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn remove_collaborator(&self, _a: &Actor, _id: RepoId, _u: UserId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn protections(&self, _a: &Actor, id: RepoId) -> Result<Vec<BranchProtection>> {
        let rows: Vec<(String, i32, bool, bool, Vec<String>, bool, bool, bool, bool, bool)> = sqlx::query_as(
            "SELECT pattern, required_approvals, dismiss_stale_reviews, require_code_owner_review, required_checks, strict, require_resolved_threads, block_force_push, block_deletion, allow_direct_push
             FROM branch_protections WHERE repo_id = $1",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| BranchProtection {
                pattern: r.0,
                required_approvals: r.1 as u32,
                dismiss_stale_reviews: r.2,
                require_code_owner_review: r.3,
                required_checks: r.4,
                strict: r.5,
                require_resolved_threads: r.6,
                block_force_push: r.7,
                block_deletion: r.8,
                allow_direct_push: r.9,
            })
            .collect())
    }
    async fn set_protection(&self, _a: &Actor, _id: RepoId, _p: BranchProtection, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn remove_protection(&self, _a: &Actor, _id: RepoId, _p: &str, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
}

pub struct FakeAccounts {
    pub pool: PgPool,
}

#[async_trait]
impl Accounts for FakeAccounts {
    async fn get_user(&self, id: UserId) -> Result<User> {
        let r: Option<(UserId, String, String, String, bool, chrono::DateTime<Utc>)> =
            sqlx::query_as("SELECT id, kind, username::text, display_name, is_admin, created_at FROM users WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        user_of(r.ok_or(Error::NotFound)?)
    }
    async fn get_by_username(&self, _actor: &Actor, username: &str) -> Result<User> {
        let r: Option<(UserId, String, String, String, bool, chrono::DateTime<Utc>)> =
            sqlx::query_as("SELECT id, kind, username::text, display_name, is_admin, created_at FROM users WHERE username = $1")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?;
        user_of(r.ok_or(Error::NotFound)?)
    }
    async fn count_users(&self) -> Result<i64> {
        unavailable()
    }
    async fn create_user(&self, _a: &Actor, _n: NewUser, _c: &ClientInfo) -> Result<User> {
        unavailable()
    }
    async fn issue_link(&self, _a: &Actor, _u: UserId, _p: LinkPurpose, _t: u32, _c: &ClientInfo) -> Result<OneTimeLink> {
        unavailable()
    }
    async fn set_password(&self, _a: &Actor, _u: UserId, _p: &str, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn sign_out_everywhere(&self, _a: &Actor, _u: UserId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn unlock(&self, _a: &Actor, _u: UserId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn suspend(&self, _a: &Actor, _u: UserId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn set_can_run_ci(&self, _a: &Actor, _u: UserId, _v: bool, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn list_ssh_keys(&self, _a: &Actor, _u: UserId) -> Result<Vec<SshKey>> {
        unavailable()
    }
    async fn add_ssh_key(&self, _a: &Actor, _u: UserId, _t: &str, _k: &str, _c: &ClientInfo) -> Result<SshKey> {
        unavailable()
    }
    async fn delete_ssh_key(&self, _a: &Actor, _i: ch_core::SshKeyId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
    async fn list_tokens(&self, _a: &Actor, _u: UserId) -> Result<Vec<AccessToken>> {
        unavailable()
    }
    async fn create_token(
        &self,
        _a: &Actor,
        _u: UserId,
        _n: &str,
        _s: Vec<String>,
        _e: u32,
        _c: &ClientInfo,
    ) -> Result<(AccessToken, String)> {
        unavailable()
    }
    async fn delete_token(&self, _a: &Actor, _i: ch_core::TokenId, _c: &ClientInfo) -> Result<()> {
        unavailable()
    }
}

fn user_of(r: (UserId, String, String, String, bool, chrono::DateTime<Utc>)) -> Result<User> {
    Ok(User {
        id: r.0,
        kind: UserKind::parse(&r.1).unwrap_or(UserKind::User),
        username: r.2,
        display_name: r.3,
        email: None,
        is_admin: r.4,
        can_run_ci: false,
        suspended: false,
        has_2fa: false,
        has_password: true,
        invited_by: None,
        created_at: r.5,
    })
}

// ---------------------------------------------------------------- checks

#[derive(Default)]
pub struct FakeChecks {
    pub passing: Mutex<Vec<String>>,
    pub broken: Mutex<bool>,
}

impl FakeChecks {
    pub fn pass(&self, ctx: &str) {
        self.passing.lock().unwrap().push(ctx.to_string());
    }
}

#[async_trait]
impl Checks for FakeChecks {
    async fn check_runs_for_sha(&self, _r: RepoId, _s: &str) -> Result<Vec<CheckRun>> {
        Ok(vec![])
    }
    async fn statuses_for_sha(&self, _r: RepoId, _s: &str) -> Result<Vec<CommitStatus>> {
        Ok(vec![])
    }
    async fn combined(&self, _r: RepoId, _s: &str) -> Result<CombinedState> {
        Ok(CombinedState::Success)
    }
    async fn create_status(
        &self,
        _a: &Actor,
        _r: RepoId,
        _s: &str,
        _st: &str,
        _c: &str,
        _d: Option<&str>,
        _t: Option<&str>,
    ) -> Result<CommitStatus> {
        unavailable()
    }
    async fn required_satisfied(&self, _r: RepoId, _s: &str, contexts: &[String]) -> Result<bool> {
        if *self.broken.lock().unwrap() {
            return Err(Error::unavailable("ci is down"));
        }
        let p = self.passing.lock().unwrap();
        Ok(contexts.iter().all(|c| p.contains(c)))
    }
}

// ---------------------------------------------------------------- git

#[derive(Default)]
pub struct GitState {
    /// `(repo, full ref name)` to sha.
    pub refs: HashMap<(i64, String), String>,
    /// `(repo, base, head)` to a compare result. Without an entry a default one is made.
    pub compares: HashMap<(i64, String, String), Compare>,
    pub mergeable: Option<MergeableState>,
    pub merge_outcome: Option<MergeOutcome>,
    pub merges: Vec<MergeRequest>,
    pub created_refs: Vec<(i64, String, String)>,
    pub deleted_refs: Vec<(i64, String)>,
    /// `(repo, path)` to file content, for blob and tree.
    pub files: HashMap<(i64, String), Vec<u8>>,
    pub file_changes: Vec<CommitFileChange>,
    pub commit_dates: HashMap<String, chrono::DateTime<Utc>>,
    pub grep_hits: Vec<SearchHit>,
    pub counter: u64,
}

#[derive(Default)]
pub struct FakeGit {
    pub st: Mutex<GitState>,
}

pub fn sha_of(n: u64) -> String {
    format!("{n:040x}")
}

pub fn sig() -> Signature {
    Signature { name: "Alice Example".into(), email: "alice@test.example".into(), at: Utc.timestamp_opt(1_700_000_000, 0).unwrap() }
}

pub fn commit_info(sha: &str, message: &str) -> CommitInfo {
    CommitInfo { sha: sha.into(), parents: vec![], tree: sha_of(0xdead), author: sig(), committer: sig(), message: message.into() }
}

pub fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
    DiffLine { kind, old_no: old, new_no: new, text: text.into() }
}

/// A one file diff with these lines in a single hunk.
pub fn file_diff(path: &str, lines: Vec<DiffLine>) -> FileDiff {
    let additions = lines.iter().filter(|l| l.kind == LineKind::Added).count() as u32;
    let deletions = lines.iter().filter(|l| l.kind == LineKind::Removed).count() as u32;
    FileDiff {
        status: FileStatus::Modified,
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        binary: false,
        collapsed: false,
        additions,
        deletions,
        hunks: vec![Hunk { header: "@@ -1,3 +1,4 @@".into(), lines }],
    }
}

/// The default diff: line 2 of `src/lib.rs` changed, so lines 1 to 4 are in the hunk.
pub fn default_diff() -> Diff {
    Diff {
        files: vec![file_diff(
            "src/lib.rs",
            vec![
                line(LineKind::Context, Some(1), Some(1), "fn a() {}"),
                line(LineKind::Removed, Some(2), None, "fn b() {}"),
                line(LineKind::Added, None, Some(2), "fn b2() {}"),
                line(LineKind::Added, None, Some(3), "fn c() {}"),
                line(LineKind::Context, Some(3), Some(4), "fn d() {}"),
            ],
        )],
    }
}

impl FakeGit {
    pub fn set_ref(&self, repo: RepoId, name: &str, sha: &str) {
        self.st.lock().unwrap().refs.insert((repo.0, name.to_string()), sha.to_string());
    }
    pub fn branch(&self, repo: RepoId, name: &str, sha: &str) {
        self.set_ref(repo, &format!("refs/heads/{name}"), sha);
    }
    pub fn sha_of_branch(&self, repo: RepoId, name: &str) -> String {
        self.st.lock().unwrap().refs.get(&(repo.0, format!("refs/heads/{name}"))).cloned().unwrap_or_default()
    }
    pub fn set_compare(&self, repo: RepoId, base: &str, head: &str, c: Compare) {
        self.st.lock().unwrap().compares.insert((repo.0, base.into(), head.into()), c);
    }
    pub fn set_mergeable(&self, m: MergeableState) {
        self.st.lock().unwrap().mergeable = Some(m);
    }
    pub fn set_merge_outcome(&self, o: MergeOutcome) {
        self.st.lock().unwrap().merge_outcome = Some(o);
    }
    pub fn put_file(&self, repo: RepoId, path: &str, content: &str) {
        self.st.lock().unwrap().files.insert((repo.0, path.to_string()), content.as_bytes().to_vec());
    }
    pub fn merges(&self) -> Vec<MergeRequest> {
        self.st.lock().unwrap().merges.clone()
    }
    pub fn created_refs(&self) -> Vec<(i64, String, String)> {
        self.st.lock().unwrap().created_refs.clone()
    }
    pub fn deleted_refs(&self) -> Vec<(i64, String)> {
        self.st.lock().unwrap().deleted_refs.clone()
    }
    pub fn file_changes(&self) -> Vec<CommitFileChange> {
        self.st.lock().unwrap().file_changes.clone()
    }
}

#[async_trait]
impl GitService for FakeGit {
    async fn refs(&self, repo: RepoId, prefix: Option<&str>) -> Result<Vec<GitRef>> {
        let st = self.st.lock().unwrap();
        Ok(st
            .refs
            .iter()
            .filter(|((r, n), _)| *r == repo.0 && prefix.is_none_or(|p| n.starts_with(p)))
            .map(|((_, n), s)| GitRef { name: n.clone(), sha: s.clone() })
            .collect())
    }
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        let st = self.st.lock().unwrap();
        for cand in [rev.to_string(), format!("refs/heads/{rev}"), format!("refs/tags/{rev}")] {
            if let Some(s) = st.refs.get(&(repo.0, cand)) {
                return Ok(s.clone());
            }
        }
        if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(rev.to_string());
        }
        Err(Error::NotFound)
    }
    async fn tree(&self, repo: RepoId, _rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
        let st = self.st.lock().unwrap();
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let out: Vec<TreeEntry> = st
            .files
            .iter()
            .filter(|((r, p), _)| *r == repo.0 && p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
            .map(|((_, p), c)| TreeEntry {
                name: p[prefix.len()..].to_string(),
                path: p.clone(),
                kind: EntryKind::Blob,
                mode: "100644".into(),
                sha: sha_of(1),
                size: Some(c.len() as u64),
            })
            .collect();
        if out.is_empty() { Err(Error::NotFound) } else { Ok(out) }
    }
    async fn blob(&self, repo: RepoId, _rev: &str, path: &str, _max: u64) -> Result<Blob> {
        let st = self.st.lock().unwrap();
        let c = st.files.get(&(repo.0, path.to_string())).ok_or(Error::NotFound)?;
        Ok(Blob { sha: sha_of(2), size: c.len() as u64, is_binary: false, truncated: false, content: c.clone() })
    }
    async fn log(&self, _r: RepoId, _rev: &str, _p: Option<&str>, _s: u32, _l: u32) -> Result<Vec<CommitInfo>> {
        unavailable()
    }
    async fn commit(&self, _repo: RepoId, rev: &str) -> Result<CommitInfo> {
        let st = self.st.lock().unwrap();
        let mut c = commit_info(rev, "a commit");
        if let Some(d) = st.commit_dates.get(rev) {
            c.committer.at = *d;
        } else if !st.commit_dates.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(c)
    }
    /// The fakes key a fork's compares by the head repository.
    async fn compare_across(&self, _repo: RepoId, base: &str, head_repo: RepoId, head: &str) -> Result<Compare> {
        self.compare(head_repo, base, head).await
    }
    async fn compare(&self, repo: RepoId, base: &str, head: &str) -> Result<Compare> {
        let st = self.st.lock().unwrap();
        if let Some(c) = st.compares.get(&(repo.0, base.to_string(), head.to_string())) {
            return Ok(c.clone());
        }
        let same = base == head;
        Ok(Compare {
            base_sha: base.into(),
            head_sha: head.into(),
            merge_base: base.into(),
            ahead_by: if same { 0 } else { 1 },
            behind_by: 0,
            commits: if same { vec![] } else { vec![commit_info(head, "Change something\n\nFixes #1000000")] },
            diff: if same { Diff { files: vec![] } } else { default_diff() },
        })
    }
    async fn diff(&self, _r: RepoId, _b: &str, _h: &str) -> Result<Diff> {
        unavailable()
    }
    async fn grep(&self, _r: RepoId, _rev: &str, _p: &str, _l: u32) -> Result<Vec<SearchHit>> {
        Ok(self.st.lock().unwrap().grep_hits.clone())
    }
    async fn archive(&self, _r: RepoId, _rev: &str, _f: ArchiveFormat) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        unavailable()
    }
    async fn commit_file_change(&self, _a: &Actor, req: CommitFileChange, _c: &ClientInfo) -> Result<String> {
        let mut st = self.st.lock().unwrap();
        st.counter += 1;
        let sha = sha_of(0xf000 + st.counter);
        st.refs.insert((req.repo_id.0, req.branch.clone()), sha.clone());
        st.file_changes.push(req);
        Ok(sha)
    }
    async fn create_ref(&self, _a: &Actor, repo: RepoId, name: &str, sha: &str, _c: &ClientInfo) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        st.refs.insert((repo.0, name.to_string()), sha.to_string());
        st.created_refs.push((repo.0, name.to_string(), sha.to_string()));
        Ok(())
    }
    async fn delete_ref(&self, _a: &Actor, repo: RepoId, name: &str, _c: &ClientInfo) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        st.refs.remove(&(repo.0, name.to_string()));
        st.deleted_refs.push((repo.0, name.to_string()));
        Ok(())
    }
    async fn test_merge(&self, _r: RepoId, _b: &str, _hr: RepoId, _h: &str) -> Result<MergeableState> {
        Ok(self.st.lock().unwrap().mergeable.unwrap_or(MergeableState::Clean))
    }
    async fn merge(&self, _a: &Actor, req: MergeRequest, _c: &ClientInfo) -> Result<MergeOutcome> {
        let mut st = self.st.lock().unwrap();
        st.merges.push(req.clone());
        let out = match st.merge_outcome.clone() {
            Some(o) => o,
            None => {
                st.counter += 1;
                MergeOutcome::Merged { sha: sha_of(0xa000 + st.counter) }
            }
        };
        if let MergeOutcome::Merged { sha } = &out {
            st.refs.insert((req.repo_id.0, req.base_ref.clone()), sha.clone());
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- the world

pub struct World {
    pub env: TestEnv,
    pub core: Core,
    pub git: Arc<FakeGit>,
    pub checks: Arc<FakeChecks>,
    pub alice: UserId,
    pub bob: UserId,
    pub carol: UserId,
    pub dave: UserId,
    pub eve: UserId,
    /// Private repository `alice/demo`. Bob has write, carol read, dave triage. Eve has nothing.
    pub repo: RepoId,
    pub pool: PgPool,
}

impl World {
    pub async fn new(pool: PgPool) -> Self {
        let env = TestEnv::new(pool.clone()).await;
        let alice = env.user("alice").await;
        let bob = env.user("bob").await;
        let carol = env.user("carol").await;
        let dave = env.user("dave").await;
        let eve = env.user("eve").await;
        let repo = env.repo(alice, "demo").await;
        env.collaborator(repo, bob, "write").await;
        env.collaborator(repo, carol, "read").await;
        env.collaborator(repo, dave, "triage").await;

        let git = Arc::new(FakeGit::default());
        git.branch(repo, "main", &sha_of(1));
        git.branch(repo, "feature", &sha_of(2));
        git.branch(repo, "other", &sha_of(3));
        let checks = Arc::new(FakeChecks::default());
        let core = Core::new(Self::deps(&env, &pool, git.clone(), checks.clone()).await);
        World { env, core, git, checks, alice, bob, carol, dave, eve, repo, pool }
    }

    pub async fn builder(env: &TestEnv, pool: &PgPool, git: Arc<FakeGit>) -> ch_core::AppStateBuilder {
        let mut b = env.builder().await;
        let authz = Arc::new(TestAuthz { pool: pool.clone() });
        b.repos = Arc::new(FakeRepos { pool: pool.clone(), authz: authz.clone() });
        b.accounts = Arc::new(FakeAccounts { pool: pool.clone() });
        b.authz = authz;
        b.git = git;
        b
    }

    pub async fn deps(env: &TestEnv, pool: &PgPool, git: Arc<FakeGit>, checks: Arc<FakeChecks>) -> Deps {
        let b = Self::builder(env, pool, git).await;
        Deps::from_builder(&b).with_checks(checks)
    }

    /// The services and routes the application would build, over the same fakes.
    pub async fn app_state(&self) -> ch_core::AppState {
        let mut b = Self::builder(&self.env, &self.pool, self.git.clone()).await;
        b.checks = self.checks.clone();
        use ch_core::state::Install;
        ch_collab::build(&b).unwrap().install(&mut b);
        b.build()
    }

    pub fn a(&self, id: UserId) -> Actor {
        user_actor(id)
    }
    pub fn alice_a(&self) -> Actor {
        self.a(self.alice)
    }
    pub fn bob_a(&self) -> Actor {
        self.a(self.bob)
    }
    pub fn carol_a(&self) -> Actor {
        self.a(self.carol)
    }
    pub fn dave_a(&self) -> Actor {
        self.a(self.dave)
    }
    pub fn eve_a(&self) -> Actor {
        self.a(self.eve)
    }
    pub fn c(&self) -> ClientInfo {
        client()
    }

    pub async fn label(&self, name: &str) {
        self.core
            .create_label(
                &self.alice_a(),
                self.repo,
                LabelDraft { name: name.into(), color: Some("ff0000".into()), description: None },
                &self.c(),
            )
            .await
            .unwrap();
    }

    pub async fn issue(&self, by: UserId, title: &str, body: &str) -> IssueView {
        self.core
            .issue_create(
                &self.a(by),
                NewIssue {
                    repo_id: self.repo,
                    title: title.into(),
                    body: body.into(),
                    labels: vec![],
                    assignees: vec![],
                    milestone_id: None,
                },
                &self.c(),
            )
            .await
            .unwrap()
    }

    /// Opens a pull request from `feature` into `main` as `by`.
    pub async fn pull(&self, by: UserId, title: &str, body: &str) -> PullView {
        self.pull_from(by, "feature", title, body).await
    }

    pub async fn pull_from(&self, by: UserId, head: &str, title: &str, body: &str) -> PullView {
        self.core
            .pull_create(
                &self.a(by),
                NewPull {
                    repo_id: self.repo,
                    title: title.into(),
                    body: body.into(),
                    head_repo_id: self.repo,
                    head_ref: head.into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &self.c(),
            )
            .await
            .unwrap()
    }

    pub async fn protect(&self, pattern: &str, approvals: i32, checks: &[&str]) {
        sqlx::query("INSERT INTO branch_protections (repo_id, pattern, required_approvals, required_checks) VALUES ($1, $2, $3, $4)")
            .bind(self.repo)
            .bind(pattern)
            .bind(approvals)
            .bind(checks.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .execute(&self.pool)
            .await
            .unwrap();
    }

    pub async fn events(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT kind FROM outbox_events ORDER BY id").fetch_all(&self.pool).await.unwrap()
    }

    pub async fn count_events(&self, kind: &str) -> usize {
        self.events().await.iter().filter(|k| *k == kind).count()
    }

    pub async fn audit_actions(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT action FROM audit_events ORDER BY id").fetch_all(&self.pool).await.unwrap()
    }

    pub async fn team(&self, org: &str, name: &str) -> TeamId {
        let org_id = self.env.org(org).await;
        let id: i64 = sqlx::query_scalar("INSERT INTO teams (org_id, name) VALUES ($1, $2) RETURNING id")
            .bind(org_id)
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        TeamId(id)
    }
}

pub fn session(id: UserId, admin: bool) -> Actor {
    Actor::User { id, is_admin: admin, has_2fa: admin, via: AuthVia::Session }
}
