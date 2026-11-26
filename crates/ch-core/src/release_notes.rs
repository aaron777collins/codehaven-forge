//! Generated release notes configuration (ch-c13): `.github/release.yml`, the format GitHub
//! itself reads for its generated release notes. This module only parses the file's text and
//! answers two questions about a pull request: is it left out of the notes entirely, and which
//! section it goes under. It does no IO and no async.
//!
//! An absent file is `NotesConfig::default()`, and the caller then groups the pull requests its
//! own way. A hostile file goes through the same layers as every other repository YAML
//! (`yaml_guard`): a size cap, a rough nesting refusal before the parser, then the parser on a
//! stack sized for the deepest file the cap allows.

use crate::yaml_guard;
use marked_yaml::{LoaderOptions, Node};

/// The most the file may hold, in bytes.
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;
/// How many sections the file may list.
const MAX_CATEGORIES: usize = 20;
/// How many entries any one `labels` or `authors` list may hold.
const MAX_LIST: usize = 50;
/// How many characters any one string in the file may hold.
const MAX_STRING: usize = 100;
/// The most leading spaces any line may have. The parser recurses with the nesting of the file,
/// and this turns a hostile file away before it sees one; a real file runs about 6 levels deep.
const MAX_INDENT: usize = 60;
/// The most `[` and `{` characters the whole file may have. Flow collections nest on one line.
const MAX_FLOW_BRACKETS: usize = 400;

/// One section of the notes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Category {
    /// The heading the section gets.
    pub title: String,
    /// A pull request with any of these labels goes here. The label `*` matches any label.
    pub labels: Vec<String>,
    /// A pull request with any of these labels, or by any of these authors, skips this section.
    pub exclude_labels: Vec<String>,
    /// See `exclude_labels`.
    pub exclude_authors: Vec<String>,
}

/// What a repository's `.github/release.yml` says. `NotesConfig::default()` (no categories,
/// nothing excluded) means the file is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotesConfig {
    /// A pull request with any of these labels is left out of the notes entirely.
    pub exclude_labels: Vec<String>,
    /// A pull request by any of these authors is left out of the notes entirely.
    pub exclude_authors: Vec<String>,
    /// The sections, in file order.
    pub categories: Vec<Category>,
}

impl NotesConfig {
    /// True when a pull request with these labels by this author is left out of the notes
    /// entirely: any of its labels is in `exclude_labels`, or its author is in `exclude_authors`.
    /// Compare ignoring ASCII case.
    pub fn excludes(&self, labels: &[String], author: &str) -> bool {
        self.exclude_authors.iter().any(|a| a.eq_ignore_ascii_case(author))
            || labels.iter().any(|l| self.exclude_labels.iter().any(|e| e.eq_ignore_ascii_case(l)))
    }

    /// Which category the pull request goes under, as an index into `categories`, or `None` when
    /// it is not shown. First the exclude rule above (excluded gives `None`). Then the first
    /// category, in file order, that does not exclude it (a label in its `exclude_labels`, or
    /// its author in its `exclude_authors`) and has a label equal to one of the pull request's
    /// labels (ignoring ASCII case) or the label `*`.
    pub fn category_of(&self, labels: &[String], author: &str) -> Option<usize> {
        if self.excludes(labels, author) {
            return None;
        }
        self.categories.iter().position(|c| {
            let excluded = c.exclude_authors.iter().any(|a| a.eq_ignore_ascii_case(author))
                || labels.iter().any(|l| c.exclude_labels.iter().any(|e| e.eq_ignore_ascii_case(l)));
            !excluded && c.labels.iter().any(|l| l == "*" || labels.iter().any(|p| p.eq_ignore_ascii_case(l)))
        })
    }

    /// Whether the file named any categories, so the caller knows to use its own default grouping.
    pub fn has_categories(&self) -> bool {
        !self.categories.is_empty()
    }
}

/// A rough nesting refusal run before the parser: the parser recurses with the nesting of the
/// file and a stack overflow takes the whole process down, so anything that could nest deeply is
/// turned away first. Deliberately rough (indent width, explicit-key markers, flow brackets): it
/// only has to be safe, and it errs towards refusing.
fn nested_too_deeply(text: &str) -> bool {
    // The whole text, not one line: flow collections nest on a single line.
    if text.matches(['[', '{']).count() > MAX_FLOW_BRACKETS {
        return true;
    }
    // `str::lines` splits on LF only, which is what `normalise` has already made of the text.
    text.lines().any(|line| {
        let content = line.trim_start_matches(' ');
        line.len() - content.len() > MAX_INDENT || content.starts_with("? ") || content.starts_with(": ")
    })
}

/// Reads the file. The error is one plain sentence a person can act on, for example "the file is
/// larger than 16 KB", "not valid YAML: ...", "`changelog` must be a mapping", "category 2 has no
/// title".
pub fn parse_config(text: &str) -> Result<NotesConfig, String> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err("the file is larger than 16 KB".into());
    }
    // One normalisation of every line break, then the refusal and the parser read the same text,
    // and the parser runs on a stack sized for the deepest file the cap allows.
    let text = yaml_guard::normalise(text);
    if text.trim().is_empty() {
        return Ok(NotesConfig::default());
    }
    if nested_too_deeply(&text) {
        return Err("the file is nested too deeply".into());
    }
    yaml_guard::on_parser_stack(MAX_CONFIG_BYTES, || parse_text(&text))?
}

fn parse_text(text: &str) -> Result<NotesConfig, String> {
    let opts = LoaderOptions::default().error_on_duplicate_keys(true).prevent_coercion(true);
    let root = marked_yaml::parse_yaml_with_options(0, text, opts).map_err(|e| format!("not valid YAML: {e}"))?;
    let Node::Mapping(top) = &root else { return Ok(NotesConfig::default()) };
    let mut config = NotesConfig::default();
    for (k, v) in top.iter() {
        if k.as_str() != "changelog" {
            continue; // the only top-level key the format has; unknown keys are ignored
        }
        let Node::Mapping(section) = v else { return Err("`changelog` must be a mapping".into()) };
        for (k, v) in section.iter() {
            match k.as_str() {
                "exclude" => {
                    read_exclude(v, "`changelog.exclude`", &mut config.exclude_labels, &mut config.exclude_authors)?;
                }
                "categories" => {
                    let Node::Sequence(items) = v else { return Err("`categories` must be a list".into()) };
                    if items.len() > MAX_CATEGORIES {
                        return Err(format!("`changelog` has at most {MAX_CATEGORIES} categories"));
                    }
                    for (i, item) in items.iter().enumerate() {
                        config.categories.push(read_category(i + 1, item)?);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(config)
}

/// An `exclude:` block, at the changelog level or inside a category: labels and authors left out.
fn read_exclude(node: &Node, place: &str, exclude_labels: &mut Vec<String>, exclude_authors: &mut Vec<String>) -> Result<(), String> {
    let Node::Mapping(m) = node else { return Err(format!("{place} must be a mapping")) };
    for (k, v) in m.iter() {
        match k.as_str() {
            "labels" => *exclude_labels = string_list(v, place, "label")?,
            "authors" => *exclude_authors = string_list(v, place, "author")?,
            _ => {}
        }
    }
    Ok(())
}

fn read_category(n: usize, item: &Node) -> Result<Category, String> {
    let Node::Mapping(m) = item else { return Err(format!("category {n} must be a mapping")) };
    let mut category = Category::default();
    for (k, v) in m.iter() {
        match k.as_str() {
            "title" => {
                let Node::Scalar(s) = v else { return Err(format!("the title of category {n} must be text")) };
                category.title = s.as_str().trim().to_string();
            }
            "labels" => category.labels = string_list(v, &format!("category {n}"), "label")?,
            "exclude" => {
                read_exclude(v, &format!("the exclude of category {n}"), &mut category.exclude_labels, &mut category.exclude_authors)?;
            }
            _ => {}
        }
    }
    if category.title.is_empty() {
        return Err(format!("category {n} has no title"));
    }
    if category.title.chars().count() > MAX_STRING {
        return Err(format!("the title of category {n} is longer than {MAX_STRING} characters"));
    }
    Ok(category)
}

/// A `labels` or `authors` value: a list, or one string on its own. Every entry is trimmed,
/// empty ones are dropped, and the list and string limits are enforced here.
fn string_list(node: &Node, place: &str, what: &str) -> Result<Vec<String>, String> {
    let entries: Vec<&str> = match node {
        Node::Scalar(s) => vec![s.as_str()],
        Node::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                let Node::Scalar(s) = item else {
                    return Err(format!("the {what}s in {place} must be a list of strings"));
                };
                out.push(s.as_str());
            }
            out
        }
        _ => return Err(format!("the {what}s in {place} must be a list of strings")),
    };
    if entries.len() > MAX_LIST {
        return Err(format!("too many {what}s in {place}"));
    }
    let mut list = Vec::with_capacity(entries.len());
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.chars().count() > MAX_STRING {
            return Err(format!("a {what} in {place} is longer than {MAX_STRING} characters"));
        }
        if !trimmed.is_empty() {
            list.push(trimmed.to_string());
        }
    }
    Ok(list)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const GITHUB_EXAMPLE: &str = "changelog:
  exclude:
    labels:
      - ignore-for-release
    authors:
      - octocat
  categories:
    - title: Breaking Changes
      labels:
        - Semver-Major
        - breaking-change
    - title: Exciting New Features
      labels: [Semver-Minor, enhancement]
      exclude:
        labels: [skip]
        authors: [dependabot]
    - title: Other Changes
      labels:
        - \"*\"
";

    #[test]
    fn parses_github_example() {
        let config = parse_config(GITHUB_EXAMPLE).unwrap();
        let titles: Vec<&str> = config.categories.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Breaking Changes", "Exciting New Features", "Other Changes"]);
        assert_eq!(config.categories[0].labels, ["Semver-Major", "breaking-change"]);
        assert_eq!(config.categories[1].labels, ["Semver-Minor", "enhancement"]);
        assert_eq!(config.categories[2].labels, ["*"]);
        assert_eq!(config.exclude_labels, ["ignore-for-release"]);
        assert_eq!(config.exclude_authors, ["octocat"]);
        assert_eq!(config.categories[1].exclude_labels, ["skip"]);
        assert_eq!(config.categories[1].exclude_authors, ["dependabot"]);
        assert!(config.has_categories());
    }

    #[test]
    fn empty_or_missing_changelog_is_the_default() {
        for text in ["", "  \n", "foo: 1\n"] {
            assert_eq!(parse_config(text).unwrap(), NotesConfig::default(), "{text:?}");
        }
    }

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn category_of_uses_first_match_and_star() {
        let config = parse_config(GITHUB_EXAMPLE).unwrap();
        assert_eq!(config.category_of(&labels(&["enhancement"]), "alice"), Some(1));
        assert_eq!(config.category_of(&labels(&["Semver-Major", "enhancement"]), "alice"), Some(0), "first in file order");
        assert_eq!(config.category_of(&labels(&["whatever"]), "alice"), Some(2), "the `*` category");
        assert_eq!(config.category_of(&labels(&["ignore-for-release"]), "alice"), None);
        assert_eq!(config.category_of(&labels(&["enhancement"]), "OCTOCAT"), None, "the author compares ignoring case");
        // `skip` sits in category 1's own exclude, not the global one: category 1 passes it over
        // and the `*` category takes it.
        assert_eq!(config.category_of(&labels(&["enhancement", "skip"]), "alice"), Some(2));
    }

    #[test]
    fn no_categories_means_none() {
        let config = NotesConfig::default();
        assert_eq!(config.category_of(&["bug".into()], "a"), None);
        assert!(!config.has_categories());
        let only_excludes = NotesConfig { exclude_labels: vec!["wontfix".into()], ..NotesConfig::default() };
        assert!(only_excludes.excludes(&["WONTFIX".into()], "someone"));
        assert!(!only_excludes.excludes(&["bug".into()], "someone"));
        assert!(!only_excludes.has_categories());
    }

    #[test]
    fn bad_files_are_refused_with_a_sentence() {
        let bad = |text: &str, want: &str| {
            let e = parse_config(text).unwrap_err();
            assert!(e.contains(want), "{e}");
        };
        bad("changelog: 3\n", "mapping");
        bad("changelog:\n  categories:\n    - labels: [x]\n", "category 1");
        let many = format!("changelog:\n  categories:\n{}", "    - title: t\n".repeat(21));
        assert!(parse_config(&many).is_err(), "at most {MAX_CATEGORIES} categories");
        bad(&"x".repeat(MAX_CONFIG_BYTES + 1), "16 KB");
        let mut padded = String::from("changelog:\n  categories:\n    - title: a\n      labels: [x]\n");
        padded.push_str(&"\n".repeat(MAX_CONFIG_BYTES - padded.len()));
        assert!(parse_config(&padded).is_ok());
    }

    /// A hostile file must never overflow the stack or abort the process: returning is the proof
    /// (ch-qop). The dash chain nests one level per two bytes and goes past the refusal to the
    /// parser's own stack; the bracket chain is turned away before it.
    #[test]
    fn hostile_files_do_not_abort() {
        let deep = format!("{}x\n", "- ".repeat(8000));
        assert!(deep.len() < MAX_CONFIG_BYTES);
        let _ = parse_config(&deep);
        assert!(parse_config(&"[".repeat(8000)).is_err());
        // A lone CR between keys reads like an LF (ch-qop).
        let cr = "changelog:\r  exclude:\r    labels:\r      - x\r  categories:\r    - title: t\r";
        let lf = cr.replace('\r', "\n");
        assert_eq!(parse_config(cr).unwrap(), parse_config(&lf).unwrap());
    }
}
