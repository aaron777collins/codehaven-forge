//! GitHub flavoured markdown to sanitized HTML.
//!
//! The page's Content-Security-Policy allows no inline style, no script and only same-origin
//! images, so the output is built for that: code blocks carry highlight classes, off-site
//! images become links, task list boxes are plain `<input>`s, and ammonia removes whatever a
//! writer could use to run script or restyle the page.

use crate::{escape_html, highlight, slug};
use ammonia::{Builder, UrlRelative};
use pulldown_cmark::{CodeBlockKind, CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct MdContext {
    /// Repository owner, "" outside a repository.
    pub owner: String,
    pub repo: String,
    /// The ref relative links resolve against. "" turns relative link rewriting off.
    pub git_ref: String,
    /// Directory of the markdown file inside the repo: "" for the root, no slashes at the ends.
    pub dir: String,
    pub link_refs: bool,
    pub heading_ids: bool,
}

fn has_scheme(url: &str) -> bool {
    let mut chars = url.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return false;
        }
    }
    false
}

fn is_relative(url: &str) -> bool {
    !url.is_empty() && !has_scheme(url) && !url.starts_with('/') && !url.starts_with('#') && !url.starts_with("//")
}

/// Resolves `rel` against `dir`, never going above the repository root. Returns the path and
/// whether it ended with a slash.
pub fn resolve_path(dir: &str, rel: &str) -> (String, bool) {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let trailing = rel.ends_with('/');
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    (parts.join("/"), trailing)
}

fn encode_path(p: &str) -> String {
    let mut out = String::new();
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn rewrite_relative(url: &str, ctx: &MdContext, image: bool) -> String {
    let (path_part, frag) = match url.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (url, None),
    };
    let path_part = path_part.split('?').next().unwrap_or("");
    let (path, trailing) = resolve_path(&ctx.dir, path_part);
    let kind = if image {
        "raw"
    } else if trailing {
        "tree"
    } else {
        "blob"
    };
    let mut out = format!("/{}/{}/{kind}/{}/{}", ctx.owner, ctx.repo, encode_path(&ctx.git_ref), encode_path(&path));
    if out.ends_with('/') {
        out.pop();
    }
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

// ------------------------------------------------------------------ references in text

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// Splits plain text into text and link pieces. Each candidate is checked with
/// `ch_core::text::refs`, so linking agrees with what notifications treat as a reference.
fn linkify(text: &str, ctx: &MdContext) -> Vec<(bool, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(bool, String)> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    let in_repo = !ctx.owner.is_empty() && !ctx.repo.is_empty();
    let flush = |plain: &mut String, out: &mut Vec<(bool, String)>| {
        if !plain.is_empty() {
            out.push((false, std::mem::take(plain)));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let boundary = prev.is_none_or(|p| !is_word(p) && p != '/' && p != '@' && p != '#');
        if c == '@' && boundary {
            let mut j = i + 1;
            while j < chars.len() && name_char(chars[j]) {
                j += 1;
            }
            while j > i + 1 && chars[j - 1] == '.' {
                j -= 1;
            }
            let name: String = chars[i + 1..j].iter().collect();
            let cand: String = chars[i..j].iter().collect();
            if !name.is_empty() && name.len() <= 39 && !name.contains('/') && !ch_core::text::refs(&cand).mentions.is_empty() {
                flush(&mut plain, &mut out);
                out.push((true, format!("<a class=\"mention\" href=\"/{}\">@{}</a>", encode_path(&name), escape_html(&name))));
                i = j;
                continue;
            }
        }
        if c == '#' && boundary && in_repo {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let after_ok = chars.get(j).is_none_or(|a| !is_word(*a));
            let cand: String = chars[i..j].iter().collect();
            if j > i + 1 && after_ok && !ch_core::text::refs(&cand).issues.is_empty() {
                flush(&mut plain, &mut out);
                out.push((
                    true,
                    format!(
                        "<a class=\"issue-ref\" href=\"/{}/{}/issues/{}\">{}</a>",
                        encode_path(&ctx.owner),
                        encode_path(&ctx.repo),
                        &cand[1..],
                        escape_html(&cand)
                    ),
                ));
                i = j;
                continue;
            }
        }
        if is_word(c) && boundary {
            // owner/repo#12 and commit shas start at a word.
            let mut j = i;
            while j < chars.len() && (name_char(chars[j]) || chars[j] == '/') {
                j += 1;
            }
            let mut k = j;
            if chars.get(j) == Some(&'#') {
                k = j + 1;
                while k < chars.len() && chars[k].is_ascii_digit() {
                    k += 1;
                }
            }
            let word: String = chars[i..j].iter().collect();
            if k > j + 1 && word.contains('/') && chars.get(k).is_none_or(|a| !is_word(*a)) {
                let cand: String = chars[i..k].iter().collect();
                let refs = ch_core::text::refs(&cand);
                if let Some(r) = refs.issues.first()
                    && let Some((o, rp)) = &r.repo
                {
                    flush(&mut plain, &mut out);
                    out.push((
                        true,
                        format!(
                            "<a class=\"issue-ref\" href=\"/{}/{}/issues/{}\">{}</a>",
                            encode_path(o),
                            encode_path(rp),
                            r.number,
                            escape_html(&cand)
                        ),
                    ));
                    i = k;
                    continue;
                }
            }
            let hex = (7..=40).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_hexdigit());
            let mixed = word.bytes().any(|b| b.is_ascii_digit()) && word.bytes().any(|b| (b'a'..=b'f').contains(&b.to_ascii_lowercase()));
            if hex && mixed && in_repo && chars.get(j).is_none_or(|a| !is_word(*a)) && !ch_core::text::refs(&word).commits.is_empty() {
                flush(&mut plain, &mut out);
                let lower = word.to_lowercase();
                out.push((
                    true,
                    format!(
                        "<a class=\"sha-ref\" href=\"/{}/{}/commit/{lower}\"><code>{}</code></a>",
                        encode_path(&ctx.owner),
                        encode_path(&ctx.repo),
                        &lower[..7]
                    ),
                ));
                i = j;
                continue;
            }
            // Not a reference: keep the whole word as text so its inside is not rescanned.
            for ch in &chars[i..j.max(i + 1)] {
                plain.push(*ch);
            }
            i = j.max(i + 1);
            continue;
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut out);
    out
}

// ------------------------------------------------------------------ rendering

const HL_CLASSES: &[&str] = &[
    "hl-comment",
    "hl-string",
    "hl-number",
    "hl-keyword",
    "hl-function",
    "hl-type",
    "hl-constant",
    "hl-variable",
    "hl-param",
    "hl-operator",
    "hl-punct",
    "hl-tag",
    "hl-attr",
    "hl-heading",
    "hl-bold",
    "hl-italic",
    "hl-escape",
    "hl-link",
    "hl-inserted",
    "hl-deleted",
    "hl-invalid",
];

fn sanitizer() -> Builder<'static> {
    let mut b = Builder::default();
    b.link_rel(Some("noopener noreferrer nofollow"));
    b.url_relative(UrlRelative::PassThrough);
    b.url_schemes(HashSet::from(["http", "https", "mailto"]));
    b.strip_comments(true);
    b.add_tags(["input", "kbd", "details", "summary", "section"]);
    b.add_tag_attributes("input", ["type", "checked", "disabled"]);
    b.add_tag_attributes("div", ["align"]);
    b.add_tag_attributes("p", ["align"]);
    for h in ["h1", "h2", "h3", "h4", "h5", "h6"] {
        b.add_tag_attributes(h, ["id"]);
        b.add_allowed_classes(h, [] as [&str; 0]);
    }
    b.add_tag_attributes("a", ["id"]);
    b.add_tag_attributes("li", ["id"]);
    b.add_allowed_classes("span", HL_CLASSES.iter().copied());
    b.add_allowed_classes("a", ["mention", "issue-ref", "sha-ref", "anchor"]);
    b.add_allowed_classes("pre", ["hl-block"]);
    b.add_allowed_classes("div", ["table-wrap", "suggestion"]);
    b.add_allowed_classes("p", ["suggestion-label"]);
    // Off-site images are never loaded: the policy blocks them anyway, and dropping the
    // source means a stray `<img>` cannot even try.
    b.attribute_filter(|tag, attr, value| {
        if tag == "img" && attr == "src" && !value.starts_with('/') {
            return None;
        }
        Some(value.into())
    });
    b
}

fn heading_level(l: pulldown_cmark::HeadingLevel) -> usize {
    l as usize
}

pub fn render(src: &str, ctx: &MdContext) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(src, opts);
    let mut out: Vec<Event> = Vec::new();
    let mut code: Option<(String, String)> = None;
    let mut link_depth = 0usize;
    let mut image_depth = 0usize;
    let mut image_as_link: Vec<bool> = Vec::new();
    let mut heading: Option<(usize, usize, String)> = None;
    let mut used_ids: HashMap<String, usize> = HashMap::new();

    for ev in parser {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, body)) = code.take() {
                    // A review's ```suggestion block is the change it proposes, so it says so.
                    if lang == "suggestion" {
                        out.push(Event::Html(CowStr::from(format!(
                            "<div class=\"suggestion\"><p class=\"suggestion-label\">Suggested change</p>{}</div>",
                            highlight::highlight_block(&body, "")
                        ))));
                    } else {
                        out.push(Event::Html(CowStr::from(highlight::highlight_block(&body, &lang))));
                    }
                }
            }
            Event::Text(t) if code.is_some() => {
                if let Some((_, body)) = code.as_mut() {
                    body.push_str(&t);
                }
            }
            Event::Start(Tag::Heading { level, .. }) if ctx.heading_ids => {
                heading = Some((out.len(), heading_level(level), String::new()));
                out.push(Event::Text(CowStr::from("")));
            }
            Event::End(TagEnd::Heading(level)) if heading.is_some() => {
                if let Some((at, _, text)) = heading.take() {
                    let base = slug(&text);
                    let base = if base.is_empty() { "section".to_string() } else { base };
                    let n = used_ids.entry(base.clone()).or_insert(0);
                    let id = if *n == 0 { base.clone() } else { format!("{base}-{n}") };
                    *n += 1;
                    let lv = heading_level(level);
                    out[at] = Event::Html(CowStr::from(format!("<h{lv} id=\"{}\">", escape_html(&id))));
                    out.push(Event::Html(CowStr::from(format!(
                        "<a class=\"anchor\" href=\"#{}\" aria-label=\"Link to this section\">#</a></h{lv}>\n",
                        escape_html(&id)
                    ))));
                }
            }
            Event::Start(Tag::Table(al)) => {
                out.push(Event::Html(CowStr::from("<div class=\"table-wrap\">")));
                out.push(Event::Start(Tag::Table(al)));
            }
            Event::End(TagEnd::Table) => {
                out.push(Event::End(TagEnd::Table));
                out.push(Event::Html(CowStr::from("</div>")));
            }
            Event::TaskListMarker(done) => {
                out.push(Event::Html(CowStr::from(if done {
                    "<input type=\"checkbox\" disabled checked> "
                } else {
                    "<input type=\"checkbox\" disabled> "
                })));
            }
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
                link_depth += 1;
                let dest = if is_relative(&dest_url) && !ctx.git_ref.is_empty() && !ctx.owner.is_empty() {
                    rewrite_relative(&dest_url, ctx, false)
                } else {
                    dest_url.to_string()
                };
                out.push(Event::Start(Tag::Link { link_type, dest_url: CowStr::from(dest), title, id }));
            }
            Event::End(TagEnd::Link) => {
                link_depth = link_depth.saturating_sub(1);
                out.push(Event::End(TagEnd::Link));
            }
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                image_depth += 1;
                let rel = is_relative(&dest_url) || dest_url.starts_with('/');
                if rel && !ctx.git_ref.is_empty() && !ctx.owner.is_empty() && is_relative(&dest_url) {
                    image_as_link.push(false);
                    out.push(Event::Start(Tag::Image {
                        link_type,
                        dest_url: CowStr::from(rewrite_relative(&dest_url, ctx, true)),
                        title,
                        id,
                    }));
                } else if dest_url.starts_with('/') && !dest_url.starts_with("//") {
                    image_as_link.push(false);
                    out.push(Event::Start(Tag::Image { link_type, dest_url, title, id }));
                } else if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
                    image_as_link.push(true);
                    out.push(Event::Start(Tag::Link { link_type: LinkType::Inline, dest_url, title, id }));
                } else {
                    // data: and anything else: show the alt text only.
                    image_as_link.push(true);
                    out.push(Event::Start(Tag::Link { link_type: LinkType::Inline, dest_url: CowStr::from(""), title, id }));
                }
            }
            Event::End(TagEnd::Image) => {
                image_depth = image_depth.saturating_sub(1);
                if image_as_link.pop().unwrap_or(false) {
                    out.push(Event::End(TagEnd::Link));
                } else {
                    out.push(Event::End(TagEnd::Image));
                }
            }
            Event::Text(t) => {
                if let Some((_, _, text)) = heading.as_mut() {
                    text.push_str(&t);
                }
                if ctx.link_refs && link_depth == 0 && image_depth == 0 {
                    for (is_html, piece) in linkify(&t, ctx) {
                        out.push(if is_html { Event::Html(CowStr::from(piece)) } else { Event::Text(CowStr::from(piece)) });
                    }
                } else {
                    out.push(Event::Text(t));
                }
            }
            Event::Code(c) => {
                if let Some((_, _, text)) = heading.as_mut() {
                    text.push_str(&c);
                }
                out.push(Event::Code(c));
            }
            other => out.push(other),
        }
    }
    let mut html = String::with_capacity(src.len() * 2);
    pulldown_cmark::html::push_html(&mut html, out.into_iter());
    sanitizer().clean(&html).to_string()
}

/// Markdown stripped to plain text for previews: whitespace collapsed, cut at `max_chars`.
pub fn plain_excerpt(src: &str, max_chars: usize) -> String {
    let mut text = String::new();
    for ev in Parser::new(src) {
        match ev {
            Event::Text(t) | Event::Code(t) => text.push_str(&t),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::Item) => text.push(' '),
            _ => {}
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let cut: String = collapsed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}\u{2026}", cut.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> MdContext {
        MdContext { owner: "ada".into(), repo: "lh".into(), git_ref: "main".into(), dir: String::new(), link_refs: true, heading_ids: true }
    }

    #[test]
    fn a_suggestion_block_is_labelled() {
        let html = render("Try this:\n\n```suggestion\nlet x = 2;\n```\n", &MdContext::default());
        assert!(
            html.contains("<div class=\"suggestion\"><p class=\"suggestion-label\">Suggested change</p><pre class=\"hl-block\"><code>"),
            "{html}"
        );
        assert!(html.contains("let x = 2;"));
        let plain = render("```rust\nlet x = 2;\n```\n", &MdContext::default());
        assert!(!plain.contains("Suggested change"));
    }

    #[test]
    fn scripts_handlers_and_styles_are_removed() {
        let h = render(
            "<script>alert(1)</script>\n\n<img src=\"/x.png\" onerror=\"alert(1)\">\n\n<p style=\"color:red\" onclick=\"x()\">hi</p>",
            &repo(),
        );
        assert!(!h.contains("script") && !h.contains("onerror") && !h.contains("style") && !h.contains("onclick"), "{h}");
        assert!(h.contains("hi"));
    }

    #[test]
    fn forms_iframes_and_objects_go() {
        let h = render(
            "<iframe src=\"https://x.test\"></iframe><form action=\"/x\"><input name=\"a\"></form><object data=\"x\"></object>",
            &repo(),
        );
        assert!(!h.contains("<iframe") && !h.contains("<form") && !h.contains("<object"), "{h}");
    }

    #[test]
    fn an_off_site_image_becomes_a_link() {
        let h = render("![logo](https://cdn.example/a.png)", &repo());
        assert!(!h.contains("<img"), "{h}");
        assert!(h.contains("<a href=\"https://cdn.example/a.png\""), "{h}");
        assert!(h.contains("logo"));
        let raw = render("<img src=\"https://cdn.example/a.png\" alt=\"x\">", &repo());
        assert!(!raw.contains("cdn.example"), "{raw}");
    }

    #[test]
    fn data_images_and_javascript_links_are_dropped() {
        let h = render("![x](data:image/png;base64,AAAA) [bad](javascript:alert(1)) [ok](https://ok.test)", &repo());
        assert!(!h.contains("data:") && !h.contains("javascript:"), "{h}");
        assert!(h.contains("https://ok.test"));
        let v = render("[v](vbscript:x)", &repo());
        assert!(!v.contains("vbscript"));
    }

    #[test]
    fn relative_links_and_images_point_into_the_repo() {
        let mut c = repo();
        c.dir = "docs".into();
        let h = render("[g](guide.md) [up](../README.md#top) [dir](sub/) ![i](img/a.png)", &c);
        assert!(h.contains("href=\"/ada/lh/blob/main/docs/guide.md\""), "{h}");
        assert!(h.contains("href=\"/ada/lh/blob/main/README.md#top\""), "{h}");
        assert!(h.contains("href=\"/ada/lh/tree/main/docs/sub\""), "{h}");
        assert!(h.contains("src=\"/ada/lh/raw/main/docs/img/a.png\""), "{h}");
    }

    #[test]
    fn dot_dot_cannot_leave_the_repo() {
        let mut c = repo();
        c.dir = "a".into();
        let h = render("[x](../../../../etc/passwd)", &c);
        assert!(h.contains("href=\"/ada/lh/blob/main/etc/passwd\""), "{h}");
        assert_eq!(resolve_path("a/b", "../../.."), (String::new(), false));
    }

    #[test]
    fn absolute_and_anchor_links_are_untouched() {
        let h = render("[a](/ada/other) [b](#frag) [c](https://x.test/p)", &repo());
        assert!(h.contains("href=\"/ada/other\"") && h.contains("href=\"#frag\"") && h.contains("href=\"https://x.test/p\""), "{h}");
        assert!(h.contains("rel=\"noopener noreferrer nofollow\""));
    }

    #[test]
    fn fenced_code_is_highlighted() {
        let h = render("```rust\nfn main() {}\n```\n", &repo());
        assert!(h.contains("hl-block") && h.contains("hl-keyword"), "{h}");
        let plain = render("```\n<b>x</b>\n```\n", &repo());
        assert!(plain.contains("&lt;b&gt;"), "{plain}");
    }

    #[test]
    fn mentions_issues_and_shas_are_linked() {
        let h = render("Thanks @bob, fixes #12, see other/thing#3 and abc1234 for details.", &repo());
        assert!(h.contains("href=\"/bob\""), "{h}");
        assert!(h.contains("href=\"/ada/lh/issues/12\""), "{h}");
        assert!(h.contains("href=\"/other/thing/issues/3\""), "{h}");
        assert!(h.contains("href=\"/ada/lh/commit/abc1234\""), "{h}");
    }

    #[test]
    fn things_that_only_look_like_references_are_not() {
        let h = render("mail foo@bar.test, version a#1b, word deadbeef and 1234567 plain", &repo());
        assert!(!h.contains("class=\"mention\""), "{h}");
        assert!(!h.contains("issue-ref"), "{h}");
        assert!(!h.contains("sha-ref"), "{h}");
    }

    #[test]
    fn nothing_is_linked_inside_code_or_links() {
        let h = render("`@bob #12` and [@bob #12](https://x.test)", &repo());
        assert!(!h.contains("class=\"mention\"") && !h.contains("issue-ref"), "{h}");
    }

    #[test]
    fn issue_and_sha_links_need_a_repo() {
        let none = MdContext { link_refs: true, ..Default::default() };
        let h = render("see #12 and abc1234 and @bob", &none);
        assert!(!h.contains("issue-ref") && !h.contains("sha-ref"), "{h}");
        assert!(h.contains("href=\"/bob\""));
    }

    #[test]
    fn task_lists_use_disabled_checkboxes() {
        let h = render("- [x] done\n- [ ] todo\n", &repo());
        assert!(h.contains("<input type=\"checkbox\" disabled=\"\" checked=\"\">") || h.contains("checked"), "{h}");
        assert_eq!(h.matches("<input").count(), 2, "{h}");
    }

    #[test]
    fn headings_get_unique_ids() {
        let h = render("# Setup\n\n## Setup\n\n## Other thing\n", &repo());
        assert!(h.contains("id=\"setup\"") && h.contains("id=\"setup-1\"") && h.contains("id=\"other-thing\""), "{h}");
        assert!(h.contains("class=\"anchor\""));
        let off = render("# Setup\n", &MdContext { heading_ids: false, ..repo() });
        assert!(!off.contains("id="), "{off}");
    }

    #[test]
    fn tables_scroll_inside_a_wrapper() {
        let h = render("| a | b |\n| - | - |\n| 1 | 2 |\n", &repo());
        assert!(h.contains("<div class=\"table-wrap\"><table>"), "{h}");
    }

    #[test]
    fn strikethrough_and_emphasis() {
        let h = render("~~gone~~ *new* **bold**", &repo());
        assert!(h.contains("<del>gone</del>") && h.contains("<em>new</em>") && h.contains("<strong>bold</strong>"), "{h}");
    }

    #[test]
    fn excerpt_strips_markup_and_cuts() {
        assert_eq!(plain_excerpt("# Title\n\nSome **bold** text.", 100), "Title Some bold text.");
        let long = plain_excerpt(&"word ".repeat(100), 20);
        assert!(long.chars().count() <= 20 && long.ends_with('\u{2026}'), "{long}");
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for s in ["", "@", "#", "@@@@", "#####", "a/b#", "/#1", "![](", "[](", "<", "\u{1b}[31m", "é@é #é", "@a.b.c.", "ab/cd#12x"] {
            let _ = render(s, &repo());
        }
    }
}
