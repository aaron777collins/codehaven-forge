//! What every repository page needs: the repo, the caller's permission, the sidebar, and the
//! split of `main/src/lib.rs` into a ref and a path.

use crate::chrome::{Chrome, RepoNav};
use crate::ctx::Ctx;
use crate::error::{WebError, WebResult};
use crate::fmt;
use ch_core::model::{GitRef, Repo};
use ch_core::{Error, Permission, RepoId};

pub struct RepoCtx {
    pub repo: Repo,
    pub perm: Permission,
}

impl RepoCtx {
    /// Loads the repo the caller may read. A repo they may not read is the same 404 as one
    /// that does not exist.
    pub async fn load(ctx: &Ctx, owner: &str, name: &str) -> WebResult<Self> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let repo = ctx.state.repos.get_by_name(&ctx.actor, owner, name).await?;
        let perm = ctx.state.authz.repo_permission(&ctx.actor, repo.id).await?;
        if perm == Permission::None {
            return Err(WebError::not_found());
        }
        Ok(RepoCtx { repo, perm })
    }

    pub fn id(&self) -> RepoId {
        self.repo.id
    }

    pub fn base(&self) -> String {
        format!("/{}/{}", fmt::enc(&self.repo.owner_name), fmt::enc(&self.repo.name))
    }

    pub fn nav(&self, section: &str) -> RepoNav {
        RepoNav::new(&self.repo, self.perm, section)
    }

    pub fn require(&self, needed: Permission) -> WebResult<()> {
        if self.perm.allows(needed) {
            Ok(())
        } else {
            Err(WebError::Core(Error::forbidden(format!("This needs {} access to {}.", needed.as_str(), self.repo.full_name()))))
        }
    }

    /// Writes stop on an archived repository.
    pub fn require_writable(&self, needed: Permission) -> WebResult<()> {
        self.require(needed)?;
        if self.repo.archived {
            return Err(WebError::Core(Error::forbidden("This repository is archived, so it is read only.")));
        }
        Ok(())
    }

    pub async fn chrome(&self, ctx: &Ctx, title: &str, section: &str) -> Chrome {
        let full = self.repo.full_name();
        let t = if title.is_empty() { full } else { format!("{title} - {full}") };
        let mut nav = self.nav(section);
        nav.workspace = self.workspace_link(ctx).await;
        ctx.chrome(&t).await.repo(nav)
    }

    /// The workspace URL for the header button, or empty when the button must not show: only a
    /// signed-in reader gets it, never a signed-out public read, and only while an admin has
    /// the editor on within what the config allows.
    pub async fn workspace_link(&self, ctx: &Ctx) -> String {
        if !ctx.signed_in() || ctx.public_read {
            return String::new();
        }
        match ctx.state.repos.workspace_settings().await {
            Ok(s) if s.editor_on(&ctx.state.config) => format!("{}/workspace", self.base()),
            _ => String::new(),
        }
    }

    /// The https clone address, shown on the repo home page.
    pub fn clone_https(&self, ctx: &Ctx) -> String {
        format!("{}/{}/{}.git", ctx.state.config.server.external_url, self.repo.owner_name, self.repo.name)
    }

    pub fn clone_ssh(&self, ctx: &Ctx) -> Option<String> {
        if !ctx.state.config.ssh.enabled {
            return None;
        }
        let (host, port) = ctx.state.config.ssh.external.rsplit_once(':').unwrap_or((&ctx.state.config.ssh.external, "22"));
        Some(if port == "22" {
            format!("git@{host}:{}/{}.git", self.repo.owner_name, self.repo.name)
        } else {
            format!("ssh://git@{host}:{port}/{}/{}.git", self.repo.owner_name, self.repo.name)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
    Commit,
}

/// A resolved ref from a URL such as `/tree/feature/x/src`.
#[derive(Debug, Clone)]
pub struct RefPath {
    pub name: String,
    pub kind: RefKind,
    pub path: String,
}

impl RefPath {
    /// `ref` encoded for a URL, slashes kept.
    pub fn enc(&self) -> String {
        fmt::enc_path(&self.name)
    }
}

/// Splits `rest` into the longest matching branch or tag name and the path below it. Branch
/// names can contain slashes, so the split cannot be done on the first `/`. A commit sha
/// (7 to 40 hex digits) is the last resort, then the first segment as written.
pub fn split_ref(rest: &str, branches: &[String], tags: &[String]) -> RefPath {
    let rest = rest.trim_matches('/');
    let mut best: Option<(usize, RefKind)> = None;
    for (list, kind) in [(branches, RefKind::Branch), (tags, RefKind::Tag)] {
        for name in list {
            let hit = rest == name || rest.strip_prefix(name.as_str()).is_some_and(|r| r.starts_with('/'));
            if hit && best.is_none_or(|(len, _)| name.len() > len) {
                best = Some((name.len(), kind));
            }
        }
    }
    if let Some((len, kind)) = best {
        return RefPath { name: rest[..len].to_string(), kind, path: rest[len..].trim_start_matches('/').to_string() };
    }
    let (first, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let is_sha = (7..=40).contains(&first.len()) && first.bytes().all(|b| b.is_ascii_hexdigit());
    RefPath { name: first.to_string(), kind: if is_sha { RefKind::Commit } else { RefKind::Branch }, path: tail.to_string() }
}

pub async fn branch_names(ctx: &Ctx, repo: RepoId) -> WebResult<Vec<String>> {
    let refs = ctx.state.git.refs(repo, Some("refs/heads/")).await?;
    Ok(names(&refs, "refs/heads/"))
}

pub async fn tag_names(ctx: &Ctx, repo: RepoId) -> WebResult<Vec<String>> {
    let refs = ctx.state.git.refs(repo, Some("refs/tags/")).await?;
    Ok(names(&refs, "refs/tags/"))
}

fn names(refs: &[GitRef], prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().map(|r| r.name.strip_prefix(prefix).unwrap_or(&r.name).to_string()).collect();
    v.sort();
    v
}

/// Loads branches and tags and splits `rest`. The default branch sorts first in pickers.
pub async fn resolve_ref(ctx: &Ctx, rc: &RepoCtx, rest: &str) -> WebResult<(RefPath, Vec<String>, Vec<String>)> {
    let branches = branch_names(ctx, rc.id()).await?;
    let tags = tag_names(ctx, rc.id()).await?;
    let rp = if rest.trim_matches('/').is_empty() {
        RefPath { name: rc.repo.default_branch.clone(), kind: RefKind::Branch, path: String::new() }
    } else {
        split_ref(rest, &branches, &tags)
    };
    Ok((rp, branches, tags))
}

/// The branch and tag dropdown. `base` is the URL prefix a ref name is appended to, such as
/// `/alice/demo/tree`, and `suffix` is the path to keep when switching.
#[derive(Debug, Clone, Default)]
pub struct BranchPicker {
    pub current: String,
    pub kind: String,
    pub short: String,
    pub branches: Vec<PickItem>,
    pub tags: Vec<PickItem>,
    pub more_branches: bool,
    pub all_branches_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct PickItem {
    pub name: String,
    pub href: String,
    pub current: bool,
}

impl BranchPicker {
    pub fn new(
        rp: &RefPath,
        branches: &[String],
        tags: &[String],
        default_branch: &str,
        base: &str,
        suffix: &str,
        repo_base: &str,
    ) -> Self {
        let url = |name: &str| {
            let tail = if suffix.is_empty() { String::new() } else { format!("/{}", fmt::enc_path(suffix)) };
            format!("{base}/{}{tail}", fmt::enc_path(name))
        };
        let mut bs: Vec<&String> = branches.iter().collect();
        bs.sort_by_key(|b| (b.as_str() != default_branch, b.to_lowercase()));
        let more = bs.len() > 40;
        let items = |list: Vec<&String>, kind: RefKind| -> Vec<PickItem> {
            list.into_iter()
                .take(40)
                .map(|n| PickItem { name: n.clone(), href: url(n), current: *n == rp.name && rp.kind == kind })
                .collect()
        };
        let tag_refs: Vec<&String> = tags.iter().rev().collect();
        BranchPicker {
            current: rp.name.clone(),
            kind: match rp.kind {
                RefKind::Branch => "Branch".into(),
                RefKind::Tag => "Tag".into(),
                RefKind::Commit => "Commit".into(),
            },
            short: if rp.kind == RefKind::Commit { fmt::short_sha(&rp.name) } else { rp.name.clone() },
            branches: items(bs, RefKind::Branch),
            tags: items(tag_refs, RefKind::Tag),
            more_branches: more,
            all_branches_url: format!("{repo_base}/branches"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn longest_branch_name_wins() {
        let b = v(&["main", "feature", "feature/login"]);
        let r = split_ref("feature/login/src/lib.rs", &b, &[]);
        assert_eq!((r.name.as_str(), r.path.as_str(), r.kind), ("feature/login", "src/lib.rs", RefKind::Branch));
        let r = split_ref("feature/src", &b, &[]);
        assert_eq!((r.name.as_str(), r.path.as_str()), ("feature", "src"));
        let r = split_ref("main", &b, &[]);
        assert_eq!((r.name.as_str(), r.path.as_str()), ("main", ""));
    }

    #[test]
    fn a_name_that_only_starts_the_same_is_not_a_match() {
        let b = v(&["main"]);
        let r = split_ref("mainline/x", &b, &[]);
        assert_eq!(r.name, "mainline");
    }

    #[test]
    fn tags_and_shas() {
        let r = split_ref("v1.0/docs/a.md", &[], &v(&["v1.0"]));
        assert_eq!((r.name.as_str(), r.kind, r.path.as_str()), ("v1.0", RefKind::Tag, "docs/a.md"));
        let r = split_ref("0123abc/readme", &[], &[]);
        assert_eq!(r.kind, RefKind::Commit);
        let r = split_ref("somebranch/readme", &[], &[]);
        assert_eq!(r.kind, RefKind::Branch);
    }

    #[test]
    fn picker_puts_default_first_and_marks_current() {
        let rp = RefPath { name: "dev".into(), kind: RefKind::Branch, path: "src".into() };
        let p = BranchPicker::new(&rp, &v(&["zeta", "dev", "main"]), &v(&["v1", "v2"]), "main", "/a/b/tree", "src", "/a/b");
        assert_eq!(p.branches[0].name, "main");
        assert!(p.branches.iter().any(|b| b.name == "dev" && b.current));
        assert_eq!(p.branches[0].href, "/a/b/tree/main/src");
        assert_eq!(p.tags[0].name, "v2");
        assert_eq!(p.short, "dev");
    }
}
