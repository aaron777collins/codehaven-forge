//! The trait implementations: `Repos` and `GitService` on [`GitCore`]. Each method is a thin
//! delegate to the code in `repos`, `read` and `write`, so permission rules live in one place.

use crate::core::GitCore;
use async_trait::async_trait;
use bytes::Bytes;
use ch_core::model::*;
use ch_core::services::{ArchiveFormat, GitService, Repos};
use ch_core::{Actor, Permission, RepoId, Result, UserId};
use futures::stream::BoxStream;

#[async_trait]
impl Repos for GitCore {
    async fn create(&self, actor: &Actor, new: NewRepo, client: &ClientInfo) -> Result<Repo> {
        self.do_create_repo(actor, new, client).await
    }
    async fn get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo> {
        self.do_get_by_name(actor, owner, name).await
    }
    async fn get(&self, actor: &Actor, id: RepoId) -> Result<Repo> {
        self.do_get(actor, id).await
    }
    async fn list_for_owner(&self, actor: &Actor, owner: UserId) -> Result<Vec<Repo>> {
        self.do_list_for_owner(actor, owner).await
    }
    async fn rename(&self, actor: &Actor, id: RepoId, new_name: &str, client: &ClientInfo) -> Result<Repo> {
        self.do_rename(actor, id, new_name, client).await
    }
    async fn transfer(&self, actor: &Actor, id: RepoId, new_owner: UserId, client: &ClientInfo) -> Result<Repo> {
        self.do_transfer(actor, id, new_owner, client).await
    }
    async fn fork(&self, actor: &Actor, id: RepoId, into: UserId, client: &ClientInfo) -> Result<Repo> {
        self.do_fork(actor, id, into, None, client).await
    }
    async fn forks(&self, actor: &Actor, id: RepoId) -> Result<Vec<Repo>> {
        self.do_forks(actor, id).await
    }
    async fn social(&self, actor: &Actor, id: RepoId) -> Result<RepoSocial> {
        self.do_social(actor, id).await
    }
    async fn set_star(&self, actor: &Actor, id: RepoId, starred: bool) -> Result<()> {
        self.do_set_star(actor, id, starred).await
    }
    async fn delete(&self, actor: &Actor, id: RepoId, client: &ClientInfo) -> Result<()> {
        self.do_delete(actor, id, client).await
    }
    async fn collaborators(&self, actor: &Actor, id: RepoId) -> Result<Vec<Collaborator>> {
        self.do_collaborators(actor, id).await
    }
    async fn set_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, permission: Permission, client: &ClientInfo) -> Result<()> {
        self.do_set_collaborator(actor, id, user, permission, client).await
    }
    async fn remove_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, client: &ClientInfo) -> Result<()> {
        self.do_remove_collaborator(actor, id, user, client).await
    }
    async fn protections(&self, actor: &Actor, id: RepoId) -> Result<Vec<BranchProtection>> {
        self.do_protections(actor, id).await
    }
    async fn set_protection(&self, actor: &Actor, id: RepoId, protection: BranchProtection, client: &ClientInfo) -> Result<()> {
        self.do_set_protection(actor, id, protection, client).await
    }
    async fn remove_protection(&self, actor: &Actor, id: RepoId, pattern: &str, client: &ClientInfo) -> Result<()> {
        self.do_remove_protection(actor, id, pattern, client).await
    }
    async fn update(&self, actor: &Actor, id: RepoId, update: ch_core::web_contract::RepoUpdate, client: &ClientInfo) -> Result<Repo> {
        // The web form's smaller update is the same thing as the settings update with the
        // rest left alone.
        let settings = RepoSettingsUpdate {
            description: update.description,
            visibility: update.visibility,
            default_branch: update.default_branch,
            archived: update.archived,
            topics: update.topics,
            ..Default::default()
        };
        self.do_update_settings(actor, id, settings, client).await
    }
    async fn update_settings(&self, actor: &Actor, id: RepoId, update: RepoSettingsUpdate, client: &ClientInfo) -> Result<Repo> {
        self.do_update_settings(actor, id, update, client).await
    }
    async fn list_accessible(&self, actor: &Actor, page: u32, per_page: u32) -> Result<Vec<Repo>> {
        self.do_list_accessible(actor, page, per_page).await
    }
    async fn set_tier(&self, actor: &Actor, id: RepoId, to: Visibility, client: &ClientInfo) -> Result<Repo> {
        self.do_set_tier(actor, id, to, client).await
    }
    async fn fork_as(&self, actor: &Actor, id: RepoId, into: UserId, visibility: Visibility, client: &ClientInfo) -> Result<Repo> {
        self.do_fork_as(actor, id, into, None, visibility, client).await
    }
    async fn set_contribution_policy(&self, actor: &Actor, id: RepoId, policy: ContributionPolicy, client: &ClientInfo) -> Result<Repo> {
        self.do_set_contribution_policy(actor, id, policy, client).await
    }
    async fn public_repos_allowed(&self) -> Result<bool> {
        self.public_allowed().await
    }
    async fn publish_summary(&self, actor: &Actor, id: RepoId) -> Result<PublishSummary> {
        self.do_publish_summary(actor, id).await
    }
    async fn set_public_repos_allowed(&self, actor: &Actor, on: bool, client: &ClientInfo) -> Result<u64> {
        self.do_set_public_switch(actor, on, client).await
    }
    async fn workspace_settings(&self) -> Result<WorkspaceSettings> {
        self.do_workspace_settings().await
    }
    async fn set_workspace_settings(&self, actor: &Actor, settings: WorkspaceSettings, client: &ClientInfo) -> Result<()> {
        self.do_set_workspace_settings(actor, settings, client).await
    }
}

/// Reads take no actor: the caller has already checked access to the repository. Writes check
/// it again, because they are the dangerous half.
#[async_trait]
impl GitService for GitCore {
    async fn refs(&self, repo: RepoId, prefix: Option<&str>) -> Result<Vec<GitRef>> {
        self.read_refs(repo, prefix).await
    }
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        self.read_resolve(repo, rev).await
    }
    async fn tree(&self, repo: RepoId, rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
        self.read_tree(repo, rev, path).await
    }
    async fn blob(&self, repo: RepoId, rev: &str, path: &str, max_bytes: u64) -> Result<Blob> {
        self.read_blob(repo, rev, path, max_bytes).await
    }
    async fn log(&self, repo: RepoId, rev: &str, path: Option<&str>, skip: u32, limit: u32) -> Result<Vec<CommitInfo>> {
        self.read_log(repo, rev, path, skip, limit).await
    }
    async fn commit(&self, repo: RepoId, rev: &str) -> Result<CommitInfo> {
        self.read_commit(repo, rev).await
    }
    async fn compare(&self, repo: RepoId, base: &str, head: &str) -> Result<Compare> {
        self.read_compare(repo, base, head).await
    }
    async fn compare_across(&self, repo: RepoId, base: &str, head_repo: RepoId, head: &str) -> Result<Compare> {
        self.read_compare_across(repo, base, head_repo, head).await
    }
    async fn diff(&self, repo: RepoId, base: &str, head: &str) -> Result<Diff> {
        self.read_diff(repo, base, head).await
    }
    async fn grep(&self, repo: RepoId, rev: &str, pattern: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.read_grep(repo, rev, pattern, limit).await
    }
    async fn archive(&self, repo: RepoId, rev: &str, format: ArchiveFormat) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        self.read_archive(repo, rev, format).await
    }
    async fn commit_file_change(&self, actor: &Actor, req: CommitFileChange, client: &ClientInfo) -> Result<String> {
        self.do_commit_file_change(actor, req, client).await
    }
    async fn create_ref(&self, actor: &Actor, repo: RepoId, name: &str, sha: &str, client: &ClientInfo) -> Result<()> {
        self.do_create_ref(actor, repo, name, sha, client).await
    }
    async fn delete_ref(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        self.do_delete_ref(actor, repo, name, client).await
    }
    async fn test_merge(&self, repo: RepoId, base_ref: &str, head_repo: RepoId, head_ref: &str) -> Result<MergeableState> {
        self.do_test_merge(repo, base_ref, head_repo, head_ref).await
    }
    async fn merge(&self, actor: &Actor, req: MergeRequest, client: &ClientInfo) -> Result<MergeOutcome> {
        self.do_merge(actor, req, client).await
    }
    async fn workspace_branch_head(&self, repo: RepoId, branch: &str) -> Result<Option<String>> {
        self.ws_branch_head(repo, branch).await
    }
    async fn workspace_is_ancestor(&self, repo: RepoId, sha: &str, head: &str) -> Result<bool> {
        self.ws_is_ancestor(repo, sha, head).await
    }
    async fn workspace_tree(&self, repo: RepoId, sha: &str, limits: WorkspaceTreeLimits) -> Result<WorkspaceTree> {
        self.ws_tree(repo, sha, limits).await
    }
    async fn workspace_blob(&self, repo: RepoId, sha: &str, path: &[u8], max_bytes: u64) -> Result<WorkspaceBlob> {
        self.ws_blob(repo, sha, path, max_bytes).await
    }
    async fn workspace_search(&self, repo: RepoId, sha: &str, text: &str, limits: WorkspaceSearchLimits) -> Result<WorkspaceSearch> {
        self.ws_search(repo, sha, text, limits).await
    }
    async fn workspace_snapshot(
        &self,
        repo: RepoId,
        sha: &str,
        prefix: &[u8],
        limits: WorkspaceSnapshotLimits,
    ) -> Result<WorkspaceSnapshot> {
        self.ws_snapshot(repo, sha, prefix, limits).await
    }
    async fn workspace_direct_commit_allowed(&self, actor: &Actor, repo: RepoId, branch: &str) -> Result<bool> {
        self.ws_direct_commit_allowed(actor, repo, branch).await
    }
    async fn update_ref(&self, actor: &Actor, repo: RepoId, name: &str, sha: &str, force: bool, client: &ClientInfo) -> Result<()> {
        self.do_update_ref(actor, repo, name, sha, force, client).await
    }
    async fn blame(&self, repo: RepoId, rev: &str, path: &str) -> Result<Blame> {
        self.read_blame(repo, rev, path).await
    }
    async fn reachable(&self, repo: RepoId, sha: &str) -> Result<bool> {
        self.read_reachable(repo, sha).await
    }
    async fn commit_diff(&self, repo: RepoId, sha: &str) -> Result<Diff> {
        Ok((*GitCore::commit_diff(self, repo, sha).await?).clone())
    }
}
