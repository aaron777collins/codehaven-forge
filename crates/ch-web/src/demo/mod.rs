//! Demo mode: the whole `AppState` served from fixed data, for visual review, screenshots and
//! the page tests. Nothing here is compiled into a normal build (the `demo` feature is off).
//!
//! `demo_state()` gives the state, `router()` gives the pages behind a stand-in for the real
//! gate (it resolves the session cookie and applies the same signed out rules), so a test or a
//! screenshot sees what a person sees.

pub mod world;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use ch_core::actor::{Actor, AuthVia, Permission};
use ch_core::audit::{AuditEvent, AuditFilter, Outcome, StoredAudit, action};
use ch_core::collab::{AssetInfo, ReleaseView, ThreadView, UserRef};
use ch_core::events::{DomainEvent, StoredEvent};
use ch_core::model::*;
use ch_core::services::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::web_contract::*;
use ch_core::*;
use chrono::Utc;
use futures::stream::{self, BoxStream};
use std::net::SocketAddr;
use std::sync::Arc;
use world::*;

pub const DEMO_PASSWORD: &str = "correct horse battery staple";
pub const DEMO_INVITE: &str = "chi_demoInviteToken0123456789abcdef";
pub const DEMO_LINK: &str = "chl_demoPasswordLink0123456789abcdef";
pub const ADA_COOKIE: &str = "demo-session-ada";
pub const BOB_COOKIE: &str = "demo-session-bob";
/// An administrator who has not turned two-factor on yet.
pub const ADMIN_NO_2FA_COOKIE: &str = "demo-session-admin-no2fa";
pub const ORIGIN: &str = "http://127.0.0.1:3200";

#[derive(Clone, Default)]
pub struct Demo;

fn find_repo(id: RepoId) -> Result<Repo> {
    repos().into_iter().find(|r| r.id == id).ok_or(Error::NotFound)
}

fn visible(actor: &Actor, r: &Repo) -> bool {
    match actor {
        Actor::User { id, .. } => r.visibility != Visibility::Private || *id == r.owner_id || *id == ADA,
        _ => false,
    }
}

/// A demo pull request with the parts a `PullView` adds, as far as the pages use them.
fn demo_pull_view(pr: PullRequest) -> ch_core::collab::PullView {
    let who = |id: UserId| UserRef {
        id,
        username: users().into_iter().find(|u| u.id == id).map(|u| u.username).unwrap_or_else(|| "ghost".into()),
        display_name: String::new(),
        kind: UserKind::User,
    };
    let repo = ch_core::collab::RepoRef { id: LIGHTHOUSE, owner: "ada".into(), name: "lighthouse".into() };
    ch_core::collab::PullView {
        issue: ch_core::collab::IssueView {
            issue: pr.issue.clone(),
            author: who(pr.issue.author_id),
            assignees: Vec::new(),
            labels: Vec::new(),
            milestone: None,
            closed_by: None,
            reactions: Default::default(),
            lock_reason: None,
            pinned: false,
            merged_at: pr.merged_at,
            repo_full_name: "ada/lighthouse".into(),
        },
        head_repo: Some(repo.clone()),
        base_repo: repo,
        merged_by: pr.merged_by.map(who),
        merge_method: None,
        stats: Default::default(),
        requested: Default::default(),
        review_comments: 0,
        auto_merge: None,
        maintainer_can_modify: false,
        pull: pr,
    }
}

fn uid(actor: &Actor) -> Option<UserId> {
    actor.user_id()
}

fn is_lh(repo: RepoId) -> bool {
    repo == LIGHTHOUSE
}

#[async_trait]
impl Authn for Demo {
    async fn from_session(&self, cookie: &str, _c: &ClientInfo) -> Result<Actor> {
        Ok(match cookie {
            ADA_COOKIE => Actor::User { id: ADA, is_admin: true, has_2fa: true, via: AuthVia::Session },
            BOB_COOKIE => Actor::User { id: BOB, is_admin: false, has_2fa: false, via: AuthVia::Session },
            ADMIN_NO_2FA_COOKIE => Actor::User { id: ADA, is_admin: true, has_2fa: false, via: AuthVia::Session },
            _ => Actor::Anonymous,
        })
    }
    async fn from_basic(&self, _u: &str, _s: &str, _c: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_bearer(&self, _t: &str, _c: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_ssh_key(&self, _f: &str) -> Result<Option<SshIdentity>> {
        Ok(None)
    }
    async fn recheck(&self, credential: &ch_core::live::Credential) -> Result<Option<Actor>> {
        let ch_core::live::Credential::Session { hash } = credential else { return Ok(None) };
        for c in [ADA_COOKIE, BOB_COOKIE, ADMIN_NO_2FA_COOKIE] {
            if ch_db::tokens::hash_token(c) == *hash {
                return self.from_session(c, &ClientInfo::local()).await.map(Some);
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl Invites for Demo {
    async fn create(&self, _a: &Actor, req: NewInvite, _c: &ClientInfo) -> Result<CreatedInvite> {
        let invite = Invite {
            id: InviteId(9),
            created_by: Some(ADA),
            email: req.email,
            grants_admin: req.grants_admin,
            grants_ci: req.grants_ci,
            org_id: None,
            team_id: None,
            note: req.note,
            expires_at: Utc::now() + chrono::Duration::hours(req.expires_in_hours.unwrap_or(168) as i64),
            used_at: None,
            used_by: None,
            revoked_at: None,
            created_at: Utc::now(),
        };
        Ok(CreatedInvite { invite, token: DEMO_INVITE.into(), url: format!("{ORIGIN}/invite/{DEMO_INVITE}") })
    }
    async fn list(&self, _a: &Actor) -> Result<Vec<Invite>> {
        let base = |id: i64, email: Option<&str>, note: &str, used: bool, rev: bool, exp_days: i64| Invite {
            id: InviteId(id),
            created_by: Some(ADA),
            email: email.map(str::to_string),
            grants_admin: false,
            grants_ci: true,
            org_id: None,
            team_id: None,
            note: Some(note.into()),
            expires_at: Utc::now() + chrono::Duration::days(exp_days),
            used_at: if used { Some(ago(20, 0)) } else { None },
            used_by: if used { Some(CLEO) } else { None },
            revoked_at: if rev { Some(ago(3, 0)) } else { None },
            created_at: ago(25, 0),
        };
        Ok(vec![
            base(3, Some("dana@example.test"), "For Dana", false, false, 5),
            base(2, None, "Team link", true, false, 1),
            base(1, Some("old@example.test"), "Expired", false, false, -4),
        ])
    }
    async fn revoke(&self, _a: &Actor, _id: InviteId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn peek(&self, token: &str) -> Result<InvitePeek> {
        if token == DEMO_INVITE { Ok(InvitePeek { invited_by: Some("Ada Marlow".into()), email: None }) } else { Err(Error::NotFound) }
    }
    async fn accept(&self, token: &str, account: NewAccount, _c: &ClientInfo) -> Result<User> {
        if token != DEMO_INVITE {
            return Err(Error::NotFound);
        }
        let mut errs = Vec::new();
        if account.username == "ada" {
            errs.push(FieldError::new("user", "username", "already_exists"));
        }
        if account.password.len() < 12 {
            errs.push(FieldError::custom("user", "password", "Use at least 12 characters."));
        }
        if !errs.is_empty() {
            return Err(Error::Validation(errs));
        }
        Ok(user(UserId(50), &account.username, &account.username, false, UserKind::User))
    }
}

#[async_trait]
impl Authz for Demo {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        let r = find_repo(repo)?;
        let Some(me) = uid(actor) else { return Ok(Permission::None) };
        Ok(if me == ADA || me == r.owner_id {
            Permission::Admin
        } else if me == BOB && is_lh(repo) {
            Permission::Write
        } else if r.visibility != Visibility::Private {
            Permission::Read
        } else {
            Permission::None
        })
    }
    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        Ok(match (uid(actor), org.0) {
            (Some(ADA), 10) => Some(OrgRole::Owner),
            (Some(BOB), 10) => Some(OrgRole::Member),
            _ => None,
        })
    }
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let p = self.repo_permission(actor, repo).await?;
        if p == Permission::None {
            Err(Error::NotFound)
        } else if p.allows(needed) {
            Ok(())
        } else {
            Err(Error::forbidden("not allowed"))
        }
    }
}

#[async_trait]
impl Accounts for Demo {
    async fn get_user(&self, id: UserId) -> Result<User> {
        users().into_iter().find(|u| u.id == id).ok_or(Error::NotFound)
    }
    async fn get_by_username(&self, _a: &Actor, username: &str) -> Result<User> {
        users().into_iter().find(|u| u.username.eq_ignore_ascii_case(username)).ok_or(Error::NotFound)
    }
    async fn count_users(&self) -> Result<i64> {
        Ok(4)
    }
    async fn create_user(&self, _a: &Actor, new: NewUser, _c: &ClientInfo) -> Result<User> {
        Ok(user(UserId(60), &new.username, &new.username, new.is_admin, UserKind::User))
    }
    async fn issue_link(&self, _a: &Actor, _u: UserId, _p: LinkPurpose, ttl: u32, _c: &ClientInfo) -> Result<OneTimeLink> {
        Ok(OneTimeLink {
            token: DEMO_LINK.into(),
            path: format!("/password/{DEMO_LINK}"),
            expires_at: Utc::now() + chrono::Duration::minutes(ttl as i64),
        })
    }
    async fn set_password(&self, _a: &Actor, _u: UserId, _p: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn sign_out_everywhere(&self, _a: &Actor, _u: UserId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn unlock(&self, _a: &Actor, _u: UserId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn suspend(&self, _a: &Actor, _u: UserId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn set_can_run_ci(&self, _a: &Actor, _u: UserId, _v: bool, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn list_ssh_keys(&self, _a: &Actor, user: UserId) -> Result<Vec<SshKey>> {
        Ok(vec![SshKey {
            id: SshKeyId(1),
            user_id: user,
            title: "Laptop".into(),
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:uN3k0xVq7mE2yH1c9Zr8LwB5sT4aJd6PfG0oIeKXhYc".into(),
            created_at: ago(40, 0),
            last_used_at: Some(ago(0, 5)),
        }])
    }
    async fn add_ssh_key(&self, _a: &Actor, user: UserId, title: &str, _k: &str, _c: &ClientInfo) -> Result<SshKey> {
        Ok(SshKey {
            id: SshKeyId(2),
            user_id: user,
            title: title.into(),
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:demo".into(),
            created_at: Utc::now(),
            last_used_at: None,
        })
    }
    async fn delete_ssh_key(&self, _a: &Actor, _id: SshKeyId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn list_tokens(&self, _a: &Actor, user: UserId) -> Result<Vec<AccessToken>> {
        Ok(vec![AccessToken {
            id: TokenId(1),
            user_id: user,
            name: "laptop git".into(),
            prefix: "chp_a1B2".into(),
            scopes: vec!["repo".into(), "workflow".into()],
            expires_at: Utc::now() + chrono::Duration::days(60),
            last_used_at: Some(ago(1, 0)),
            created_at: ago(30, 0),
        }])
    }
    async fn create_token(
        &self,
        _a: &Actor,
        user: UserId,
        name: &str,
        scopes: Vec<String>,
        days: u32,
        _c: &ClientInfo,
    ) -> Result<(AccessToken, String)> {
        let t = AccessToken {
            id: TokenId(2),
            user_id: user,
            name: name.into(),
            prefix: "chp_zZ9y".into(),
            scopes,
            expires_at: Utc::now() + chrono::Duration::days(days as i64),
            last_used_at: None,
            created_at: Utc::now(),
        };
        Ok((t, "chp_zZ9yDemoTokenSecretValue01234".into()))
    }
    async fn delete_token(&self, _a: &Actor, _id: TokenId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Repos for Demo {
    async fn create(&self, _a: &Actor, new: NewRepo, _c: &ClientInfo) -> Result<Repo> {
        if new.name == "lighthouse" {
            return Err(Error::Validation(vec![FieldError::new("repo", "name", "already_exists")]));
        }
        let mut r = repos().remove(0);
        r.name = new.name;
        r.description = new.description;
        Ok(r)
    }
    async fn get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo> {
        repos()
            .into_iter()
            .find(|r| r.owner_name.eq_ignore_ascii_case(owner) && r.name.eq_ignore_ascii_case(name) && visible(actor, r))
            .ok_or(Error::NotFound)
    }
    async fn get(&self, actor: &Actor, id: RepoId) -> Result<Repo> {
        find_repo(id).and_then(|r| if visible(actor, &r) { Ok(r) } else { Err(Error::NotFound) })
    }
    async fn list_for_owner(&self, actor: &Actor, owner: UserId) -> Result<Vec<Repo>> {
        Ok(repos().into_iter().filter(|r| r.owner_id == owner && visible(actor, r)).collect())
    }
    async fn update(&self, _a: &Actor, id: RepoId, u: RepoUpdate, _c: &ClientInfo) -> Result<Repo> {
        let mut r = find_repo(id)?;
        if let Some(d) = u.description {
            r.description = d;
        }
        Ok(r)
    }
    async fn rename(&self, _a: &Actor, id: RepoId, new_name: &str, _c: &ClientInfo) -> Result<Repo> {
        let mut r = find_repo(id)?;
        r.name = new_name.into();
        Ok(r)
    }
    async fn transfer(&self, _a: &Actor, id: RepoId, _o: UserId, _c: &ClientInfo) -> Result<Repo> {
        find_repo(id)
    }
    async fn fork(&self, _a: &Actor, id: RepoId, _i: UserId, _c: &ClientInfo) -> Result<Repo> {
        find_repo(id)
    }
    async fn fork_as(&self, _a: &Actor, id: RepoId, into: UserId, v: Visibility, _c: &ClientInfo) -> Result<Repo> {
        let mut r = find_repo(id)?;
        if v > r.visibility {
            return Err(Error::invalid("Repository", "visibility", "A fork cannot be more visible than the repository it came from."));
        }
        r.owner_id = into;
        r.owner_name = users().into_iter().find(|u| u.id == into).map(|u| u.username).unwrap_or_default();
        r.fork_parent_id = Some(id);
        r.visibility = v;
        Ok(r)
    }
    async fn forks(&self, a: &Actor, id: RepoId) -> Result<Vec<Repo>> {
        // Bob's internal fork of lighthouse. A private fork would not be listed to Ada here.
        if !is_lh(id) {
            return Ok(vec![]);
        }
        let mut f = find_repo(id)?;
        f.id = RepoId(90);
        f.owner_id = BOB;
        f.owner_name = "bob".into();
        f.fork_parent_id = Some(id);
        Ok(if visible(a, &f) { vec![f] } else { vec![] })
    }
    async fn social(&self, _a: &Actor, id: RepoId) -> Result<RepoSocial> {
        Ok(if is_lh(id) { RepoSocial { stars: 12, forks: 1, starred: false } } else { RepoSocial::default() })
    }
    async fn set_star(&self, _a: &Actor, _id: RepoId, _on: bool) -> Result<()> {
        Ok(())
    }
    async fn delete(&self, _a: &Actor, _id: RepoId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn public_repos_allowed(&self) -> Result<bool> {
        Ok(true)
    }
    async fn workspace_settings(&self) -> Result<WorkspaceSettings> {
        Ok(WorkspaceSettings { editor: true, terminal: false })
    }
    async fn set_workspace_settings(&self, _a: &Actor, _s: WorkspaceSettings, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn publish_summary(&self, _a: &Actor, id: RepoId) -> Result<PublishSummary> {
        find_repo(id)?;
        Ok(PublishSummary {
            branches: 3,
            tags: 1,
            pull_refs: 2,
            size_kb: 120,
            issues: 4,
            pulls: 2,
            comments: 9,
            comments_by_others: 5,
            releases: 1,
            release_files: 2,
            forks: 1,
            pulls_from_private_forks: 0,
            linked_packages: vec!["lighthouse".into()],
            snapshot: "demo-snapshot".into(),
        })
    }
    async fn collaborators(&self, _a: &Actor, _id: RepoId) -> Result<Vec<Collaborator>> {
        Ok(collaborators())
    }
    async fn set_collaborator(&self, _a: &Actor, _id: RepoId, _u: UserId, _p: Permission, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn remove_collaborator(&self, _a: &Actor, _id: RepoId, _u: UserId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn protections(&self, _a: &Actor, _id: RepoId) -> Result<Vec<BranchProtection>> {
        Ok(protections())
    }
    async fn set_protection(&self, _a: &Actor, _id: RepoId, _p: BranchProtection, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn remove_protection(&self, _a: &Actor, _id: RepoId, _p: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
}

fn entries_at(path: &str) -> Vec<TreeEntry> {
    let prefix = if path.is_empty() { String::new() } else { format!("{}/", path.trim_matches('/')) };
    let mut seen: Vec<TreeEntry> = Vec::new();
    for (p, content) in files() {
        let Some(rest) = p.strip_prefix(&prefix) else { continue };
        let (name, is_dir) = match rest.split_once('/') {
            Some((d, _)) => (d.to_string(), true),
            None => (rest.to_string(), false),
        };
        if seen.iter().any(|e| e.name == name) {
            continue;
        }
        seen.push(TreeEntry {
            path: format!("{prefix}{name}"),
            kind: if is_dir { EntryKind::Tree } else { EntryKind::Blob },
            mode: if is_dir { "040000".into() } else { "100644".into() },
            sha: sha(77),
            size: if is_dir { None } else { Some(content.len() as u64) },
            name,
        });
    }
    seen
}

/// The whole tree of the other demo repository: one README.
fn readme_entry() -> WorkspaceEntry {
    let content = b"# Notes\n\nNothing secret here.\n";
    WorkspaceEntry {
        path_b64: base64::engine::general_purpose::STANDARD.encode("README.md"),
        path: Some("README.md".into()),
        mode: "100644".into(),
        kind: "blob".into(),
        sha: sha(78),
        size: Some(content.len() as u64),
    }
}

fn resolve_rev(repo: RepoId, rev: &str) -> Result<String> {
    if !is_lh(repo) {
        return if rev == "main" || rev == "HEAD" { Ok(sha(6)) } else { Err(Error::NotFound) };
    }
    Ok(match rev {
        "main" | "HEAD" => sha(6),
        "feature/signal-colours" => sha(21),
        "feature/fog" | "fix/typo" => sha(5),
        // The merged pull request 7 kept its branch, so its page offers to delete it.
        "refs/heads/fix/long-messages" => sha(5),
        "v0.2.0" => sha(3),
        "v0.1.0" => sha(1),
        other => {
            let all: Vec<String> = history().into_iter().chain(feature_commits()).map(|c| c.sha).collect();
            all.into_iter().find(|s| other.len() >= 7 && s.starts_with(other)).ok_or(Error::NotFound)?
        }
    })
}

#[async_trait]
impl GitService for Demo {
    async fn refs(&self, repo: RepoId, prefix: Option<&str>) -> Result<Vec<GitRef>> {
        let mut v = vec![GitRef { name: "refs/heads/main".into(), sha: sha(6) }];
        if is_lh(repo) {
            v.push(GitRef { name: "refs/heads/feature/signal-colours".into(), sha: sha(21) });
            v.push(GitRef { name: "refs/heads/feature/fog".into(), sha: sha(5) });
            v.push(GitRef { name: "refs/tags/v0.2.0".into(), sha: sha(3) });
            v.push(GitRef { name: "refs/tags/v0.1.0".into(), sha: sha(1) });
        }
        Ok(v.into_iter().filter(|r| prefix.is_none_or(|p| r.name.starts_with(p))).collect())
    }
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        resolve_rev(repo, rev)
    }
    async fn tree(&self, repo: RepoId, rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
        resolve_rev(repo, rev)?;
        if !is_lh(repo) {
            return Ok(vec![TreeEntry {
                name: "README.md".into(),
                path: "README.md".into(),
                kind: EntryKind::Blob,
                mode: "100644".into(),
                sha: sha(77),
                size: Some(40),
            }]);
        }
        let e = entries_at(path);
        if e.is_empty() { Err(Error::NotFound) } else { Ok(e) }
    }
    async fn blob(&self, repo: RepoId, rev: &str, path: &str, max: u64) -> Result<Blob> {
        resolve_rev(repo, rev)?;
        if !is_lh(repo) {
            let c = b"# Notes\n\nNothing secret here.\n".to_vec();
            return Ok(Blob { sha: sha(78), size: c.len() as u64, is_binary: false, truncated: false, content: c });
        }
        let (_, content) = files().into_iter().find(|(p, _)| *p == path).ok_or(Error::NotFound)?;
        let is_binary = content.contains(&0);
        let truncated = content.len() as u64 > max;
        Ok(Blob {
            sha: sha(78),
            size: content.len() as u64,
            is_binary,
            truncated,
            content: if truncated { content[..max as usize].to_vec() } else { content },
        })
    }
    async fn log(&self, repo: RepoId, rev: &str, _p: Option<&str>, skip: u32, limit: u32) -> Result<Vec<CommitInfo>> {
        resolve_rev(repo, rev)?;
        let mut h = if rev == "feature/signal-colours" { feature_commits().into_iter().chain(history()).collect() } else { history() };
        h = h.into_iter().skip(skip as usize).take(limit as usize).collect();
        Ok(h)
    }
    async fn commit(&self, repo: RepoId, rev: &str) -> Result<CommitInfo> {
        let s = resolve_rev(repo, rev)?;
        history().into_iter().chain(feature_commits()).find(|c| c.sha == s).ok_or(Error::NotFound)
    }
    async fn compare(&self, repo: RepoId, base: &str, head: &str) -> Result<Compare> {
        let (b, h) = (resolve_rev(repo, base)?, resolve_rev(repo, head)?);
        if b == h {
            return Ok(Compare {
                base_sha: b.clone(),
                head_sha: h,
                merge_base: b,
                ahead_by: 0,
                behind_by: 0,
                commits: vec![],
                diff: Diff { files: vec![] },
            });
        }
        Ok(Compare {
            base_sha: b.clone(),
            head_sha: h,
            merge_base: sha(6),
            ahead_by: 2,
            behind_by: 0,
            commits: feature_commits(),
            diff: sample_diff(),
        })
    }
    async fn diff(&self, repo: RepoId, base: &str, head: &str) -> Result<Diff> {
        if !base.starts_with("4b825dc") {
            resolve_rev(repo, base)?;
        }
        resolve_rev(repo, head)?;
        Ok(sample_diff())
    }
    async fn grep(&self, _r: RepoId, _rev: &str, _p: &str, _l: u32) -> Result<Vec<SearchHit>> {
        Ok(vec![])
    }
    async fn blame(&self, repo: RepoId, rev: &str, path: &str) -> Result<Blame> {
        let b = self.blob(repo, rev, path, 1_000_000).await?;
        let lines = String::from_utf8_lossy(&b.content).lines().count() as u32;
        let first = lines.div_ceil(3);
        let mk = |n: u8, start: u32, count: u32, who: &str, d: i64, s: &str| BlameHunk {
            sha: sha(n),
            author: who.into(),
            at: ago(d, 0),
            summary: s.into(),
            start_line: start,
            line_count: count,
        };
        let hunks = vec![
            mk(3, 1, first, "Ada Marlow", 21, "Add the flash method and a CLI"),
            mk(6, 1 + first, first.min(lines.saturating_sub(first)), "Bob Okafor", 1, "Add colour channels to Beam"),
            mk(5, 1 + 2 * first, lines.saturating_sub(2 * first), "Ada Marlow", 4, "Print a warning for long messages"),
        ];
        Ok(Blame {
            ranges: hunks
                .into_iter()
                .map(|h| BlameRange {
                    start_line: h.start_line,
                    line_count: h.line_count,
                    sha: h.sha,
                    author: Signature { name: h.author, email: String::new(), at: h.at },
                    summary: h.summary,
                    orig_path: path.to_string(),
                    orig_start_line: h.start_line,
                })
                .collect(),
        })
    }
    async fn archive(&self, _r: RepoId, _rev: &str, _f: ArchiveFormat) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        Ok(Box::pin(stream::iter(vec![Ok(Bytes::from_static(b"PK demo archive"))])))
    }
    async fn commit_file_change(&self, _a: &Actor, req: CommitFileChange, _c: &ClientInfo) -> Result<String> {
        if req.branch == "main" && req.new_branch.is_none() && req.message.contains("protected") {
            return Err(Error::forbidden("main is protected. Make a new branch and open a pull request."));
        }
        Ok(sha(30))
    }
    async fn create_ref(&self, _a: &Actor, _r: RepoId, _n: &str, _s: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn delete_ref(&self, _a: &Actor, _r: RepoId, _n: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn test_merge(&self, _r: RepoId, _b: &str, _h: RepoId, _hr: &str) -> Result<MergeableState> {
        Ok(MergeableState::Clean)
    }
    async fn merge(&self, _a: &Actor, _r: MergeRequest, _c: &ClientInfo) -> Result<MergeOutcome> {
        Ok(MergeOutcome::Merged { sha: sha(40) })
    }
    async fn workspace_branch_head(&self, repo: RepoId, branch: &str) -> Result<Option<String>> {
        Ok(resolve_rev(repo, branch).ok())
    }
    async fn workspace_is_ancestor(&self, _r: RepoId, sha: &str, head: &str) -> Result<bool> {
        Ok(sha == head)
    }
    async fn workspace_tree(&self, repo: RepoId, _rev: &str, _limits: WorkspaceTreeLimits) -> Result<WorkspaceTree> {
        if !is_lh(repo) {
            return Ok(WorkspaceTree { entries: vec![readme_entry()], cut: false });
        }
        let mut entries = Vec::new();
        let mut folders: Vec<&str> = Vec::new();
        for (p, content) in files() {
            // Every distinct folder on the path, `.codehaven/workflows` as well as `src`.
            let mut dir = p;
            while let Some((parent, _)) = dir.rsplit_once('/') {
                if !folders.contains(&parent) {
                    folders.push(parent);
                }
                dir = parent;
            }
            entries.push(WorkspaceEntry {
                path_b64: base64::engine::general_purpose::STANDARD.encode(p),
                path: Some(p.to_string()),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: sha(78),
                size: Some(content.len() as u64),
            });
        }
        for f in folders {
            entries.push(WorkspaceEntry {
                path_b64: base64::engine::general_purpose::STANDARD.encode(f),
                path: Some(f.to_string()),
                mode: "040000".into(),
                kind: "tree".into(),
                sha: sha(77),
                size: None,
            });
        }
        Ok(WorkspaceTree { entries, cut: false })
    }
    async fn workspace_blob(&self, repo: RepoId, _rev: &str, path: &[u8], max_bytes: u64) -> Result<WorkspaceBlob> {
        let content = if is_lh(repo) {
            let p = std::str::from_utf8(path).map_err(|_| Error::NotFound)?;
            files().into_iter().find(|(f, _)| *f == p).map(|(_, c)| c).ok_or(Error::NotFound)?
        } else {
            b"# Notes\n\nNothing secret here.\n".to_vec()
        };
        let size = content.len() as u64;
        if size > max_bytes {
            return Ok(WorkspaceBlob { bytes: vec![], size, mode: "100644".into(), too_large: true });
        }
        Ok(WorkspaceBlob { bytes: content, size, mode: "100644".into(), too_large: false })
    }
    async fn workspace_search(&self, repo: RepoId, _rev: &str, text: &str, limits: WorkspaceSearchLimits) -> Result<WorkspaceSearch> {
        let needle = text.trim().to_lowercase();
        // The other repository has the one README the `blob` method serves.
        let contents: Vec<(&str, Vec<u8>)> =
            if is_lh(repo) { files() } else { vec![("README.md", b"# Notes\n\nNothing secret here.\n".to_vec())] };
        let mut hits = Vec::new();
        let mut cut = false;
        for (p, content) in &contents {
            for (i, line) in String::from_utf8_lossy(content).lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    hits.push(WorkspaceHit {
                        path_b64: base64::engine::general_purpose::STANDARD.encode(p),
                        path: Some(p.to_string()),
                        line: i as u64 + 1,
                        text: line.chars().take(300).collect(),
                    });
                    if hits.len() >= limits.max_hits {
                        cut = true;
                        break;
                    }
                }
            }
            if cut {
                break;
            }
        }
        Ok(WorkspaceSearch { hits, cut, timed_out: false })
    }
    async fn workspace_direct_commit_allowed(&self, _a: &Actor, _r: RepoId, _b: &str) -> Result<bool> {
        Ok(true)
    }
}

#[async_trait]
impl Issues for Demo {
    async fn create(&self, _a: &Actor, new: NewIssue, _c: &ClientInfo) -> Result<Issue> {
        Ok(issue(20, &new.title, &new.body, true, ADA, &[], &[], 0, 0))
    }
    async fn get(&self, _a: &Actor, repo: RepoId, n: i64) -> Result<Issue> {
        if !is_lh(repo) {
            return Err(Error::NotFound);
        }
        issues().into_iter().chain(pulls().into_iter().map(|p| p.issue)).find(|i| i.number == n).ok_or(Error::NotFound)
    }
    async fn list(&self, _a: &Actor, repo: RepoId, f: IssueFilter) -> Result<Vec<Issue>> {
        if !is_lh(repo) {
            return Ok(vec![]);
        }
        let mut v: Vec<Issue> = issues().into_iter().chain(pulls().into_iter().map(|p| p.issue)).collect();
        if let Some(o) = f.open {
            v.retain(|i| i.open == o);
        }
        for l in &f.labels {
            v.retain(|i| i.labels.contains(l));
        }
        if let Some(q) = &f.query {
            let q = q.to_lowercase();
            v.retain(|i| i.title.to_lowercase().contains(&q));
        }
        v.sort_by_key(|i| std::cmp::Reverse(i.number));
        Ok(v.into_iter().take(f.per_page.max(1) as usize).collect())
    }
    async fn set_state(&self, a: &Actor, repo: RepoId, n: i64, open: bool, _r: Option<&str>, _c: &ClientInfo) -> Result<Issue> {
        let mut i = Issues::get(self, a, repo, n).await?;
        i.open = open;
        Ok(i)
    }
    async fn edit(&self, a: &Actor, repo: RepoId, n: i64, t: Option<String>, b: Option<String>, _c: &ClientInfo) -> Result<Issue> {
        let mut i = Issues::get(self, a, repo, n).await?;
        if let Some(t) = t {
            i.title = t;
        }
        if let Some(b) = b {
            i.body = b;
        }
        Ok(i)
    }
    async fn set_labels(&self, a: &Actor, repo: RepoId, n: i64, labels: Vec<String>, _c: &ClientInfo) -> Result<Issue> {
        let mut i = Issues::get(self, a, repo, n).await?;
        i.labels = labels;
        Ok(i)
    }
    async fn comments(&self, _a: &Actor, _repo: RepoId, n: i64) -> Result<Vec<Comment>> {
        Ok(issue_comments(n))
    }
    async fn add_comment(&self, _a: &Actor, _repo: RepoId, n: i64, body: &str, _c: &ClientInfo) -> Result<Comment> {
        Ok(Comment {
            id: CommentId(99),
            issue_id: IssueId(100 + n),
            author_id: ADA,
            body: body.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
    }
}

#[async_trait]
impl Pulls for Demo {
    async fn create(&self, _a: &Actor, new: NewPull, _c: &ClientInfo) -> Result<PullRequest> {
        Ok(pull(15, &new.title, &new.body, "open", ADA, &new.head_ref, &new.base_ref, 0))
    }
    async fn get(&self, _a: &Actor, repo: RepoId, n: i64) -> Result<PullRequest> {
        if !is_lh(repo) {
            return Err(Error::NotFound);
        }
        pulls().into_iter().find(|p| p.issue.number == n).ok_or(Error::NotFound)
    }
    async fn list(&self, _a: &Actor, repo: RepoId, f: IssueFilter) -> Result<Vec<PullRequest>> {
        if !is_lh(repo) {
            return Ok(vec![]);
        }
        let mut v = pulls();
        if let Some(o) = f.open {
            v.retain(|p| p.issue.open == o);
        }
        Ok(v)
    }
    async fn merge_gate(&self, _a: &Actor, _repo: RepoId, n: i64) -> Result<GateReport> {
        Ok(if n == 13 {
            GateReport { mergeable: false, reasons: vec!["The branch is a draft.".into()] }
        } else if n == 14 {
            GateReport { mergeable: true, reasons: vec![] }
        } else {
            GateReport {
                mergeable: false,
                reasons: vec!["1 approving review is required.".into(), "The check test has not passed.".into()],
            }
        })
    }
    async fn merge(&self, a: &Actor, repo: RepoId, n: i64, _m: MergeMethod, _c: &ClientInfo) -> Result<PullRequest> {
        if n == 99 {
            return Err(Error::MergeBlocked(vec!["1 approving review is required.".into()]));
        }
        Pulls::get(self, a, repo, n).await
    }
    async fn view(&self, a: &Actor, repo: RepoId, n: i64) -> Result<ch_core::collab::PullView> {
        Ok(demo_pull_view(Pulls::get(self, a, repo, n).await?))
    }
    async fn merge_with(
        &self,
        a: &Actor,
        repo: RepoId,
        n: i64,
        o: ch_core::collab::MergeOptions,
        c: &ClientInfo,
    ) -> Result<ch_core::collab::PullView> {
        let pr = Pulls::get(self, a, repo, n).await?;
        if o.expected_head_sha.as_deref().is_some_and(|s| s != pr.head_sha) {
            return Err(Error::conflict("The head branch was modified. Review the changes and try again"));
        }
        let pr = Pulls::merge(self, a, repo, n, o.method.unwrap_or(MergeMethod::Merge), c).await?;
        Ok(demo_pull_view(pr))
    }
    async fn close(&self, a: &Actor, repo: RepoId, n: i64, _c: &ClientInfo) -> Result<PullRequest> {
        Pulls::get(self, a, repo, n).await
    }
}

#[async_trait]
impl Reviews for Demo {
    async fn list(&self, _a: &Actor, _repo: RepoId, n: i64) -> Result<Vec<Review>> {
        Ok(if n == 14 { reviews() } else { vec![] })
    }
    async fn submit(
        &self,
        _a: &Actor,
        _repo: RepoId,
        _n: i64,
        state: ReviewState,
        body: &str,
        _c: Vec<NewReviewComment>,
        _cl: &ClientInfo,
    ) -> Result<Review> {
        Ok(Review {
            id: ReviewId(9),
            pull_id: PullId(14),
            author_id: ADA,
            state,
            body: body.into(),
            commit_sha: sha(21),
            submitted_at: Some(Utc::now()),
        })
    }
    async fn requested(&self, _a: &Actor, repo: RepoId, n: i64) -> Result<ch_core::collab::RequestedReviewers> {
        let mut r = ch_core::collab::RequestedReviewers::default();
        if is_lh(repo) && n == 14 {
            r.users.push(UserRef { id: BOB, username: "bob".into(), display_name: "Bob".into(), kind: UserKind::User });
        }
        Ok(r)
    }
    async fn remove_request(&self, _a: &Actor, _repo: RepoId, _n: i64, _u: Vec<UserId>, _t: Vec<TeamId>, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn request_review(&self, _a: &Actor, _repo: RepoId, _n: i64, _u: Vec<UserId>, _t: Vec<TeamId>, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn threads(&self, _a: &Actor, _repo: RepoId, n: i64) -> Result<Vec<ThreadView>> {
        Ok(if n == 14 { threads() } else { vec![] })
    }
    async fn apply_suggestion(&self, _a: &Actor, _repo: RepoId, comment: CommentId, _c: &ClientInfo) -> Result<String> {
        if comment == CommentId(33) {
            Ok(sha(22))
        } else {
            Err(Error::invalid("PullRequestReviewComment", "body", "This comment has no suggestion to apply"))
        }
    }
    async fn resolve_thread(&self, _a: &Actor, _repo: RepoId, thread: i64, _r: bool, _c: &ClientInfo) -> Result<()> {
        // Like the real service: a thread that is not there is a NotFound, not a quiet ok.
        if threads().iter().any(|t| t.id == thread) { Ok(()) } else { Err(Error::NotFound) }
    }
}

#[async_trait]
impl Releases for Demo {
    async fn list(&self, _a: &Actor, repo: RepoId) -> Result<Vec<Release>> {
        Ok(if is_lh(repo) { releases() } else { vec![] })
    }
    async fn get_by_tag(&self, _a: &Actor, _repo: RepoId, tag: &str) -> Result<Release> {
        releases().into_iter().find(|r| r.tag_name == tag).ok_or(Error::NotFound)
    }
    async fn latest(&self, _a: &Actor, repo: RepoId) -> Result<Release> {
        if is_lh(repo) { Ok(releases().remove(0)) } else { Err(Error::NotFound) }
    }
    async fn views(&self, _a: &Actor, repo: RepoId) -> Result<Vec<ReleaseView>> {
        Ok(if is_lh(repo) { releases().into_iter().map(release_view).collect() } else { vec![] })
    }
    async fn view_by_tag(&self, _a: &Actor, _repo: RepoId, tag: &str) -> Result<ReleaseView> {
        releases().into_iter().find(|r| r.tag_name == tag).map(release_view).ok_or(Error::NotFound)
    }
    async fn open_asset(
        &self,
        _a: &Actor,
        repo: RepoId,
        tag: &str,
        name: &str,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        let found = if is_lh(repo) { releases().into_iter().find(|r| r.tag_name == tag).map(release_view) } else { None };
        let info = found.and_then(|v| v.assets.into_iter().find(|a| a.name == name)).ok_or(Error::NotFound)?;
        let body = Bytes::from_static(b"demo asset\n");
        Ok((info, Box::pin(stream::iter(vec![Ok(body)]))))
    }
}

/// A demo release with the one file the demo gives to 0.2.0.
fn release_view(release: Release) -> ReleaseView {
    let assets = if release.tag_name == "v0.2.0" {
        vec![AssetInfo {
            id: 1,
            release_id: release.id,
            name: "lighthouse-0.2.0-linux.tar.gz".into(),
            label: String::new(),
            content_type: "application/gzip".into(),
            size: 2_411_724,
            download_count: 7,
            uploader: None,
            created_at: release.created_at,
        }]
    } else {
        vec![]
    };
    let author = UserRef { id: release.author_id, username: "ada".into(), display_name: "Ada Marlow".into(), kind: UserKind::User };
    ReleaseView { target_commitish: "main".into(), author, assets, release }
}

#[async_trait]
impl Search for Demo {
    async fn repos(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let q = query.to_lowercase();
        Ok(repos()
            .into_iter()
            .filter(|r| visible(actor, r) && (q.is_empty() || r.name.contains(&q) || r.description.to_lowercase().contains(&q)))
            .take(limit as usize)
            .map(|r| SearchHit { kind: "repo".into(), id: r.id.0, title: r.full_name(), snippet: r.description })
            .collect())
    }
    async fn issues(&self, _a: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let q = query.to_lowercase();
        Ok(issues()
            .into_iter()
            .filter(|i| i.title.to_lowercase().contains(&q))
            .take(limit as usize)
            .map(|i| SearchHit {
                kind: "issue".into(),
                id: i.id.0,
                title: format!("#{} {}", i.number, i.title),
                snippet: i.body.chars().take(80).collect(),
            })
            .collect())
    }
    async fn users(&self, _a: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let q = query.to_lowercase();
        Ok(users()
            .into_iter()
            .filter(|u| u.username.contains(&q))
            .take(limit as usize)
            .map(|u| SearchHit { kind: "user".into(), id: u.id.0, title: u.username, snippet: u.display_name })
            .collect())
    }
    async fn code(&self, _a: &Actor, r: RepoId, q: &str, _l: u32) -> Result<Vec<SearchHit>> {
        Ok(if is_lh(r) && "pub struct Beam {".contains(q) {
            vec![SearchHit { kind: "code".into(), id: 4, title: "src/beam.rs".into(), snippet: "pub struct Beam {".into() }]
        } else {
            vec![]
        })
    }
    async fn code_all(&self, a: &Actor, q: &str, l: u32) -> Result<Vec<CodeHit>> {
        if a.is_anonymous() {
            return Ok(vec![]);
        }
        Ok(Search::code(self, a, LIGHTHOUSE, q, l)
            .await?
            .into_iter()
            .map(|h| CodeHit {
                repo_id: LIGHTHOUSE,
                repo: "ada/lighthouse".into(),
                branch: "main".into(),
                path: h.title,
                line: h.id,
                text: h.snippet,
            })
            .collect())
    }
}

#[async_trait]
impl Notifications for Demo {
    async fn inbox(&self, _a: &Actor, unread_only: bool, _p: u32, _pp: u32) -> Result<Vec<Notification>> {
        Ok(notifications().into_iter().filter(|n| !unread_only || n.unread).collect())
    }
    async fn unread_count(&self, actor: &Actor) -> Result<i64> {
        Ok(if uid(actor) == Some(ADA) { 2 } else { 0 })
    }
    async fn mark_read(&self, _a: &Actor, _id: Option<NotificationId>) -> Result<()> {
        Ok(())
    }
    async fn subscribe(&self, _a: &Actor, _r: RepoId, _l: &str) -> Result<()> {
        Ok(())
    }
    async fn watch_level(&self, _a: &Actor, _r: RepoId) -> Result<String> {
        Ok("participating".into())
    }
    async fn mark_done(&self, _a: &Actor, _id: NotificationId) -> Result<()> {
        Ok(())
    }
    async fn set_thread_subscription(&self, _a: &Actor, _t: &str, _id: i64, _s: bool) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Webhooks for Demo {
    async fn list(&self, _a: &Actor, repo: Option<RepoId>, _o: Option<OrgId>) -> Result<Vec<Webhook>> {
        Ok(vec![Webhook {
            id: WebhookId(1),
            repo_id: repo,
            org_id: None,
            url: "https://hooks.example.test/codehaven".into(),
            events: vec!["push".into(), "pull_request".into()],
            active: true,
        }])
    }
    async fn create(&self, _a: &Actor, new: NewWebhook, _c: &ClientInfo) -> Result<Webhook> {
        Ok(Webhook { id: WebhookId(2), repo_id: new.repo_id, org_id: new.org_id, url: new.url, events: new.events, active: new.active })
    }
    async fn delete(&self, _a: &Actor, _id: WebhookId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn redeliver(&self, _a: &Actor, _d: i64) -> Result<()> {
        Ok(())
    }
    async fn get(&self, a: &Actor, _id: WebhookId) -> Result<Webhook> {
        Webhooks::list(self, a, None, None).await?.into_iter().next().ok_or(Error::NotFound)
    }
    async fn update(&self, a: &Actor, id: WebhookId, _change: WebhookChange, _c: &ClientInfo) -> Result<Webhook> {
        Webhooks::get(self, a, id).await
    }
    async fn deliveries(&self, _a: &Actor, _id: WebhookId, _limit: u32) -> Result<Vec<WebhookDelivery>> {
        Ok(vec![])
    }
    async fn ping(&self, _a: &Actor, _id: WebhookId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Checks for Demo {
    async fn check_runs_for_sha(&self, repo: RepoId, s: &str) -> Result<Vec<CheckRun>> {
        if !is_lh(repo) {
            return Ok(vec![]);
        }
        let run = |id: i64, name: &str, st: &str, c: Option<&str>| CheckRun {
            id,
            repo_id: repo,
            head_sha: s.into(),
            name: name.into(),
            status: st.into(),
            conclusion: c.map(str::to_string),
            details_url: Some(format!("/ada/lighthouse/actions/runs/52/jobs/{}", 520 + id)),
        };
        Ok(if s == sha(21) {
            vec![run(1, "test", "completed", Some("success")), run(2, "lint", "completed", Some("success"))]
        } else if s == sha(6) {
            vec![run(1, "test", "completed", Some("success"))]
        } else if s == sha(5) {
            vec![run(1, "test", "completed", Some("failure"))]
        } else {
            vec![]
        })
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
        repo: RepoId,
        s: &str,
        state: &str,
        context: &str,
        d: Option<&str>,
        t: Option<&str>,
    ) -> Result<CommitStatus> {
        Ok(CommitStatus {
            id: 1,
            repo_id: repo,
            sha: s.into(),
            context: context.into(),
            state: state.into(),
            description: d.map(str::to_string),
            target_url: t.map(str::to_string),
        })
    }
    async fn required_satisfied(&self, _r: RepoId, _s: &str, _c: &[String]) -> Result<bool> {
        Ok(true)
    }
}

#[async_trait]
impl Ci for Demo {
    async fn runs(&self, _a: &Actor, repo: RepoId, _p: u32, _pp: u32) -> Result<Vec<WorkflowRun>> {
        Ok(if is_lh(repo) { runs() } else { vec![] })
    }
    async fn get_run(&self, _a: &Actor, _repo: RepoId, run: RunId) -> Result<WorkflowRun> {
        runs().into_iter().find(|r| r.id == run).ok_or(Error::NotFound)
    }
    async fn dispatch(&self, _a: &Actor, _repo: RepoId, _w: &str, _r: &str, _i: serde_json::Value, _c: &ClientInfo) -> Result<RunId> {
        Ok(RunId(54))
    }
    async fn cancel(&self, _a: &Actor, _repo: RepoId, _run: RunId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn badge(
        &self,
        _a: &Actor,
        repo: RepoId,
        file: &str,
        _b: Option<&str>,
        _e: Option<&str>,
    ) -> Result<ch_core::model::WorkflowBadge> {
        if !is_lh(repo) || file != "ci.yml" {
            return Err(Error::NotFound);
        }
        Ok(ch_core::model::WorkflowBadge { workflow_name: "CI".into(), conclusion: Some("success".into()) })
    }
    async fn rerun(&self, _a: &Actor, _repo: RepoId, _run: RunId, _f: bool, _c: &ClientInfo) -> Result<RunId> {
        Ok(RunId(55))
    }
    async fn log_range(&self, _a: &Actor, _repo: RepoId, _job: JobId, range: LogRange) -> Result<Bytes> {
        let all = log_text().into_bytes();
        let from = (range.from as usize).min(all.len());
        let to = range.to.map(|t| (t as usize).min(all.len())).unwrap_or(all.len()).max(from);
        Ok(Bytes::from(all[from..to].to_vec()))
    }
    async fn create_runner_token(&self, _a: &Actor, _repo: Option<RepoId>, _c: &ClientInfo) -> Result<RunnerRegistrationToken> {
        Ok(RunnerRegistrationToken {
            token: "chr_demoRegistrationToken0123456789".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
    }
}

#[async_trait]
impl Sessions for Demo {
    async fn sign_in(&self, username: &str, password: &str, _c: &ClientInfo) -> Result<SignInOutcome> {
        if password != DEMO_PASSWORD {
            return Ok(SignInOutcome::Rejected);
        }
        Ok(match username {
            "ada" => SignInOutcome::Session(NewSession { cookie_value: ADA_COOKIE.into(), max_age_secs: 3600 }),
            "bob" => SignInOutcome::TwoFactorRequired { pending: "pending-bob".into() },
            _ => SignInOutcome::Rejected,
        })
    }
    async fn complete_two_factor(&self, pending: &str, code: &str, _c: &ClientInfo) -> Result<Option<NewSession>> {
        Ok(if pending == "pending-bob" && code == "123456" {
            Some(NewSession { cookie_value: BOB_COOKIE.into(), max_age_secs: 3600 })
        } else {
            None
        })
    }
    async fn sign_out(&self, _c: &str, _cl: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn sessions(&self, _a: &Actor, current: &str) -> Result<Vec<SessionInfo>> {
        Ok(vec![
            SessionInfo {
                id: SessionId(1),
                ip: "203.0.113.7".into(),
                user_agent: Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 Version/17.5 Safari/605.1.15".into()),
                created_at: ago(2, 0),
                last_seen_at: ago(0, 0),
                current: current == ADA_COOKIE || current.is_empty(),
            },
            SessionInfo {
                id: SessionId(2),
                ip: "198.51.100.23".into(),
                user_agent: Some("Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0".into()),
                created_at: ago(9, 0),
                last_seen_at: ago(3, 0),
                current: false,
            },
        ])
    }
    async fn revoke_session(&self, _a: &Actor, _id: SessionId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn security_log(&self, _a: &Actor, _l: i64) -> Result<Vec<StoredAudit>> {
        Ok(audit_rows())
    }
    async fn begin_two_factor_setup(&self, _a: &Actor, _c: &ClientInfo) -> Result<TwoFactorSetup> {
        Ok(TwoFactorSetup {
            secret: "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP".into(),
            otpauth_url: "otpauth://totp/Codehaven:ada?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Codehaven".into(),
        })
    }
    async fn confirm_two_factor_setup(&self, _a: &Actor, current: &str, code: &str, _c: &ClientInfo) -> Result<TwoFactorOn> {
        if code != "123456" {
            return Err(Error::Validation(vec![FieldError::custom("two_factor", "code", "That code did not match.")]));
        }
        Ok(TwoFactorOn {
            recovery_codes: (1..=10).map(|i| format!("rcv-{i:04}-demo-{i:04}")).collect(),
            session: NewSession { cookie_value: current.into(), max_age_secs: 3600 },
        })
    }
    async fn disable_two_factor(&self, _a: &Actor, current: &str, _p: &str, _c: &ClientInfo) -> Result<NewSession> {
        Ok(NewSession { cookie_value: current.into(), max_age_secs: 3600 })
    }
    async fn peek_link(&self, token: &str) -> Result<LinkPeek> {
        if token == DEMO_LINK { Ok(LinkPeek { username: "ada".into(), purpose: LinkPurpose::SetPassword }) } else { Err(Error::NotFound) }
    }
    async fn redeem_link(&self, token: &str, pw: &str, _c: &ClientInfo) -> Result<()> {
        if token != DEMO_LINK {
            return Err(Error::NotFound);
        }
        if pw.len() < 12 {
            return Err(Error::Validation(vec![FieldError::custom("user", "password", "Use at least 12 characters.")]));
        }
        Ok(())
    }
    async fn change_password(&self, _a: &Actor, cookie: &str, current: &str, new: &str, _c: &ClientInfo) -> Result<NewSession> {
        if current != DEMO_PASSWORD {
            return Err(Error::Validation(vec![FieldError::custom("user", "current_password", "That is not your current password.")]));
        }
        if new.len() < 12 {
            return Err(Error::Validation(vec![FieldError::custom("user", "password", "Use at least 12 characters.")]));
        }
        Ok(NewSession { cookie_value: cookie.into(), max_age_secs: 3600 })
    }
    async fn update_profile(&self, _a: &Actor, u: UpdateProfile, _c: &ClientInfo) -> Result<User> {
        let mut me = user(ADA, "ada", "Ada Marlow", true, UserKind::User);
        if let Some(n) = u.display_name {
            me.display_name = n;
        }
        if let Some(e) = u.email {
            me.email = Some(e);
        }
        Ok(me)
    }
}

pub fn audit_rows() -> Vec<StoredAudit> {
    let c = ClientInfo { ip: [203, 0, 113, 7].into(), user_agent: Some("Safari".into()) };
    let a = Actor::User { id: ADA, is_admin: true, has_2fa: true, via: AuthVia::Session };
    let mk = |id: i64, act: &str, o: Outcome, h: i64| StoredAudit {
        id: AuditId(id),
        at: ago(0, h),
        event: AuditEvent::new(&a, &c, act, o).target("user", "1"),
    };
    vec![
        mk(5, action::INVITE_CREATED, Outcome::Success, 2),
        mk(4, action::SIGN_IN, Outcome::Success, 5),
        mk(3, action::SIGN_IN_FAILED, Outcome::Failure, 30),
        mk(2, action::TOKEN_CREATED, Outcome::Success, 50),
        mk(1, action::SSH_KEY_ADDED, Outcome::Success, 70),
    ]
}

#[async_trait]
impl People for Demo {
    async fn list_users(&self, _a: &Actor, query: Option<&str>, limit: u32) -> Result<Vec<User>> {
        let q = query.unwrap_or("").to_lowercase();
        Ok(users()
            .into_iter()
            .filter(|u| q.is_empty() || u.username.contains(&q) || u.display_name.to_lowercase().contains(&q))
            .take(limit as usize)
            .collect())
    }
    async fn orgs_of(&self, _a: &Actor, user: UserId) -> Result<Vec<User>> {
        Ok(if user == ADA || user == BOB { users().into_iter().filter(|u| u.kind == UserKind::Org).collect() } else { vec![] })
    }
    async fn org_members(&self, _a: &Actor, _o: OrgId) -> Result<Vec<Member>> {
        let u = users();
        Ok(vec![Member { user: u[0].clone(), role: OrgRole::Owner }, Member { user: u[1].clone(), role: OrgRole::Member }])
    }
    async fn teams(&self, _a: &Actor, o: OrgId) -> Result<Vec<Team>> {
        Ok(vec![Team {
            id: TeamId(1),
            org_id: o,
            name: "maintainers".into(),
            description: "Can merge anything".into(),
            permission: Permission::Maintain,
            members: 2,
        }])
    }
    async fn team_members(&self, _a: &Actor, _t: TeamId) -> Result<Vec<User>> {
        Ok(users().into_iter().take(2).collect())
    }
    async fn create_org(&self, _a: &Actor, name: &str, display: &str, _c: &ClientInfo) -> Result<User> {
        Ok(user(UserId(70), name, display, false, UserKind::Org))
    }
    async fn site_stats(&self, _a: &Actor) -> Result<SiteStats> {
        Ok(SiteStats {
            users: 3,
            orgs: 1,
            repos: 4,
            runners_online: 1,
            queued_jobs: 2,
            data_dir_total_bytes: 400 * 1024 * 1024 * 1024,
            data_dir_free_bytes: 119 * 1024 * 1024 * 1024,
            database_ok: true,
            version: "0.1.0".into(),
        })
    }
}

#[async_trait]
impl Tracker for Demo {
    async fn labels(&self, _a: &Actor, repo: RepoId) -> Result<Vec<Label>> {
        Ok(if is_lh(repo) { labels() } else { vec![] })
    }
    async fn create_label(&self, _a: &Actor, repo: RepoId, name: &str, color: &str, d: &str, _c: &ClientInfo) -> Result<Label> {
        Ok(Label { id: LabelId(50), repo_id: repo, name: name.into(), color: color.into(), description: d.into() })
    }
    async fn delete_label(&self, _a: &Actor, _r: RepoId, _n: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn milestones(&self, _a: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>> {
        Ok(if is_lh(repo) { milestones().into_iter().filter(|m| open.is_none_or(|o| m.open == o)).collect() } else { vec![] })
    }
    async fn create_milestone(
        &self,
        _a: &Actor,
        repo: RepoId,
        t: &str,
        d: &str,
        due: Option<chrono::DateTime<Utc>>,
        _c: &ClientInfo,
    ) -> Result<Milestone> {
        Ok(Milestone {
            id: MilestoneId(9),
            repo_id: repo,
            title: t.into(),
            description: d.into(),
            due_on: due,
            open: true,
            open_issues: 0,
            closed_issues: 0,
        })
    }
    async fn set_milestone_state(&self, _a: &Actor, _r: RepoId, _id: MilestoneId, _o: bool, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn set_milestone(&self, _a: &Actor, _r: RepoId, _n: i64, _m: Option<MilestoneId>, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn set_assignees(&self, _a: &Actor, _r: RepoId, _n: i64, _u: Vec<UserId>, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn review_comments(&self, _a: &Actor, _r: RepoId, n: i64) -> Result<Vec<ReviewComment>> {
        Ok(if n == 14 { review_comments() } else { vec![] })
    }
    async fn create_release(&self, _a: &Actor, new: NewRelease, _c: &ClientInfo) -> Result<Release> {
        Ok(Release {
            id: ReleaseId(3),
            repo_id: new.repo_id,
            tag_name: new.tag_name,
            name: new.name,
            body: new.body,
            draft: new.draft,
            prerelease: new.prerelease,
            author_id: ADA,
            created_at: Utc::now(),
            published_at: Some(Utc::now()),
        })
    }
    async fn delete_release(&self, _a: &Actor, _r: RepoId, _id: ReleaseId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Pipelines for Demo {
    async fn workflows(&self, _a: &Actor, repo: RepoId, _r: &str) -> Result<Vec<WorkflowDef>> {
        Ok(if is_lh(repo) { workflows() } else { vec![] })
    }
    async fn jobs(&self, _a: &Actor, _repo: RepoId, run: RunId) -> Result<Vec<WorkflowJob>> {
        if runs().iter().any(|r| r.id == run) { Ok(jobs(run.0)) } else { Err(Error::NotFound) }
    }
    async fn runners(&self, _a: &Actor, repo: Option<RepoId>) -> Result<Vec<Runner>> {
        Ok(runners().into_iter().filter(|r| repo.is_none() || r.repo_id.is_none() || r.repo_id == repo).collect())
    }
    async fn revoke_runner(&self, _a: &Actor, _id: RunnerId, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn secrets(&self, _a: &Actor, _r: RepoId) -> Result<Vec<SecretMeta>> {
        Ok(vec![
            SecretMeta { name: "DEPLOY_KEY".into(), updated_at: ago(12, 0) },
            SecretMeta { name: "REGISTRY_TOKEN".into(), updated_at: ago(40, 0) },
        ])
    }
    async fn set_secret(&self, _a: &Actor, _r: RepoId, _n: &str, _v: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn delete_secret(&self, _a: &Actor, _r: RepoId, _n: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn variables(&self, _a: &Actor, _r: RepoId) -> Result<Vec<Variable>> {
        Ok(vec![Variable { name: "RUST_LOG".into(), value: "info".into(), updated_at: ago(5, 0) }])
    }
    async fn set_variable(&self, _a: &Actor, _r: RepoId, _n: &str, _v: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn delete_variable(&self, _a: &Actor, _r: RepoId, _n: &str, _c: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn artifacts(&self, _a: &Actor, _r: RepoId, run: RunId) -> Result<Vec<Artifact>> {
        Ok(if run.0 == 52 {
            vec![Artifact {
                id: 1,
                name: "lighthouse-linux-x64".into(),
                size: 2_400_000,
                sha256: "ab".repeat(32),
                created_at: ago(0, 3),
                expired: false,
            }]
        } else {
            vec![]
        })
    }
}

// ------------------------------------------------------------------ storage fakes

#[async_trait]
impl Audit for Demo {
    async fn record(&self, _tx: &mut DbTx<'_>, _e: AuditEvent) -> Result<()> {
        Ok(())
    }
    async fn record_standalone(&self, _e: AuditEvent) -> Result<()> {
        Ok(())
    }
    async fn query(&self, _a: &Actor, f: AuditFilter) -> Result<Vec<StoredAudit>> {
        Ok(audit_rows().into_iter().filter(|r| f.action.as_ref().is_none_or(|a| &r.event.action == a)).collect())
    }
}

#[async_trait]
impl EventBus for Demo {
    async fn emit(&self, _tx: &mut DbTx<'_>, _e: DomainEvent) -> Result<i64> {
        Ok(1)
    }
    async fn poll(&self, _c: &str, _l: i64) -> Result<Vec<StoredEvent>> {
        Ok(vec![])
    }
    async fn ack(&self, _c: &str, _id: i64) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl BlobStore for Demo {
    async fn put_stream(&self, _b: BoxStream<'static, std::io::Result<Bytes>>, _m: u64) -> Result<BlobRef> {
        Ok(BlobRef { sha256: "ab".repeat(32), size: 0 })
    }
    async fn put_bytes(&self, d: Bytes) -> Result<BlobRef> {
        Ok(BlobRef { sha256: "ab".repeat(32), size: d.len() as u64 })
    }
    async fn get_stream(&self, _s: &str) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        Ok(Box::pin(stream::iter(vec![Ok(Bytes::from_static(b"artifact bytes"))])))
    }
    async fn size(&self, _s: &str) -> Result<Option<u64>> {
        Ok(Some(14))
    }
    async fn delete(&self, _s: &str) -> Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------------ state and router

pub fn config() -> Config {
    let mut c = Config::for_tests();
    c.server.name = "Codehaven".into();
    c.server.external_url = ORIGIN.into();
    c.ssh.external = "127.0.0.1:2222".into();
    c
}

/// A full `AppState` backed by the demo data. The database pool never connects.
pub fn demo_state() -> AppState {
    let d = Arc::new(Demo);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .unwrap_or_else(|_| unreachable!("a lazy pool does not connect"));
    let mut b = AppStateBuilder::new(Arc::new(config()), pool, d.clone(), d.clone(), d.clone());
    b.authn = d.clone();
    b.invites = d.clone();
    b.authz = d.clone();
    b.accounts = d.clone();
    b.repos = d.clone();
    b.git = d.clone();
    b.issues = d.clone();
    b.pulls = d.clone();
    b.reviews = d.clone();
    b.releases = d.clone();
    b.search = d.clone();
    b.notifications = d.clone();
    b.webhooks = d.clone();
    b.checks = d.clone();
    b.ci = d.clone();
    b.sessions = d.clone();
    b.people = d.clone();
    b.tracker = d.clone();
    b.pipelines = d;
    b.build()
}

/// What the real gate does for pages: resolve the session cookie, let signed out visitors reach
/// only the public routes, send everyone else to the sign-in page.
async fn demo_gate(axum::extract::State(state): axum::extract::State<AppState>, mut req: Request, next: Next) -> Response {
    let cookie = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == ch_core::access::session_cookie_name(false))
        .map(|(_, v)| v.to_string());
    let actor = match &cookie {
        Some(c) => state.authn.from_session(c, &ClientInfo::local()).await.unwrap_or(Actor::Anonymous),
        None => Actor::Anonymous,
    };
    let public = ch_core::access::is_public(req.method(), req.uri().path());
    if actor == Actor::Anonymous && !public {
        let next_q = ch_core::access::encode_query_value(req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
        let mut r = StatusCode::SEE_OTHER.into_response();
        if let Ok(v) = HeaderValue::from_str(&format!("/login?next={next_q}")) {
            r.headers_mut().insert(header::LOCATION, v);
        }
        return r;
    }
    if let (false, Some(c)) = (actor.is_anonymous(), cookie.as_deref()) {
        req.extensions_mut().insert(ch_core::live::Credential::Session { hash: ch_db::tokens::hash_token(c) });
    }
    req.extensions_mut().insert(actor);
    req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    let mut resp = next.run(req).await;
    // The headers the real server adds, so screenshots and tests see the same CSP.
    let h = resp.headers_mut();
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"),
        );
    }
    resp
}

/// The pages and static files, served from demo data.
pub fn router() -> Router {
    let state = demo_state();
    crate::routes::build().router.layer(middleware::from_fn_with_state(state.clone(), demo_gate)).with_state(state)
}

#[allow(dead_code)]
fn _body(_: Body) {}
