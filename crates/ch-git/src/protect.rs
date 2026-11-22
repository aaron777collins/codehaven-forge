//! Branch protection: which patterns match a branch, and whether a ref update is allowed
//! (ARCHITECTURE.md section 6.4). Pure functions, so every rule has a table-driven test.

use ch_core::model::BranchProtection;
use regex::Regex;

/// Where a ref change came from. A pull request merge may land on a branch that refuses
/// direct pushes, because the merge gate already checked reviews and checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `git push` over HTTP or SSH.
    Push,
    /// A file edit made through the web UI or the contents API.
    Web,
    /// The merge button, or the merge API.
    PullRequestMerge,
    /// The server itself: branch creation for pull request refs, imports, forks.
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    Create,
    /// `forced` is true when the new commit does not descend from the old one.
    Update {
        forced: bool,
    },
    Delete,
}

/// Branch name patterns: `*` matches any characters except `/`, `**` matches anything
/// including `/`, `?` matches one character except `/`. Everything else is literal.
pub fn pattern_matches(pattern: &str, branch: &str) -> bool {
    let mut re = String::with_capacity(pattern.len() + 8);
    re.push('^');
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    re.push_str(".*");
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).map(|r| r.is_match(branch)).unwrap_or(false)
}

/// Every protection whose pattern matches this branch.
pub fn matching<'a>(protections: &'a [BranchProtection], branch: &str) -> Vec<&'a BranchProtection> {
    protections.iter().filter(|p| pattern_matches(&p.pattern, branch)).collect()
}

/// Decides one branch update. `Err` carries the sentence git shows the person.
pub fn check_branch_update(
    protections: &[BranchProtection],
    branch: &str,
    default_branch: &str,
    kind: UpdateKind,
    origin: Origin,
) -> Result<(), String> {
    if kind == UpdateKind::Delete && branch == default_branch {
        return Err(format!("'{branch}' is the default branch and cannot be deleted. Change the default branch first."));
    }
    let rules = matching(protections, branch);
    if rules.is_empty() {
        return Ok(());
    }
    match kind {
        UpdateKind::Delete => {
            if rules.iter().any(|r| r.block_deletion) {
                return Err(format!("Branch '{branch}' is protected and cannot be deleted."));
            }
        }
        UpdateKind::Update { forced: true } => {
            if rules.iter().any(|r| r.block_force_push) {
                return Err(format!("Force pushes to '{branch}' are not allowed."));
            }
        }
        UpdateKind::Update { forced: false } | UpdateKind::Create => {}
    }
    let direct_ok = matches!(origin, Origin::PullRequestMerge | Origin::System) || rules.iter().all(|r| r.allow_direct_push);
    // Creating a branch is allowed: a pull request needs the branch to exist before it can
    // be opened. Only changing the contents of a protected branch needs a pull request.
    if !direct_ok && matches!(kind, UpdateKind::Update { .. }) {
        return Err(format!("Branch '{branch}' is protected. Open a pull request to change it."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str) -> BranchProtection {
        BranchProtection { pattern: pattern.into(), block_force_push: true, block_deletion: true, ..Default::default() }
    }

    #[test]
    fn patterns() {
        for (p, b, want) in [
            ("main", "main", true),
            ("main", "main2", false),
            ("main", "xmain", false),
            ("release/*", "release/1.0", true),
            ("release/*", "release/1.0/hotfix", false),
            ("release/**", "release/1.0/hotfix", true),
            ("*", "main", true),
            ("*", "feature/x", false),
            ("**", "feature/x", true),
            ("v?.x", "v1.x", true),
            ("v?.x", "v10.x", false),
            ("a.b", "aXb", false),
            ("feat(1)", "feat(1)", true),
            ("[ab]", "a", false),
            ("", "", true),
        ] {
            assert_eq!(pattern_matches(p, b), want, "{p} vs {b}");
        }
    }

    #[test]
    fn unprotected_branches_are_free() {
        let rules = vec![rule("main")];
        for kind in [UpdateKind::Create, UpdateKind::Update { forced: true }, UpdateKind::Delete] {
            assert!(check_branch_update(&rules, "feature", "main", kind, Origin::Push).is_ok());
        }
    }

    #[test]
    fn direct_pushes_to_a_protected_branch_are_refused() {
        let rules = vec![rule("main")];
        let e = check_branch_update(&rules, "main", "main", UpdateKind::Update { forced: false }, Origin::Push).unwrap_err();
        assert!(e.contains("pull request"), "{e}");
        assert!(check_branch_update(&rules, "main", "main", UpdateKind::Update { forced: false }, Origin::Web).is_err());
    }

    #[test]
    fn a_merge_can_land_but_never_force_or_delete() {
        let rules = vec![rule("main")];
        assert!(check_branch_update(&rules, "main", "main", UpdateKind::Update { forced: false }, Origin::PullRequestMerge).is_ok());
        assert!(check_branch_update(&rules, "main", "main", UpdateKind::Update { forced: true }, Origin::PullRequestMerge).is_err());
        assert!(check_branch_update(&rules, "main", "trunk", UpdateKind::Delete, Origin::PullRequestMerge).is_err());
    }

    #[test]
    fn allow_direct_push_opens_only_the_direct_rule() {
        let mut r = rule("main");
        r.allow_direct_push = true;
        let rules = vec![r];
        assert!(check_branch_update(&rules, "main", "main", UpdateKind::Update { forced: false }, Origin::Push).is_ok());
        assert!(check_branch_update(&rules, "main", "main", UpdateKind::Update { forced: true }, Origin::Push).is_err());
    }

    #[test]
    fn force_push_can_be_allowed_explicitly() {
        let mut r = rule("dev");
        r.allow_direct_push = true;
        r.block_force_push = false;
        assert!(check_branch_update(&[r], "dev", "main", UpdateKind::Update { forced: true }, Origin::Push).is_ok());
    }

    #[test]
    fn the_strictest_matching_rule_wins() {
        let mut loose = rule("release/*");
        loose.allow_direct_push = true;
        loose.block_force_push = false;
        let strict = rule("release/1.*");
        let rules = vec![loose, strict];
        assert!(check_branch_update(&rules, "release/1.0", "main", UpdateKind::Update { forced: false }, Origin::Push).is_err());
        assert!(check_branch_update(&rules, "release/2.0", "main", UpdateKind::Update { forced: false }, Origin::Push).is_ok());
        assert!(check_branch_update(&rules, "release/1.0", "main", UpdateKind::Update { forced: true }, Origin::Push).is_err());
    }

    #[test]
    fn the_default_branch_cannot_be_deleted_even_unprotected() {
        let e = check_branch_update(&[], "main", "main", UpdateKind::Delete, Origin::Push).unwrap_err();
        assert!(e.contains("default branch"), "{e}");
        assert!(check_branch_update(&[], "other", "main", UpdateKind::Delete, Origin::Push).is_ok());
    }

    #[test]
    fn creating_a_branch_under_a_protected_pattern_is_allowed() {
        let rules = vec![rule("release/*")];
        assert!(check_branch_update(&rules, "release/9", "main", UpdateKind::Create, Origin::Push).is_ok());
        assert!(check_branch_update(&rules, "release/9", "main", UpdateKind::Update { forced: false }, Origin::Push).is_err());
    }
}
