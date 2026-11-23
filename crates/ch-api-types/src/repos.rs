//! Repositories, branches, commits, refs and file contents.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::{Permissions, ShaRef, SimpleUser};

/// `GET /repos/{owner}/{repo}`: a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub full_name: String,
    pub owner: SimpleUser,
    pub private: bool,
    pub html_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fork: bool,
    pub url: String,
    pub clone_url: String,
    pub ssh_url: String,
    pub default_branch: String,
    /// `public`, `internal` or `private`.
    pub visibility: String,
    /// `everyone` or `grants`.
    #[serde(default = "default_everyone")]
    pub contribution_policy: String,
    pub archived: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub topics: Vec<String>,
    pub has_issues: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<DateTime<Utc>>,
    /// Size of the git store in kilobytes.
    pub size: i64,
    pub stargazers_count: i64,
    pub watchers_count: i64,
    pub forks_count: i64,
    pub open_issues_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// The repository this one was forked from, when `fork` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<Repository>>,
    /// Codehaven extension: the repository has no commits yet.
    #[serde(default)]
    pub empty: bool,
    #[serde(default = "default_true")]
    pub allow_squash_merge: bool,
    #[serde(default = "default_true")]
    pub allow_merge_commit: bool,
    #[serde(default = "default_true")]
    pub allow_rebase_merge: bool,
    #[serde(default)]
    pub delete_branch_on_merge: bool,
}

fn default_true() -> bool {
    true
}

fn default_everyone() -> String {
    "everyone".into()
}

/// `POST /user/repos` and `POST /orgs/{org}/repos`: create a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRepo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    /// `private`, `internal` or `public`. When present it decides, whatever `private` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_init: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

/// `PATCH /repos/{owner}/{repo}`: edit a repository. Absent fields stay unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRepo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    /// `private`, `internal` or `public`. When present it decides, whatever `private` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// `everyone` or `grants`: who may open issues, comment and send pull requests without access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_squash_merge: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_merge_commit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_rebase_merge: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_branch_on_merge: Option<bool>,
}

/// `GET /repos/{owner}/{repo}/topics` and `PUT /repos/{owner}/{repo}/topics`: topic names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topics {
    #[serde(default)]
    pub names: Vec<String>,
}

/// The commit a branch or tag points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCommit {
    pub sha: String,
    pub url: String,
}

/// `GET /repos/{owner}/{repo}/branches/{branch}`: a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    pub commit: BranchCommit,
    pub protected: bool,
}

/// Status checks that must pass before merging into a protected branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredStatusChecks {
    /// Require branches to be up to date before merging.
    pub strict: bool,
    #[serde(default)]
    pub contexts: Vec<String>,
}

/// Review requirements that must be met before merging into a protected branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredReviews {
    pub required_approving_review_count: u32,
    pub dismiss_stale_reviews: bool,
    pub require_code_owner_reviews: bool,
}

/// `GET /repos/{owner}/{repo}/branches/{branch}/protection`: the rules on a protected branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchProtection {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_status_checks: Option<RequiredStatusChecks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_pull_request_reviews: Option<RequiredReviews>,
    pub enforce_admins: bool,
    pub required_conversation_resolution: bool,
    pub allow_force_pushes: bool,
    pub allow_deletions: bool,
    pub allow_direct_push: bool,
}

/// `PUT /repos/{owner}/{repo}/branches/{branch}/protection`: replace the rules on a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateBranchProtection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_status_checks: Option<RequiredStatusChecks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_pull_request_reviews: Option<RequiredReviews>,
    pub enforce_admins: bool,
    pub required_conversation_resolution: bool,
    pub allow_force_pushes: bool,
    pub allow_deletions: bool,
    pub allow_direct_push: bool,
}

/// `GET /repos/{owner}/{repo}/tags`: a tag in a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub commit: BranchCommit,
    pub zipball_url: String,
    pub tarball_url: String,
}

/// Who authored or committed a git change, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
    pub date: DateTime<Utc>,
}

/// The git-level part of a commit, as stored in the object database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetail {
    pub author: GitIdentity,
    pub committer: GitIdentity,
    pub message: String,
    pub tree: ShaRef,
    pub comment_count: u32,
}

/// Line counts of a commit, present when diffs are computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitStats {
    pub additions: i64,
    pub deletions: i64,
    pub total: i64,
}

/// One file changed by a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFile {
    pub filename: String,
    /// `added`, `removed`, `modified`, `renamed` or `changed`.
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_filename: Option<String>,
    pub sha: String,
}

/// `GET /repos/{owner}/{repo}/commits/{ref}`: a commit with its git metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub url: String,
    pub html_url: String,
    pub commit: CommitDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<SimpleUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committer: Option<SimpleUser>,
    #[serde(default)]
    pub parents: Vec<ShaRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<CommitStats>,
    #[serde(default)]
    pub files: Vec<CommitFile>,
}

/// How two refs stand relative to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompareStatus {
    Ahead,
    Behind,
    Diverged,
    Identical,
}

/// `GET /repos/{owner}/{repo}/compare/{base}...{head}`: the diff between two refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compare {
    pub url: String,
    pub html_url: String,
    pub status: CompareStatus,
    pub ahead_by: i64,
    pub behind_by: i64,
    pub total_commits: i64,
    #[serde(default)]
    pub commits: Vec<Commit>,
    #[serde(default)]
    pub files: Vec<CommitFile>,
    pub merge_base_commit: Commit,
}

/// What a content path holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentKind {
    File,
    Dir,
    Symlink,
    Submodule,
}

/// `GET /repos/{owner}/{repo}/contents/{path}`: a file, or one entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentEntry {
    pub name: String,
    pub path: String,
    pub sha: String,
    pub size: i64,
    #[serde(rename = "type")]
    pub kind: ContentKind,
    pub url: String,
    pub html_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// `PUT /repos/{owner}/{repo}/contents/{path}`: create or replace a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutContent {
    pub message: String,
    /// The new file body, base64 encoded.
    pub content: String,
    /// The blob being replaced. Required when updating, absent when creating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committer: Option<GitIdentity>,
}

/// The response to `PUT /repos/{owner}/{repo}/contents/{path}`: the stored file and its commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutContentResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentEntry>,
    pub commit: Commit,
}

/// `DELETE /repos/{owner}/{repo}/contents/{path}`: delete a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteContent {
    pub message: String,
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The git object a ref points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRefObject {
    pub sha: String,
    /// `commit`, `tag`, `tree` or `blob`.
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
}

/// `GET /repos/{owner}/{repo}/git/ref/{ref}`: a git reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub url: String,
    pub object: GitRefObject,
}

/// `POST /repos/{owner}/{repo}/git/refs`: create a git reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRef {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
}

/// `PATCH /repos/{owner}/{repo}/git/refs/{ref}`: move a git reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRef {
    pub sha: String,
    #[serde(default)]
    pub force: bool,
}

/// `GET /repos/{owner}/{repo}/readme`: the README file, same shape as any content file.
pub type Readme = ContentEntry;

/// `GET /repos/{owner}/{repo}/collaborators`: a user with their access to a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collaborator {
    #[serde(flatten)]
    pub user: SimpleUser,
    pub permissions: Permissions,
    pub role_name: String,
}

/// `PUT /repos/{owner}/{repo}/collaborators/{user}`: invite a collaborator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddCollaborator {
    pub permission: String,
}

/// `POST /repos/{owner}/{repo}/forks`: fork a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, id: i64) -> Repository {
        Repository {
            id,
            name: name.into(),
            full_name: format!("alice/{name}"),
            owner: crate::user("alice", 1),
            private: false,
            html_url: format!("https://code.example.com/alice/{name}"),
            description: Some("a self-hosted forge".into()),
            fork: false,
            url: format!("https://code.example.com/api/v3/repos/alice/{name}"),
            clone_url: format!("https://code.example.com/alice/{name}.git"),
            ssh_url: format!("git@code.example.com:alice/{name}.git"),
            default_branch: "main".into(),
            visibility: "public".into(),
            contribution_policy: "everyone".into(),
            archived: false,
            disabled: false,
            topics: vec!["rust".into(), "forge".into()],
            has_issues: true,
            created_at: crate::t0(),
            updated_at: crate::t0(),
            pushed_at: Some(crate::t0()),
            size: 42,
            stargazers_count: 1,
            watchers_count: 1,
            forks_count: 0,
            open_issues_count: 2,
            permissions: Some(Permissions { admin: false, maintain: false, push: true, triage: true, pull: true }),
            parent: None,
            empty: false,
            allow_squash_merge: true,
            allow_merge_commit: true,
            allow_rebase_merge: false,
            delete_branch_on_merge: true,
        }
    }

    fn file() -> CommitFile {
        CommitFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 10,
            deletions: 2,
            changes: 12,
            patch: Some("@@ -1,2 +1,4 @@\n pub mod repos;".into()),
            previous_filename: None,
            sha: "b10b1234".into(),
        }
    }

    fn commit(sha: &str) -> Commit {
        let identity = GitIdentity { name: "Alice".into(), email: "alice@example.com".into(), date: crate::t0() };
        Commit {
            sha: sha.into(),
            url: format!("https://code.example.com/api/v3/repos/alice/codehaven/commits/{sha}"),
            html_url: format!("https://code.example.com/alice/codehaven/commit/{sha}"),
            commit: CommitDetail {
                author: identity.clone(),
                committer: identity,
                message: "add branch protection".into(),
                tree: ShaRef { sha: "0d1a5e5f".into(), url: None },
                comment_count: 1,
            },
            author: Some(crate::user("alice", 1)),
            committer: Some(crate::user("alice", 1)),
            parents: vec![ShaRef { sha: "a11ce000".into(), url: None }],
            stats: Some(CommitStats { additions: 10, deletions: 2, total: 12 }),
            files: vec![file()],
        }
    }

    #[test]
    fn repository_roundtrip() {
        let mut value = repo("codehaven", 10);
        value.parent = Some(Box::new(repo("upstream", 11)));
        crate::roundtrip("repos_repository", &value);
    }

    #[test]
    fn create_repo_roundtrip() {
        let value = CreateRepo {
            name: "codehaven".into(),
            description: Some("a self-hosted forge".into()),
            private: Some(false),
            visibility: None,
            auto_init: Some(true),
            default_branch: Some("main".into()),
        };
        crate::roundtrip("repos_create_repo", &value);
    }

    #[test]
    fn update_repo_roundtrip() {
        let value = UpdateRepo {
            name: None,
            description: Some("a self-hosted forge".into()),
            private: Some(true),
            visibility: None,
            contribution_policy: None,
            default_branch: None,
            archived: Some(false),
            allow_squash_merge: None,
            allow_merge_commit: Some(false),
            allow_rebase_merge: None,
            delete_branch_on_merge: Some(true),
        };
        crate::roundtrip("repos_update_repo", &value);
    }

    #[test]
    fn topics_roundtrip() {
        let value = Topics { names: vec!["rust".into(), "forge".into()] };
        crate::roundtrip("repos_topics", &value);
    }

    #[test]
    fn branch_roundtrip() {
        let value = Branch {
            name: "main".into(),
            commit: BranchCommit {
                sha: "c0ffee01".into(),
                url: "https://code.example.com/api/v3/repos/alice/codehaven/commits/c0ffee01".into(),
            },
            protected: true,
        };
        crate::roundtrip("repos_branch", &value);
    }

    #[test]
    fn branch_protection_roundtrip() {
        let value = BranchProtection {
            url: "https://code.example.com/api/v3/repos/alice/codehaven/branches/main/protection".into(),
            required_status_checks: Some(RequiredStatusChecks { strict: true, contexts: vec!["ci".into()] }),
            required_pull_request_reviews: Some(RequiredReviews {
                required_approving_review_count: 1,
                dismiss_stale_reviews: true,
                require_code_owner_reviews: false,
            }),
            enforce_admins: true,
            required_conversation_resolution: true,
            allow_force_pushes: false,
            allow_deletions: false,
            allow_direct_push: false,
        };
        crate::roundtrip("repos_branch_protection", &value);
    }

    #[test]
    fn update_branch_protection_roundtrip() {
        let value = UpdateBranchProtection {
            required_status_checks: Some(RequiredStatusChecks { strict: false, contexts: vec!["ci".into()] }),
            required_pull_request_reviews: Some(RequiredReviews {
                required_approving_review_count: 2,
                dismiss_stale_reviews: false,
                require_code_owner_reviews: true,
            }),
            enforce_admins: false,
            required_conversation_resolution: true,
            allow_force_pushes: false,
            allow_deletions: false,
            allow_direct_push: true,
        };
        crate::roundtrip("repos_update_branch_protection", &value);
    }

    #[test]
    fn tag_roundtrip() {
        let value = Tag {
            name: "v0.1.0".into(),
            commit: BranchCommit {
                sha: "dead10cc".into(),
                url: "https://code.example.com/api/v3/repos/alice/codehaven/commits/dead10cc".into(),
            },
            zipball_url: "https://code.example.com/alice/codehaven/zipball/v0.1.0".into(),
            tarball_url: "https://code.example.com/alice/codehaven/tarball/v0.1.0".into(),
        };
        crate::roundtrip("repos_tag", &value);
    }

    #[test]
    fn commit_roundtrip() {
        crate::roundtrip("repos_commit", &commit("c0ffee01"));
    }

    #[test]
    fn compare_roundtrip() {
        let value = Compare {
            url: "https://code.example.com/api/v3/repos/alice/codehaven/compare/main...feature".into(),
            html_url: "https://code.example.com/alice/codehaven/compare/main...feature".into(),
            status: CompareStatus::Ahead,
            ahead_by: 1,
            behind_by: 0,
            total_commits: 1,
            commits: vec![commit("c0ffee01")],
            files: vec![file()],
            merge_base_commit: commit("dead10cc"),
        };
        crate::roundtrip("repos_compare", &value);
    }

    #[test]
    fn content_entry_roundtrip() {
        let value = ContentEntry {
            name: "README.md".into(),
            path: "docs/README.md".into(),
            sha: "b10b1234".into(),
            size: 12,
            kind: ContentKind::File,
            url: "https://code.example.com/api/v3/repos/alice/codehaven/contents/docs/README.md".into(),
            html_url: "https://code.example.com/alice/codehaven/blob/main/docs/README.md".into(),
            download_url: Some("https://code.example.com/raw/alice/codehaven/main/docs/README.md".into()),
            encoding: Some("base64".into()),
            content: Some("IyBDb2RlaGF2ZW4K".into()),
        };
        crate::roundtrip("repos_content_entry", &value);
    }

    #[test]
    fn put_content_roundtrip() {
        let value = PutContent {
            message: "update readme".into(),
            content: "IyBDb2RlaGF2ZW4K".into(),
            sha: Some("b10b1234".into()),
            branch: Some("docs".into()),
            committer: Some(GitIdentity { name: "Alice".into(), email: "alice@example.com".into(), date: crate::t0() }),
        };
        crate::roundtrip("repos_put_content", &value);
    }

    #[test]
    fn put_content_response_roundtrip() {
        let value = PutContentResponse {
            content: Some(ContentEntry {
                name: "README.md".into(),
                path: "README.md".into(),
                sha: "b10b1234".into(),
                size: 12,
                kind: ContentKind::File,
                url: "https://code.example.com/api/v3/repos/alice/codehaven/contents/README.md".into(),
                html_url: "https://code.example.com/alice/codehaven/blob/main/README.md".into(),
                download_url: None,
                encoding: None,
                content: None,
            }),
            commit: commit("c0ffee01"),
        };
        crate::roundtrip("repos_put_content_response", &value);
    }

    #[test]
    fn delete_content_roundtrip() {
        let value = DeleteContent { message: "remove stale doc".into(), sha: "b10b1234".into(), branch: Some("docs".into()) };
        crate::roundtrip("repos_delete_content", &value);
    }

    #[test]
    fn git_ref_roundtrip() {
        let value = GitRef {
            git_ref: "refs/heads/main".into(),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/git/ref/refs/heads/main".into(),
            object: GitRefObject {
                sha: "c0ffee01".into(),
                kind: "commit".into(),
                url: "https://code.example.com/api/v3/repos/alice/codehaven/git/commits/c0ffee01".into(),
            },
        };
        crate::roundtrip("repos_git_ref", &value);
    }

    #[test]
    fn create_ref_roundtrip() {
        let value = CreateRef { git_ref: "refs/heads/feature".into(), sha: "c0ffee01".into() };
        crate::roundtrip("repos_create_ref", &value);
    }

    #[test]
    fn update_ref_roundtrip() {
        let value = UpdateRef { sha: "dead10cc".into(), force: true };
        crate::roundtrip("repos_update_ref", &value);
    }

    #[test]
    fn collaborator_roundtrip() {
        let value = Collaborator {
            user: crate::user("kim", 2),
            permissions: Permissions { push: true, pull: true, ..Default::default() },
            role_name: "write".into(),
        };
        crate::roundtrip("repos_collaborator", &value);
    }

    #[test]
    fn add_collaborator_roundtrip() {
        let value = AddCollaborator { permission: "maintain".into() };
        crate::roundtrip("repos_add_collaborator", &value);
    }

    #[test]
    fn fork_request_roundtrip() {
        let value = ForkRequest { organization: Some("haven".into()), name: None, private: None, visibility: None };
        crate::roundtrip("repos_fork_request", &value);
    }

    #[test]
    fn repository_from_json_ignores_unknown_fields() {
        let raw = r#"{
            "id": 10,
            "node_id": "R_kgDO",
            "name": "codehaven",
            "full_name": "alice/codehaven",
            "private": false,
            "owner": {
                "login": "alice",
                "id": 1,
                "node_id": "MDQ6VXNlcjE=",
                "avatar_url": "https://code.example.com/avatars/1.svg",
                "gravatar_id": "",
                "url": "https://code.example.com/api/v3/users/alice",
                "html_url": "https://code.example.com/alice",
                "type": "User",
                "site_admin": false
            },
            "html_url": "https://code.example.com/alice/codehaven",
            "description": "a self-hosted forge",
            "fork": false,
            "url": "https://code.example.com/api/v3/repos/alice/codehaven",
            "git_url": "git://code.example.com/alice/codehaven.git",
            "ssh_url": "git@code.example.com:alice/codehaven.git",
            "clone_url": "https://code.example.com/alice/codehaven.git",
            "svn_url": "https://code.example.com/alice/codehaven",
            "hooks_url": "https://code.example.com/api/v3/repos/alice/codehaven/hooks",
            "default_branch": "main",
            "visibility": "public",
            "archived": false,
            "disabled": false,
            "topics": ["rust", "forge"],
            "has_issues": true,
            "has_projects": true,
            "has_downloads": true,
            "has_wiki": false,
            "is_template": false,
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-02-03T04:05:06Z",
            "pushed_at": "2026-02-03T04:05:06Z",
            "size": 42,
            "stargazers_count": 3,
            "watchers_count": 3,
            "forks_count": 1,
            "open_issues_count": 2,
            "watchers": 3,
            "license": null,
            "allow_forking": true,
            "allow_squash_merge": true,
            "allow_merge_commit": true,
            "allow_rebase_merge": true,
            "allow_update_branch": false,
            "delete_branch_on_merge": true,
            "squash_merge_commit_title": "PR_TITLE",
            "empty": false,
            "permissions": {"admin": false, "maintain": false, "push": true, "triage": false, "pull": true}
        }"#;
        let value: Repository = serde_json::from_str(raw).expect("parse repository json");
        assert_eq!(value.full_name, "alice/codehaven");
        assert_eq!(value.owner.kind, "User");
        assert_eq!(value.topics, vec!["rust".to_string(), "forge".to_string()]);
        assert!(value.permissions.is_some_and(|p| p.push));
    }
}
