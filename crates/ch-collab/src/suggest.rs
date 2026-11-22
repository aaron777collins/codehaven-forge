//! Suggested changes in code review: a fenced block labelled `suggestion` inside a review
//! comment replaces the commented lines when applied.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    OutOfRange { line_count: usize },
    BadRange,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::OutOfRange { line_count } => write!(f, "the file only has {line_count} lines"),
            ApplyError::BadRange => write!(f, "the line range is not valid"),
        }
    }
}

struct Fence {
    ch: char,
    len: usize,
}

/// An opening or closing fence: up to three spaces, then three or more of one character.
fn fence_of(line: &str) -> Option<(Fence, &str)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    Some((Fence { ch, len }, &rest[len..]))
}

/// A suggestion info string is `suggestion`, optionally followed by whitespace or `:` and more.
fn is_suggestion_info(info: &str) -> bool {
    let info = info.trim();
    match info.strip_prefix("suggestion") {
        Some("") => true,
        Some(rest) => rest.starts_with(|c: char| c.is_whitespace() || c == ':'),
        None => false,
    }
}

pub fn extract(body: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = body.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some((open, info)) = fence_of(lines[i].trim_end_matches('\r')) else {
            i += 1;
            continue;
        };
        // A backtick fence's info string cannot contain a backtick (that would be inline code).
        if open.ch == '`' && info.contains('`') {
            i += 1;
            continue;
        }
        let suggestion = is_suggestion_info(info);
        let mut j = i + 1;
        let mut found_close = None;
        while j < lines.len() {
            if let Some((close, rest)) = fence_of(lines[j].trim_end_matches('\r'))
                && close.ch == open.ch
                && close.len >= open.len
                && rest.trim().is_empty()
            {
                found_close = Some(j);
                break;
            }
            j += 1;
        }
        let end = found_close.unwrap_or(lines.len());
        if suggestion {
            return Some(lines[i + 1..end].iter().map(|l| l.trim_end_matches('\r').to_string()).collect());
        }
        // Not a suggestion: skip the whole block so fences inside it are not read.
        i = end + 1;
    }
    None
}

/// Lines as an editor counts them.
pub fn line_count(file: &str) -> usize {
    if file.is_empty() { 0 } else { file.split_inclusive('\n').count() }
}

pub fn apply(file: &str, start: u32, end: u32, replacement: &[String]) -> Result<String, ApplyError> {
    if start == 0 || start > end {
        return Err(ApplyError::BadRange);
    }
    let lines: Vec<&str> = file.split_inclusive('\n').collect();
    let n = lines.len();
    if end as usize > n {
        return Err(ApplyError::OutOfRange { line_count: n });
    }
    let (s, e) = (start as usize - 1, end as usize);
    let crlf = lines[s..e].iter().any(|l| l.ends_with("\r\n")) || lines.first().is_some_and(|l| l.ends_with("\r\n"));
    let eol = if crlf { "\r\n" } else { "\n" };
    let last_had_newline = lines[e - 1].ends_with('\n');
    let mut out = String::with_capacity(file.len());
    for l in &lines[..s] {
        out.push_str(l);
    }
    for (i, r) in replacement.iter().enumerate() {
        out.push_str(r);
        let is_last = i + 1 == replacement.len();
        // The replaced region's final line may have had no newline (end of file).
        if !(is_last && e == n && !last_had_newline) {
            out.push_str(eol);
        }
    }
    for l in &lines[e..] {
        out.push_str(l);
    }
    // Deleting the last lines of a file with no trailing newline leaves the previous line's
    // newline in place, which is the least surprising result.
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn extracts_plain_block() {
        assert_eq!(extract("Try this:\n```suggestion\nlet x = 1;\nlet y = 2;\n```\nthanks").unwrap(), v(&["let x = 1;", "let y = 2;"]));
    }

    #[test]
    fn longer_and_tilde_fences() {
        assert_eq!(extract("````suggestion\na\n```\nb\n````").unwrap(), v(&["a", "```", "b"]));
        assert_eq!(extract("~~~suggestion\nx\n~~~").unwrap(), v(&["x"]));
    }

    #[test]
    fn info_string_variants() {
        assert!(extract("```suggestion:-0+2\nx\n```").is_some());
        assert!(extract("```suggestion foo\nx\n```").is_some());
        assert!(extract("```suggestions\nx\n```").is_none());
        assert!(extract("```rust\nx\n```").is_none());
    }

    #[test]
    fn indented_fence() {
        assert_eq!(extract("  ```suggestion\n  x\n  ```").unwrap(), v(&["  x"]));
        assert!(extract("    ```suggestion\nx\n```").is_none());
    }

    #[test]
    fn unterminated_runs_to_the_end() {
        assert_eq!(extract("```suggestion\na\nb").unwrap(), v(&["a", "b"]));
    }

    #[test]
    fn empty_block_deletes() {
        assert_eq!(extract("```suggestion\n```").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn nested_in_outer_block_is_not_a_suggestion() {
        assert!(extract("````markdown\n```suggestion\nx\n```\n````").is_none());
        assert!(extract("```\n```suggestion\nx\n").is_none());
    }

    #[test]
    fn first_block_only_and_no_block() {
        assert_eq!(extract("```suggestion\na\n```\n```suggestion\nb\n```").unwrap(), v(&["a"]));
        assert!(extract("just words").is_none());
        assert!(extract("").is_none());
    }

    #[test]
    fn skips_other_blocks_before_a_suggestion() {
        assert_eq!(extract("```rust\nfn x() {}\n```\n```suggestion\ny\n```").unwrap(), v(&["y"]));
    }

    #[test]
    fn crlf_body() {
        assert_eq!(extract("```suggestion\r\na\r\n```\r\n").unwrap(), v(&["a"]));
    }

    #[test]
    fn counts_lines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count("\n"), 1);
    }

    #[test]
    fn replaces_one_line() {
        assert_eq!(apply("a\nb\nc\n", 2, 2, &v(&["B"])).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn replaces_a_range_with_more_lines() {
        assert_eq!(apply("a\nb\nc\nd\n", 2, 3, &v(&["x", "y", "z"])).unwrap(), "a\nx\ny\nz\nd\n");
    }

    #[test]
    fn deletes_lines() {
        assert_eq!(apply("a\nb\nc\n", 2, 2, &[]).unwrap(), "a\nc\n");
        assert_eq!(apply("a\nb\nc\n", 1, 3, &[]).unwrap(), "");
    }

    #[test]
    fn first_and_last_lines() {
        assert_eq!(apply("a\nb\n", 1, 1, &v(&["A"])).unwrap(), "A\nb\n");
        assert_eq!(apply("a\nb\n", 2, 2, &v(&["B"])).unwrap(), "a\nB\n");
    }

    #[test]
    fn no_trailing_newline_is_kept() {
        assert_eq!(apply("a\nb", 2, 2, &v(&["B"])).unwrap(), "a\nB");
        assert_eq!(apply("a\nb", 1, 1, &v(&["A"])).unwrap(), "A\nb");
        assert_eq!(apply("a\nb", 2, 2, &v(&["x", "y"])).unwrap(), "a\nx\ny");
    }

    #[test]
    fn crlf_files_stay_crlf() {
        assert_eq!(apply("a\r\nb\r\nc\r\n", 2, 2, &v(&["B", "B2"])).unwrap(), "a\r\nB\r\nB2\r\nc\r\n");
    }

    #[test]
    fn range_errors() {
        assert_eq!(apply("a\n", 0, 1, &[]), Err(ApplyError::BadRange));
        assert_eq!(apply("a\nb\n", 2, 1, &[]), Err(ApplyError::BadRange));
        assert_eq!(apply("a\nb\n", 1, 3, &[]), Err(ApplyError::OutOfRange { line_count: 2 }));
        assert_eq!(apply("", 1, 1, &[]), Err(ApplyError::OutOfRange { line_count: 0 }));
    }

    #[test]
    fn unicode_content() {
        assert_eq!(apply("café\nné\n", 2, 2, &v(&["été"])).unwrap(), "café\nété\n");
    }

    #[test]
    fn whole_file() {
        assert_eq!(apply("a\nb\n", 1, 2, &v(&["z"])).unwrap(), "z\n");
    }
}
