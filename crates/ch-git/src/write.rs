//! Everything that changes a repository without a client push: file edits, refs, merges.
//!
//! All of it runs in the bare repository with no working tree. Objects are written with
//! `hash-object`, trees with a throwaway index, merges with `merge-tree --write-tree`, and the
//! branch moves with a compare-and-swap `update-ref` (ARCHITECTURE.md sections 6.5 and 6.7).
//! A concurrent push makes the swap fail cleanly instead of losing commits.

use crate::command::{GitCommand, check_path, check_rev};
use crate::core::{GitCore, ZERO_SHA};
use crate::pipeline::RefChange;
use crate::protect::Origin;
use ch_core::model::*;
use ch_core::{Actor, Error, Permission, RepoId, Result};

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A path edited through the API: no empty or dot segments, and nothing that names git's own
/// directory (`.git`, in any case, and the sneaky spellings some filesystems fold to it).
pub(crate) fn check_edit_path(path: &str) -> Result<String> {
    let p = check_path(path)?;
    if p.is_empty() || p.split('/').any(|s| s.is_empty()) {
        return Err(Error::invalid("File", "path", "a file path is required"));
    }
    if p.split('/').any(|s| {
        let l = s.to_ascii_lowercase();
        l == ".git" || l.starts_with(".git ") || l.starts_with(".git.") && l.trim_end_matches('.') == ".git" || l == "git~1"
    }) {
        return Err(Error::invalid("File", "path", "the .git directory cannot be edited"));
    }
    Ok(p.to_string())
}

/// `GIT_AUTHOR_*` and `GIT_COMMITTER_*` for a commit made on someone's behalf.
fn identity_env(cmd: GitCommand, author: &Signature, committer: &Signature) -> GitCommand {
    let date = |s: &Signature| format!("{} +0000", s.at.timestamp());
    cmd.env("GIT_AUTHOR_NAME", &author.name)
        .env("GIT_AUTHOR_EMAIL", &author.email)
        .env("GIT_AUTHOR_DATE", date(author))
        .env("GIT_COMMITTER_NAME", &committer.name)
        .env("GIT_COMMITTER_EMAIL", &committer.email)
        .env("GIT_COMMITTER_DATE", date(committer))
}

/// A commit message always ends in exactly one newline.
fn message_bytes(message: &str) -> Vec<u8> {
    let mut m = message.trim_end().to_string();
    m.push('\n');
    m.into_bytes()
}

/// What `merge-tree --write-tree` said.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TreeMerge {
    Clean { tree: String },
    Conflict { paths: Vec<String> },
}

/// Parses `git merge-tree --write-tree --name-only --no-messages -z`: the tree, then each
/// conflicted path, all NUL-terminated.
pub(crate) fn parse_merge_tree(code: i32, stdout: &[u8]) -> Option<TreeMerge> {
    let mut parts = stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned());
    let tree = parts.next()?;
    match code {
        0 => Some(TreeMerge::Clean { tree }),
        1 => {
            let mut paths: Vec<String> = parts.collect();
            paths.sort();
            paths.dedup();
            Some(TreeMerge::Conflict { paths })
        }
        _ => None,
    }
}

impl GitCore {
    /// Makes sure `sha` exists in `repo`, fetching it from another repository in the same fork
    /// network when it only exists there (the head of a pull request from a fork).
    pub(crate) async fn ensure_object(&self, caller: Option<&Actor>, repo: RepoId, sha: &str) -> Result<()> {
        if !is_sha(sha) {
            return Err(Error::invalid("Object", "sha", "not a full commit id"));
        }
        let have = |id: RepoId| async move {
            self.exec_any(self.git(id).args(["cat-file", "-e", &format!("{sha}^{{commit}}")])).await.map(|o| o.code == 0).unwrap_or(false)
        };
        if have(repo).await {
            return Ok(());
        }
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT fork_root_id FROM repositories WHERE id = $1").bind(repo.0).fetch_optional(&self.db).await?;
        let root = row.and_then(|r| r.0).unwrap_or(repo.0);
        let candidates: Vec<(i64,)> = sqlx::query_as("SELECT id FROM repositories WHERE (id = $1 OR fork_root_id = $1) AND id <> $2")
            .bind(root)
            .bind(repo.0)
            .fetch_all(&self.db)
            .await?;
        for (id,) in candidates {
            let src = RepoId(id);
            // A commit id the caller supplied is copied only from a repository the caller can
            // read. Without this, anyone who knew a commit id in a private fork could pull its
            // objects into a repository they control and read them there.
            if let Some(a) = caller
                && !a.is_system()
                && !self.authz.repo_permission(a, src).await.map(|p| p.allows(Permission::Read)).unwrap_or(false)
            {
                continue;
            }
            if !self.store.exists(src) || !have(src).await {
                continue;
            }
            let cmd = self
                .git(repo)
                .args(["-c", "uploadpack.allowAnySHA1InWant=true", "fetch", "--quiet", "--no-tags", "--no-write-fetch-head"])
                .arg(self.store.repo_path(src))
                .arg(sha)
                .timeout(std::time::Duration::from_secs(300));
            self.exec(cmd).await?;
            return Ok(());
        }
        Err(Error::invalid("Object", "sha", "that commit does not exist in this repository"))
    }

    pub(crate) async fn do_create_ref(&self, actor: &Actor, repo_id: RepoId, name: &str, sha: &str, client: &ClientInfo) -> Result<()> {
        let repo = self.repo_row(repo_id).await?;
        self.require(actor, &repo, Permission::Write).await?;
        let _w = self.writes.write(repo.id).await?;
        let full = if name.starts_with("refs/") { name.to_string() } else { format!("refs/heads/{name}") };
        check_rev(&full)?;
        // The namespace first: `ensure_object` may copy a commit in from another repository of
        // the fork network, and a request that is refused anyway must not get that far.
        if !crate::pipeline::namespace_allowed(&full) && !actor.is_system() {
            return Err(Error::invalid("Reference", "ref", "only refs/heads/ and refs/tags/ can be changed"));
        }
        self.ensure_object(Some(actor), repo_id, sha).await?;
        let existing = self.read_refs(repo_id, Some(&full)).await?.into_iter().any(|r| r.name == full);
        if existing {
            return Err(Error::Validation(vec![ch_core::FieldError::new("Reference", "ref", "already_exists")]));
        }
        let origin = if actor.is_system() { Origin::System } else { Origin::Web };
        self.apply_ref_change(actor, &repo, RefChange { name: full, old: None, new: Some(sha.to_string()) }, origin, client).await
    }

    pub(crate) async fn do_delete_ref(&self, actor: &Actor, repo_id: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        let repo = self.repo_row(repo_id).await?;
        self.require(actor, &repo, Permission::Write).await?;
        let _w = self.writes.write(repo.id).await?;
        let full = if name.starts_with("refs/") { name.to_string() } else { format!("refs/heads/{name}") };
        check_rev(&full)?;
        let current = self.read_refs(repo_id, Some(&full)).await?.into_iter().find(|r| r.name == full).ok_or(Error::NotFound)?;
        // A tag ref may point at a tag object, so read the raw value for the swap.
        let raw = self.exec(self.git(repo_id).args(["rev-parse", "--verify", "--quiet", &full])).await?.stdout_str();
        let old = if is_sha(&raw) { raw } else { current.sha };
        let origin = if actor.is_system() { Origin::System } else { Origin::Web };
        self.apply_ref_change(actor, &repo, RefChange { name: full, old: Some(old), new: None }, origin, client).await
    }

    pub(crate) async fn do_update_ref(
        &self,
        actor: &Actor,
        repo_id: RepoId,
        name: &str,
        sha: &str,
        force: bool,
        client: &ClientInfo,
    ) -> Result<()> {
        let repo = self.repo_row(repo_id).await?;
        self.require(actor, &repo, Permission::Write).await?;
        let _w = self.writes.write(repo.id).await?;
        let full = if name.starts_with("refs/") { name.to_string() } else { format!("refs/heads/{name}") };
        check_rev(&full)?;
        // The namespace first: `ensure_object` may copy a commit in from another repository of
        // the fork network, and a request that is refused anyway must not get that far.
        if !crate::pipeline::namespace_allowed(&full) && !actor.is_system() {
            return Err(Error::invalid("Reference", "ref", "only refs/heads/ and refs/tags/ can be changed"));
        }
        self.ensure_object(Some(actor), repo_id, sha).await?;
        let raw = self.exec_any(self.git(repo_id).args(["rev-parse", "--verify", "--quiet", &full])).await?;
        if raw.code != 0 {
            return Err(Error::NotFound);
        }
        let old = raw.stdout_str();
        if !force && !self.is_fast_forward(&repo, &old, sha).await? {
            return Err(Error::invalid("Reference", "sha", "that update is not a fast-forward"));
        }
        let origin = if actor.is_system() { Origin::System } else { Origin::Web };
        self.apply_ref_change(actor, &repo, RefChange { name: full, old: Some(old), new: Some(sha.to_string()) }, origin, client).await
    }

    /// Create, edit, rename and delete files in one commit, then move the branch.
    pub(crate) async fn do_commit_file_change(&self, actor: &Actor, req: CommitFileChange, client: &ClientInfo) -> Result<String> {
        let repo = self.repo_row(req.repo_id).await?;
        self.require(actor, &repo, Permission::Write).await?;
        let _w = self.writes.write(repo.id).await?;
        if req.changes.is_empty() {
            return Err(Error::invalid("Commit", "changes", "nothing to commit"));
        }
        if req.message.trim().is_empty() {
            return Err(Error::invalid("Commit", "message", "a commit message is required"));
        }
        let source_ref = format!("refs/heads/{}", req.branch);
        check_rev(&source_ref)?;
        let base: Option<String> = {
            let out = self
                .exec_any(self.git(req.repo_id).args(["rev-parse", "--verify", "--quiet", &format!("{source_ref}^{{commit}}")]))
                .await?;
            if out.code == 0 { Some(out.stdout_str()) } else { None }
        };
        if base.is_none() && !(repo.is_empty && req.new_branch.is_none()) {
            return Err(Error::NotFound);
        }
        // A commit to a new branch may start from an older head of `branch`, as long as it is
        // still in that branch's history: "commit to a new branch from where I started".
        let mut base = base;
        if let (Some(expected), Some(current)) = (&req.expected_sha, &base)
            && expected != current
        {
            let older_start = req.new_branch.is_some()
                && is_sha(expected)
                && self
                    .exec_any(self.git(req.repo_id).args(["merge-base", "--is-ancestor", expected.as_str(), current.as_str()]))
                    .await
                    .map(|o| o.code == 0)
                    .unwrap_or(false);
            if !older_start {
                return Err(Error::conflict("The branch has changed since you opened this file. Reload and try again."));
            }
            base = Some(expected.clone());
        }

        // Existing modes, so editing an executable keeps it executable.
        let mut paths = Vec::new();
        for c in &req.changes {
            paths.push(check_edit_path(&c.path)?);
            if let Some(from) = &c.renamed_from {
                paths.push(check_edit_path(from)?);
            }
        }
        let mut modes = std::collections::HashMap::new();
        if let Some(b) = &base {
            let out = self
                .exec(self.git(req.repo_id).env("GIT_LITERAL_PATHSPECS", "1").args(["ls-tree", "-z", "-r", b, "--"]).args(paths.clone()))
                .await?;
            for rec in out.stdout.split(|x| *x == 0).filter(|r| !r.is_empty()) {
                let text = String::from_utf8_lossy(rec);
                if let Some((meta, path)) = text.split_once('\t') {
                    let mode = meta.split_whitespace().next().unwrap_or("100644");
                    modes.insert(path.to_string(), mode.to_string());
                }
            }
        }

        // A symlink or a submodule is never written over: its bytes mean something else. A
        // rename out of one is refused too: its mode would follow to the new path.
        for c in req.changes.iter().filter(|c| c.content.is_some()) {
            let path = check_edit_path(&c.path)?;
            let from = c.renamed_from.as_deref().map(check_edit_path).transpose()?;
            if matches!(modes.get(&path).map(String::as_str), Some("120000" | "160000"))
                || matches!(from.as_deref().and_then(|f| modes.get(f)).map(String::as_str), Some("120000" | "160000"))
            {
                return Err(Error::invalid("File", "path", "a link or a submodule cannot be edited here"));
            }
        }

        // A private index file: nothing here touches the repository's own state.
        let scratch = tempfile::tempdir().map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
        let index = scratch.path().join("index");
        let with_index = |c: GitCommand| c.env("GIT_INDEX_FILE", index.to_string_lossy());
        if let Some(b) = &base {
            self.exec(with_index(self.git(req.repo_id).args(["read-tree", b]))).await?;
        }
        if let Some(b) = &base {
            self.refuse_file_folder_clashes(req.repo_id, b, &req.changes).await?;
        }
        let mut removals = Vec::new();
        let mut puts = Vec::new();
        for c in &req.changes {
            let path = check_edit_path(&c.path)?;
            if let Some(from) = &c.renamed_from {
                let from = check_edit_path(from)?;
                removals.push(format!("0 {ZERO_SHA}\t{from}\0"));
            }
            match &c.content {
                None => removals.push(format!("0 {ZERO_SHA}\t{path}\0")),
                Some(bytes) => {
                    let blob = self
                        .exec(self.git(req.repo_id).args(["hash-object", "-w", "--stdin"]).stdin_bytes(bytes.clone()))
                        .await?
                        .stdout_str();
                    let from_mode = c.renamed_from.as_ref().and_then(|f| modes.get(f.trim_matches('/')));
                    let mode = modes.get(&path).or(from_mode).cloned().unwrap_or_else(|| "100644".into());
                    puts.push(format!("{mode} {blob}\t{path}\0"));
                }
            }
        }
        // Removals first, so one commit can delete a file `a` and add `a/b`. With `-z` the
        // paths are taken literally instead of C-unquoted, and git's own HFS and NTFS checks
        // refuse the spellings those filesystems fold to `.git`.
        let update = with_index(
            self.git(req.repo_id)
                .args(["-c", "core.protectHFS=true", "-c", "core.protectNTFS=true", "update-index", "-z", "--add", "--index-info"])
                .stdin_bytes([removals.concat(), puts.concat()].concat().into_bytes()),
        );
        if let Err(e) = self.exec(update).await {
            let s = e.stderr().to_string();
            if s.contains("invalid path") {
                return Err(Error::invalid("File", "path", "that file name is not allowed"));
            }
            if s.contains("both as a file and as a directory") || s.contains("conflict") {
                return Err(Error::conflict("A file and a folder cannot share a name."));
            }
            return Err(e.into());
        }
        let tree = self.exec(with_index(self.git(req.repo_id).arg("write-tree"))).await?.stdout_str();
        if let Some(b) = &base {
            let base_tree = self.exec(self.git(req.repo_id).args(["rev-parse", &format!("{b}^{{tree}}")])).await?.stdout_str();
            if base_tree == tree {
                return Err(Error::invalid("Commit", "changes", "these changes leave every file as it was"));
            }
        }
        let mut commit = self.git(req.repo_id).args(["commit-tree", &tree]);
        if let Some(b) = &base {
            commit = commit.args(["-p", b]);
        }
        let commit = identity_env(commit.stdin_bytes(message_bytes(&req.message)), &req.author, &req.author);
        let new_sha = self.exec(commit).await?.stdout_str();

        let (target, old) = match &req.new_branch {
            Some(nb) => (format!("refs/heads/{nb}"), None),
            None => (source_ref, base.clone()),
        };
        let origin = if actor.is_system() { Origin::System } else { Origin::Web };
        self.apply_ref_change(actor, &repo, RefChange { name: target, old, new: Some(new_sha.clone()) }, origin, client).await?;
        Ok(new_sha)
    }

    /// `update-index --index-info` quietly replaces a file with a folder of the same name. For an
    /// edit made by a person that would destroy a file, so check the base tree first: no new path
    /// may sit under an existing file, and none may be an existing folder.
    async fn refuse_file_folder_clashes(&self, repo: RepoId, base: &str, changes: &[FileChange]) -> Result<()> {
        let mut specs = Vec::new();
        for c in changes.iter().filter(|c| c.content.is_some()) {
            let path = check_edit_path(&c.path)?;
            let parts: Vec<&str> = path.split('/').collect();
            for i in 1..parts.len() {
                specs.push(format!("{base}:{}", parts[..i].join("/")));
            }
            specs.push(format!("{base}:{path}"));
        }
        if specs.is_empty() {
            return Ok(());
        }
        // Paths this commit removes, so a file can become a folder of the same name in one step.
        let removed: std::collections::HashSet<String> = changes
            .iter()
            .filter_map(|c| {
                if c.content.is_none() {
                    check_edit_path(&c.path).ok()
                } else {
                    c.renamed_from.as_deref().and_then(|f| check_edit_path(f).ok())
                }
            })
            .collect();
        // `%(objecttype)` prints exactly `blob`, `tree` or `commit` for a found object and
        // `<input> missing` for a missing one. Splitting on spaces would read a type word out
        // of a path such as `absent tree`.
        let out = self
            .exec(
                self.git(repo)
                    .args(["cat-file", "--batch-check=%(objecttype)"])
                    .stdin_bytes(format!("{}\n", specs.join("\n")).into_bytes()),
            )
            .await?;
        let text = out.stdout_str();
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        for c in changes.iter().filter(|c| c.content.is_some()) {
            let path = check_edit_path(&c.path)?;
            let parts: Vec<&str> = path.split('/').collect();
            let depth = parts.len();
            for level in 0..depth {
                let kind = lines.get(i).copied().unwrap_or("missing");
                i += 1;
                let is_leaf = level + 1 == depth;
                if (!is_leaf && kind == "blob" && !removed.contains(&parts[..level + 1].join("/"))) || (is_leaf && kind == "tree") {
                    return Err(Error::conflict("A file and a folder cannot share a name."));
                }
            }
        }
        Ok(())
    }

    /// Where a merge would land: the tree, or the files that conflict.
    async fn merge_trees(&self, repo: RepoId, base: &str, head: &str, merge_base: Option<&str>) -> Result<TreeMerge> {
        let mut cmd = self.git(repo).args(["merge-tree", "--write-tree", "--name-only", "--no-messages", "-z"]);
        if let Some(mb) = merge_base {
            cmd = cmd.arg(format!("--merge-base={mb}"));
        }
        let out = self.exec_any(cmd.args([base, head]).timeout(std::time::Duration::from_secs(120))).await?;
        if out.code == 128 && out.stderr_str().contains("unrelated histories") {
            return Ok(TreeMerge::Conflict { paths: Vec::new() });
        }
        parse_merge_tree(out.code, &out.stdout).ok_or_else(|| Error::internal(anyhow::anyhow!("merge-tree failed: {}", out.stderr_str())))
    }

    pub(crate) async fn do_test_merge(&self, repo: RepoId, base_ref: &str, head_repo: RepoId, head_ref: &str) -> Result<MergeableState> {
        let _w = self.writes.write(repo).await?;
        let base = self.read_resolve(repo, base_ref).await?;
        let head = self.read_resolve(head_repo, head_ref).await?;
        if head_repo != repo {
            self.ensure_object(None, repo, &head).await?;
        }
        Ok(match self.merge_trees(repo, &base, &head, None).await {
            Ok(TreeMerge::Clean { .. }) => MergeableState::Clean,
            Ok(TreeMerge::Conflict { .. }) => MergeableState::Conflict,
            Err(e) => {
                tracing::warn!(error = %e, "test merge could not run");
                MergeableState::Unknown
            }
        })
    }

    pub(crate) async fn do_merge(&self, actor: &Actor, req: MergeRequest, client: &ClientInfo) -> Result<MergeOutcome> {
        let repo = self.repo_row(req.repo_id).await?;
        self.require(actor, &repo, Permission::Write).await?;
        let _w = self.writes.write(repo.id).await?;
        let base_ref = format!("refs/heads/{}", req.base_ref.trim_start_matches("refs/heads/"));
        check_rev(&base_ref)?;
        let raw =
            self.exec_any(self.git(req.repo_id).args(["rev-parse", "--verify", "--quiet", &format!("{base_ref}^{{commit}}")])).await?;
        if raw.code != 0 {
            return Err(Error::NotFound);
        }
        let base = raw.stdout_str();
        if base != req.expected_base_sha {
            return Ok(MergeOutcome::BaseMoved { current_sha: base });
        }
        let head = self.read_resolve(req.head_repo_id, &req.head_ref).await?;
        if req.head_repo_id != req.repo_id {
            self.ensure_object(None, req.repo_id, &head).await?;
        }
        let mut message = req.title.trim().to_string();
        if !req.message.trim().is_empty() {
            message.push_str("\n\n");
            message.push_str(req.message.trim());
        }

        let new_sha = match req.method {
            MergeMethod::Merge | MergeMethod::Squash => {
                let tree = match self.merge_trees(req.repo_id, &base, &head, None).await? {
                    TreeMerge::Clean { tree } => tree,
                    TreeMerge::Conflict { paths } => return Ok(MergeOutcome::Conflict { paths }),
                };
                let mut cmd = self.git(req.repo_id).args(["commit-tree", &tree, "-p", &base]);
                if req.method == MergeMethod::Merge {
                    cmd = cmd.args(["-p", &head]);
                }
                let cmd = identity_env(cmd.stdin_bytes(message_bytes(&message)), &req.author, &req.committer);
                self.exec(cmd).await?.stdout_str()
            }
            MergeMethod::Rebase => {
                let range = format!("{base}..{head}");
                let list = self
                    .exec(self.git(req.repo_id).args([
                        "rev-list",
                        "--reverse",
                        "--topo-order",
                        "--no-merges",
                        "--end-of-options",
                        &range,
                        "--",
                    ]))
                    .await?
                    .stdout_str();
                let mut onto = base.clone();
                for c in list.lines().filter(|l| !l.is_empty()) {
                    let info = self.read_commit(req.repo_id, c).await?;
                    let parent = info.parents.first().cloned().unwrap_or_else(|| crate::read::EMPTY_TREE.to_string());
                    let tree = match self.merge_trees(req.repo_id, &onto, c, Some(&parent)).await? {
                        TreeMerge::Clean { tree } => tree,
                        TreeMerge::Conflict { paths } => return Ok(MergeOutcome::Conflict { paths }),
                    };
                    let onto_tree = self.exec(self.git(req.repo_id).args(["rev-parse", &format!("{onto}^{{tree}}")])).await?.stdout_str();
                    if tree == onto_tree {
                        // This commit's change is already in the base, so there is nothing to replay.
                        continue;
                    }
                    let cmd = self.git(req.repo_id).args(["commit-tree", &tree, "-p", &onto]);
                    let cmd = identity_env(cmd.stdin_bytes(message_bytes(&info.message)), &info.author, &req.committer);
                    onto = self.exec(cmd).await?.stdout_str();
                }
                if onto == base {
                    return Err(Error::invalid("Merge", "head", "there is nothing to merge"));
                }
                onto
            }
        };

        let change = RefChange { name: base_ref, old: Some(base), new: Some(new_sha.clone()) };
        // "Update branch" writes into a pull request's head branch, past no merge gate, so it is a
        // direct write as far as branch protection goes (ch-jnm).
        let origin = if req.update_branch { Origin::Web } else { Origin::PullRequestMerge };
        match self.apply_ref_change(actor, &repo, change, origin, client).await {
            Ok(()) => Ok(MergeOutcome::Merged { sha: new_sha }),
            Err(Error::Conflict(_)) => {
                let current = self.read_resolve(req.repo_id, &req.base_ref).await.unwrap_or_default();
                Ok(MergeOutcome::BaseMoved { current_sha: current })
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_paths() {
        assert_eq!(check_edit_path("src/lib.rs").unwrap(), "src/lib.rs");
        assert_eq!(check_edit_path("/README.md").unwrap(), "README.md");
        for bad in ["", "/", ".git/config", "a/.git/hooks/x", ".GIT/config", "a//b", "../x", "a/./b", ".git."] {
            assert!(check_edit_path(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(check_edit_path(".github/workflows/ci.yml").is_ok());
        assert!(check_edit_path(".gitignore").is_ok());
    }

    #[test]
    fn merge_tree_output() {
        assert_eq!(parse_merge_tree(0, b"abc123\0"), Some(TreeMerge::Clean { tree: "abc123".into() }));
        assert_eq!(
            parse_merge_tree(1, b"abc123\0b.txt\0a.txt\0a.txt\0"),
            Some(TreeMerge::Conflict { paths: vec!["a.txt".into(), "b.txt".into()] })
        );
        assert_eq!(parse_merge_tree(128, b""), None);
        assert_eq!(parse_merge_tree(0, b""), None);
    }

    #[test]
    fn messages_end_in_one_newline() {
        assert_eq!(message_bytes("hello"), b"hello\n");
        assert_eq!(message_bytes("hello\n\n\n"), b"hello\n");
    }

    #[test]
    fn sha_shape() {
        assert!(is_sha(&"a".repeat(40)));
        assert!(!is_sha("abc"));
        assert!(!is_sha(&"g".repeat(40)));
    }
}
