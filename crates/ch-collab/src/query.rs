//! The issue search language: `is:open label:bug author:@me sort:updated-desc some words`.
//!
//! Pure and total: no input makes it panic, and anything it does not understand becomes
//! free text, so a typo narrows nothing instead of failing the page.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Me,
    Name(String),
}

impl Who {
    fn parse(v: &str) -> Who {
        let n = v.strip_prefix('@').unwrap_or(v);
        if n.eq_ignore_ascii_case("me") { Who::Me } else { Who::Name(n.to_string()) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Issue,
    Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortField {
    #[default]
    Created,
    Updated,
    Comments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Desc,
    Asc,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedQuery {
    /// What is left after the qualifiers, words joined by single spaces.
    pub text: String,
    pub open: Option<bool>,
    pub kind: Option<Kind>,
    pub merged: Option<bool>,
    pub draft: Option<bool>,
    pub locked: Option<bool>,
    pub labels: Vec<String>,
    pub exclude_labels: Vec<String>,
    pub no_label: bool,
    pub authors: Vec<Who>,
    pub assignees: Vec<Who>,
    pub no_assignee: bool,
    pub mentions: Vec<Who>,
    pub involves: Vec<Who>,
    pub review_requested: Vec<Who>,
    pub milestone: Option<String>,
    pub no_milestone: bool,
    pub repo: Option<(String, String)>,
    pub sort: SortField,
    pub dir: SortDir,
}

impl ParsedQuery {
    pub fn is_empty(&self) -> bool {
        *self == ParsedQuery::default()
    }
}

/// Splits on whitespace outside double quotes. Quotes stay in the token for `interpret`.
fn split(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut prev_backslash = false;
    for c in input.chars() {
        if c == '"' && !(in_quote && prev_backslash) {
            in_quote = !in_quote;
            cur.push(c);
        } else if c.is_whitespace() && !in_quote {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
        prev_backslash = c == '\\' && !prev_backslash;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Removes one pair of surrounding quotes (the closing one may be missing) and `\"` escapes.
fn unquote(v: &str) -> String {
    match v.strip_prefix('"') {
        Some(rest) => {
            let rest = rest.strip_suffix('"').filter(|r| !r.ends_with('\\')).unwrap_or(rest);
            rest.replace("\\\"", "\"")
        }
        None => v.to_string(),
    }
}

pub fn parse(input: &str) -> ParsedQuery {
    let mut q = ParsedQuery::default();
    let mut text: Vec<String> = Vec::new();
    for tok in split(input) {
        if !interpret(&tok, &mut q) {
            let t = unquote(&tok);
            if !t.trim().is_empty() {
                text.push(t);
            }
        }
    }
    q.text = text.join(" ");
    q
}

/// True when the token was a qualifier and has been applied.
fn interpret(tok: &str, q: &mut ParsedQuery) -> bool {
    let (negated, body) = match tok.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, tok),
    };
    let Some(idx) = body.find(':') else { return false };
    let name = &body[..idx];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        return false;
    }
    let value = unquote(&body[idx + 1..]);
    if value.trim().is_empty() {
        return false;
    }
    let name = name.to_ascii_lowercase();
    if negated {
        if name == "label" {
            q.exclude_labels.push(value);
            return true;
        }
        return false;
    }
    let lower = value.to_ascii_lowercase();
    match name.as_str() {
        "is" => match lower.as_str() {
            "open" => q.open = Some(true),
            "closed" => q.open = Some(false),
            "issue" => q.kind = Some(Kind::Issue),
            "pr" | "pull" => q.kind = Some(Kind::Pull),
            "merged" => q.merged = Some(true),
            "unmerged" => q.merged = Some(false),
            "draft" => q.draft = Some(true),
            "locked" => q.locked = Some(true),
            "unlocked" => q.locked = Some(false),
            _ => return false,
        },
        "no" => match lower.as_str() {
            "label" => q.no_label = true,
            "assignee" => q.no_assignee = true,
            "milestone" => q.no_milestone = true,
            _ => return false,
        },
        "label" => q.labels.push(value),
        "author" => q.authors.push(Who::parse(&value)),
        "assignee" => q.assignees.push(Who::parse(&value)),
        "mentions" => q.mentions.push(Who::parse(&value)),
        "involves" => q.involves.push(Who::parse(&value)),
        "review-requested" => q.review_requested.push(Who::parse(&value)),
        "milestone" => q.milestone = Some(value),
        "repo" => match value.split_once('/') {
            Some((o, n)) if !o.is_empty() && !n.is_empty() => q.repo = Some((o.to_string(), n.to_string())),
            _ => return false,
        },
        "sort" => {
            let (field, dir) = match lower.split_once('-') {
                Some((f, d)) => (f, Some(d)),
                None => (lower.as_str(), None),
            };
            let field = match field {
                "created" => SortField::Created,
                "updated" => SortField::Updated,
                "comments" => SortField::Comments,
                _ => return true,
            };
            let dir = match dir {
                None | Some("desc") => SortDir::Desc,
                Some("asc") => SortDir::Asc,
                Some(_) => return true,
            };
            q.sort = field;
            q.dir = dir;
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_blank() {
        assert!(parse("").is_empty());
        assert!(parse("   \t ").is_empty());
    }

    #[test]
    fn states_and_kinds() {
        let q = parse("is:open is:pr");
        assert_eq!(q.open, Some(true));
        assert_eq!(q.kind, Some(Kind::Pull));
        assert_eq!(parse("is:closed is:issue").open, Some(false));
        assert_eq!(parse("is:closed is:issue").kind, Some(Kind::Issue));
        assert_eq!(parse("is:pull").kind, Some(Kind::Pull));
        assert_eq!(parse("is:merged").merged, Some(true));
        assert_eq!(parse("is:unmerged").merged, Some(false));
        assert_eq!(parse("is:draft").draft, Some(true));
        assert_eq!(parse("is:locked").locked, Some(true));
        assert_eq!(parse("is:unlocked").locked, Some(false));
    }

    #[test]
    fn last_state_wins() {
        assert_eq!(parse("is:open is:closed").open, Some(false));
    }

    #[test]
    fn labels_quoted_and_negated() {
        let q = parse(r#"label:bug label:"good first issue" -label:wontfix"#);
        assert_eq!(q.labels, vec!["bug", "good first issue"]);
        assert_eq!(q.exclude_labels, vec!["wontfix"]);
        assert!(q.text.is_empty());
    }

    #[test]
    fn people() {
        let q = parse("author:@me assignee:alice mentions:@bob involves:Carol review-requested:@ME");
        assert_eq!(q.authors, vec![Who::Me]);
        assert_eq!(q.assignees, vec![Who::Name("alice".into())]);
        assert_eq!(q.mentions, vec![Who::Name("bob".into())]);
        assert_eq!(q.involves, vec![Who::Name("Carol".into())]);
        assert_eq!(q.review_requested, vec![Who::Me]);
        assert_eq!(parse("author:me").authors, vec![Who::Me]);
    }

    #[test]
    fn no_qualifiers() {
        let q = parse("no:label no:assignee no:milestone");
        assert!(q.no_label && q.no_assignee && q.no_milestone);
        assert!(parse("no:something").text == "no:something");
    }

    #[test]
    fn milestone_and_repo() {
        let q = parse(r#"milestone:"v1.0" repo:octo/haven"#);
        assert_eq!(q.milestone.as_deref(), Some("v1.0"));
        assert_eq!(q.repo, Some(("octo".into(), "haven".into())));
        let q = parse("repo:haven");
        assert_eq!(q.repo, None);
        assert_eq!(q.text, "repo:haven");
        assert_eq!(parse("repo:/x").repo, None);
    }

    #[test]
    fn sort_variants() {
        let q = parse("sort:updated-desc");
        assert_eq!((q.sort, q.dir), (SortField::Updated, SortDir::Desc));
        let q = parse("sort:created-asc");
        assert_eq!((q.sort, q.dir), (SortField::Created, SortDir::Asc));
        let q = parse("sort:comments-desc");
        assert_eq!((q.sort, q.dir), (SortField::Comments, SortDir::Desc));
        let q = parse("sort:updated");
        assert_eq!((q.sort, q.dir), (SortField::Updated, SortDir::Desc));
        let q = parse("sort:nonsense sort:updated-sideways");
        assert_eq!((q.sort, q.dir), (SortField::Created, SortDir::Desc));
        assert!(q.text.is_empty());
    }

    #[test]
    fn free_text_and_phrases() {
        assert_eq!(parse("login  fails   is:open").text, "login fails");
        assert_eq!(parse(r#""hello world" is:open"#).text, "hello world");
        assert_eq!(parse("fix: crash").text, "fix: crash");
    }

    #[test]
    fn unknown_and_empty_qualifiers_are_text() {
        assert_eq!(parse("foo:bar").text, "foo:bar");
        assert_eq!(parse("label:").text, "label:");
        assert_eq!(parse("-author:bob").text, "-author:bob");
        assert_eq!(parse("is:weird").text, "is:weird");
    }

    #[test]
    fn case_insensitive_names_and_keywords() {
        let q = parse("IS:Open LABEL:Bug Sort:Updated-ASC");
        assert_eq!(q.open, Some(true));
        assert_eq!(q.labels, vec!["Bug"]);
        assert_eq!((q.sort, q.dir), (SortField::Updated, SortDir::Asc));
    }

    #[test]
    fn unterminated_quote_runs_to_the_end() {
        let q = parse(r#"label:"good first"#);
        assert_eq!(q.labels, vec!["good first"]);
        assert_eq!(parse(r#"is:open "unfinished phrase"#).text, "unfinished phrase");
    }

    #[test]
    fn escaped_quotes() {
        assert_eq!(parse(r#"label:"say \"hi\"""#).labels, vec![r#"say "hi""#]);
    }

    #[test]
    fn unicode() {
        let q = parse("café ☕ label:bug");
        assert_eq!(q.text, "café ☕");
        assert_eq!(q.labels, vec!["bug"]);
        assert_eq!(parse("label:été").labels, vec!["été"]);
        assert_eq!(parse("é:é").text, "é:é");
    }

    #[test]
    fn never_panics() {
        for s in [
            "\"",
            "-",
            ":",
            "::",
            "label:\"",
            "é:é",
            "\\",
            "\"\\",
            "-:",
            "-label:",
            "is:\"",
            "a:b:c",
            "\u{0}",
            "sort:-",
            "sort:-asc",
            "repo:/",
            "repo:a/",
        ] {
            let _ = parse(s);
        }
    }

    #[test]
    fn is_empty_only_when_all_default() {
        assert!(!parse("x").is_empty());
        assert!(!parse("is:open").is_empty());
        assert!(!parse("sort:updated").is_empty());
    }
}
