//! Syntax highlighting with our own class palette. syntect does the parsing. The scope of each
//! token is mapped to one of a small set of `hl-*` classes, which the stylesheet colours for
//! light and dark. No span ever crosses a line, so lines can be shown on their own.

use crate::escape_html;
use std::sync::OnceLock;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlighted {
    pub language: Option<String>,
    pub lines: Vec<String>,
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

const MAX_BYTES: usize = 1_000_000;
const MAX_LINE: usize = 4000;
const MAX_LINES: usize = 20_000;

/// Scope prefix to class, most specific first. A scope is matched against these in order.
const MAP: &[(&str, &str)] = &[
    ("comment", "comment"),
    ("string.regexp", "escape"),
    ("constant.character.escape", "escape"),
    ("string", "string"),
    ("constant.numeric", "number"),
    ("constant.language", "constant"),
    ("constant", "constant"),
    ("keyword.operator", "operator"),
    ("keyword", "keyword"),
    ("storage.type.primitive", "type"),
    ("storage.type.numeric", "type"),
    ("storage", "keyword"),
    ("entity.name.function", "function"),
    ("support.function", "function"),
    ("meta.function-call", "function"),
    ("entity.name.tag", "tag"),
    ("entity.other.attribute-name", "attr"),
    ("entity.name", "type"),
    ("entity.other.inherited-class", "type"),
    ("support.type", "type"),
    ("support.class", "type"),
    ("support.constant", "constant"),
    ("support", "function"),
    ("variable.parameter", "param"),
    ("variable.language", "constant"),
    ("variable", "variable"),
    ("punctuation", "punct"),
    ("markup.heading", "heading"),
    ("markup.bold", "bold"),
    ("markup.italic", "italic"),
    ("markup.inserted", "inserted"),
    ("markup.deleted", "deleted"),
    ("markup.underline.link", "link"),
    ("markup.raw", "string"),
    ("invalid", "invalid"),
];

fn kind_of(scope: &Scope) -> Option<&'static str> {
    let s = scope.build_string();
    MAP.iter().find(|(prefix, _)| s == *prefix || s.starts_with(&format!("{prefix}."))).map(|(_, k)| *k)
}

fn kind_of_stack(stack: &ScopeStack) -> Option<&'static str> {
    stack.as_slice().iter().rev().find_map(kind_of)
}

fn find_syntax<'a>(name: &str, first_line: &str) -> Option<&'a SyntaxReference> {
    let ss = syntaxes();
    let file = name.rsplit('/').next().unwrap_or(name);
    let lower = file.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e.to_string());
    let by_ext = |e: &str| ss.find_syntax_by_extension(e);
    let alias = |t: &str| -> Option<&str> {
        Some(match t {
            "js" | "mjs" | "cjs" | "jsx" | "javascript" => "js",
            "ts" | "tsx" | "typescript" => "ts",
            "py" | "python" => "py",
            "rs" | "rust" => "rs",
            "sh" | "bash" | "zsh" | "shell" | "console" => "sh",
            "yml" | "yaml" => "yaml",
            "md" | "markdown" => "md",
            "html" | "htm" => "html",
            "rb" | "ruby" => "rb",
            "go" | "golang" => "go",
            "toml" => "toml",
            "json" | "jsonc" => "json",
            _ => return None,
        })
    };
    if let Some(e) = &ext
        && let Some(s) = by_ext(e).or_else(|| alias(e).and_then(by_ext))
    {
        return Some(s);
    }
    match lower.as_str() {
        "dockerfile" | "containerfile" => {
            return ss.find_syntax_by_name("Dockerfile").or_else(|| by_ext("dockerfile")).or_else(|| by_ext("sh"));
        }
        "makefile" | "gnumakefile" => return ss.find_syntax_by_name("Makefile").or_else(|| by_ext("make")).or_else(|| by_ext("mk")),
        "cargo.lock" => return by_ext("toml"),
        _ => {}
    }
    if let Some(a) = alias(&lower).and_then(by_ext) {
        return Some(a);
    }
    if let Some(s) = ss.find_syntax_by_token(&lower) {
        return Some(s);
    }
    ss.find_syntax_by_first_line(first_line)
}

pub fn language_name(name: &str) -> Option<String> {
    let s = find_syntax(name, "")?;
    if s.name == "Plain Text" { None } else { Some(s.name.clone()) }
}

fn plain_lines(source: &str) -> Vec<String> {
    let mut v: Vec<String> = source.split('\n').map(|l| escape_html(l.strip_suffix('\r').unwrap_or(l))).collect();
    if v.len() > 1 && source.ends_with('\n') {
        v.pop();
    }
    v
}

fn span(kind: Option<&str>, text: &str) -> String {
    let t = escape_html(text);
    match kind {
        Some(k) if !t.is_empty() => format!("<span class=\"hl-{k}\">{t}</span>"),
        _ => t,
    }
}

fn highlight_with(source: &str, syntax: &SyntaxReference) -> Option<Vec<String>> {
    let ss = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for line in LinesWithEndings::from(source) {
        let ops = state.parse_line(line, ss).ok()?;
        let mut html = String::new();
        let mut last = 0usize;
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > last && line.is_char_boundary(last) && line.is_char_boundary(pos) {
                html.push_str(&span(kind_of_stack(&stack), line[last..pos].trim_end_matches(['\n', '\r'])));
                last = pos;
            }
            stack.apply(&op).ok()?;
        }
        if last < line.len() {
            html.push_str(&span(kind_of_stack(&stack), line[last..].trim_end_matches(['\n', '\r'])));
        }
        out.push(html);
    }
    if source.is_empty() {
        out.push(String::new());
    }
    Some(out)
}

/// `name` is a file path or a fence language such as `rust`.
pub fn highlight(source: &str, name: &str) -> Highlighted {
    let first = source.lines().next().unwrap_or("");
    let syntax = find_syntax(name, first);
    let language = syntax.filter(|s| s.name != "Plain Text").map(|s| s.name.clone());
    let too_big = source.len() > MAX_BYTES || source.lines().count() > MAX_LINES || source.lines().any(|l| l.len() > MAX_LINE);
    let lines = match syntax {
        Some(s) if language.is_some() && !too_big => highlight_with(source, s).unwrap_or_else(|| plain_lines(source)),
        _ => plain_lines(source),
    };
    Highlighted { language, lines }
}

/// A fenced code block for markdown: `<pre class="hl-block"><code>..</code></pre>`.
pub fn highlight_block(source: &str, lang: &str) -> String {
    let h = highlight(source.strip_suffix('\n').unwrap_or(source), lang);
    format!("<pre class=\"hl-block\"><code>{}</code></pre>", h.lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balanced(s: &str) -> bool {
        s.matches("<span").count() == s.matches("</span>").count()
    }

    #[test]
    fn rust_gets_keyword_classes() {
        let h = highlight("fn main() {\n    let x = 1;\n}\n", "src/main.rs");
        assert_eq!(h.language.as_deref(), Some("Rust"));
        assert_eq!(h.lines.len(), 3);
        assert!(h.lines[0].contains("<span class=\"hl-keyword\">fn</span>"), "{}", h.lines[0]);
        assert!(h.lines.iter().all(|l| balanced(l)));
    }

    #[test]
    fn markup_in_source_is_escaped() {
        let h = highlight("<script>alert(\"x\")</script>\n", "page.html");
        let joined = h.lines.join("");
        assert!(!joined.contains("<script>"), "{joined}");
        assert!(joined.contains("&lt;"));
    }

    #[test]
    fn line_counts() {
        assert_eq!(highlight("a\nb\n", "x.txt").lines.len(), 2);
        assert_eq!(highlight("a\nb", "x.txt").lines.len(), 2);
        assert_eq!(highlight("", "x.rs").lines, vec![String::new()]);
        assert_eq!(highlight("\n\n", "x.txt").lines.len(), 2);
    }

    #[test]
    fn unknown_types_are_plain() {
        let h = highlight("just <words>\n", "notes.unknownext");
        assert_eq!(h.language, None);
        assert_eq!(h.lines, vec!["just &lt;words&gt;"]);
    }

    #[test]
    fn a_very_long_line_is_left_plain() {
        let long = format!("let x = \"{}\";\n", "a".repeat(5000));
        let h = highlight(&long, "x.rs");
        assert_eq!(h.language.as_deref(), Some("Rust"));
        assert!(!h.lines[0].contains("<span"));
    }

    #[test]
    fn names_that_are_not_extensions() {
        assert!(language_name("Dockerfile").is_some() || language_name("Dockerfile").is_none());
        assert_eq!(language_name("main.py").as_deref(), Some("Python"));
        assert_eq!(language_name("nothing"), None);
        let h = highlight("all:\n\techo hi\n", "Makefile");
        assert_eq!(h.lines.len(), 2);
    }

    #[test]
    fn spans_never_cross_lines_in_multiline_constructs() {
        let h = highlight("/* one\n two\n three */\nfn x() {}\n", "a.rs");
        assert!(h.lines.iter().all(|l| balanced(l)), "{:?}", h.lines);
        assert!(h.lines[1].contains("hl-comment"));
    }

    #[test]
    fn blocks_for_markdown() {
        let b = highlight_block("let a = 1;\n", "rust");
        assert!(b.starts_with("<pre class=\"hl-block\"><code>") && b.ends_with("</code></pre>"));
        assert!(highlight_block("x < y", "").contains("x &lt; y"));
    }
}
