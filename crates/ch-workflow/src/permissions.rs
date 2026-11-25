//! `permissions:` and what a job token may do with it.
//!
//! A workflow or a job may say what its token needs, as GitHub Actions does. Without that the
//! token reads contents and packages and nothing else (GitHub's restricted default). Naming any
//! scope sets every scope it does not name to none. A job's block replaces the workflow's. The
//! result is then capped twice: by the most Codehaven gives any token (see [`MAX`]), by the
//! repository's ceiling, and, for a pull request from a fork, down to reading contents.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    None,
    Read,
    Write,
}

impl Level {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Level::None),
            "read" => Some(Level::Read),
            "write" => Some(Level::Write),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Level::None => "none",
            Level::Read => "read",
            Level::Write => "write",
        }
    }
}

/// Every scope a workflow may name, as GitHub spells them.
pub const SCOPES: &[&str] = &[
    "actions",
    "attestations",
    "checks",
    "contents",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "repository-projects",
    "security-events",
    "statuses",
];

/// The most any job token gets here, per scope. Scopes not listed get none: Codehaven has no
/// deployments, discussions, OIDC, Pages API or security events for a token to use.
pub const MAX: &[(&str, Level)] = &[
    ("actions", Level::Read),
    ("checks", Level::Write),
    ("contents", Level::Write),
    ("issues", Level::Write),
    ("packages", Level::Write),
    ("pull-requests", Level::Write),
    ("statuses", Level::Write),
];

/// What a workflow or job wrote under `permissions:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "scopes", rename_all = "snake_case")]
pub enum Declared {
    ReadAll,
    WriteAll,
    /// Only the named scopes. `{}` is an empty map: every scope none.
    Scopes(BTreeMap<String, Level>),
}

impl Declared {
    /// Reads the text form (`read-all`, `write-all`). The mapping form is read by the parser.
    pub fn from_word(s: &str) -> Option<Self> {
        match s {
            "read-all" => Some(Declared::ReadAll),
            "write-all" => Some(Declared::WriteAll),
            _ => None,
        }
    }

    fn level(&self, scope: &str) -> Level {
        match self {
            Declared::ReadAll => Level::Read,
            Declared::WriteAll => Level::Write,
            Declared::Scopes(m) => m.get(scope).copied().unwrap_or(Level::None),
        }
    }
}

/// Checks one entry of a `permissions:` mapping. `Err` is the message for the diagnostic.
pub fn check_entry(scope: &str, value: &str) -> Result<Level, String> {
    if !SCOPES.contains(&scope) {
        return Err(format!("unknown permission `{scope}`. Known: {}", SCOPES.join(", ")));
    }
    let Some(level) = Level::parse(value) else {
        return Err(format!("permission `{scope}` must be read, write or none"));
    };
    if scope == "id-token" && level == Level::Read {
        return Err("permission `id-token` must be write or none".into());
    }
    Ok(level)
}

/// What a job token may do. Stored with the job as JSON, read back when the token is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenPermissions {
    pub actions: Level,
    pub checks: Level,
    pub contents: Level,
    pub issues: Level,
    pub packages: Level,
    #[serde(rename = "pull-requests")]
    pub pull_requests: Level,
    pub statuses: Level,
}

impl TokenPermissions {
    /// The token of a job whose workflow names no permissions.
    pub fn default_token() -> Self {
        TokenPermissions { contents: Level::Read, packages: Level::Read, ..Default::default() }
    }

    fn get_mut(&mut self, scope: &str) -> Option<&mut Level> {
        Some(match scope {
            "actions" => &mut self.actions,
            "checks" => &mut self.checks,
            "contents" => &mut self.contents,
            "issues" => &mut self.issues,
            "packages" => &mut self.packages,
            "pull-requests" => &mut self.pull_requests,
            "statuses" => &mut self.statuses,
            _ => return None,
        })
    }

    /// Every scope lowered to at most `cap`.
    pub fn capped(mut self, cap: Level) -> Self {
        for (scope, _) in MAX {
            if let Some(l) = self.get_mut(scope) {
                *l = (*l).min(cap);
            }
        }
        self
    }

    /// Each scope lowered to what `other` has, for a called workflow that may only narrow.
    pub fn intersect(mut self, other: &TokenPermissions) -> Self {
        for (scope, _) in MAX {
            let theirs = other.clone_level(scope);
            if let Some(l) = self.get_mut(scope) {
                *l = (*l).min(theirs);
            }
        }
        self
    }

    fn clone_level(&self, scope: &str) -> Level {
        let mut copy = *self;
        copy.get_mut(scope).map(|l| *l).unwrap_or(Level::None)
    }

    /// The scopes as GitHub names them with their level, for the job's log.
    pub fn describe(&self) -> Vec<(&'static str, Level)> {
        MAX.iter().map(|(s, _)| (*s, self.clone_level(s))).collect()
    }
}

/// The repository setting that caps every job token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ceiling {
    /// Tokens only ever read.
    Read,
    /// Tokens write what the workflow asks for, up to [`MAX`].
    Write,
}

/// The token a job gets. `job` replaces `workflow` when present. `fork` is a pull request from
/// a fork, whose token only ever reads contents.
pub fn effective(workflow: Option<&Declared>, job: Option<&Declared>, ceiling: Ceiling, fork: bool) -> TokenPermissions {
    let declared = job.or(workflow);
    let mut out = match declared {
        None => TokenPermissions::default_token(),
        Some(d) => {
            let mut t = TokenPermissions::default();
            for (scope, most) in MAX {
                if let Some(l) = t.get_mut(scope) {
                    *l = d.level(scope).min(*most);
                }
            }
            t
        }
    };
    if ceiling == Ceiling::Read {
        out = out.capped(Level::Read);
    }
    if fork {
        out = TokenPermissions { contents: out.contents.min(Level::Read), ..Default::default() };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(pairs: &[(&str, Level)]) -> Declared {
        Declared::Scopes(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    #[test]
    fn nothing_declared_reads_contents_and_packages_only() {
        let t = effective(None, None, Ceiling::Write, false);
        assert_eq!(t, TokenPermissions { contents: Level::Read, packages: Level::Read, ..Default::default() });
    }

    #[test]
    fn an_empty_mapping_gives_nothing() {
        assert_eq!(effective(Some(&scopes(&[])), None, Ceiling::Write, false), TokenPermissions::default());
    }

    #[test]
    fn read_all_and_write_all_stop_at_the_most_a_token_gets() {
        let r = effective(Some(&Declared::ReadAll), None, Ceiling::Write, false);
        assert_eq!(r.contents, Level::Read);
        assert_eq!(r.actions, Level::Read);
        let w = effective(Some(&Declared::WriteAll), None, Ceiling::Write, false);
        assert_eq!(w.contents, Level::Write);
        assert_eq!(w.issues, Level::Write);
        // Actions is read at most: a token never starts or cancels runs.
        assert_eq!(w.actions, Level::Read);
    }

    #[test]
    fn naming_one_scope_sets_the_others_to_none() {
        let t = effective(Some(&scopes(&[("issues", Level::Write)])), None, Ceiling::Write, false);
        assert_eq!(t, TokenPermissions { issues: Level::Write, ..Default::default() });
    }

    #[test]
    fn a_job_block_replaces_the_workflow_block() {
        let wf = scopes(&[("contents", Level::Write)]);
        let job = scopes(&[("statuses", Level::Write)]);
        let t = effective(Some(&wf), Some(&job), Ceiling::Write, false);
        assert_eq!(t, TokenPermissions { statuses: Level::Write, ..Default::default() });
    }

    #[test]
    fn a_read_ceiling_caps_every_scope() {
        let t = effective(Some(&Declared::WriteAll), None, Ceiling::Read, false);
        assert!(t.describe().iter().all(|(_, l)| *l <= Level::Read));
        assert_eq!(t.contents, Level::Read);
    }

    #[test]
    fn a_fork_reads_contents_and_nothing_else() {
        let t = effective(Some(&Declared::WriteAll), None, Ceiling::Write, true);
        assert_eq!(t, TokenPermissions { contents: Level::Read, ..Default::default() });
        // A fork whose workflow asked for no contents gets none.
        let t = effective(Some(&scopes(&[("issues", Level::Write)])), None, Ceiling::Write, true);
        assert_eq!(t, TokenPermissions::default());
    }

    #[test]
    fn id_token_write_gives_nothing() {
        let t = effective(Some(&scopes(&[("id-token", Level::Write)])), None, Ceiling::Write, false);
        assert_eq!(t, TokenPermissions::default());
    }

    #[test]
    fn entries_are_checked() {
        assert_eq!(check_entry("contents", "write"), Ok(Level::Write));
        assert!(check_entry("content", "write").unwrap_err().contains("unknown permission"));
        assert!(check_entry("contents", "admin").is_err());
        assert!(check_entry("id-token", "read").is_err());
        assert_eq!(check_entry("id-token", "write"), Ok(Level::Write));
    }

    #[test]
    fn a_called_workflow_only_narrows() {
        let caller = TokenPermissions { contents: Level::Read, issues: Level::Write, ..Default::default() };
        let callee = TokenPermissions { contents: Level::Write, issues: Level::Read, ..Default::default() };
        assert_eq!(callee.intersect(&caller), TokenPermissions { contents: Level::Read, issues: Level::Read, ..Default::default() });
    }

    #[test]
    fn stored_json_uses_github_names_and_survives_missing_keys() {
        let t = TokenPermissions { pull_requests: Level::Write, ..Default::default() };
        let s = serde_json::to_string(&t).unwrap_or_default();
        assert!(s.contains("\"pull-requests\":\"write\""), "{s}");
        let back: TokenPermissions = serde_json::from_str("{\"contents\":\"write\"}").unwrap_or_default();
        assert_eq!(back, TokenPermissions { contents: Level::Write, ..Default::default() });
    }
}
