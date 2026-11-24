//! HTML for code and diffs. These are built as strings in Rust, every dynamic piece escaped
//! here, because a diff can have thousands of rows and the templates only need to place it.
//! Highlighting comes from `ch_render`, which has already escaped what it returns.

use crate::fmt;
use ch_core::model::{Diff, FileDiff, FileStatus, Hunk, LineKind};
use ch_render::escape_html;
use std::collections::HashMap;
use std::fmt::Write;

/// How many code lines a page shows before it points at the raw file.
pub const MAX_LINES: usize = 5000;

/// A numbered, highlighted listing. Each row has an `id` of `LC<n>` so `#L12` can scroll and
/// highlight it (the stylesheet uses `:target`).
pub fn code_table(lines: &[String]) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 80).sum::<usize>() + 64);
    out.push_str("<table class=\"code\" data-code><tbody>");
    for (i, l) in lines.iter().enumerate().take(MAX_LINES) {
        let n = i + 1;
        let _ = write!(
            out,
            "<tr id=\"LC{n}\"><td class=\"ln\"><a href=\"#LC{n}\" data-line=\"{n}\" aria-label=\"Line {n}\">{n}</a></td><td class=\"lc\">{l}</td></tr>"
        );
    }
    out.push_str("</tbody></table>");
    out
}

/// One run of blame: its commit shows on the first row only.
pub struct BlameRow {
    pub start: bool,
    pub sha: String,
    pub commit_href: String,
    pub author: String,
    pub ago: String,
    pub summary: String,
}

pub fn blame_table(lines: &[String], rows: &[BlameRow]) -> String {
    let mut out = String::new();
    out.push_str("<table class=\"code blame\" data-code><tbody>");
    for (i, l) in lines.iter().enumerate().take(MAX_LINES) {
        let n = i + 1;
        let b = rows.get(i);
        let class = if b.is_some_and(|b| b.start) { " class=\"blame-start\"" } else { "" };
        let _ = write!(out, "<tr id=\"LC{n}\"{class}>");
        match b {
            Some(b) if b.start => {
                let _ = write!(
                    out,
                    "<td class=\"bl\"><a class=\"bl-sha\" href=\"{}\" title=\"{}\">{}</a><span class=\"bl-who\">{}</span><span class=\"bl-when\">{}</span></td>",
                    escape_html(&b.commit_href),
                    escape_html(&b.summary),
                    escape_html(&fmt::short_sha(&b.sha)),
                    escape_html(&b.author),
                    escape_html(&b.ago)
                );
            }
            _ => out.push_str("<td class=\"bl\"></td>"),
        }
        let _ = write!(out, "<td class=\"ln\"><a href=\"#LC{n}\" data-line=\"{n}\">{n}</a></td><td class=\"lc\">{l}</td></tr>");
    }
    out.push_str("</tbody></table>");
    out
}

// ------------------------------------------------------------------ diffs

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Unified,
    Split,
}

impl DiffMode {
    pub fn from_query(s: Option<&str>) -> Self {
        if s == Some("split") { DiffMode::Split } else { DiffMode::Unified }
    }
}

/// An inline review comment ready to draw under its line. The body arrives already rendered
/// from markdown, which has escaped whatever the author typed.
#[derive(Debug, Clone)]
pub struct InlineComment {
    pub id: i64,
    pub author: String,
    pub ago: String,
    pub body_html: String,
    pub is_reply: bool,
    /// The comment holds a `suggestion` block on the new side of the diff.
    pub suggestion: bool,
}

/// One thread of inline comments. It sits under the line of its first comment, and a reply
/// posts back to that same anchor.
#[derive(Debug, Clone)]
pub struct InlineThread {
    pub id: i64,
    pub path: String,
    /// `left` or `right`.
    pub side: String,
    pub line: u32,
    pub resolved: bool,
    /// Display name of whoever resolved the thread. Empty while it is open.
    pub resolved_by: String,
    pub comments: Vec<InlineComment>,
}

/// Where the forms inside a thread post, and which of them this viewer may use.
#[derive(Default)]
pub struct ThreadForms {
    pub csrf: String,
    pub inline_action: String,
    pub resolve_action: String,
    /// `files` when the forms sit on the files tab, empty on the conversation tab.
    pub back: String,
    pub can_reply: bool,
    pub can_resolve: bool,
    /// Where "Apply suggestion" posts. Empty when this viewer cannot apply one.
    pub apply_action: String,
}

/// Where a diff is shown: decides whether lines get a "comment" button and where threads sit.
#[derive(Default)]
pub struct DiffOptions {
    pub can_comment: bool,
    /// Threads by the `(path, side, line)` of their first comment.
    pub comments: HashMap<(String, String, u32), Vec<InlineThread>>,
    /// The URL a file's "view file" link points at, with `{path}` replaced.
    pub file_url: Option<String>,
    pub forms: ThreadForms,
}

pub struct DiffSummary {
    pub files: usize,
    pub additions: u32,
    pub deletions: u32,
}

pub fn summarize(d: &Diff) -> DiffSummary {
    DiffSummary {
        files: d.files.len(),
        additions: d.files.iter().map(|f| f.additions).sum(),
        deletions: d.files.iter().map(|f| f.deletions).sum(),
    }
}

pub fn file_anchor(path: &str) -> String {
    format!("diff-{}", ch_render::slug(path))
}

fn status_word(s: FileStatus) -> &'static str {
    match s {
        FileStatus::Added => "Added",
        FileStatus::Modified => "Modified",
        FileStatus::Removed => "Removed",
        FileStatus::Renamed => "Renamed",
        FileStatus::Copied => "Copied",
    }
}

pub fn diff_html(diff: &Diff, mode: DiffMode, opts: &DiffOptions) -> String {
    let mut out = String::new();
    for f in &diff.files {
        file_html(&mut out, f, mode, opts);
    }
    if opts.can_comment {
        out.push_str(COMMENT_TEMPLATE);
    }
    out
}

const COMMENT_TEMPLATE: &str = "<template id=\"inline-comment-template\"><tr class=\"inline-form-row\"><td colspan=\"4\"><form method=\"post\" class=\"inline-form stack\" data-inline-form><input type=\"hidden\" name=\"path\"><input type=\"hidden\" name=\"side\"><input type=\"hidden\" name=\"line\"><input type=\"hidden\" name=\"back\" value=\"files\"><label class=\"sr-only\">Comment</label><textarea name=\"body\" rows=\"3\" required placeholder=\"Leave a comment\"></textarea><div class=\"row\"><button class=\"btn btn-primary\" type=\"submit\">Add comment</button><button class=\"btn\" type=\"button\" data-cancel-inline>Cancel</button></div></form></td></tr></template>";

fn file_html(out: &mut String, f: &FileDiff, mode: DiffMode, opts: &DiffOptions) {
    let path = f.new_path.as_deref().or(f.old_path.as_deref()).unwrap_or("(unknown)");
    let title = match (&f.old_path, &f.new_path) {
        (Some(o), Some(n)) if o != n => format!("{} \u{2192} {}", escape_html(o), escape_html(n)),
        _ => escape_html(path),
    };
    let _ = write!(
        out,
        "<section class=\"diff-file\" id=\"{}\"><header class=\"diff-head\"><span class=\"diff-status diff-{}\">{}</span><a class=\"diff-path\" href=\"#{}\">{}</a><span class=\"diff-counts\"><span class=\"add\">+{}</span> <span class=\"del\">-{}</span></span>",
        file_anchor(path),
        status_word(f.status).to_lowercase(),
        status_word(f.status),
        file_anchor(path),
        title,
        f.additions,
        f.deletions
    );
    if let (Some(url), Some(new)) = (&opts.file_url, &f.new_path)
        && f.status != FileStatus::Removed
    {
        let _ = write!(out, "<a class=\"diff-view\" href=\"{}\">View file</a>", escape_html(&url.replace("{path}", &fmt::enc_path(new))));
    }
    out.push_str("</header>");
    if f.binary {
        out.push_str("<p class=\"diff-note muted\">Binary file not shown.</p></section>");
        return;
    }
    if f.collapsed {
        out.push_str("<p class=\"diff-note muted\">This file is too large to show here.</p></section>");
        return;
    }
    if f.hunks.is_empty() {
        out.push_str("<p class=\"diff-note muted\">No content changes.</p></section>");
        return;
    }
    let hl = highlight_hunks(path, &f.hunks);
    match mode {
        DiffMode::Unified => unified(out, path, &f.hunks, &hl, opts),
        DiffMode::Split => split(out, path, &f.hunks, &hl, opts),
    }
    out.push_str("</section>");
}

/// Highlights each hunk as two small files, the old side and the new side, so a string or a
/// comment that spans lines still colours right. Returns one html string per hunk line.
fn highlight_hunks(path: &str, hunks: &[Hunk]) -> Vec<Vec<String>> {
    let total: usize = hunks.iter().map(|h| h.lines.len()).sum();
    hunks
        .iter()
        .map(|h| {
            if total > 4000 {
                return h.lines.iter().map(|l| escape_html(&l.text)).collect();
            }
            let side = |keep: LineKind| -> Vec<String> {
                let text: Vec<&str> =
                    h.lines.iter().filter(|l| l.kind == LineKind::Context || l.kind == keep).map(|l| l.text.as_str()).collect();
                let joined = text.join("\n");
                ch_render::highlight::highlight(&joined, path).lines
            };
            let (old, new) = (side(LineKind::Removed), side(LineKind::Added));
            let (mut oi, mut ni) = (0usize, 0usize);
            h.lines
                .iter()
                .map(|l| match l.kind {
                    LineKind::Context => {
                        oi += 1;
                        ni += 1;
                        new.get(ni - 1).cloned().unwrap_or_else(|| escape_html(&l.text))
                    }
                    LineKind::Added => {
                        ni += 1;
                        new.get(ni - 1).cloned().unwrap_or_else(|| escape_html(&l.text))
                    }
                    LineKind::Removed => {
                        oi += 1;
                        old.get(oi - 1).cloned().unwrap_or_else(|| escape_html(&l.text))
                    }
                })
                .collect()
        })
        .collect()
}

fn num(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

fn comment_rows(out: &mut String, cols: usize, threads: &[InlineThread], forms: &ThreadForms) {
    for t in threads {
        let _ = write!(out, "<tr class=\"inline-thread\"><td colspan=\"{cols}\">{}</td></tr>", thread_html(t, forms));
    }
}

/// One thread as a person interacts with it: the comments, then the forms that answer or
/// resolve them. Shared by the diff rows and the conversation tab. Resolved threads start
/// collapsed, with their state on the summary line.
pub fn thread_html(t: &InlineThread, f: &ThreadForms) -> String {
    let n = t.comments.len();
    let mut out = format!("<div class=\"thread{}\" id=\"thread-{}\">", if t.resolved { " resolved" } else { "" }, t.id);
    let open = if t.resolved { "" } else { " open" };
    let _ = write!(out, "<details class=\"thread-comments\"{open}><summary class=\"thread-summary\">");
    if t.resolved {
        let by = if t.resolved_by.is_empty() { String::new() } else { format!(" by {}", escape_html(&t.resolved_by)) };
        let _ = write!(out, "<span class=\"chip\">Resolved</span>{by} ");
    }
    let _ = write!(out, "{n} comment{}", if n == 1 { "" } else { "s" });
    if t.resolved {
        out.push_str(", resolved");
    }
    out.push_str("</summary>");
    for c in &t.comments {
        let _ = write!(
            out,
            "<article class=\"inline-comment{}\" id=\"rc{}\"><header><strong>{}</strong> <span class=\"muted\">{}</span></header><div class=\"markdown\">{}</div>",
            if c.is_reply { " reply" } else { "" },
            c.id,
            escape_html(&c.author),
            escape_html(&c.ago),
            c.body_html
        );
        // The service checks write access on the head branch and that the code has not moved.
        if c.suggestion && !t.resolved && !f.apply_action.is_empty() {
            let id = c.id.to_string();
            let _ = write!(out, "<form method=\"post\" action=\"{}\" class=\"apply-suggestion\">", escape_html(&f.apply_action));
            for (name, value) in [("_csrf", f.csrf.as_str()), ("comment", id.as_str()), ("back", f.back.as_str())] {
                let _ = write!(out, "<input type=\"hidden\" name=\"{name}\" value=\"{}\">", escape_html(value));
            }
            out.push_str("<button class=\"btn small\" type=\"submit\">Apply suggestion</button></form>");
        }
        out.push_str("</article>");
    }
    out.push_str("</details><div class=\"thread-actions\">");
    // A reply keeps the thread: it posts to the first comment, at its own anchor.
    if f.can_reply
        && let Some(root) = t.comments.first()
    {
        let (line, root_id) = (t.line.to_string(), root.id.to_string());
        let _ = write!(
            out,
            "<details class=\"thread-reply\"><summary class=\"btn small\">Reply</summary><form method=\"post\" action=\"{}\" class=\"inline-form stack\">",
            escape_html(&f.inline_action)
        );
        for (name, value) in [
            ("_csrf", f.csrf.as_str()),
            ("path", t.path.as_str()),
            ("side", t.side.as_str()),
            ("line", line.as_str()),
            ("in_reply_to", root_id.as_str()),
            ("back", f.back.as_str()),
        ] {
            let _ = write!(out, "<input type=\"hidden\" name=\"{name}\" value=\"{}\">", escape_html(value));
        }
        out.push_str("<label class=\"sr-only\">Reply</label><textarea name=\"body\" rows=\"3\" required placeholder=\"Reply\"></textarea>");
        out.push_str("<div class=\"row\"><button class=\"btn btn-primary\" type=\"submit\">Reply</button></div></form></details>");
    }
    if f.can_resolve {
        let (resolved, word, id) = (if t.resolved { "0" } else { "1" }, if t.resolved { "Unresolve" } else { "Resolve" }, t.id.to_string());
        let _ = write!(out, "<form method=\"post\" action=\"{}\" class=\"thread-resolve\">", escape_html(&f.resolve_action));
        for (name, value) in [("_csrf", f.csrf.as_str()), ("thread", id.as_str()), ("resolved", resolved), ("back", f.back.as_str())] {
            let _ = write!(out, "<input type=\"hidden\" name=\"{name}\" value=\"{}\">", escape_html(value));
        }
        let _ = write!(out, "<button class=\"btn small\" type=\"submit\">{word}</button></form>");
    }
    out.push_str("</div></div>");
    out
}

fn unified(out: &mut String, path: &str, hunks: &[Hunk], hl: &[Vec<String>], opts: &DiffOptions) {
    out.push_str("<table class=\"diff diff-unified\" tabindex=\"0\"><tbody>");
    for (h, lines) in hunks.iter().zip(hl) {
        let _ = write!(
            out,
            "<tr class=\"hunk\"><td class=\"ln\"></td><td class=\"ln\"></td><td class=\"sign\"></td><td class=\"lc hunk-head\">{}</td></tr>",
            escape_html(&h.header)
        );
        for (l, html) in h.lines.iter().zip(lines) {
            let (class, sign, word) = match l.kind {
                LineKind::Context => ("ctx", " ", "unchanged"),
                LineKind::Added => ("add", "+", "added"),
                LineKind::Removed => ("del", "-", "removed"),
            };
            let (side, line) = if l.kind == LineKind::Removed { ("left", l.old_no) } else { ("right", l.new_no) };
            let button = match (opts.can_comment, line) {
                (true, Some(n)) => format!(
                    "<button type=\"button\" class=\"line-comment-btn\" data-comment-path=\"{}\" data-comment-side=\"{side}\" data-comment-line=\"{n}\" aria-label=\"Comment on line {n}\">+</button>",
                    escape_html(path)
                ),
                _ => String::new(),
            };
            let _ = write!(
                out,
                "<tr class=\"{class}\"><td class=\"ln\">{}</td><td class=\"ln\">{}{button}</td><td class=\"sign\" aria-hidden=\"true\">{sign}</td><td class=\"lc\"><span class=\"sr-only\">{word}: </span>{html}</td></tr>",
                num(l.old_no),
                num(l.new_no)
            );
            if let Some(n) = line
                && let Some(cs) = opts.comments.get(&(path.to_string(), side.to_string(), n))
            {
                comment_rows(out, 4, cs, &opts.forms);
            }
        }
    }
    out.push_str("</tbody></table>");
}

fn split(out: &mut String, path: &str, hunks: &[Hunk], hl: &[Vec<String>], opts: &DiffOptions) {
    out.push_str("<table class=\"diff diff-split\" tabindex=\"0\"><tbody>");
    for (h, lines) in hunks.iter().zip(hl) {
        let _ = write!(out, "<tr class=\"hunk\"><td colspan=\"4\" class=\"lc hunk-head\">{}</td></tr>", escape_html(&h.header));
        let mut i = 0;
        while i < h.lines.len() {
            let l = &h.lines[i];
            match l.kind {
                LineKind::Context => {
                    let cell = format!("<td class=\"ln\">{}</td><td class=\"lc\">{}</td>", num(l.old_no), lines[i]);
                    let cell2 = format!("<td class=\"ln\">{}</td><td class=\"lc\">{}</td>", num(l.new_no), lines[i]);
                    let _ = write!(out, "<tr class=\"ctx\">{cell}{cell2}</tr>");
                    i += 1;
                }
                _ => {
                    // Pair a run of removals with the run of additions that follows.
                    let mut dels = Vec::new();
                    while i < h.lines.len() && h.lines[i].kind == LineKind::Removed {
                        dels.push(i);
                        i += 1;
                    }
                    let mut adds = Vec::new();
                    while i < h.lines.len() && h.lines[i].kind == LineKind::Added {
                        adds.push(i);
                        i += 1;
                    }
                    for k in 0..dels.len().max(adds.len()) {
                        let left = dels.get(k).map(|&j| (&h.lines[j], &lines[j]));
                        let right = adds.get(k).map(|&j| (&h.lines[j], &lines[j]));
                        let cell = |c: Option<(&ch_core::model::DiffLine, &String)>, class: &str, side: &str| match c {
                            Some((l, html)) => {
                                let n = if side == "left" { l.old_no } else { l.new_no };
                                let button = match (opts.can_comment, n) {
                                    (true, Some(n)) => format!(
                                        "<button type=\"button\" class=\"line-comment-btn\" data-comment-path=\"{}\" data-comment-side=\"{side}\" data-comment-line=\"{n}\" aria-label=\"Comment on line {n}\">+</button>",
                                        escape_html(path)
                                    ),
                                    _ => String::new(),
                                };
                                format!("<td class=\"ln {class}\">{}{button}</td><td class=\"lc {class}\">{html}</td>", num(n))
                            }
                            None => "<td class=\"ln blank\"></td><td class=\"lc blank\"></td>".to_string(),
                        };
                        let _ = write!(out, "<tr>{}{}</tr>", cell(left, "del", "left"), cell(right, "add", "right"));
                        for (c, side) in [(left, "left"), (right, "right")] {
                            if let Some((l, _)) = c {
                                let n = if side == "left" { l.old_no } else { l.new_no };
                                if let Some(n) = n
                                    && let Some(cs) = opts.comments.get(&(path.to_string(), side.to_string(), n))
                                {
                                    comment_rows(out, 4, cs, &opts.forms);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out.push_str("</tbody></table>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::model::DiffLine;

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine { kind, old_no: old, new_no: new, text: text.into() }
    }

    fn sample() -> Diff {
        Diff {
            files: vec![FileDiff {
                status: FileStatus::Modified,
                old_path: Some("src/a.rs".into()),
                new_path: Some("src/a.rs".into()),
                binary: false,
                collapsed: false,
                additions: 1,
                deletions: 1,
                hunks: vec![Hunk {
                    header: "@@ -1,2 +1,2 @@".into(),
                    lines: vec![
                        line(LineKind::Context, Some(1), Some(1), "fn main() {"),
                        line(LineKind::Removed, Some(2), None, "    println!(\"<old>\");"),
                        line(LineKind::Added, None, Some(2), "    println!(\"<new>\");"),
                    ],
                }],
            }],
        }
    }

    #[test]
    fn code_table_numbers_rows_and_caps() {
        let lines: Vec<String> = (0..MAX_LINES + 10).map(|i| format!("l{i}")).collect();
        let html = code_table(&lines);
        assert!(html.contains("id=\"LC1\""));
        assert!(html.contains(&format!("id=\"LC{MAX_LINES}\"")));
        assert!(!html.contains(&format!("id=\"LC{}\"", MAX_LINES + 1)));
    }

    #[test]
    fn unified_diff_labels_lines_for_screen_readers_and_escapes() {
        let html = diff_html(&sample(), DiffMode::Unified, &DiffOptions::default());
        assert!(html.contains("<span class=\"sr-only\">added: </span>"));
        assert!(html.contains("<span class=\"sr-only\">removed: </span>"));
        assert!(!html.contains("<old>") && !html.contains("<new>"));
        assert!(html.contains("&lt;new&gt;") || html.contains("lt;new"));
        assert!(!html.contains("line-comment-btn"));
    }

    #[test]
    fn split_diff_pairs_removed_with_added() {
        let html = diff_html(&sample(), DiffMode::Split, &DiffOptions::default());
        assert!(html.contains("diff-split"));
        assert_eq!(html.matches("<tr><td class=\"ln del\">").count(), 1);
    }

    #[test]
    fn comment_buttons_and_threads_appear_only_when_allowed() {
        let mut opts = DiffOptions { can_comment: true, ..Default::default() };
        opts.comments.insert(("src/a.rs".into(), "right".into(), 2), vec![thread(9, false)]);
        let html = diff_html(&sample(), DiffMode::Unified, &opts);
        assert!(html.contains("data-comment-line=\"2\""));
        assert!(html.contains("id=\"rc31\""));
        assert!(html.contains("inline-comment-template"));
    }

    fn thread(id: i64, resolved: bool) -> InlineThread {
        InlineThread {
            id,
            path: "src/a.rs".into(),
            side: "right".into(),
            line: 2,
            resolved,
            resolved_by: if resolved { "Ada Marlow<script>".into() } else { String::new() },
            comments: vec![
                InlineComment {
                    id: 31,
                    author: "bob<script>".into(),
                    ago: "now".into(),
                    body_html: "<p>first</p>".into(),
                    is_reply: false,
                    suggestion: true,
                },
                InlineComment {
                    id: 32,
                    author: "cleo".into(),
                    ago: "now".into(),
                    body_html: "<p>second</p>".into(),
                    is_reply: true,
                    suggestion: false,
                },
            ],
        }
    }

    fn forms() -> ThreadForms {
        ThreadForms {
            csrf: "tok".into(),
            inline_action: "/o/r/pulls/14/inline".into(),
            resolve_action: "/o/r/pulls/14/resolve".into(),
            back: "files".into(),
            can_reply: true,
            can_resolve: true,
            apply_action: "/o/r/pulls/14/suggestion".into(),
        }
    }

    #[test]
    fn a_resolved_thread_collapses_with_reply_and_unresolve_forms() {
        let html = thread_html(&thread(41, true), &forms());
        // The reply keeps the thread: it posts to the first comment, at its own anchor.
        assert!(html.contains("<form method=\"post\" action=\"/o/r/pulls/14/inline\" "));
        for hidden in [
            "name=\"_csrf\" value=\"tok\"",
            "name=\"path\" value=\"src/a.rs\"",
            "name=\"side\" value=\"right\"",
            "name=\"line\" value=\"2\"",
            "name=\"in_reply_to\" value=\"31\"",
            "name=\"back\" value=\"files\"",
        ] {
            assert!(html.contains(&format!("<input type=\"hidden\" {hidden}>")), "missing {hidden}");
        }
        assert!(html.contains("<textarea name=\"body\"") && html.contains("placeholder=\"Reply\""));
        assert!(html.contains("<label class=\"sr-only\">Reply</label>"));
        // A resolved thread offers unresolve.
        assert!(html.contains("<form method=\"post\" action=\"/o/r/pulls/14/resolve\" "));
        assert!(html.contains("name=\"thread\" value=\"41\""));
        assert!(html.contains("name=\"resolved\" value=\"0\""));
        assert!(html.contains(">Unresolve</button>"));
        // It starts collapsed, with the state on the one line summary.
        assert!(html.contains("<details class=\"thread-comments\">"));
        assert!(html.contains("2 comments, resolved"));
        assert!(html.contains("<span class=\"chip\">Resolved</span>"));
        assert!(html.contains("by Ada Marlow&lt;script&gt;"));
        // Names a person controls are escaped; comment bodies arrive pre-rendered.
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn an_open_thread_opens_by_default_and_offers_resolve() {
        let html = thread_html(&thread(42, false), &forms());
        assert!(html.contains("<details class=\"thread-comments\" open>"));
        assert!(html.contains("2 comments</summary>"));
        assert!(html.contains("name=\"thread\" value=\"42\""));
        assert!(html.contains("name=\"resolved\" value=\"1\""));
        assert!(html.contains(">Resolve</button>"));
        assert!(!html.contains("chip\">Resolved"));
    }

    #[test]
    fn a_suggestion_can_be_applied_from_an_open_thread_only() {
        let html = thread_html(&thread(42, false), &forms());
        assert_eq!(html.matches(">Apply suggestion</button>").count(), 1, "only the comment that holds one");
        assert!(html.contains("<form method=\"post\" action=\"/o/r/pulls/14/suggestion\" class=\"apply-suggestion\">"));
        assert!(html.contains("name=\"comment\" value=\"31\""));
        assert!(!thread_html(&thread(41, true), &forms()).contains("Apply suggestion"), "not once resolved");
        let f = ThreadForms { apply_action: String::new(), ..forms() };
        assert!(!thread_html(&thread(42, false), &f).contains("Apply suggestion"), "not without write access");
    }

    #[test]
    fn a_viewer_who_cannot_reply_or_resolve_gets_no_forms() {
        let f = ThreadForms { can_reply: false, can_resolve: false, ..forms() };
        let html = thread_html(&thread(41, true), &f);
        assert!(!html.contains("name=\"in_reply_to\""));
        assert!(!html.contains("name=\"thread\""));
        assert!(html.contains("id=\"rc31\""), "the comments are still shown");
    }

    #[test]
    fn a_thread_inside_a_diff_carries_both_forms() {
        let mut opts = DiffOptions { can_comment: true, forms: forms(), ..Default::default() };
        opts.comments.insert(("src/a.rs".into(), "right".into(), 2), vec![thread(41, false)]);
        let html = diff_html(&sample(), DiffMode::Unified, &opts);
        assert!(html.contains("inline-thread"));
        assert!(html.contains("name=\"in_reply_to\" value=\"31\""));
        assert!(html.contains("name=\"thread\" value=\"41\""));
        assert!(html.contains("thread-reply"));
    }

    #[test]
    fn binary_and_collapsed_files_say_so() {
        let mut d = sample();
        d.files[0].binary = true;
        assert!(diff_html(&d, DiffMode::Unified, &DiffOptions::default()).contains("Binary file"));
        let mut d = sample();
        d.files[0].collapsed = true;
        assert!(diff_html(&d, DiffMode::Unified, &DiffOptions::default()).contains("too large"));
    }

    #[test]
    fn blame_shows_commit_on_first_row_of_a_run() {
        let rows = vec![
            BlameRow {
                start: true,
                sha: "abcdef123".into(),
                commit_href: "/c/abcdef123".into(),
                author: "Ada".into(),
                ago: "2 days ago".into(),
                summary: "init".into(),
            },
            BlameRow {
                start: false,
                sha: "abcdef123".into(),
                commit_href: "/c/abcdef123".into(),
                author: "Ada".into(),
                ago: "2 days ago".into(),
                summary: "init".into(),
            },
        ];
        let html = blame_table(&["a".into(), "b".into()], &rows);
        assert_eq!(html.matches("bl-sha").count(), 1);
        assert!(html.contains("abcdef1"));
    }
}
