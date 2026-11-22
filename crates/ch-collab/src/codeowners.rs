//! CODEOWNERS parsing and matching.
//!
//! Patterns follow the rules GitHub documents for CODEOWNERS, which are gitignore's with one
//! difference: `docs/*` matches the files directly in `docs` and nothing deeper.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    User(String),
    Team { org: String, team: String },
    Email(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub owners: Vec<Owner>,
    /// 1-based line in the file.
    pub line: usize,
}

pub fn parse(text: &str) -> Vec<Rule> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rules = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let tokens = tokenize(raw);
        let Some((pattern, rest)) = tokens.split_first() else { continue };
        let owners = rest.iter().filter_map(|t| owner_of(t)).collect();
        rules.push(Rule { pattern: pattern.clone(), owners, line: idx + 1 });
    }
    rules
}

/// Splits a line on unescaped whitespace, stops at a `#` that starts a token, and turns
/// `\ ` into a space inside a token.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut have = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(' ') => {
                    cur.push(' ');
                    chars.next();
                    have = true;
                }
                Some('#') if !have => {
                    cur.push('#');
                    chars.next();
                    have = true;
                }
                _ => {
                    cur.push('\\');
                    have = true;
                }
            },
            '#' if !have => break,
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}

fn owner_of(t: &str) -> Option<Owner> {
    if let Some(name) = t.strip_prefix('@') {
        if name.is_empty() {
            return None;
        }
        return Some(match name.split_once('/') {
            Some((org, team)) if !org.is_empty() && !team.is_empty() => Owner::Team { org: org.to_string(), team: team.to_string() },
            Some(_) => return None,
            None => Owner::User(name.to_string()),
        });
    }
    match t.split_once('@') {
        Some((local, domain)) if !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') => {
            Some(Owner::Email(t.to_string()))
        }
        _ => None,
    }
}

/// The rule that owns `path`: the last one in the file that matches. A matching rule with no
/// owners removes ownership, so it answers `None`.
pub fn owners_for<'a>(rules: &'a [Rule], path: &str) -> Option<&'a Rule> {
    let path = path.trim_start_matches('/');
    let rule = rules.iter().rev().find(|r| matches(&r.pattern, path))?;
    if rule.owners.is_empty() { None } else { Some(rule) }
}

/// Every distinct owner across the paths, in the order first seen.
pub fn owners_of_paths(rules: &[Rule], paths: &[String]) -> Vec<Owner> {
    let mut out: Vec<Owner> = Vec::new();
    for p in paths {
        if let Some(rule) = owners_for(rules, p) {
            for o in &rule.owners {
                if !out.contains(o) {
                    out.push(o.clone());
                }
            }
        }
    }
    out
}

pub fn matches(pattern: &str, path: &str) -> bool {
    let path = path.trim_start_matches('/');
    if path.is_empty() || pattern.trim().is_empty() {
        return false;
    }
    let dir_only = pattern.ends_with('/');
    let trimmed = pattern.trim_end_matches('/');
    let leading_slash = trimmed.starts_with('/');
    let core = trimmed.trim_start_matches('/');
    if core.is_empty() {
        return false;
    }
    let psegs: Vec<&str> = core.split('/').collect();
    let anchored = leading_slash || psegs.len() > 1;
    let last_has_wildcard = psegs.last().is_some_and(|s| *s != "**" && s.chars().any(|c| c == '*' || c == '?'));
    let segs: Vec<&str> = path.split('/').collect();
    let n = segs.len();
    for k in 1..=n {
        let is_file = k == n;
        if is_file && dir_only {
            continue;
        }
        // `docs/*` matches the files in `docs`, not whole subtrees. Everything else that
        // matches a directory also matches what is inside it.
        if !is_file && anchored && last_has_wildcard {
            continue;
        }
        let ok = if anchored { match_segments(&psegs, &segs[..k]) } else { segment_glob(psegs[0], segs[k - 1]) };
        if ok {
            return true;
        }
    }
    false
}

/// `**` matches any number of whole segments, and at the end it needs at least one.
fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    let m = pat.len();
    let n = path.len();
    let mut f = vec![false; n + 1];
    f[0] = true;
    for (i, seg) in pat.iter().enumerate() {
        let mut g = vec![false; n + 1];
        if *seg == "**" {
            let zero_ok = i + 1 != m;
            for j in 0..=n {
                g[j] = (zero_ok && f[j]) || (j > 0 && (f[j - 1] || g[j - 1]));
            }
        } else {
            for j in 0..n {
                g[j + 1] = f[j] && segment_glob(seg, path[j]);
            }
        }
        f = g;
    }
    f[n]
}

/// `*` and `?` inside one path segment, `\x` for a literal `x`. Quadratic at worst.
fn segment_glob(pat: &str, text: &str) -> bool {
    #[derive(Clone, Copy)]
    enum T {
        Lit(char),
        One,
        Star,
    }
    let mut toks = Vec::new();
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                Some(x) => toks.push(T::Lit(x)),
                None => toks.push(T::Lit('\\')),
            },
            '*' => {
                if !matches!(toks.last(), Some(T::Star)) {
                    toks.push(T::Star)
                }
            }
            '?' => toks.push(T::One),
            c => toks.push(T::Lit(c)),
        }
    }
    let s: Vec<char> = text.chars().collect();
    let m = s.len();
    let mut prev = vec![false; m + 1];
    prev[0] = true;
    for t in &toks {
        let mut cur = vec![false; m + 1];
        match t {
            T::Star => {
                cur[0] = prev[0];
                for j in 1..=m {
                    cur[j] = prev[j] || cur[j - 1];
                }
            }
            T::One => {
                cur[1..].copy_from_slice(&prev[..m]);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(n: &str) -> Owner {
        Owner::User(n.into())
    }

    #[test]
    fn parses_comments_blanks_and_owners() {
        let r = parse("# top\n\n*.rs @alice @acme/rust-people bob@example.com\n/docs/ @carol # trailing\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].pattern, "*.rs");
        assert_eq!(r[0].line, 3);
        assert_eq!(
            r[0].owners,
            vec![user("alice"), Owner::Team { org: "acme".into(), team: "rust-people".into() }, Owner::Email("bob@example.com".into())]
        );
        assert_eq!(r[1].owners, vec![user("carol")]);
    }

    #[test]
    fn crlf_and_bom() {
        let r = parse("\u{feff}*.md @a\r\n/x @b\r\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].pattern, "*.md");
        assert_eq!(r[1].owners, vec![user("b")]);
    }

    #[test]
    fn escaped_spaces_and_hash() {
        let r = parse("my\\ file.txt @a\n\\#notes @b\n");
        assert_eq!(r[0].pattern, "my file.txt");
        assert_eq!(r[1].pattern, "#notes");
    }

    #[test]
    fn rules_without_owners_and_garbage_tokens() {
        let r = parse("/vendor/\nsrc/ @a nonsense also-garbage @ @/x\n");
        assert!(r[0].owners.is_empty());
        assert_eq!(r[1].owners, vec![user("a")]);
    }

    #[test]
    fn star_matches_any_depth() {
        assert!(matches("*.js", "a.js"));
        assert!(matches("*.js", "src/a/b.js"));
        assert!(!matches("*.js", "src/a/b.jsx"));
    }

    #[test]
    fn bare_name_matches_anywhere_and_below() {
        assert!(matches("foo", "foo"));
        assert!(matches("foo", "x/foo"));
        assert!(matches("foo", "x/foo/y.txt"));
        assert!(!matches("foo", "x/foobar"));
    }

    #[test]
    fn leading_slash_anchors() {
        assert!(matches("/build/", "build/out.o"));
        assert!(!matches("/build/", "src/build/out.o"));
        assert!(matches("/build", "build/out.o"));
        assert!(matches("/README.md", "README.md"));
        assert!(!matches("/README.md", "docs/README.md"));
    }

    #[test]
    fn middle_slash_anchors() {
        assert!(matches("docs/a.md", "docs/a.md"));
        assert!(!matches("docs/a.md", "x/docs/a.md"));
    }

    #[test]
    fn docs_star_is_one_level() {
        assert!(matches("docs/*", "docs/a.md"));
        assert!(!matches("docs/*", "docs/a/b.md"));
    }

    #[test]
    fn trailing_slash_is_a_directory_anywhere() {
        assert!(matches("apps/", "apps/x.rs"));
        assert!(matches("apps/", "src/apps/x.rs"));
        assert!(!matches("apps/", "apps"));
    }

    #[test]
    fn double_star() {
        assert!(matches("a/**/b", "a/b"));
        assert!(matches("a/**/b", "a/x/b"));
        assert!(matches("a/**/b", "a/x/y/b"));
        assert!(!matches("a/**/b", "a/x/c"));
        assert!(matches("docs/**", "docs/a/b.md"));
        assert!(!matches("docs/**", "docs"));
        assert!(matches("**/logs", "x/y/logs"));
        assert!(matches("**/logs", "logs/a.txt"));
    }

    #[test]
    fn question_mark_and_class_of_chars() {
        assert!(matches("a?c.txt", "abc.txt"));
        assert!(!matches("a?c.txt", "a/c.txt"));
        assert!(!matches("a?c.txt", "ac.txt"));
    }

    #[test]
    fn case_sensitive() {
        assert!(!matches("README.md", "readme.md"));
    }

    #[test]
    fn last_match_wins() {
        let r = parse("* @everyone\n*.rs @rustaceans\ndocs/ @writers\n");
        assert_eq!(owners_for(&r, "src/main.rs").unwrap().owners, vec![user("rustaceans")]);
        assert_eq!(owners_for(&r, "docs/x.rs").unwrap().owners, vec![user("writers")]);
        assert_eq!(owners_for(&r, "LICENSE").unwrap().owners, vec![user("everyone")]);
    }

    #[test]
    fn empty_rule_clears_ownership() {
        let r = parse("* @everyone\n/vendor/\n");
        assert!(owners_for(&r, "vendor/x.c").is_none());
        assert!(owners_for(&r, "src/x.c").is_some());
    }

    #[test]
    fn owners_of_paths_dedups_in_order() {
        let r = parse("*.rs @a @b\n*.md @b @c\n");
        let o = owners_of_paths(&r, &["x.rs".into(), "y.md".into(), "z.rs".into(), "none.txt".into()]);
        assert_eq!(o, vec![user("a"), user("b"), user("c")]);
    }

    #[test]
    fn unicode_paths() {
        assert!(matches("café/*.md", "café/été.md"));
        assert!(matches("*.é", "x/y.é"));
    }

    #[test]
    fn pathological_pattern_returns() {
        let text = "a".repeat(5000);
        assert!(!matches("*a*a*a*a*a*b", &text));
        assert!(!matches("**/**/**/**/b", &format!("{}/c", "a/".repeat(500).trim_end_matches('/'))));
    }

    #[test]
    fn empty_inputs() {
        assert!(!matches("", "a"));
        assert!(!matches("a", ""));
        assert!(!matches("/", "a"));
        assert!(parse("").is_empty());
    }
}
