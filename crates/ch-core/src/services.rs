//! The service traits. This is the contract between streams (ARCHITECTURE.md section 4.3).
//!
//! All are `#[async_trait]`, object safe, and take `&Actor` where permission matters.
//! A domain crate implements its traits and the `codehaven` binary puts them into
//! `AppState` as `Arc<dyn Trait>`. A change after M0 is a small pull request against this
//! file, announced to every stream.
//!
//! Rules for implementors:
//! - `Anonymous` is granted nothing. Authorization failures on things the caller may not
//!   know exist return `Error::NotFound`, not `Forbidden`.
//! - Every mutation records an audit event through `Audit::record` and emits its
//!   `DomainEvent` through `EventBus::emit`, in the same transaction as the change.

use crate::actor::{Actor, Permission};
pub use crate::apps::Apps;
use crate::audit::{AuditEvent, AuditFilter, StoredAudit};
use crate::collab::*;
use crate::error::{Error, Result};
use crate::events::{DomainEvent, StoredEvent};
pub use crate::identity::{Credentials, Directory, Orgs, TwoFactor};
use crate::ids::*;
use crate::model::*;
pub use crate::pages::Pages;
use crate::web_contract::RepoUpdate;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

pub type DbTx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// The answer of a collaboration method nobody implemented. Several collab traits grew past
/// their first version, and a fake that predates the new methods keeps compiling and fails
/// closed with this.
fn unbuilt<T>(what: &str) -> Result<T> {
    Err(crate::error::Error::unavailable(format!("{what} is not built yet")))
}

// ---------------------------------------------------------------- identity (ch-auth)

// `from_*` names say where a credential came from, not that these convert `self`.
#[allow(clippy::wrong_self_convention)]
#[async_trait]
pub trait Authn: Send + Sync {
    /// `Actor::Anonymous` when the cookie is unknown, expired or only half signed in.
    async fn from_session(&self, cookie_value: &str, client: &ClientInfo) -> Result<Actor>;
    /// Username and password, or username and personal access token. Failures count
    /// toward the same rate limits and lockout as the sign-in form.
    async fn from_basic(&self, username: &str, secret: &str, client: &ClientInfo) -> Result<Actor>;
    /// A personal access token, a CI job token or a runner credential.
    async fn from_bearer(&self, token: &str, client: &ClientInfo) -> Result<Actor>;
    /// An OAuth access token of a connected app, for `/mcp` only. `from_bearer` never accepts
    /// one, which keeps each kind of token on its own door. A token that is not live, whose
    /// grant ended, whose person is suspended or has turned two-factor off, is `Unauthorized`.
    /// An unknown token is never counted against the failed-auth budget of the address.
    async fn from_app_bearer(&self, _token: &str, _client: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_ssh_key(&self, fingerprint: &str) -> Result<Option<SshIdentity>>;
    /// The actor a credential resolves to right now, or `None` when it no longer signs anyone
    /// in. Unlike `from_session` it never moves a session's last-seen time or expiry, and it
    /// counts no failure, so a long stream can call it every second. Fails closed by default.
    async fn recheck(&self, _credential: &crate::live::Credential) -> Result<Option<Actor>> {
        Ok(None)
    }
    /// The fingerprint of a user's current password hash, for `Credential::Basic`. `None`
    /// when the account has no password. Fails closed by default.
    async fn password_fingerprint(&self, _user: UserId) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[async_trait]
pub trait Invites: Send + Sync {
    /// Site admins only (and `Actor::System` for the CLI).
    async fn create(&self, actor: &Actor, req: NewInvite, client: &ClientInfo) -> Result<CreatedInvite>;
    async fn list(&self, actor: &Actor) -> Result<Vec<Invite>>;
    async fn revoke(&self, actor: &Actor, id: InviteId, client: &ClientInfo) -> Result<()>;
    /// Expired, used, revoked and made-up tokens all return `Error::NotFound`.
    async fn peek(&self, token: &str) -> Result<InvitePeek>;
    /// One transaction: `UPDATE invites SET used_at = now() ... WHERE used_at IS NULL AND
    /// revoked_at IS NULL AND expires_at > now() RETURNING ...`, then create the user. Two
    /// people racing one link get one account between them.
    async fn accept(&self, token: &str, account: NewAccount, client: &ClientInfo) -> Result<User>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgRole {
    Owner,
    Member,
}

#[async_trait]
pub trait Authz: Send + Sync {
    /// The permission model of ARCHITECTURE.md section 7.2. `Anonymous` gets `View` on a
    /// public repository while the instance allows public repositories, and `None` otherwise.
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission>;
    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>>;
    /// `Error::NotFound` when the actor cannot see the repo at all (below `View`),
    /// `Error::Forbidden` when they can see it but lack `needed`.
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()>;
    /// True when the actor's access to the repository comes from a grant (ownership, org
    /// ownership or base permission, a collaborator or team row, site admin), not only from
    /// the repository's tier. The contribution setting asks this.
    async fn has_grant(&self, actor: &Actor, repo: RepoId) -> Result<bool> {
        Ok(self.repo_permission(actor, repo).await? >= Permission::Read)
    }
}

#[async_trait]
pub trait Accounts: Send + Sync {
    async fn get_user(&self, id: UserId) -> Result<User>;
    async fn get_by_username(&self, actor: &Actor, username: &str) -> Result<User>;
    async fn count_users(&self) -> Result<i64>;
    /// CLI only (`Actor::System`). Creates an account with no password.
    async fn create_user(&self, actor: &Actor, new: NewUser, client: &ClientInfo) -> Result<User>;
    /// Mints a one-time link for the account. Valid for `ttl_minutes`.
    async fn issue_link(
        &self,
        actor: &Actor,
        user: UserId,
        purpose: LinkPurpose,
        ttl_minutes: u32,
        client: &ClientInfo,
    ) -> Result<OneTimeLink>;
    /// "Forgot your password": the account a username or email names, when one should get a
    /// reset link by email now, and that link. `None` for no such account, a suspended one, no
    /// email, or a link already sent in the last ten minutes. The caller answers the same in
    /// every case. Unlike an admin's reset link, this one ends no session until it is used,
    /// since anyone can ask for it.
    async fn request_password_reset(&self, login: &str, client: &ClientInfo) -> Result<Option<(String, OneTimeLink)>> {
        let _ = (login, client);
        Err(Error::unavailable("password reset by email is not built yet"))
    }
    /// Sets a password directly. Only `Actor::System` with `--password-stdin`, for the e2e harness.
    async fn set_password(&self, actor: &Actor, user: UserId, password: &str, client: &ClientInfo) -> Result<()>;
    /// Ends every session of the account. Lockout state is separate: see `unlock`.
    async fn sign_out_everywhere(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn unlock(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn suspend(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn set_can_run_ci(&self, actor: &Actor, user: UserId, value: bool, client: &ClientInfo) -> Result<()>;
    async fn list_ssh_keys(&self, actor: &Actor, user: UserId) -> Result<Vec<SshKey>>;
    async fn add_ssh_key(&self, actor: &Actor, user: UserId, title: &str, public_key: &str, client: &ClientInfo) -> Result<SshKey>;
    async fn delete_ssh_key(&self, actor: &Actor, id: SshKeyId, client: &ClientInfo) -> Result<()>;
    async fn list_tokens(&self, actor: &Actor, user: UserId) -> Result<Vec<AccessToken>>;
    /// Returns the token record and the secret, shown once. Expiry is required.
    async fn create_token(
        &self,
        actor: &Actor,
        user: UserId,
        name: &str,
        scopes: Vec<String>,
        expires_in_days: u32,
        client: &ClientInfo,
    ) -> Result<(AccessToken, String)>;
    async fn delete_token(&self, actor: &Actor, id: TokenId, client: &ClientInfo) -> Result<()>;
}

// ---------------------------------------------------------------- git (ch-git)

#[async_trait]
pub trait Repos: Send + Sync {
    async fn create(&self, actor: &Actor, new: NewRepo, client: &ClientInfo) -> Result<Repo>;
    /// `Error::NotFound` for a repo the actor cannot read, same as one that does not exist.
    async fn get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo>;
    async fn get(&self, actor: &Actor, id: RepoId) -> Result<Repo>;
    async fn list_for_owner(&self, actor: &Actor, owner: UserId) -> Result<Vec<Repo>>;
    async fn rename(&self, actor: &Actor, id: RepoId, new_name: &str, client: &ClientInfo) -> Result<Repo>;
    async fn transfer(&self, actor: &Actor, id: RepoId, new_owner: UserId, client: &ClientInfo) -> Result<Repo>;
    async fn fork(&self, actor: &Actor, id: RepoId, into: UserId, client: &ClientInfo) -> Result<Repo>;
    /// Description, visibility, default branch, archive flag and topics.
    async fn update(&self, actor: &Actor, id: RepoId, update: RepoUpdate, client: &ClientInfo) -> Result<Repo> {
        let _ = (actor, id, update, client);
        Err(Error::unavailable("update is not built yet"))
    }
    async fn delete(&self, actor: &Actor, id: RepoId, client: &ClientInfo) -> Result<()>;
    async fn collaborators(&self, actor: &Actor, id: RepoId) -> Result<Vec<Collaborator>>;
    async fn set_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, permission: Permission, client: &ClientInfo) -> Result<()>;
    async fn remove_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, client: &ClientInfo) -> Result<()>;
    async fn protections(&self, actor: &Actor, id: RepoId) -> Result<Vec<BranchProtection>>;
    async fn set_protection(&self, actor: &Actor, id: RepoId, protection: BranchProtection, client: &ClientInfo) -> Result<()>;
    async fn remove_protection(&self, actor: &Actor, id: RepoId, pattern: &str, client: &ClientInfo) -> Result<()>;
    /// Settings, topics and archive state (ARCHITECTURE.md section 4.3 lists `settings`).
    /// Added by the git stream with a default body so fakes in other streams keep compiling.
    async fn update_settings(&self, actor: &Actor, id: RepoId, update: RepoSettingsUpdate, client: &ClientInfo) -> Result<Repo> {
        let _ = (actor, id, update, client);
        Err(Error::unavailable("update_settings is not built yet"))
    }
    /// Every repository the actor can read, most recently pushed first. Page is 1-based.
    async fn list_accessible(&self, actor: &Actor, page: u32, per_page: u32) -> Result<Vec<Repo>> {
        let _ = (actor, page, per_page);
        Err(Error::unavailable("list_accessible is not built yet"))
    }
    /// The one way a repository's tier changes (epic ch-4pn). Public needs a browser session of
    /// the account owner and the instance switch; widening rebuilds the git store first.
    async fn set_tier(&self, actor: &Actor, id: RepoId, to: Visibility, client: &ClientInfo) -> Result<Repo> {
        let _ = (actor, id, to, client);
        Err(Error::unavailable("set_tier is not built yet"))
    }
    /// Star counts and fork counts for the header, and whether this person starred it.
    async fn social(&self, actor: &Actor, id: RepoId) -> Result<RepoSocial> {
        let _ = (actor, id);
        Err(Error::unavailable("social is not built yet"))
    }
    /// Stars or unstars a repository for the signed-in person. Needs an account that can see it.
    async fn set_star(&self, actor: &Actor, id: RepoId, starred: bool) -> Result<()> {
        let _ = (actor, id, starred);
        Err(Error::unavailable("set_star is not built yet"))
    }
    /// The direct forks of a repository that the actor can read, newest first. A fork the
    /// actor cannot read is left out and not counted.
    async fn forks(&self, actor: &Actor, id: RepoId) -> Result<Vec<Repo>> {
        let _ = (actor, id);
        Err(Error::unavailable("forks is not built yet"))
    }
    /// A new repository filled from a git URL anyone can clone over https (ch-ejm). Signed in,
    /// never a connected app, created private or internal like any new repository. The fetch
    /// reaches only public addresses, pinned and without redirects.
    async fn import(&self, actor: &Actor, new: NewRepo, url: &str, client: &ClientInfo) -> Result<Repo> {
        let _ = (actor, new, url, client);
        Err(Error::unavailable("importing is not built yet"))
    }
    /// A private repository for an existing account, filled from a bare git repository at `path`
    /// on this server's own disk (inside the container). For `codehaven admin` only: the system
    /// actor, never a person, so the path is the operator's. `run_ci` also announces the
    /// default branch as a push by the owner, so the repository's own workflows start. Nothing
    /// is overwritten: an existing name is refused.
    async fn import_local(&self, actor: &Actor, new: NewRepo, path: &str, run_ci: bool, client: &ClientInfo) -> Result<Repo> {
        let _ = (actor, new, path, run_ci, client);
        Err(Error::unavailable("importing is not built yet"))
    }
    /// A fork at a chosen tier: private by default, never above the source, never public.
    async fn fork_as(&self, actor: &Actor, id: RepoId, into: UserId, visibility: Visibility, client: &ClientInfo) -> Result<Repo> {
        let _ = visibility;
        self.fork(actor, id, into, client).await
    }
    /// Who may contribute without a grant. Repo admin, owner rule as for the tier.
    async fn set_contribution_policy(&self, actor: &Actor, id: RepoId, policy: ContributionPolicy, client: &ClientInfo) -> Result<Repo> {
        let _ = (actor, id, policy, client);
        Err(Error::unavailable("set_contribution_policy is not built yet"))
    }
    /// What making the repository public would expose. Repo admin.
    async fn publish_summary(&self, actor: &Actor, id: RepoId) -> Result<PublishSummary> {
        let _ = (actor, id);
        Err(Error::unavailable("publish_summary is not built yet"))
    }
    /// The instance switch for public repositories.
    async fn public_repos_allowed(&self) -> Result<bool> {
        Ok(false)
    }
    /// Turns the switch on or off. Site admin with two-factor, or `System`. Off lowers every
    /// public repository to internal in the same transaction; on raises none. Returns how many
    /// repositories were lowered.
    async fn set_public_repos_allowed(&self, actor: &Actor, on: bool, client: &ClientInfo) -> Result<u64> {
        let _ = (actor, on, client);
        Err(Error::unavailable("set_public_repos_allowed is not built yet"))
    }
    /// The workspace switches. `WorkspaceSettings::default()` when never set.
    async fn workspace_settings(&self) -> Result<WorkspaceSettings> {
        Ok(WorkspaceSettings::default())
    }
    /// Stores the workspace switches. Site admin with two-factor, or `System`; anyone else gets
    /// `NotFound`. Audited as `instance.workspace_changed`.
    async fn set_workspace_settings(&self, actor: &Actor, settings: WorkspaceSettings, client: &ClientInfo) -> Result<()> {
        let _ = (actor, settings, client);
        Err(Error::unavailable("set_workspace_settings is not built yet"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveFormat {
    Zip,
    TarGz,
}

#[async_trait]
pub trait GitService: Send + Sync {
    async fn refs(&self, repo: RepoId, prefix: Option<&str>) -> Result<Vec<GitRef>>;
    /// Resolves a branch, tag, sha or `HEAD` to a commit sha.
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String>;
    async fn tree(&self, repo: RepoId, rev: &str, path: &str) -> Result<Vec<TreeEntry>>;
    async fn blob(&self, repo: RepoId, rev: &str, path: &str, max_bytes: u64) -> Result<Blob>;
    async fn log(&self, repo: RepoId, rev: &str, path: Option<&str>, skip: u32, limit: u32) -> Result<Vec<CommitInfo>>;
    async fn commit(&self, repo: RepoId, rev: &str) -> Result<CommitInfo>;
    async fn compare(&self, repo: RepoId, base: &str, head: &str) -> Result<Compare>;
    /// Compares `base` in `repo` with `head` in `head_repo`, a repository of the same fork
    /// network. A fork keeps its own objects, so once either side has commits the other lacks,
    /// neither store alone holds both. This reads both without copying anything into either
    /// (ch-8tj). The caller checked that the viewer can read both repositories.
    async fn compare_across(&self, repo: RepoId, base: &str, head_repo: RepoId, head: &str) -> Result<Compare> {
        if repo == head_repo {
            return self.compare(repo, base, head).await;
        }
        let _ = (base, head);
        Err(Error::unavailable("compare_across is not built yet"))
    }
    async fn diff(&self, repo: RepoId, base: &str, head: &str) -> Result<Diff>;
    async fn grep(&self, repo: RepoId, rev: &str, pattern: &str, limit: u32) -> Result<Vec<SearchHit>>;
    async fn archive(&self, repo: RepoId, rev: &str, format: ArchiveFormat) -> Result<BoxStream<'static, std::io::Result<Bytes>>>;
    /// Create, edit, rename and delete files in one commit, compare-and-swap on the ref.
    async fn commit_file_change(&self, actor: &Actor, req: CommitFileChange, client: &ClientInfo) -> Result<String>;
    async fn create_ref(&self, actor: &Actor, repo: RepoId, name: &str, sha: &str, client: &ClientInfo) -> Result<()>;
    async fn delete_ref(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()>;
    /// Computes mergeability without writing refs.
    async fn test_merge(&self, repo: RepoId, base_ref: &str, head_repo: RepoId, head_ref: &str) -> Result<MergeableState>;
    async fn merge(&self, actor: &Actor, req: MergeRequest, client: &ClientInfo) -> Result<MergeOutcome>;
    // ---- Browser workspace (spec sections 5 and 8). Reads take no actor: the caller checked
    // read access. Every read is pinned to one commit id the caller already checked with
    // `workspace_is_ancestor`.

    /// The commit `refs/heads/<branch>` points at, by exact name: never a tag, a short id or
    /// revision syntax. `None` when there is no such branch.
    async fn workspace_branch_head(&self, repo: RepoId, branch: &str) -> Result<Option<String>> {
        let _ = (repo, branch);
        Err(Error::unavailable("workspace_branch_head is not built yet"))
    }
    /// True when `sha` is a full commit id (40 or 64 lower-case hex) of a commit that is `head`
    /// or an ancestor of it. Anything else, including an unknown object, is `false`.
    async fn workspace_is_ancestor(&self, repo: RepoId, sha: &str, head: &str) -> Result<bool> {
        let _ = (repo, sha, head);
        Err(Error::unavailable("workspace_is_ancestor is not built yet"))
    }
    /// Every entry of the tree at `sha`, recursively, streamed and cut at `limits`.
    async fn workspace_tree(&self, repo: RepoId, sha: &str, limits: WorkspaceTreeLimits) -> Result<WorkspaceTree> {
        let _ = (repo, sha, limits);
        Err(Error::unavailable("workspace_tree is not built yet"))
    }
    /// The raw bytes of the blob at `path` (raw bytes, not decoded) in commit `sha`. The size is
    /// checked before reading: past `max_bytes` the bytes are empty and `too_large` is set.
    async fn workspace_blob(&self, repo: RepoId, sha: &str, path: &[u8], max_bytes: u64) -> Result<WorkspaceBlob> {
        let _ = (repo, sha, path, max_bytes);
        Err(Error::unavailable("workspace_blob is not built yet"))
    }
    /// Literal, case-insensitive search of text files at `sha`, cut at `limits`.
    async fn workspace_search(&self, repo: RepoId, sha: &str, text: &str, limits: WorkspaceSearchLimits) -> Result<WorkspaceSearch> {
        let _ = (repo, sha, text, limits);
        Err(Error::unavailable("workspace_search is not built yet"))
    }
    /// Every regular file under `prefix` (raw path bytes of a folder, empty for the whole tree)
    /// at commit `sha`, with its bytes, cut at `limits`: over a cap nothing is read.
    async fn workspace_snapshot(
        &self,
        repo: RepoId,
        sha: &str,
        prefix: &[u8],
        limits: WorkspaceSnapshotLimits,
    ) -> Result<WorkspaceSnapshot> {
        let _ = (repo, sha, prefix, limits);
        Err(Error::unavailable("workspace_snapshot is not built yet"))
    }
    /// Whether branch protection lets this actor commit straight to `branch` from the web.
    async fn workspace_direct_commit_allowed(&self, actor: &Actor, repo: RepoId, branch: &str) -> Result<bool> {
        let _ = (actor, repo, branch);
        Err(Error::unavailable("workspace_direct_commit_allowed is not built yet"))
    }

    /// Moves an existing ref to `sha`. Without `force` the move must be a fast-forward.
    /// Goes through the same protection checks and events as a push. Added by the git stream.
    async fn update_ref(&self, actor: &Actor, repo: RepoId, name: &str, sha: &str, force: bool, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, name, sha, force, client);
        Err(Error::unavailable("update_ref is not built yet"))
    }
    /// Who last changed each line of `path` at `rev`. Added by the git stream after M0, with a
    /// default body so a fake `GitService` in another stream keeps compiling.
    async fn blame(&self, repo: RepoId, rev: &str, path: &str) -> Result<Blame> {
        let _ = (repo, rev, path);
        Err(Error::unavailable("blame is not built yet"))
    }
    /// Whether `sha` is a commit that some branch or tag of this repository reaches. Forks share
    /// objects, so a commit can exist in the object store without belonging to this repository.
    async fn reachable(&self, repo: RepoId, sha: &str) -> Result<bool> {
        let _ = (repo, sha);
        Err(Error::unavailable("reachable is not built yet"))
    }
    /// The commits `base..head` (all the commits `head` reaches when there is no `base`), newest
    /// first, as full shas, at most `limit` of them. The default reads a comparison, which keeps
    /// 250 at most; the real store asks git for the list.
    async fn range_shas(&self, repo: RepoId, base: Option<&str>, head: &str, limit: u32) -> Result<Vec<String>> {
        let base = base.ok_or_else(|| Error::unavailable("a range with no base is not built here"))?;
        let c = self.compare(repo, base, head).await?;
        Ok(c.commits.into_iter().rev().take(limit as usize).map(|c| c.sha).collect())
    }
    /// What one commit changed. A root commit is compared with the empty tree.
    async fn commit_diff(&self, repo: RepoId, sha: &str) -> Result<Diff> {
        let _ = (repo, sha);
        Err(Error::unavailable("commit_diff is not built yet"))
    }
}

/// Which git program a client asked for over SSH or HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitWire {
    UploadPack,
    ReceivePack,
    UploadArchive,
}

/// How a client talks to git. SSH is `Interactive`: one long-lived duplex process. Smart HTTP
/// is two stateless shapes: first the ref advertisement, then request and response pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitMode {
    Interactive,
    AdvertiseRefs,
    StatelessRpc,
}

/// Who is on the other end of a git connection. A deploy key is not an account, so it has its
/// own shape: it reaches exactly one repository, read-only unless it was made writable.
#[derive(Debug, Clone)]
pub enum GitCaller {
    Actor(Actor),
    Deploy { repo_id: RepoId, read_only: bool },
}

/// A running git process, ready to be wired to a network connection. Dropping `stdin` sends
/// the process end-of-input. `done` resolves with the exit status once the process has ended
/// and the server has finished recording what the push changed.
pub struct GitSession {
    pub stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    pub stderr: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    pub done: futures::future::BoxFuture<'static, i32>,
}

/// Runs git for a client connection. Implemented in ch-git so that SSH (in ch-ssh, which may
/// not depend on ch-git) goes through the same permission checks, hooks and push pipeline as
/// smart HTTP.
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait GitTransport: Send + Sync {
    /// Resolves `owner/repo`, checks the caller may read (or write, for `ReceivePack`) and
    /// starts the process. `Error::NotFound` for a repository the caller cannot see.
    /// `protocol` is the client's `GIT_PROTOCOL` value, for protocol v2.
    async fn open(
        &self,
        caller: GitCaller,
        owner: &str,
        repo: &str,
        wire: GitWire,
        mode: GitMode,
        protocol: Option<String>,
        client: &ClientInfo,
    ) -> Result<GitSession>;
}

// ---------------------------------------------------------------- collaboration (ch-collab)

#[async_trait]
pub trait Issues: Send + Sync {
    async fn create(&self, actor: &Actor, new: NewIssue, client: &ClientInfo) -> Result<Issue>;
    async fn get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Issue>;
    async fn list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<Issue>>;
    async fn set_state(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        open: bool,
        reason: Option<&str>,
        client: &ClientInfo,
    ) -> Result<Issue>;
    async fn edit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        title: Option<String>,
        body: Option<String>,
        client: &ClientInfo,
    ) -> Result<Issue>;
    async fn set_labels(&self, actor: &Actor, repo: RepoId, number: i64, labels: Vec<String>, client: &ClientInfo) -> Result<Issue>;
    async fn comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Comment>>;
    async fn add_comment(&self, actor: &Actor, repo: RepoId, number: i64, body: &str, client: &ClientInfo) -> Result<Comment>;

    // ------- everything below was added by the collab stream. The defaults fail closed.

    /// One issue or pull request with names, labels and counts filled in.
    async fn view(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<IssueView> {
        let _ = (actor, repo, number);
        unbuilt("issues")
    }
    /// A list with everything filled in. The filter's `query` takes the search language
    /// (`is:open label:bug assignee:@me sort:updated-desc`).
    async fn views(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<IssueView>> {
        let _ = (actor, repo, filter);
        unbuilt("issues")
    }
    /// Issues by row id, silently leaving out the ones the actor cannot read. For search results.
    async fn views_by_ids(&self, actor: &Actor, ids: &[IssueId]) -> Result<Vec<IssueView>> {
        let _ = (actor, ids);
        unbuilt("issues")
    }
    /// Issues and pull requests across every repository the actor can read.
    async fn for_user(&self, actor: &Actor, scope: UserIssueScope, filter: IssueFilter) -> Result<Vec<IssueView>> {
        let _ = (actor, scope, filter);
        unbuilt("issues")
    }
    /// Comments and events in time order.
    async fn timeline(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<TimelineItem>> {
        let _ = (actor, repo, number);
        unbuilt("issues")
    }
    async fn comment_views(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<CommentView>> {
        let _ = (actor, repo, number);
        unbuilt("issues")
    }
    async fn comment_view(&self, actor: &Actor, repo: RepoId, comment: CommentId) -> Result<CommentView> {
        let _ = (actor, repo, comment);
        unbuilt("issues")
    }
    async fn edit_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, body: &str, client: &ClientInfo) -> Result<CommentView> {
        let _ = (actor, repo, comment, body, client);
        unbuilt("issues")
    }
    async fn delete_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, comment, client);
        unbuilt("issues")
    }
    /// The whole `PATCH /issues/{n}` in one transaction.
    async fn update(&self, actor: &Actor, repo: RepoId, number: i64, patch: IssuePatch, client: &ClientInfo) -> Result<IssueView> {
        let _ = (actor, repo, number, patch, client);
        unbuilt("issues")
    }
    async fn add_labels(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        labels: Vec<String>,
        client: &ClientInfo,
    ) -> Result<Vec<LabelInfo>> {
        let _ = (actor, repo, number, labels, client);
        unbuilt("issues")
    }
    async fn remove_label(&self, actor: &Actor, repo: RepoId, number: i64, label: &str, client: &ClientInfo) -> Result<Vec<LabelInfo>> {
        let _ = (actor, repo, number, label, client);
        unbuilt("issues")
    }
    async fn add_assignees(&self, actor: &Actor, repo: RepoId, number: i64, users: Vec<UserId>, client: &ClientInfo) -> Result<IssueView> {
        let _ = (actor, repo, number, users, client);
        unbuilt("issues")
    }
    async fn remove_assignees(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        let _ = (actor, repo, number, users, client);
        unbuilt("issues")
    }
    /// The milestone's number in the repo, or `None` to clear it.
    async fn set_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        milestone: Option<i64>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        let _ = (actor, repo, number, milestone, client);
        unbuilt("issues")
    }
    async fn set_locked(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        locked: bool,
        reason: Option<String>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        let _ = (actor, repo, number, locked, reason, client);
        unbuilt("issues")
    }
    async fn set_pinned(&self, actor: &Actor, repo: RepoId, number: i64, pinned: bool, client: &ClientInfo) -> Result<IssueView> {
        let _ = (actor, repo, number, pinned, client);
        unbuilt("issues")
    }
    /// The pinned issues of a repository, in the order set for them (ch-u6j).
    async fn pinned(&self, actor: &Actor, repo: RepoId) -> Result<Vec<IssueView>> {
        let _ = (actor, repo);
        unbuilt("issues")
    }
    /// Puts the pinned issues in the order given, which must name each pinned issue once.
    async fn reorder_pinned(&self, actor: &Actor, repo: RepoId, numbers: Vec<i64>, client: &ClientInfo) -> Result<Vec<IssueView>> {
        let _ = (actor, repo, numbers, client);
        unbuilt("issues")
    }
    /// Deletes an issue with its comments, for repository admins only. Audited. Pull requests
    /// cannot be deleted.
    async fn delete(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, number, client);
        unbuilt("issues")
    }
    /// Moves an issue, with its comments, to another repository. Needs write access on both, and
    /// the target must be no more visible than the source. Labels, the milestone and assignees
    /// belong to the old repository and are left behind. Returns the issue at its new address.
    async fn transfer(&self, actor: &Actor, repo: RepoId, number: i64, target: RepoId, client: &ClientInfo) -> Result<IssueView> {
        let _ = (actor, repo, number, target, client);
        unbuilt("issues")
    }
    /// People who can be assigned: the owner and everyone with at least triage access.
    async fn assignable(&self, actor: &Actor, repo: RepoId) -> Result<Vec<UserRef>> {
        let _ = (actor, repo);
        unbuilt("issues")
    }
    async fn labels(&self, actor: &Actor, repo: RepoId) -> Result<Vec<LabelInfo>> {
        let _ = (actor, repo);
        unbuilt("issues")
    }
    async fn label(&self, actor: &Actor, repo: RepoId, name: &str) -> Result<LabelInfo> {
        let _ = (actor, repo, name);
        unbuilt("issues")
    }
    async fn create_label(&self, actor: &Actor, repo: RepoId, draft: LabelDraft, client: &ClientInfo) -> Result<LabelInfo> {
        let _ = (actor, repo, draft, client);
        unbuilt("issues")
    }
    async fn update_label(&self, actor: &Actor, repo: RepoId, name: &str, edit: LabelEdit, client: &ClientInfo) -> Result<LabelInfo> {
        let _ = (actor, repo, name, edit, client);
        unbuilt("issues")
    }
    async fn delete_label(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, name, client);
        unbuilt("issues")
    }
    async fn milestones(&self, actor: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>> {
        let _ = (actor, repo, open);
        unbuilt("issues")
    }
    async fn milestone(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Milestone> {
        let _ = (actor, repo, number);
        unbuilt("issues")
    }
    async fn create_milestone(&self, actor: &Actor, repo: RepoId, draft: MilestoneDraft, client: &ClientInfo) -> Result<Milestone> {
        let _ = (actor, repo, draft, client);
        unbuilt("issues")
    }
    async fn update_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        edit: MilestoneEdit,
        client: &ClientInfo,
    ) -> Result<Milestone> {
        let _ = (actor, repo, number, edit, client);
        unbuilt("issues")
    }
    async fn delete_milestone(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, number, client);
        unbuilt("issues")
    }
    /// Adding the same reaction twice returns the one that is there.
    async fn react(&self, actor: &Actor, repo: RepoId, target: ReactionTarget, content: &str, client: &ClientInfo) -> Result<ReactionRow> {
        let _ = (actor, repo, target, content, client);
        unbuilt("issues")
    }
    async fn reactions(&self, actor: &Actor, repo: RepoId, target: ReactionTarget) -> Result<Vec<ReactionRow>> {
        let _ = (actor, repo, target);
        unbuilt("issues")
    }
    /// Removes the caller's own reaction. Someone else's reaction is `NotFound`.
    async fn unreact(&self, actor: &Actor, repo: RepoId, target: ReactionTarget, reaction_id: i64, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, target, reaction_id, client);
        unbuilt("issues")
    }
    /// `None` when the caller never chose, and follows the default rules.
    async fn subscription(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Option<bool>> {
        let _ = (actor, repo, number);
        unbuilt("issues")
    }
    async fn set_subscription(&self, actor: &Actor, repo: RepoId, number: i64, subscribed: bool) -> Result<()> {
        let _ = (actor, repo, number, subscribed);
        unbuilt("issues")
    }
    /// Issue templates from `.codehaven/ISSUE_TEMPLATE` or `.github/ISSUE_TEMPLATE` on the default branch.
    async fn templates(&self, actor: &Actor, repo: RepoId) -> Result<Vec<IssueTemplate>> {
        let _ = (actor, repo);
        unbuilt("issues")
    }
}

#[async_trait]
pub trait Pulls: Send + Sync {
    async fn create(&self, actor: &Actor, new: NewPull, client: &ClientInfo) -> Result<PullRequest>;
    /// `create`, with the person's say-so that the head's commits may be copied into a public
    /// base from a repository that is not public (`publish_head`, ch-4pn.14). Without it, such a
    /// pull request is refused with a `publish_head` field error that says what it would publish.
    async fn create_publishing(&self, actor: &Actor, new: NewPull, publish_head: bool, client: &ClientInfo) -> Result<PullRequest> {
        let _ = publish_head;
        self.create(actor, new, client).await
    }
    async fn get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullRequest>;
    async fn list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<PullRequest>>;
    async fn merge_gate(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<GateReport>;
    /// `Error::MergeBlocked(reasons)` (HTTP 405) when the gate is not clear.
    async fn merge(&self, actor: &Actor, repo: RepoId, number: i64, method: MergeMethod, client: &ClientInfo) -> Result<PullRequest>;
    async fn close(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullRequest>;

    // ------- added by the collab stream. The defaults fail closed.

    async fn view(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullView> {
        let _ = (actor, repo, number);
        unbuilt("pulls")
    }
    async fn views(&self, actor: &Actor, repo: RepoId, filter: PullFilter) -> Result<Vec<PullView>> {
        let _ = (actor, repo, filter);
        unbuilt("pulls")
    }
    /// The open pull request for a head branch and base branch, if there is one. For the
    /// "open a pull request" banner on a branch page.
    async fn open_for_branch(
        &self,
        actor: &Actor,
        repo: RepoId,
        head_repo: RepoId,
        head_ref: &str,
        base_ref: &str,
    ) -> Result<Option<PullView>> {
        let _ = (actor, repo, head_repo, head_ref, base_ref);
        unbuilt("pulls")
    }
    async fn update(&self, actor: &Actor, repo: RepoId, number: i64, patch: PullPatch, client: &ClientInfo) -> Result<PullView> {
        let _ = (actor, repo, number, patch, client);
        unbuilt("pulls")
    }
    async fn reopen(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullView> {
        let _ = (actor, repo, number, client);
        unbuilt("pulls")
    }
    async fn set_draft(&self, actor: &Actor, repo: RepoId, number: i64, draft: bool, client: &ClientInfo) -> Result<PullView> {
        let _ = (actor, repo, number, draft, client);
        unbuilt("pulls")
    }
    /// The diff the "Files changed" tab shows: from the merge base to the head.
    async fn files(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Diff> {
        let _ = (actor, repo, number);
        unbuilt("pulls")
    }
    async fn commits(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<CommitInfo>> {
        let _ = (actor, repo, number);
        unbuilt("pulls")
    }
    /// True when this pull request's changes must not be shown to `actor`: its head is in a
    /// repository the actor cannot read and its author never agreed to publish it in this
    /// public repository. The pages that show the diff or the commits ask first.
    async fn head_hidden(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<bool> {
        let _ = (actor, repo, number);
        Ok(false)
    }
    /// Merges the base branch into the head branch ("Update branch").
    async fn update_branch(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullView> {
        let _ = (actor, repo, number, client);
        unbuilt("pulls")
    }
    /// `Some(method)` turns auto-merge on, `None` turns it off.
    async fn set_auto_merge(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        method: Option<MergeMethod>,
        client: &ClientInfo,
    ) -> Result<PullView> {
        let _ = (actor, repo, number, method, client);
        unbuilt("pulls")
    }
    /// Whether the repository merges only through its merge queue.
    async fn merge_queue_enabled(&self, actor: &Actor, repo: RepoId) -> Result<bool> {
        let _ = (actor, repo);
        Ok(false)
    }
    /// Turns the merge queue on or off. Repository admins only. Turning it off empties the queue.
    async fn set_merge_queue(&self, actor: &Actor, repo: RepoId, on: bool, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, on, client);
        unbuilt("pulls")
    }
    /// Where the pull request stands in the repository's merge queue, when it is in it.
    async fn queue_state(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Option<QueueView>> {
        let _ = (actor, repo, number);
        Ok(None)
    }
    /// Puts the pull request in the repository's merge queue.
    async fn queue_add(&self, actor: &Actor, repo: RepoId, number: i64, method: MergeMethod, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, number, method, client);
        unbuilt("pulls")
    }
    async fn queue_remove(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, number, client);
        unbuilt("pulls")
    }
    /// `merge` with a custom title and message, a method that is checked against the repo
    /// settings, and an optional head sha to guard against a push that landed after review.
    async fn merge_with(&self, actor: &Actor, repo: RepoId, number: i64, options: MergeOptions, client: &ClientInfo) -> Result<PullView> {
        let _ = (actor, repo, number, options, client);
        unbuilt("pulls")
    }
}

#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait Reviews: Send + Sync {
    async fn list(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Review>>;
    async fn submit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        state: ReviewState,
        body: &str,
        comments: Vec<NewReviewComment>,
        client: &ClientInfo,
    ) -> Result<Review>;
    async fn request_review(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        client: &ClientInfo,
    ) -> Result<()>;
    async fn resolve_thread(&self, actor: &Actor, repo: RepoId, thread_id: i64, resolved: bool, client: &ClientInfo) -> Result<()>;

    // ------- added by the collab stream. The defaults fail closed.

    async fn views(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ReviewView>> {
        let _ = (actor, repo, number);
        unbuilt("reviews")
    }
    /// Every inline thread, with its comments. Pending comments are shown only to their author.
    async fn threads(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ThreadView>> {
        let _ = (actor, repo, number);
        unbuilt("reviews")
    }
    /// One inline comment. With `review` it joins that pending review, without it the comment
    /// is posted straight away. A comment with `in_reply_to` joins that comment's thread.
    async fn add_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        comment: NewReviewComment,
        review: Option<ReviewId>,
        client: &ClientInfo,
    ) -> Result<ReviewCommentView> {
        let _ = (actor, repo, number, comment, review, client);
        unbuilt("reviews")
    }
    async fn comment(&self, actor: &Actor, repo: RepoId, comment: CommentId) -> Result<ReviewCommentView> {
        let _ = (actor, repo, comment);
        unbuilt("reviews")
    }
    async fn edit_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        comment: CommentId,
        body: &str,
        client: &ClientInfo,
    ) -> Result<ReviewCommentView> {
        let _ = (actor, repo, comment, body, client);
        unbuilt("reviews")
    }
    async fn delete_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, comment, client);
        unbuilt("reviews")
    }
    /// Turns the caller's pending review into a submitted one.
    async fn submit_pending(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        state: ReviewState,
        body: &str,
        client: &ClientInfo,
    ) -> Result<Review> {
        let _ = (actor, repo, number, review, state, body, client);
        unbuilt("reviews")
    }
    async fn delete_pending(&self, actor: &Actor, repo: RepoId, number: i64, review: ReviewId, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, number, review, client);
        unbuilt("reviews")
    }
    /// Maintainers (write access) only. The review stops counting.
    async fn dismiss(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        message: &str,
        client: &ClientInfo,
    ) -> Result<Review> {
        let _ = (actor, repo, number, review, message, client);
        unbuilt("reviews")
    }
    async fn requested(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<RequestedReviewers> {
        let _ = (actor, repo, number);
        unbuilt("reviews")
    }
    async fn remove_request(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        client: &ClientInfo,
    ) -> Result<()> {
        let _ = (actor, repo, number, users, teams, client);
        unbuilt("reviews")
    }
    /// Commits a `suggestion` block to the head branch and returns the new head sha.
    async fn apply_suggestion(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<String> {
        let _ = (actor, repo, comment, client);
        unbuilt("reviews")
    }
}

#[async_trait]
pub trait Releases: Send + Sync {
    async fn list(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Release>>;
    async fn get_by_tag(&self, actor: &Actor, repo: RepoId, tag: &str) -> Result<Release>;
    async fn latest(&self, actor: &Actor, repo: RepoId) -> Result<Release>;

    // ------- added by the collab stream. The defaults fail closed.

    /// Drafts are listed only to people with write access.
    async fn views(&self, actor: &Actor, repo: RepoId) -> Result<Vec<ReleaseView>> {
        let _ = (actor, repo);
        unbuilt("releases")
    }
    async fn view(&self, actor: &Actor, repo: RepoId, id: ReleaseId) -> Result<ReleaseView> {
        let _ = (actor, repo, id);
        unbuilt("releases")
    }
    async fn view_by_tag(&self, actor: &Actor, repo: RepoId, tag: &str) -> Result<ReleaseView> {
        let _ = (actor, repo, tag);
        unbuilt("releases")
    }
    async fn latest_view(&self, actor: &Actor, repo: RepoId) -> Result<ReleaseView> {
        let _ = (actor, repo);
        unbuilt("releases")
    }
    async fn create(&self, actor: &Actor, repo: RepoId, draft: ReleaseDraft, client: &ClientInfo) -> Result<ReleaseView> {
        let _ = (actor, repo, draft, client);
        unbuilt("releases")
    }
    async fn update(&self, actor: &Actor, repo: RepoId, id: ReleaseId, edit: ReleaseEdit, client: &ClientInfo) -> Result<ReleaseView> {
        let _ = (actor, repo, id, edit, client);
        unbuilt("releases")
    }
    async fn delete(&self, actor: &Actor, repo: RepoId, id: ReleaseId, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, id, client);
        unbuilt("releases")
    }
    /// Pull requests merged since the previous tag, grouped by label.
    async fn generate_notes(
        &self,
        actor: &Actor,
        repo: RepoId,
        tag: &str,
        previous_tag: Option<&str>,
        target: Option<&str>,
    ) -> Result<GeneratedNotes> {
        let _ = (actor, repo, tag, previous_tag, target);
        unbuilt("releases")
    }
    #[allow(clippy::too_many_arguments)]
    async fn upload_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        release: ReleaseId,
        name: &str,
        label: Option<&str>,
        content_type: &str,
        body: BoxStream<'static, std::io::Result<Bytes>>,
        client: &ClientInfo,
    ) -> Result<AssetInfo> {
        let _ = (actor, repo, release, name, label, content_type, body, client);
        unbuilt("releases")
    }
    async fn asset(&self, actor: &Actor, repo: RepoId, asset: i64) -> Result<AssetInfo> {
        let _ = (actor, repo, asset);
        unbuilt("releases")
    }
    async fn delete_asset(&self, actor: &Actor, repo: RepoId, asset: i64, client: &ClientInfo) -> Result<()> {
        let _ = (actor, repo, asset, client);
        unbuilt("releases")
    }
    /// Renames an asset, sets its label, or both. A name another asset of the release has is a 422.
    async fn update_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        asset: i64,
        name: Option<&str>,
        label: Option<&str>,
        client: &ClientInfo,
    ) -> Result<AssetInfo> {
        let _ = (actor, repo, asset, name, label, client);
        unbuilt("releases")
    }
    /// Counts a download and streams the file. `tag` and `name` are the parts of
    /// `/{owner}/{repo}/releases/download/{tag}/{name}`.
    async fn open_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        tag: &str,
        name: &str,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        let _ = (actor, repo, tag, name);
        unbuilt("releases")
    }
    /// Same as `open_asset` by id, for the API.
    async fn open_asset_by_id(
        &self,
        actor: &Actor,
        repo: RepoId,
        asset: i64,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        let _ = (actor, repo, asset);
        unbuilt("releases")
    }
}

#[async_trait]
pub trait Search: Send + Sync {
    async fn repos(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>>;
    async fn issues(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>>;
    async fn users(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>>;
    async fn code(&self, actor: &Actor, repo: RepoId, query: &str, limit: u32) -> Result<Vec<SearchHit>>;
    /// Code on the default branch of the repositories the actor can read with an account, most
    /// recently pushed first, up to `limit` lines. Never anything signed out (ch-4pn D13).
    async fn code_all(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<CodeHit>> {
        let _ = (actor, query, limit);
        Err(Error::unavailable("code search across repositories is not built yet"))
    }
}

// ---------------------------------------------------------------- mail (ch-notify)

/// Outgoing email. Off unless `[mail.smtp]` is configured: then `enabled` is false and nothing
/// is ever sent.
#[async_trait]
pub trait Mail: Send + Sync {
    fn enabled(&self) -> bool;
    /// Sends one plain-text message. The caller has already decided the recipient may get it.
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<()>;
}

// ---------------------------------------------------------------- delivery (ch-notify)

#[async_trait]
pub trait Notifications: Send + Sync {
    async fn inbox(&self, actor: &Actor, unread_only: bool, page: u32, per_page: u32) -> Result<Vec<Notification>>;
    async fn unread_count(&self, actor: &Actor) -> Result<i64>;
    async fn mark_read(&self, actor: &Actor, id: Option<NotificationId>) -> Result<()>;
    async fn subscribe(&self, actor: &Actor, repo: RepoId, level: &str) -> Result<()>;
    /// `all`, `participating` or `ignore`. `participating` when the person never chose.
    async fn watch_level(&self, actor: &Actor, repo: RepoId) -> Result<String>;
    /// Takes the thread out of the inbox without deleting it.
    async fn mark_done(&self, actor: &Actor, id: NotificationId) -> Result<()>;
    /// Unsubscribes from, or subscribes to, one issue or pull request thread.
    async fn set_thread_subscription(&self, actor: &Actor, subject_type: &str, subject_id: i64, subscribed: bool) -> Result<()>;
    /// Whether the signed-in person gets notification emails (ch-0km). Off until they turn it on.
    async fn email_on(&self, actor: &Actor) -> Result<bool> {
        let _ = actor;
        Ok(false)
    }
    /// Turns notification emails on or off for the signed-in person, a person only, never an app
    /// or a token without the user scope.
    async fn set_email_on(&self, actor: &Actor, on: bool) -> Result<()> {
        let _ = (actor, on);
        Err(Error::unavailable("notification emails are not built yet"))
    }
}

#[async_trait]
pub trait Webhooks: Send + Sync {
    async fn list(&self, actor: &Actor, repo: Option<RepoId>, org: Option<OrgId>) -> Result<Vec<Webhook>>;
    async fn create(&self, actor: &Actor, new: NewWebhook, client: &ClientInfo) -> Result<Webhook>;
    async fn delete(&self, actor: &Actor, id: WebhookId, client: &ClientInfo) -> Result<()>;
    async fn redeliver(&self, actor: &Actor, delivery_id: i64) -> Result<()>;
    async fn get(&self, actor: &Actor, id: WebhookId) -> Result<Webhook>;
    async fn update(&self, actor: &Actor, id: WebhookId, change: WebhookChange, client: &ClientInfo) -> Result<Webhook>;
    /// Newest first. Includes the request and response so the settings page can show them.
    async fn deliveries(&self, actor: &Actor, id: WebhookId, limit: u32) -> Result<Vec<WebhookDelivery>>;
    /// Queues a `ping` event for the hook.
    async fn ping(&self, actor: &Actor, id: WebhookId, client: &ClientInfo) -> Result<()>;
}

// ---------------------------------------------------------------- CI (ch-ci)

#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait Checks: Send + Sync {
    async fn check_runs_for_sha(&self, repo: RepoId, sha: &str) -> Result<Vec<CheckRun>>;
    async fn statuses_for_sha(&self, repo: RepoId, sha: &str) -> Result<Vec<CommitStatus>>;
    async fn combined(&self, repo: RepoId, sha: &str) -> Result<CombinedState>;
    async fn create_status(
        &self,
        actor: &Actor,
        repo: RepoId,
        sha: &str,
        state: &str,
        context: &str,
        description: Option<&str>,
        target_url: Option<&str>,
    ) -> Result<CommitStatus>;
    /// True when every context in `contexts` has a passing check run or status on `sha`.
    async fn required_satisfied(&self, repo: RepoId, sha: &str, contexts: &[String]) -> Result<bool>;
    /// What `actor` may see of a commit's checks (ch-4pn.12): every latest check run and status
    /// from Read up; below Read only those recorded at a tier that covers the viewer; nothing
    /// without View. Pages and tools that show checks to a person use this, never the unfiltered
    /// reads above. The default shows nothing, so a service that does not implement it fails
    /// closed.
    async fn seen_for_sha(&self, _actor: &Actor, _repo: RepoId, _sha: &str) -> Result<crate::model::SeenChecks> {
        Ok(crate::model::SeenChecks { runs: Vec::new(), statuses: Vec::new(), state: None })
    }
}

#[async_trait]
pub trait Ci: Send + Sync {
    async fn runs(&self, actor: &Actor, repo: RepoId, page: u32, per_page: u32) -> Result<Vec<WorkflowRun>>;
    async fn get_run(&self, actor: &Actor, repo: RepoId, run: RunId) -> Result<WorkflowRun>;
    async fn dispatch(
        &self,
        actor: &Actor,
        repo: RepoId,
        workflow: &str,
        git_ref: &str,
        inputs: serde_json::Value,
        client: &ClientInfo,
    ) -> Result<RunId>;
    async fn cancel(&self, actor: &Actor, repo: RepoId, run: RunId, client: &ClientInfo) -> Result<()>;
    async fn rerun(&self, actor: &Actor, repo: RepoId, run: RunId, failed_only: bool, client: &ClientInfo) -> Result<RunId>;
    async fn log_range(&self, actor: &Actor, repo: RepoId, job: JobId, range: LogRange) -> Result<Bytes>;
    /// Why a run is waiting for a person to approve it, when it is.
    async fn run_hold_reason(&self, _actor: &Actor, _repo: RepoId, _run: RunId) -> Result<Option<String>> {
        Ok(None)
    }
    /// Why a run failed before any job: its workflow file is broken or was refused.
    async fn run_problem(&self, _actor: &Actor, _repo: RepoId, _run: RunId) -> Result<Option<String>> {
        Ok(None)
    }
    /// The status badge of the workflow file named `file` (`ci.yml`): the latest finished run on
    /// `branch` (the default branch when `None`), from `event` when given. Needs `Read`.
    async fn badge(
        &self,
        _actor: &Actor,
        _repo: RepoId,
        _file: &str,
        _branch: Option<&str>,
        _event: Option<&str>,
    ) -> Result<WorkflowBadge> {
        Err(Error::unavailable("CI is not available"))
    }
    /// The size of a job's log in bytes, so a reader can start at the end.
    async fn log_size(&self, _actor: &Actor, _repo: RepoId, _job: JobId) -> Result<u64> {
        Err(Error::unavailable("CI is not available"))
    }
    /// Site admins only. Single use, 1 hour by default. `repo` limits the runner to one repo.
    async fn create_runner_token(&self, actor: &Actor, repo: Option<RepoId>, client: &ClientInfo) -> Result<RunnerRegistrationToken>;

    // The methods below have default bodies so a fake `Ci` written against the first version of
    // this trait keeps compiling. They fail closed, like `NotBuilt`.

    /// Whether workflows may run for this repository, and if not, why.
    async fn availability(&self, _repo: RepoId) -> Result<CiAvailability> {
        Err(Error::unavailable("CI is not available"))
    }
    /// The workflow files on the default branch (or `git_ref`), with validation errors.
    async fn workflows(&self, _actor: &Actor, _repo: RepoId, _git_ref: Option<&str>) -> Result<Vec<WorkflowFile>> {
        Err(Error::unavailable("CI is not available"))
    }
    async fn jobs(&self, _actor: &Actor, _repo: RepoId, _run: RunId) -> Result<Vec<JobInfo>> {
        Err(Error::unavailable("CI is not available"))
    }
    async fn job(&self, _actor: &Actor, _repo: RepoId, _job: JobId) -> Result<JobInfo> {
        Err(Error::unavailable("CI is not available"))
    }
    async fn artifacts(&self, _actor: &Actor, _repo: RepoId, _run: RunId) -> Result<Vec<ArtifactInfo>> {
        Err(Error::unavailable("CI is not available"))
    }
    /// A person with write access lets a held run go ahead (one a connected app started).
    async fn approve_run(&self, _actor: &Actor, _repo: RepoId, _run: RunId, _client: &ClientInfo) -> Result<()> {
        Err(Error::unavailable("CI is not available"))
    }
    /// The latest commit of pull request `number` when it comes from a fork and waits for
    /// someone with write access to approve it. No run exists for it until then.
    async fn pending_fork_approval(&self, _actor: &Actor, _repo: RepoId, _number: i64) -> Result<Option<String>> {
        Ok(None)
    }
    /// Approves commit `sha` of pull request `number` from a fork and starts its runs. Needs a
    /// person with write access; `sha` must be the pull request's latest commit.
    async fn approve_fork_commit(
        &self,
        _actor: &Actor,
        _repo: RepoId,
        _number: i64,
        _sha: &str,
        _client: &ClientInfo,
    ) -> Result<Vec<RunId>> {
        Err(Error::unavailable("CI is not available"))
    }
    /// Site admins only.
    async fn runners(&self, _actor: &Actor) -> Result<Vec<RunnerSummary>> {
        Err(Error::unavailable("CI is not available"))
    }
}

// ---------------------------------------------------------------- storage (ch-db)

#[async_trait]
pub trait Audit: Send + Sync {
    /// Writes in the caller's transaction, so the record exists exactly when the change does.
    async fn record(&self, tx: &mut DbTx<'_>, event: AuditEvent) -> Result<()>;
    /// For events with no surrounding change, such as a failed sign-in.
    async fn record_standalone(&self, event: AuditEvent) -> Result<()>;
    /// Site admins only.
    async fn query(&self, actor: &Actor, filter: AuditFilter) -> Result<Vec<StoredAudit>>;
}

#[async_trait]
pub trait EventBus: Send + Sync {
    /// Writes the event in the caller's transaction. Emitters are serialised by a
    /// transaction-scoped advisory lock so ids commit in order and a cursor never skips one.
    async fn emit(&self, tx: &mut DbTx<'_>, event: DomainEvent) -> Result<i64>;
    /// Events after the consumer's cursor, oldest first, at most `limit`.
    async fn poll(&self, consumer: &str, limit: i64) -> Result<Vec<StoredEvent>>;
    /// Moves the consumer's cursor forward to `id`. Never backwards.
    async fn ack(&self, consumer: &str, id: i64) -> Result<()>;
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Streams the body into a temp file, hashes it, and moves it to `sha256/aa/bb/<hash>`.
    /// Returns the hex digest and size. `max_bytes` stops an oversized upload early.
    async fn put_stream(&self, body: BoxStream<'static, std::io::Result<Bytes>>, max_bytes: u64) -> Result<BlobRef>;
    async fn put_bytes(&self, data: Bytes) -> Result<BlobRef>;
    async fn get_stream(&self, sha256: &str) -> Result<BoxStream<'static, std::io::Result<Bytes>>>;
    async fn size(&self, sha256: &str) -> Result<Option<u64>>;
    async fn delete(&self, sha256: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub sha256: String,
    pub size: u64,
}
