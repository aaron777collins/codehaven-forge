//! A parser for `git blame --porcelain` (ARCHITECTURE.md section 6.6). Pure, no IO.
//!
//! Every line of the file gets a header `<sha> <orig_line> <final_line> [<group_size>]`. The
//! first time a commit appears, its metadata follows (`author`, `summary`, `filename`, ...);
//! later groups from the same commit repeat only the header. Then comes exactly one content
//! line, which starts with a TAB and may hold anything. Consecutive lines from one commit are
//! merged into one range.

use ch_core::model::{Blame, BlameRange, Signature};
use chrono::DateTime;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlameParseError {
    #[error("malformed blame output: {0}")]
    Malformed(String),
}

fn bad<T>(msg: impl Into<String>) -> Result<T, BlameParseError> {
    Err(BlameParseError::Malformed(msg.into()))
}

struct Meta {
    author: String,
    email: String,
    time: i64,
    summary: String,
    filename: String,
}

#[derive(Default)]
struct Partial {
    author: Option<String>,
    email: String,
    time: i64,
    summary: String,
}

struct Header {
    sha: String,
    orig: u32,
    fin: u32,
    filename: Option<String>,
}

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_header(line: &str) -> Result<Header, BlameParseError> {
    let mut f = line.split(' ');
    let (Some(sha), Some(orig), Some(fin)) = (f.next(), f.next(), f.next()) else {
        return bad(format!("header has fewer than three fields: {line:?}"));
    };
    if !is_sha(sha) {
        return bad(format!("not a commit id: {sha:?}"));
    }
    let (Ok(orig), Ok(fin)) = (orig.parse::<u32>(), fin.parse::<u32>()) else {
        return bad(format!("line numbers are not numbers: {line:?}"));
    };
    Ok(Header { sha: sha.to_string(), orig, fin, filename: None })
}

/// Parses the full output of `git blame --porcelain`.
pub fn parse_porcelain(out: &[u8]) -> Result<Blame, BlameParseError> {
    let text = String::from_utf8_lossy(out);
    let mut metas: HashMap<String, Meta> = HashMap::new();
    let mut ranges: Vec<BlameRange> = Vec::new();
    let mut cur: Option<Header> = None;
    let mut pending: Option<Partial> = None;

    for line in text.split('\n') {
        if line.starts_with('\t') {
            let Some(h) = cur.take() else { return bad("a content line came before any header") };
            let mut partial = pending.take();
            if let Some(p) = partial.take() {
                let Some(author) = p.author else { return bad(format!("commit {} has no author", h.sha)) };
                let filename = h.filename.clone().unwrap_or_default();
                metas.insert(h.sha.clone(), Meta { author, email: p.email, time: p.time, summary: p.summary, filename });
            }
            let Some(meta) = metas.get(&h.sha) else { return bad(format!("no metadata for commit {}", h.sha)) };
            let orig_path = h.filename.clone().unwrap_or_else(|| meta.filename.clone());
            let extends = ranges.last().is_some_and(|r| {
                r.sha == h.sha
                    && r.orig_path == orig_path
                    && r.start_line + r.line_count == h.fin
                    && r.orig_start_line + r.line_count == h.orig
            });
            if extends {
                if let Some(r) = ranges.last_mut() {
                    r.line_count += 1;
                }
            } else {
                ranges.push(BlameRange {
                    start_line: h.fin,
                    line_count: 1,
                    sha: h.sha.clone(),
                    author: Signature {
                        name: meta.author.clone(),
                        email: meta.email.trim_start_matches('<').trim_end_matches('>').to_string(),
                        at: DateTime::from_timestamp(meta.time, 0).unwrap_or_default(),
                    },
                    summary: meta.summary.clone(),
                    orig_path,
                    orig_start_line: h.orig,
                });
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        match cur.as_mut() {
            None => {
                let h = parse_header(line)?;
                if !metas.contains_key(&h.sha) {
                    pending = Some(Partial::default());
                }
                cur = Some(h);
            }
            Some(h) => {
                let (key, value) = line.split_once(' ').unwrap_or((line, ""));
                if key == "filename" {
                    h.filename = Some(value.to_string());
                } else if let Some(p) = pending.as_mut() {
                    match key {
                        "author" => p.author = Some(value.to_string()),
                        "author-mail" => p.email = value.to_string(),
                        "author-time" => p.time = value.trim().parse().unwrap_or(0),
                        "summary" => p.summary = value.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    if cur.is_some() {
        return bad("output ended before a content line");
    }
    Ok(Blame { ranges })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn meta(sha: &str, who: &str, summary: &str, file: &str) -> String {
        format!(
            "author {who}\nauthor-mail <{who}@x.test>\nauthor-time 1700000000\nauthor-tz +0000\ncommitter {who}\ncommitter-mail <{who}@x.test>\ncommitter-time 1700000000\ncommitter-tz +0000\nsummary {summary}\nfilename {file}\n"
        )
        .replace("<SHA>", sha)
    }

    #[test]
    fn one_commit_one_range() {
        let out = format!("{A} 1 1 3\n{}\tline one\n{A} 2 2\n\tline two\n{A} 3 3\n\tline three\n", meta(A, "alice", "Add file", "a.txt"));
        let b = parse_porcelain(out.as_bytes()).unwrap();
        assert_eq!(b.ranges.len(), 1);
        let r = &b.ranges[0];
        assert_eq!((r.start_line, r.line_count, r.orig_start_line), (1, 3, 1));
        assert_eq!(r.author.name, "alice");
        assert_eq!(r.author.email, "alice@x.test");
        assert_eq!(r.summary, "Add file");
        assert_eq!(r.orig_path, "a.txt");
        assert_eq!(r.author.at.timestamp(), 1_700_000_000);
    }

    #[test]
    fn a_commit_that_comes_back_makes_a_new_range() {
        // A A B A, the shape of an edit in the middle of a file.
        let out = format!(
            "{A} 1 1 2\n{}\tone\n{A} 2 2\n\ttwo\n{B} 1 3 1\n{}\tthree changed\n{A} 4 4 1\n\tfour\n",
            meta(A, "alice", "First", "f.txt"),
            meta(B, "bob", "Edit", "f.txt")
        );
        let b = parse_porcelain(out.as_bytes()).unwrap();
        let shape: Vec<(u32, u32, &str)> = b.ranges.iter().map(|r| (r.start_line, r.line_count, r.author.name.as_str())).collect();
        assert_eq!(shape, vec![(1, 2, "alice"), (3, 1, "bob"), (4, 1, "alice")]);
        // The repeated commit reuses what the first header said about it.
        assert_eq!(b.ranges[2].summary, "First");
        assert_eq!(b.ranges[2].orig_start_line, 4);
    }

    #[test]
    fn a_rename_shows_the_old_path() {
        let out = format!(
            "{A} 1 1 1\n{}\told text\n{B} 1 2 1\n{}\tnew text\n",
            meta(A, "alice", "Old", "old-name.txt"),
            meta(B, "bob", "Rename", "new-name.txt")
        );
        let b = parse_porcelain(out.as_bytes()).unwrap();
        assert_eq!(b.ranges[0].orig_path, "old-name.txt");
        assert_eq!(b.ranges[1].orig_path, "new-name.txt");
    }

    #[test]
    fn content_that_looks_like_metadata_is_content() {
        let out = format!("{A} 1 1 2\n{}\tauthor mallory\n{A} 2 2\n\t{B} 9 9 9\n", meta(A, "alice", "Tricky", "t.txt"));
        let b = parse_porcelain(out.as_bytes()).unwrap();
        assert_eq!(b.ranges.len(), 1);
        assert_eq!(b.ranges[0].line_count, 2);
        assert_eq!(b.ranges[0].author.name, "alice");
    }

    #[test]
    fn blank_lines_and_crlf_content() {
        let out = format!("{A} 1 1 2\n{}\t\n{A} 2 2\n\tvalue\r\n", meta(A, "alice", "Blank", "b.txt"));
        assert_eq!(parse_porcelain(out.as_bytes()).unwrap().ranges[0].line_count, 2);
    }

    #[test]
    fn empty_output_is_an_empty_blame() {
        assert!(parse_porcelain(b"").unwrap().ranges.is_empty());
        assert!(parse_porcelain(b"\n").unwrap().ranges.is_empty());
    }

    #[test]
    fn broken_output_is_an_error_not_a_panic() {
        for bad in [
            "\tcontent first\n".to_string(),
            "short header\n\tx\n".to_string(),
            format!("{A} one two\n\tx\n"),
            "nothex 1 1 1\n\tx\n".to_string(),
            format!("{A} 1 1 1\nsummary no author\nfilename f\n\tx\n"),
            format!("{A} 1 1 1\n{}", meta(A, "alice", "No content", "f")),
        ] {
            assert!(parse_porcelain(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    proptest! {
        #[test]
        fn never_panics(data in proptest::collection::vec(any::<u8>(), 0..2000)) {
            let _ = parse_porcelain(&data);
        }
    }
}
