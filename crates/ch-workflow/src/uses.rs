//! The `uses:` value of a step: parsed, and judged against the admin policy.
//!
//! [`parse_uses`] turns the string into a [`Uses`] (a local path, a docker
//! image or a repository action pinned to a git ref), and [`evaluate`] applies
//! a site admin's [`Policy`] to decide whether the runner may fetch it.

use std::sync::LazyLock;

use regex::Regex;

/// Characters that may never appear in the git ref of an action reference.
const REF_FORBIDDEN: &[char] = &['~', '^', ':', '?', '*', '[', '\\'];

/// The longest git ref an action reference may carry.
const MAX_REF_LEN: usize = 255;

// Anchored with `\z`, the exact end: `$` reads better but is also true before a
// trailing newline in some engines, and these names may not contain one.
static OWNER: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9-]{0,38}\z").ok());
static REPO: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]{1,100}\z").ok());
static PATH_SEGMENT: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._ -]{1,255}\z").ok());

/// Whether `s` matches `re`. A regex that failed to compile never matches.
fn is_match(re: &Option<Regex>, s: &str) -> bool {
    re.as_ref().is_some_and(|compiled| compiled.is_match(s))
}

/// Where a repository action lives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    /// github.com.
    GitHub,
    /// A repository on this Codehaven instance.
    Haven,
}

/// A parsed `uses:` value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Uses {
    /// `./path` inside the workspace. The string is kept exactly as written.
    Local(String),
    /// `docker://image`. The string is the image, without the prefix.
    Docker(String),
    /// A repository action. For [`Host::Haven`], `owner` has the `haven_` prefix removed.
    Repo { host: Host, owner: String, repo: String, path: String, git_ref: String },
}

/// Why a `uses:` value could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UsesError(
    /// The human-readable reason, naming the input.
    pub String,
);

/// Builds the standard "not a valid action reference" error.
fn invalid(input: &str, why: &str) -> UsesError {
    UsesError(format!("`{input}` is not a valid action reference: {why}"))
}

/// Parse a `uses:` value.
///
/// Surrounding whitespace is trimmed first. The value must be a local path
/// (`./dir`), a docker image (`docker://image`) or a repository action
/// (`owner/repo/path@ref`), with a `haven_` owner prefix selecting
/// [`Host::Haven`].
pub fn parse_uses(input: &str) -> Result<Uses, UsesError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(invalid(input, "it is empty"));
    }

    if let Some(image) = input.strip_prefix("docker://") {
        if image.is_empty() {
            return Err(invalid(input, "docker:// needs an image name"));
        }
        if image.chars().any(char::is_whitespace) {
            return Err(invalid(input, "the image name may not contain whitespace"));
        }
        return Ok(Uses::Docker(image.to_owned()));
    }

    if input.starts_with("./") {
        if input.split('/').any(|segment| segment == "..") {
            return Err(invalid(input, "a local path may not contain a .. segment"));
        }
        return Ok(Uses::Local(input.to_owned()));
    }

    if input.starts_with("../") || input.starts_with('/') || input.contains("://") {
        return Err(invalid(input, "only ./paths, docker:// images and owner/repo@version references are supported"));
    }

    let Some((left, git_ref)) = input.rsplit_once('@') else {
        return Err(invalid(input, "it needs @ and a version, such as owner/repo@v1"));
    };
    if git_ref.is_empty() {
        return Err(invalid(input, "it needs a version after @, such as owner/repo@v1"));
    }
    check_ref(input, git_ref)?;

    let segments: Vec<&str> = left.split('/').collect();
    if segments.len() < 2 {
        return Err(invalid(input, "it needs an owner and a repository, such as owner/repo@v1"));
    }
    if segments.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err(invalid(input, "it has an empty, . or .. segment"));
    }
    let (owner, repo, path) = (segments[0], segments[1], segments[2..].join("/"));

    let (host, owner) = match owner.strip_prefix("haven_") {
        Some(rest) => (Host::Haven, rest),
        None => (Host::GitHub, owner),
    };
    if !is_match(&OWNER, owner) {
        return Err(invalid(
            input,
            match host {
                Host::Haven => "the owner after haven_ may only contain letters, digits and hyphens",
                Host::GitHub => "the owner may only contain letters, digits and hyphens",
            },
        ));
    }
    if !is_match(&REPO, repo) {
        return Err(invalid(input, "the repository name may only contain letters, digits, dots, underscores and hyphens"));
    }
    if !path.is_empty() && path.split('/').any(|segment| !is_match(&PATH_SEGMENT, segment)) {
        return Err(invalid(input, "the path may only contain letters, digits, dots, spaces, underscores and hyphens"));
    }

    Ok(Uses::Repo { host, owner: owner.to_owned(), repo: repo.to_owned(), path, git_ref: git_ref.to_owned() })
}

/// Validates the git ref half of `owner/repo@ref`, which is never empty here.
fn check_ref(input: &str, git_ref: &str) -> Result<(), UsesError> {
    let why = if git_ref.chars().count() > MAX_REF_LEN {
        "the version after @ is longer than 255 characters"
    } else if git_ref.starts_with('-') {
        "the version after @ may not start with a hyphen"
    } else if git_ref.chars().any(|c| c.is_whitespace() || c.is_control()) {
        "the version after @ may not contain whitespace or control characters"
    } else if git_ref.contains("..") || git_ref.chars().any(|c| REF_FORBIDDEN.contains(&c)) {
        "the version after @ contains characters that are not allowed in a git ref"
    } else {
        return Ok(());
    };
    Err(invalid(input, why))
}

/// Which actions an instance allows to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Only local actions (`./path`).
    None,
    /// Local actions plus whatever the `allow` list names.
    Selected,
    /// Every action.
    All,
}

/// A site admin's settings for `uses:` values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    pub mode: PolicyMode,
    /// Patterns `owner/*` or `owner/repo`, GitHub names. Matching ignores case.
    pub allow: Vec<String>,
    /// Same syntax. A match always refuses.
    pub block: Vec<String>,
    /// Require every repository action to be pinned to a full commit SHA.
    pub require_sha: bool,
    /// Let actions from repositories on this Codehaven instance run.
    pub allow_codehaven_repos: bool,
}

/// The denial used when the mode is [`PolicyMode::None`].
const ONLY_LOCAL: &str =
    "Only actions in this repository (./path) may run on this instance. A site admin can change this in Site admin > Actions.";

/// Whether a `uses:` value may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// It may run.
    Allow,
    /// It may not, with a message for the workflow author.
    Deny(String),
}

/// Apply `policy` to a parsed `uses:` value.
///
/// Local actions always run; under [`PolicyMode::None`] nothing else does.
/// Docker images are not governed by this policy, so they run under any other
/// mode. Repository actions are checked against `block`, `require_sha` and the
/// mode's allow list, in that order.
pub fn evaluate(policy: &Policy, uses: &Uses) -> Verdict {
    if matches!(uses, Uses::Local(_)) {
        return Verdict::Allow;
    }
    if policy.mode == PolicyMode::None {
        return Verdict::Deny(ONLY_LOCAL.to_owned());
    }
    if matches!(uses, Uses::Docker(_)) {
        return Verdict::Allow;
    }
    let Uses::Repo { host, owner, repo, git_ref, .. } = uses else {
        return Verdict::Allow; // Local and Docker, the only other kinds, returned above.
    };
    evaluate_repo(policy, host, owner, repo, git_ref)
}

/// The repository-action half of [`evaluate`], reached with a mode other than None.
fn evaluate_repo(policy: &Policy, host: &Host, owner: &str, repo: &str, git_ref: &str) -> Verdict {
    let name = format!("{owner}/{repo}");
    let display = match host {
        Host::Haven => format!("haven_{name}"),
        Host::GitHub => name.clone(),
    };

    if *host == Host::GitHub && policy.block.iter().any(|p| pattern_matches(p, owner, repo)) {
        return Verdict::Deny(format!("{name} is blocked on this instance."));
    }
    if policy.require_sha && !is_full_sha(git_ref) {
        return Verdict::Deny(format!("{display}@{git_ref} must be pinned to a full 40 character commit SHA on this instance."));
    }
    if *host == Host::Haven {
        return if policy.allow_codehaven_repos {
            Verdict::Allow
        } else {
            Verdict::Deny(format!(
                "{display} is not allowed: actions from repositories on this instance are turned \
                 off. A site admin can allow them in Site admin > Actions."
            ))
        };
    }
    match policy.mode {
        PolicyMode::All => Verdict::Allow,
        PolicyMode::Selected if policy.allow.iter().any(|p| pattern_matches(p, owner, repo)) => Verdict::Allow,
        _ => Verdict::Deny(format!("{name} is not allowed on this instance. A site admin can allow it in Site admin > Actions.")),
    }
}

/// Whether `pattern` covers `owner/repo`. See [`valid_pattern`] for the syntax.
fn pattern_matches(pattern: &str, owner: &str, repo: &str) -> bool {
    let pattern = pattern.trim();
    if !valid_pattern(pattern) {
        return false;
    }
    let Some((pattern_owner, pattern_repo)) = pattern.split_once('/') else {
        return false;
    };
    pattern_owner.eq_ignore_ascii_case(owner) && (pattern_repo == "*" || pattern_repo.eq_ignore_ascii_case(repo))
}

/// Whether `s` is a full commit SHA: exactly 40 hex characters, either case.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `p` is a well-formed allow or block pattern.
///
/// A pattern is `owner/*` or `owner/repo`, under the GitHub owner and repo
/// rules of [`parse_uses`]. Whitespace is trimmed first.
pub fn valid_pattern(p: &str) -> bool {
    let p = p.trim();
    let Some((owner, repo)) = p.split_once('/') else {
        return false;
    };
    is_match(&OWNER, owner) && (repo == "*" || is_match(&REPO, repo))
}

/// The `allow` list a fresh instance starts with.
pub const SEED_ALLOW: &[&str] = &[
    "actions/*",
    "github/*",
    "docker/*",
    "dotnet/*",
    "codecov/codecov-action",
    "ruby/setup-ruby",
    "JamesIves/github-pages-deploy-action",
    "peaceiris/actions-gh-pages",
    "softprops/action-gh-release",
    "skills/action-update-step",
    "dtolnay/rust-toolchain",
    "Swatinem/rust-cache",
    "peter-evans/create-pull-request",
];

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn policy(mode: PolicyMode) -> Policy {
        Policy { mode, allow: Vec::new(), block: Vec::new(), require_sha: false, allow_codehaven_repos: false }
    }

    fn repo(input: &str) -> Uses {
        parse_uses(input).unwrap()
    }

    #[test]
    fn parses_a_plain_github_action() {
        assert_eq!(
            repo("actions/checkout@v7"),
            Uses::Repo { host: Host::GitHub, owner: "actions".into(), repo: "checkout".into(), path: String::new(), git_ref: "v7".into() }
        );
    }

    #[test]
    fn parses_a_path_inside_the_repo() {
        assert_eq!(
            repo("o/r/sub/dir@main"),
            Uses::Repo { host: Host::GitHub, owner: "o".into(), repo: "r".into(), path: "sub/dir".into(), git_ref: "main".into() }
        );
    }

    #[test]
    fn strips_the_haven_prefix() {
        assert_eq!(
            repo("haven_aaron/tool@v1"),
            Uses::Repo { host: Host::Haven, owner: "aaron".into(), repo: "tool".into(), path: String::new(), git_ref: "v1".into() }
        );
    }

    #[test]
    fn haven_actions_can_have_a_path_and_a_sha() {
        assert_eq!(
            repo("haven_aaron/tool/a@3d3c42e5aac5ba805825da76410c181273ba90b1"),
            Uses::Repo {
                host: Host::Haven,
                owner: "aaron".into(),
                repo: "tool".into(),
                path: "a".into(),
                git_ref: "3d3c42e5aac5ba805825da76410c181273ba90b1".into()
            }
        );
    }

    #[test]
    fn parses_a_local_action_as_written() {
        assert_eq!(repo("./.github/actions/x"), Uses::Local("./.github/actions/x".into()));
    }

    #[test]
    fn parses_a_docker_image_without_the_prefix() {
        assert_eq!(repo("docker://alpine:3.20"), Uses::Docker("alpine:3.20".into()));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(repo("  actions/checkout@v7  "), repo("actions/checkout@v7"));
    }

    #[test]
    fn rejects_invalid_references() {
        let bad = [
            "o/r",
            "o/r@",
            "../x",
            "o/r/../x@v1",
            "https://h/o/r@v1",
            "o_x/r@v1",
            "haven_/r@v1",
            "o/r@a b",
            "o/r@-x",
            "docker://",
            "./a/../b",
            "/abs@v1",
            "o//r@v1",
            "o/r@v1..2",
            "",
        ];
        for input in bad {
            let parsed = parse_uses(input);
            assert!(parsed.is_err(), "`{input}` should be rejected");
            if let Err(e) = parsed {
                let trimmed = input.trim();
                if !trimmed.is_empty() {
                    assert!(e.to_string().contains(trimmed), "message should name `{trimmed}`: {e}");
                }
            }
        }
    }

    #[test]
    fn local_actions_run_even_under_mode_none() {
        let p = policy(PolicyMode::None);
        assert_eq!(evaluate(&p, &repo("./.github/actions/x")), Verdict::Allow);
    }

    #[test]
    fn mode_none_denies_a_github_repo() {
        let p = policy(PolicyMode::None);
        assert_eq!(evaluate(&p, &repo("actions/checkout@v7")), Verdict::Deny(ONLY_LOCAL.into()));
    }

    #[test]
    fn docker_images_run_under_mode_selected() {
        let mut p = policy(PolicyMode::Selected);
        p.allow = vec!["actions/*".into()];
        assert_eq!(evaluate(&p, &repo("docker://alpine:3.20")), Verdict::Allow);
    }

    #[test]
    fn block_wins_over_allow_and_over_all() {
        let checkout = repo("actions/checkout@v7");
        let mut p = policy(PolicyMode::Selected);
        p.allow = vec!["actions/*".into()];
        p.block = vec!["actions/checkout".into()];
        assert_eq!(evaluate(&p, &checkout), Verdict::Deny("actions/checkout is blocked on this instance.".into()));

        let mut p = policy(PolicyMode::All);
        p.block = vec!["actions/*".into()];
        assert_eq!(evaluate(&p, &checkout), Verdict::Deny("actions/checkout is blocked on this instance.".into()));
    }

    #[test]
    fn require_sha_denies_tags_and_accepts_shas() {
        let mut p = policy(PolicyMode::All);
        p.require_sha = true;
        assert_eq!(
            evaluate(&p, &repo("actions/checkout@v4")),
            Verdict::Deny("actions/checkout@v4 must be pinned to a full 40 character commit SHA on this instance.".into())
        );
        assert_eq!(evaluate(&p, &repo("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1")), Verdict::Allow);
    }

    #[test]
    fn haven_repos_follow_allow_codehaven_repos() {
        let mut p = policy(PolicyMode::All);
        let uses = repo("haven_aaron/tool@v1");
        p.allow_codehaven_repos = true;
        assert_eq!(evaluate(&p, &uses), Verdict::Allow);
        p.allow_codehaven_repos = false;
        assert!(matches!(evaluate(&p, &uses), Verdict::Deny(msg) if msg.starts_with("haven_")));
    }

    #[test]
    fn mode_all_allows_an_unlisted_repo() {
        let p = policy(PolicyMode::All);
        assert_eq!(evaluate(&p, &repo("o/r@v1")), Verdict::Allow);
    }

    #[test]
    fn selected_matching_ignores_case() {
        let mut p = policy(PolicyMode::Selected);
        p.allow = vec!["actions/*".into()];
        assert_eq!(evaluate(&p, &repo("ACTIONS/Checkout@v7")), Verdict::Allow);
    }

    #[test]
    fn a_wildcard_does_not_match_a_longer_owner() {
        let mut p = policy(PolicyMode::Selected);
        p.allow = vec!["actions/*".into()];
        assert_eq!(
            evaluate(&p, &repo("actionsx/a@v1")),
            Verdict::Deny("actionsx/a is not allowed on this instance. A site admin can allow it in Site admin > Actions.".into())
        );
    }

    #[test]
    fn selected_allows_an_exact_repo() {
        let mut p = policy(PolicyMode::Selected);
        p.allow = vec!["softprops/action-gh-release".into()];
        assert_eq!(evaluate(&p, &repo("softprops/action-gh-release@v2")), Verdict::Allow);
    }

    #[test]
    fn is_full_sha_needs_exactly_40_hex_characters() {
        assert!(is_full_sha("3d3c42e5aac5ba805825da76410c181273ba90b1"));
        assert!(!is_full_sha("3d3c42e5aac5ba805825da76410c181273ba90b"), "39 characters");
        assert!(!is_full_sha("3d3c42e5aac5ba805825da76410c181273ba90bg"), "ends in g");
    }

    #[test]
    fn valid_pattern_accepts_owner_wildcard_and_repo() {
        assert!(valid_pattern("a/*"));
        assert!(valid_pattern("a/b"));
        assert!(!valid_pattern("a"));
        assert!(!valid_pattern("*/b"));
        assert!(!valid_pattern("a/b/c"));
        assert!(!valid_pattern("a_b/c"));
    }

    #[test]
    fn every_seed_pattern_is_valid() {
        for p in SEED_ALLOW {
            assert!(valid_pattern(p), "`{p}` should be a valid pattern");
        }
    }
}
