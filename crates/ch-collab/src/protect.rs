//! Branch protection patterns and how several protections combine.

use ch_core::model::BranchProtection;

/// Matches a branch name against a protection pattern. `*` matches anything except `/`, `**`
/// matches anything including `/`, `?` matches one character except `/`. Everything else is
/// literal and case sensitive. `refs/heads/` in front of either side is ignored.
pub fn pattern_matches(pattern: &str, branch: &str) -> bool {
    let p: Vec<char> = pattern.trim_start_matches("refs/heads/").chars().collect();
    let b: Vec<char> = branch.trim_start_matches("refs/heads/").chars().collect();
    glob(&p, &b)
}

/// Iterative matcher with single-star backtracking, linear in practice and never exponential.
fn glob(p: &[char], s: &[char]) -> bool {
    // Collapse the pattern into tokens first so `**` is one token.
    #[derive(Clone, Copy, PartialEq)]
    enum T {
        Lit(char),
        One,
        Star,
        Deep,
    }
    let mut toks: Vec<T> = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '*' if i + 1 < p.len() && p[i + 1] == '*' => {
                while i < p.len() && p[i] == '*' {
                    i += 1;
                }
                toks.push(T::Deep);
                continue;
            }
            '*' => toks.push(T::Star),
            '?' => toks.push(T::One),
            c => toks.push(T::Lit(c)),
        }
        i += 1;
    }
    // Dynamic programming over (token index, input index) with a rolling row.
    let m = s.len();
    let mut prev = vec![false; m + 1];
    prev[0] = true;
    // Leading stars can match the empty string.
    for t in toks.iter() {
        let mut cur = vec![false; m + 1];
        match t {
            T::Star | T::Deep => {
                cur[0] = prev[0];
                for j in 1..=m {
                    let can_eat = *t == T::Deep || s[j - 1] != '/';
                    cur[j] = prev[j] || (can_eat && cur[j - 1]);
                }
            }
            T::One => {
                for j in 1..=m {
                    cur[j] = prev[j - 1] && s[j - 1] != '/';
                }
            }
            T::Lit(c) => {
                for j in 1..=m {
                    cur[j] = prev[j - 1] && s[j - 1] == *c;
                }
            }
        }
        prev = cur;
    }
    prev[m]
}

/// What applies to a branch: every matching protection counts, and the strictest value of each
/// rule wins. `None` when nothing matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub required_approvals: u32,
    pub dismiss_stale_reviews: bool,
    pub require_code_owner_review: bool,
    pub required_checks: Vec<String>,
    pub strict: bool,
    pub require_resolved_threads: bool,
    pub block_force_push: bool,
    pub block_deletion: bool,
    pub allow_direct_push: bool,
}

pub fn rules_for(protections: &[BranchProtection], branch: &str) -> Option<Rules> {
    let mut any = false;
    let mut r = Rules { allow_direct_push: true, ..Default::default() };
    for p in protections.iter().filter(|p| pattern_matches(&p.pattern, branch)) {
        any = true;
        r.required_approvals = r.required_approvals.max(p.required_approvals);
        r.dismiss_stale_reviews |= p.dismiss_stale_reviews;
        r.require_code_owner_review |= p.require_code_owner_review;
        for c in &p.required_checks {
            if !r.required_checks.contains(c) {
                r.required_checks.push(c.clone());
            }
        }
        r.strict |= p.strict;
        r.require_resolved_threads |= p.require_resolved_threads;
        r.block_force_push |= p.block_force_push;
        r.block_deletion |= p.block_deletion;
        r.allow_direct_push &= p.allow_direct_push;
    }
    any.then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_star() {
        assert!(pattern_matches("main", "main"));
        assert!(!pattern_matches("main", "main2"));
        assert!(pattern_matches("release/*", "release/1.0"));
        assert!(!pattern_matches("release/*", "release/1.0/hotfix"));
        assert!(pattern_matches("release/**", "release/1.0/hotfix"));
        assert!(pattern_matches("*", "main"));
        assert!(!pattern_matches("*", "feature/x"));
        assert!(pattern_matches("**", "feature/x"));
        assert!(pattern_matches("v?.0", "v1.0"));
        assert!(!pattern_matches("v?.0", "v10.0"));
    }

    #[test]
    fn refs_prefix_is_ignored() {
        assert!(pattern_matches("refs/heads/main", "main"));
        assert!(pattern_matches("main", "refs/heads/main"));
    }

    #[test]
    fn case_sensitive_and_unicode() {
        assert!(!pattern_matches("Main", "main"));
        assert!(pattern_matches("café/*", "café/été"));
    }

    #[test]
    fn pathological_pattern_is_fast() {
        let input = "a".repeat(4000);
        assert!(!pattern_matches("*a*a*a*a*a*a*b", &input));
    }

    fn p(pattern: &str) -> BranchProtection {
        BranchProtection { pattern: pattern.into(), ..Default::default() }
    }

    #[test]
    fn strictest_rule_wins() {
        let mut a = p("main");
        a.required_approvals = 1;
        a.required_checks = vec!["ci".into()];
        let mut b = p("m*");
        b.required_approvals = 2;
        b.strict = true;
        b.required_checks = vec!["ci".into(), "lint".into()];
        let r = rules_for(&[a, b, p("other")], "main").unwrap();
        assert_eq!(r.required_approvals, 2);
        assert!(r.strict);
        assert_eq!(r.required_checks, vec!["ci", "lint"]);
    }

    #[test]
    fn no_match_is_none() {
        assert!(rules_for(&[p("release/*")], "main").is_none());
        assert!(rules_for(&[], "main").is_none());
    }
}
