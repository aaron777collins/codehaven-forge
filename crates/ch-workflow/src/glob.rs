//! Branch, tag and path patterns, with the rules Actions uses.
//!
//! `*` matches any run of characters except `/`. `**` matches anything including `/`. `?` makes
//! the character before it optional and `+` repeats it. `[abc]` and `[a-z]` match one
//! character. A leading `!` turns a pattern into an exclusion, and the last pattern that
//! matches decides. Those differ from shell globs, so this does not use one.

use regex::{Regex, RegexBuilder};

fn translate(pattern: &str) -> Result<String, String> {
    if pattern.is_empty() {
        return Err("the pattern is empty".into());
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    // Whether the previous thing written can take a `?` or `+`.
    let mut has_atom = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let Some(&n) = chars.get(i + 1) else { return Err("the pattern ends with a backslash".into()) };
                out.push_str(&regex::escape(&n.to_string()));
                has_atom = true;
                i += 2;
            }
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    if chars.get(i + 2) == Some(&'/') {
                        // `**/` also matches nothing, so `**/x` matches `x`.
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
                has_atom = false;
            }
            '?' | '+' => {
                if !has_atom {
                    return Err(format!("`{c}` must follow a character"));
                }
                out.push(c);
                has_atom = false;
                i += 1;
            }
            '[' => {
                let mut j = i + 1;
                let mut class = String::from("[");
                if chars.get(j) == Some(&'!') || chars.get(j) == Some(&'^') {
                    class.push('^');
                    j += 1;
                }
                let mut any = false;
                loop {
                    let Some(&cc) = chars.get(j) else { return Err("`[` has no closing `]`".into()) };
                    if cc == ']' && any {
                        break;
                    }
                    match cc {
                        '\\' | '[' | '&' | '~' | '^' => {
                            class.push('\\');
                            class.push(cc);
                        }
                        _ => class.push(cc),
                    }
                    any = true;
                    j += 1;
                }
                class.push(']');
                out.push_str(&class);
                has_atom = true;
                i = j + 1;
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                has_atom = true;
                i += 1;
            }
        }
    }
    out.push('$');
    Ok(out)
}

fn compile(pattern: &str) -> Result<Regex, String> {
    let re = translate(pattern)?;
    RegexBuilder::new(&re).size_limit(1 << 20).build().map_err(|e| format!("not a valid pattern: {e}"))
}

/// Checks a pattern, with the leading `!` allowed.
pub fn check(pattern: &str) -> Result<(), String> {
    let p = pattern.strip_prefix('!').unwrap_or(pattern);
    if pattern.starts_with('!') && p.is_empty() {
        return Err("`!` needs a pattern after it".into());
    }
    compile(p).map(|_| ())
}

/// True when one pattern (no `!`) matches the whole text.
pub fn matches(pattern: &str, text: &str) -> bool {
    compile(pattern).map(|r| r.is_match(text)).unwrap_or(false)
}

/// A list of patterns, compiled once, for testing many names against the same list. A list with
/// exclusions includes a name when the last pattern that matches it is a plain one, and a list
/// of only exclusions starts out including everything.
pub struct FilterSet {
    items: Vec<(bool, Option<Regex>)>,
    start: bool,
}

impl FilterSet {
    pub fn new(patterns: &[String]) -> Self {
        let items = patterns.iter().map(|p| (p.starts_with('!'), compile(p.strip_prefix('!').unwrap_or(p)).ok())).collect();
        Self { items, start: patterns.iter().all(|p| p.starts_with('!')) }
    }

    pub fn matches(&self, text: &str) -> bool {
        let mut included = self.start;
        for (negated, re) in &self.items {
            if re.as_ref().is_some_and(|r| r.is_match(text)) {
                included = !negated;
            }
        }
        included
    }
}

/// Like [`FilterSet`] for a single name.
pub fn filter(patterns: &[String], text: &str) -> bool {
    FilterSet::new(patterns).matches(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn m(p: &str, t: &str) -> bool {
        matches(p, t)
    }

    #[test]
    fn star_stops_at_slash_and_double_star_does_not() {
        assert!(m("main", "main"));
        assert!(!m("main", "main2"));
        assert!(m("release/*", "release/1.0"));
        assert!(!m("release/*", "release/1.0/rc"));
        assert!(m("release/**", "release/1.0/rc"));
        assert!(m("feature/**", "feature/a/b/c"));
        assert!(m("**", "any/thing/at/all"));
        assert!(m("*", "main"));
        assert!(!m("*", "a/b"));
        assert!(m("v*", "v1.2.3"));
        assert!(m("**.md", "docs/readme.md"));
        assert!(m("*.md", "readme.md"));
        assert!(!m("*.md", "docs/readme.md"));
        assert!(m("**/*.rs", "src/a/b.rs"));
        assert!(m("**/*.rs", "b.rs"), "`**/` can match nothing");
        assert!(m("docs/**", "docs/a.md"));
        assert!(!m("docs/**", "docsx/a.md"));
    }

    #[test]
    fn question_and_plus_repeat_the_previous_character() {
        assert!(m("v1?", "v"));
        assert!(m("v1?", "v1"));
        assert!(!m("v1?", "v11"));
        assert!(m("a+b", "aaab"));
        assert!(!m("a+b", "b"));
        assert!(m("[0-9]+", "2024"));
        assert!(m("v[0-9].[0-9]", "v1.2"));
        assert!(!m("v[0-9]", "va"));
        assert!(m("[!a]b", "cb"));
        assert!(!m("[!a]b", "ab"));
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        assert!(m("a.b", "a.b"));
        assert!(!m("a.b", "axb"));
        assert!(m("(x)", "(x)"));
        assert!(m("a|b", "a|b"));
        assert!(!m("a|b", "a"));
        assert!(m("{a}", "{a}"));
        assert!(m("^x$", "^x$"));
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "aXb"));
    }

    #[test]
    fn bad_patterns_are_reported() {
        for bad in ["", "!", "?x", "+", "[abc", "[]", "end\\"] {
            assert!(check(bad).is_err(), "{bad:?}");
        }
        for good in ["main", "!main", "release/**", "[a-z]*", "v?1"] {
            assert!(check(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn exclusions_apply_in_order() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(filter(&l(&["releases/**", "!releases/**-alpha"]), "releases/v1"));
        assert!(!filter(&l(&["releases/**", "!releases/**-alpha"]), "releases/v1-alpha"));
        assert!(filter(&l(&["releases/**", "!releases/**-alpha", "releases/beta-alpha"]), "releases/beta-alpha"), "a later match wins");
        assert!(!filter(&l(&["main"]), "dev"));
        assert!(filter(&l(&["!docs/**"]), "src/a.rs"), "only exclusions: everything else is in");
        assert!(!filter(&l(&["!docs/**"]), "docs/a.md"));
        let _ = filter(&[], "x");
    }

    proptest! {
        #[test]
        fn never_panics(p in "\\PC{0,30}", t in "\\PC{0,30}") {
            let _ = check(&p);
            let _ = matches(&p, &t);
        }

        // A pattern with no special characters matches exactly itself.
        #[test]
        fn literals_match_themselves(s in "[a-zA-Z0-9/_.-]{1,20}") {
            prop_assert!(matches(&s, &s));
            let longer = format!("{s}x");
            prop_assert!(!matches(&s, &longer));
        }

        #[test]
        fn double_star_matches_everything(t in "[a-zA-Z0-9/_.-]{0,30}") {
            prop_assert!(matches("**", &t));
        }

        #[test]
        fn single_star_matches_any_slashless_text(t in "[a-zA-Z0-9_.-]{0,30}") {
            prop_assert!(matches("*", &t));
            let nested = format!("{t}/x");
            prop_assert!(!matches("*", &nested));
        }
    }
}
