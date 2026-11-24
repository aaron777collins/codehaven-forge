//! Puts the storage bridge first in an HTML file (`docs/design/pages.md`, section 4).
//!
//! It must run before any script of the page, so it goes as early as the file allows. Never
//! before a doctype, which would switch the page into quirks mode: after a byte order mark,
//! white space, comments, processing instructions and the doctype, and before anything else.

/// How far into a file the doctype is looked for. Serving reads this much of an HTML file
/// before it sends anything, and streams the rest.
pub const SCAN: usize = 64 * 1024;

/// The byte offset where the bridge goes.
///
/// A leading comment ends where HTML ends it: at `-->`, at the incorrect `--!>`, or straight
/// away for the empty comments `<!-->` and `<!--->`.
pub fn offset(html: &[u8]) -> usize {
    let end = html.len().min(SCAN);
    let b = &html[..end];
    let mut i = if b.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let rest = &b[i..];
        if rest.starts_with(b"<!--") {
            let body = &rest[4..];
            // `<!-->` and `<!--->` are whole empty comments, and `--!>` closes one too.
            if body.starts_with(b">") {
                i += 5;
            } else if body.starts_with(b"->") {
                i += 6;
            } else {
                match find(body, b"-->").map(|n| (n, 3)).into_iter().chain(find(body, b"--!>").map(|n| (n, 4))).min_by_key(|&(n, _)| n) {
                    Some((n, len)) => i += 4 + n + len,
                    // An unclosed comment swallows the rest: going first is all that is left.
                    None => return i,
                }
            }
        } else if rest.starts_with(b"<?") {
            match find(rest, b">") {
                Some(n) => i += n + 1,
                None => return i,
            }
        } else if starts_with_ci(rest, b"<!doctype") {
            return match find(rest, b">") {
                Some(n) => i + n + 1,
                None => i,
            };
        } else {
            return i;
        }
    }
}

/// `html` with `script` at `offset(html)`.
pub fn inject(html: &[u8], script: &str) -> Vec<u8> {
    let at = offset(html);
    let mut out = Vec::with_capacity(html.len() + script.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(script.as_bytes());
    out.extend_from_slice(&html[at..]);
    out
}

/// JSON for a script element: no `<`, so neither `</script>` nor `<!--` can appear, and the
/// two line separators JavaScript once treated as line ends are escaped.
pub fn script_json(v: &serde_json::Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> &str {
        &s[offset(s.as_bytes())..]
    }

    #[test]
    fn after_the_doctype_and_what_may_come_before_it() {
        assert_eq!(at("<!DOCTYPE html><html>"), "<html>");
        assert_eq!(at("<!doctype html>\n<html>"), "\n<html>");
        assert_eq!(at("\u{feff}  <!-- a --> <!--b--><!DocType html><p>"), "<p>");
        assert_eq!(at("<?xml version=\"1.0\"?><!DOCTYPE html><html>"), "<html>");
    }

    #[test]
    fn first_when_there_is_no_doctype() {
        assert_eq!(at("<html><script>x()</script>"), "<html><script>x()</script>");
        assert_eq!(at("<script>x()</script><!doctype html>"), "<script>x()</script><!doctype html>");
        assert_eq!(at("  <p>hi"), "<p>hi");
        assert_eq!(at(""), "");
        assert_eq!(at("<!-- never closed <script>x()</script>"), "<!-- never closed <script>x()</script>");
    }

    #[test]
    fn empty_and_oddly_closed_comments_end_where_html_ends_them() {
        assert_eq!(at("<!--><script>x()</script>--><p>"), "<script>x()</script>--><p>");
        assert_eq!(at("<!---><script>x()</script>--><p>"), "<script>x()</script>--><p>");
        assert_eq!(at("<!-- a --!><script>x()</script>"), "<script>x()</script>");
        assert_eq!(at("<!----><!doctype html><p>"), "<p>");
        assert_eq!(at("<!-- a -- b --><!doctype html><p>"), "<p>");
        assert_eq!(at("<!--><!doctype html><script>x()</script>"), "<script>x()</script>");
    }

    #[test]
    fn injected_once_and_whole() {
        let out = inject(b"<!doctype html><title>t</title>", "<script>B</script>");
        assert_eq!(out, b"<!doctype html><script>B</script><title>t</title>");
    }

    #[test]
    fn script_json_cannot_close_the_element() {
        let s = script_json(&serde_json::json!({"p": "</script><!--\u{2028}"}));
        assert!(!s.contains('<'), "{s}");
        assert!(!s.contains('\u{2028}'));
    }
}
