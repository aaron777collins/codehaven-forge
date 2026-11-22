//! Parsers for the output of three `git diff` runs over one range, and the function that
//! merges them into `ch_core::model::Diff` (ARCHITECTURE.md section 6.6). Pure, no IO.
//!
//! The three runs list files in the same order, so a file is matched across them by position
//! and never by parsing names out of the patch text, where they may be quoted or odd.
//!
//! 1. `git diff --raw -z`: `:<oldmode> <newmode> <oldsha> <newsha> <status>[<score>]\0<path>\0`,
//!    with two paths for renames and copies.
//! 2. `git diff --numstat -z`: `<added>\t<deleted>\t<path>\0`, or `...\t\0<old>\0<new>\0` for
//!    renames, and `-` counts for binary files.
//! 3. `git diff --patch --no-color`: sections starting at `diff --git `, each with hunks.

use ch_core::model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiffParseError {
    #[error("malformed git output: {0}")]
    Malformed(String),
}

fn bad<T>(msg: impl Into<String>) -> Result<T, DiffParseError> {
    Err(DiffParseError::Malformed(msg.into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    /// `A`, `M`, `D`, `R`, `C`, `T` or `U`.
    pub status: char,
    pub score: Option<u32>,
    pub old_mode: String,
    pub new_mode: String,
    pub old_sha: String,
    pub new_sha: String,
    /// Set for renames and copies.
    pub old_path: Option<String>,
    /// The new path, or the only path.
    pub path: String,
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub fn parse_raw_z(out: &[u8]) -> Result<Vec<RawEntry>, DiffParseError> {
    let mut it = out.split(|b| *b == 0);
    let mut entries = Vec::new();
    while let Some(meta) = it.next() {
        if meta.is_empty() {
            break;
        }
        let meta = text(meta);
        let Some(rest) = meta.strip_prefix(':') else { return bad(format!("expected a record starting with ':', got {meta:?}")) };
        let f: Vec<&str> = rest.split(' ').collect();
        if f.len() != 5 {
            return bad(format!("expected five fields in {meta:?}"));
        }
        let mut chars = f[4].chars();
        let Some(status) = chars.next() else { return bad("empty status") };
        let score = chars.as_str().parse::<u32>().ok();
        let (old_path, path) = if status == 'R' || status == 'C' {
            let (Some(old), Some(new)) = (it.next(), it.next()) else { return bad("rename without two paths") };
            if old.is_empty() || new.is_empty() {
                return bad("rename without two paths");
            }
            (Some(text(old)), text(new))
        } else {
            let Some(p) = it.next() else { return bad("record without a path") };
            if p.is_empty() {
                return bad("record without a path");
            }
            (None, text(p))
        };
        entries.push(RawEntry {
            status,
            score,
            old_mode: f[0].to_string(),
            new_mode: f[1].to_string(),
            old_sha: f[2].to_string(),
            new_sha: f[3].to_string(),
            old_path,
            path,
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumStat {
    pub added: Option<u32>,
    /// `None` for a binary file.
    pub deleted: Option<u32>,
    pub path: String,
}

pub fn parse_numstat_z(out: &[u8]) -> Result<Vec<NumStat>, DiffParseError> {
    let mut it = out.split(|b| *b == 0);
    let mut stats = Vec::new();
    while let Some(tok) = it.next() {
        if tok.is_empty() {
            break;
        }
        let tok = text(tok);
        let mut f = tok.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (f.next(), f.next(), f.next()) else {
            return bad(format!("not a numstat record: {tok:?}"));
        };
        let count = |s: &str| {
            if s == "-" {
                Ok(None)
            } else {
                s.parse::<u32>().map(Some).map_err(|_| DiffParseError::Malformed(format!("not a count: {s:?}")))
            }
        };
        let path = if path.is_empty() {
            let (Some(old), Some(new)) = (it.next(), it.next()) else { return bad("rename without two paths") };
            if old.is_empty() || new.is_empty() {
                return bad("rename without two paths");
            }
            text(new)
        } else {
            path.to_string()
        };
        stats.push(NumStat { added: count(added)?, deleted: count(deleted)?, path });
    }
    Ok(stats)
}

#[derive(Debug, Clone)]
pub struct PatchFile {
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    /// Bytes of this file's section of the patch.
    pub patch_bytes: usize,
}

/// `@@ -a[,b] +c[,d] @@ ...` to `(old_start, old_count, new_start, new_count)`.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (os, oc) = pair(old)?;
    let (ns, nc) = pair(new)?;
    Some((os, oc, ns, nc))
}

pub fn parse_patch(out: &[u8]) -> Vec<PatchFile> {
    let all = String::from_utf8_lossy(out);
    // Cut into sections at lines that begin with `diff --git `. Content lines always start
    // with a space, `+`, `-` or a backslash, so this never splits inside a hunk.
    let mut starts: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    for line in all.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(pos);
        }
        pos += line.len();
    }
    let mut files = Vec::new();
    for (i, &s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(all.len());
        files.push(parse_section(&all[s..e]));
    }
    files
}

fn parse_section(section: &str) -> PatchFile {
    let mut binary = false;
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut cur: Option<(Hunk, u32, u32, u32, u32)> = None; // hunk, old_no, new_no, old_left, new_left
    for line in section.split('\n').skip(1) {
        if cur.is_none() && !line.starts_with("@@ ") {
            if line.starts_with("Binary files ") || line == "GIT binary patch" {
                binary = true;
            }
            continue;
        }
        if line.starts_with("@@ ")
            && let Some((os, oc, ns, nc)) = parse_hunk_header(line)
        {
            if let Some((h, ..)) = cur.take() {
                hunks.push(h);
            }
            cur = Some((Hunk { header: line.to_string(), lines: Vec::new() }, os, ns, oc, nc));
            continue;
        }
        let Some((h, old_no, new_no, old_left, new_left)) = cur.as_mut() else { continue };
        let mut push = |kind: LineKind, text: &str, o: Option<u32>, n: Option<u32>| {
            h.lines.push(DiffLine { kind, old_no: o, new_no: n, text: text.to_string() })
        };
        if let Some(t) = line.strip_prefix(' ') {
            push(LineKind::Context, t, Some(*old_no), Some(*new_no));
            *old_no += 1;
            *new_no += 1;
            *old_left = old_left.saturating_sub(1);
            *new_left = new_left.saturating_sub(1);
        } else if let Some(t) = line.strip_prefix('+') {
            push(LineKind::Added, t, None, Some(*new_no));
            *new_no += 1;
            *new_left = new_left.saturating_sub(1);
        } else if let Some(t) = line.strip_prefix('-') {
            push(LineKind::Removed, t, Some(*old_no), None);
            *old_no += 1;
            *old_left = old_left.saturating_sub(1);
        } else if line.starts_with('\\') {
            // "\ No newline at end of file": not a line of the file.
        } else if line.is_empty() && (*old_left > 0 || *new_left > 0) {
            // Some tools trim the single space of an empty context line.
            push(LineKind::Context, "", Some(*old_no), Some(*new_no));
            *old_no += 1;
            *new_no += 1;
            *old_left = old_left.saturating_sub(1);
            *new_left = new_left.saturating_sub(1);
        }
    }
    if let Some((h, ..)) = cur {
        hunks.push(h);
    }
    PatchFile { binary, hunks, patch_bytes: section.len() }
}

#[derive(Debug, Clone)]
pub struct DiffLimits {
    /// A file with more changed lines than this is collapsed.
    pub max_changed_lines: u32,
    /// A file whose patch is larger than this is collapsed.
    pub max_patch_bytes: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self { max_changed_lines: 20_000, max_patch_bytes: 1_048_576 }
    }
}

pub fn assemble(raw: Vec<RawEntry>, numstat: Vec<NumStat>, patch: Vec<PatchFile>, limits: &DiffLimits) -> Result<Diff, DiffParseError> {
    if raw.len() != numstat.len() {
        return bad(format!("{} files in --raw but {} in --numstat", raw.len(), numstat.len()));
    }
    if patch.len() > raw.len() {
        return bad(format!("{} files in --raw but {} in the patch", raw.len(), patch.len()));
    }
    let mut patches = patch.into_iter();
    let mut files = Vec::with_capacity(raw.len());
    for (r, n) in raw.into_iter().zip(numstat) {
        let p = patches.next().unwrap_or(PatchFile { binary: false, hunks: Vec::new(), patch_bytes: 0 });
        let status = match r.status {
            'A' => FileStatus::Added,
            'D' => FileStatus::Removed,
            'R' => FileStatus::Renamed,
            'C' => FileStatus::Copied,
            _ => FileStatus::Modified,
        };
        let binary = p.binary || n.added.is_none();
        let additions = n.added.unwrap_or(0);
        let deletions = n.deleted.unwrap_or(0);
        let collapsed = additions.saturating_add(deletions) > limits.max_changed_lines || p.patch_bytes > limits.max_patch_bytes;
        files.push(FileDiff {
            status,
            old_path: match r.status {
                'A' => None,
                'R' | 'C' => r.old_path,
                _ => Some(r.path.clone()),
            },
            new_path: if r.status == 'D' { None } else { Some(r.path) },
            binary,
            collapsed,
            additions,
            deletions,
            hunks: if collapsed || binary { Vec::new() } else { p.hunks },
        });
    }
    Ok(Diff { files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const Z: &str = "0000000";

    fn raw(status: &str, paths: &[&str]) -> Vec<u8> {
        let mut s = format!(":100644 100644 aaaaaaa bbbbbbb {status}\0");
        for p in paths {
            s.push_str(p);
            s.push('\0');
        }
        s.into_bytes()
    }

    #[test]
    fn raw_records() {
        let mut out = raw("M", &["src/a.rs"]);
        out.extend(format!(":000000 100644 {Z} bbbbbbb A\0new.txt\0").into_bytes());
        out.extend(format!(":100644 000000 aaaaaaa {Z} D\0gone.txt\0").into_bytes());
        out.extend(raw("R087", &["old.rs", "new.rs"]));
        out.extend(raw("R100", &["same-a", "same-b"]));
        let r = parse_raw_z(&out).unwrap();
        assert_eq!(r.len(), 5);
        assert_eq!((r[0].status, r[0].path.as_str(), r[0].old_path.clone()), ('M', "src/a.rs", None));
        assert_eq!(r[1].status, 'A');
        assert_eq!(r[2].status, 'D');
        assert_eq!((r[3].status, r[3].score, r[3].old_path.as_deref(), r[3].path.as_str()), ('R', Some(87), Some("old.rs"), "new.rs"));
        assert_eq!(r[4].score, Some(100));
    }

    #[test]
    fn raw_errors() {
        assert!(parse_raw_z(b"nonsense\0").is_err());
        assert!(parse_raw_z(b":100644 100644 a b M\0").is_err(), "no path");
        assert!(parse_raw_z(b":100644 100644 a b R50\0only-one\0").is_err());
        assert!(parse_raw_z(b"").unwrap().is_empty());
    }

    #[test]
    fn numstat_records() {
        let out = b"3\t1\tsrc/a.rs\0-\t-\timg.png\x002\t0\t\0old.rs\0new.rs\0";
        let n = parse_numstat_z(out).unwrap();
        assert_eq!(n.len(), 3);
        assert_eq!((n[0].added, n[0].deleted, n[0].path.as_str()), (Some(3), Some(1), "src/a.rs"));
        assert_eq!((n[1].added, n[1].deleted), (None, None));
        assert_eq!((n[2].added, n[2].path.as_str()), (Some(2), "new.rs"));
        assert!(parse_numstat_z(b"x\ty\tz\0").is_err());
        assert!(parse_numstat_z(b"1\t1\t\0lonely\0").is_err());
    }

    const PATCH: &str = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,4 @@ fn x()\n one\n-two\n+TWO\n three\n four\n@@ -10 +10,2 @@\n ten\n+ten and a half\n\\ No newline at end of file\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0..3\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,3 @@\n+a\n+b\r\n+c\n";

    #[test]
    fn patch_hunks_and_line_numbers() {
        let p = parse_patch(PATCH.as_bytes());
        assert_eq!(p.len(), 2);
        assert!(!p[0].binary);
        assert_eq!(p[0].hunks.len(), 2);
        let h = &p[0].hunks[0];
        assert_eq!(h.header, "@@ -1,4 +1,4 @@ fn x()");
        let shape: Vec<(LineKind, Option<u32>, Option<u32>, &str)> =
            h.lines.iter().map(|l| (l.kind, l.old_no, l.new_no, l.text.as_str())).collect();
        assert_eq!(
            shape,
            vec![
                (LineKind::Context, Some(1), Some(1), "one"),
                (LineKind::Removed, Some(2), None, "two"),
                (LineKind::Added, None, Some(2), "TWO"),
                (LineKind::Context, Some(3), Some(3), "three"),
                (LineKind::Context, Some(4), Some(4), "four"),
            ]
        );
        let h2 = &p[0].hunks[1];
        assert_eq!(h2.lines.len(), 2, "the no-newline marker is not a line");
        assert_eq!(h2.lines[1].new_no, Some(11));
        // New file: `-0,0` means no old lines, and CRLF keeps its carriage return.
        let n = &p[1].hunks[0];
        assert_eq!(n.lines.iter().map(|l| l.new_no).collect::<Vec<_>>(), vec![Some(1), Some(2), Some(3)]);
        assert_eq!(n.lines[1].text, "b\r");
        assert!(n.lines.iter().all(|l| l.old_no.is_none()));
    }

    #[test]
    fn binary_and_empty_sections() {
        let p = parse_patch(b"diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n");
        assert_eq!(p.len(), 1);
        assert!(p[0].binary && p[0].hunks.is_empty());
        let mode_only = parse_patch(b"diff --git a/x b/x\nold mode 100644\nnew mode 100755\n");
        assert_eq!(mode_only.len(), 1);
        assert!(!mode_only[0].binary && mode_only[0].hunks.is_empty());
        assert!(parse_patch(b"").is_empty());
    }

    #[test]
    fn an_empty_context_line_without_its_space() {
        let p = parse_patch(b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n");
        let lines = &p[0].hunks[0].lines;
        assert_eq!(lines[1].kind, LineKind::Context);
        assert_eq!(lines[1].text, "");
        assert_eq!(lines.len(), 4);
    }

    fn assemble_all(raw_bytes: &[u8], num: &[u8], patch: &str, limits: &DiffLimits) -> Result<Diff, DiffParseError> {
        assemble(parse_raw_z(raw_bytes)?, parse_numstat_z(num)?, parse_patch(patch.as_bytes()), limits)
    }

    #[test]
    fn assembling_modified_added_and_renamed() {
        let mut raw_bytes = raw("M", &["a.txt"]);
        raw_bytes.extend(format!(":000000 100644 {Z} bbbbbbb A\0new.txt\0").into_bytes());
        raw_bytes.extend(raw("R087", &["old.rs", "moved.rs"]));
        let patch = format!(
            "{PATCH}diff --git a/old.rs b/moved.rs\nsimilarity index 87%\nrename from old.rs\nrename to moved.rs\n@@ -1 +1 @@\n-x\n+y\n"
        );
        let num = b"1\t1\ta.txt\x003\t0\tnew.txt\x001\t1\t\0old.rs\0moved.rs\0";
        let d = assemble_all(&raw_bytes, num, &patch, &DiffLimits::default()).unwrap();
        assert_eq!(d.files.len(), 3);
        assert_eq!(
            (d.files[0].status, d.files[0].old_path.as_deref(), d.files[0].new_path.as_deref()),
            (FileStatus::Modified, Some("a.txt"), Some("a.txt"))
        );
        assert_eq!((d.files[1].status, d.files[1].old_path.as_deref(), d.files[1].additions), (FileStatus::Added, None, 3));
        assert_eq!(
            (d.files[2].status, d.files[2].old_path.as_deref(), d.files[2].new_path.as_deref()),
            (FileStatus::Renamed, Some("old.rs"), Some("moved.rs"))
        );
        assert_eq!(d.files[2].hunks.len(), 1);
    }

    #[test]
    fn assembling_deleted_and_binary_and_mode_only() {
        let mut raw_bytes = format!(":100644 000000 aaaaaaa {Z} D\0gone.txt\0").into_bytes();
        raw_bytes.extend(raw("M", &["i.png"]));
        raw_bytes.extend(b":100644 100755 aaaaaaa aaaaaaa M\0run.sh\0");
        let patch = "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n@@ -1 +0,0 @@\n-bye\ndiff --git a/i.png b/i.png\nBinary files a/i.png and b/i.png differ\ndiff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        let num = b"0\t1\tgone.txt\0-\t-\ti.png\x000\t0\trun.sh\0";
        let d = assemble_all(&raw_bytes, num, patch, &DiffLimits::default()).unwrap();
        assert_eq!((d.files[0].status, d.files[0].new_path.clone(), d.files[0].deletions), (FileStatus::Removed, None, 1));
        assert!(d.files[1].binary && d.files[1].hunks.is_empty() && d.files[1].additions == 0);
        assert!(!d.files[2].binary && d.files[2].hunks.is_empty());
    }

    #[test]
    fn big_files_collapse() {
        let raw_bytes = raw("M", &["big.txt"]);
        let num = b"30\t5\tbig.txt\0";
        let patch = "diff --git a/big.txt b/big.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let tight = DiffLimits { max_changed_lines: 10, max_patch_bytes: 1_000_000 };
        let d = assemble_all(&raw_bytes, num, patch, &tight).unwrap();
        assert!(d.files[0].collapsed && d.files[0].hunks.is_empty());
        assert_eq!((d.files[0].additions, d.files[0].deletions), (30, 5), "counts survive the collapse");
        let by_bytes = DiffLimits { max_changed_lines: 1000, max_patch_bytes: 10 };
        assert!(assemble_all(&raw_bytes, num, patch, &by_bytes).unwrap().files[0].collapsed);
        assert!(!assemble_all(&raw_bytes, num, patch, &DiffLimits::default()).unwrap().files[0].collapsed);
    }

    #[test]
    fn length_mismatch_is_an_error() {
        let raw_bytes = raw("M", &["a"]);
        assert!(assemble_all(&raw_bytes, b"", "", &DiffLimits::default()).is_err());
        let two_patches = "diff --git a/a b/a\ndiff --git a/b b/b\n";
        assert!(assemble_all(&raw_bytes, b"0\t0\ta\0", two_patches, &DiffLimits::default()).is_err());
    }

    #[test]
    fn empty_everything_is_an_empty_diff() {
        assert!(assemble(vec![], vec![], vec![], &DiffLimits::default()).unwrap().files.is_empty());
    }

    proptest! {
        #[test]
        fn parsers_never_panic(data in proptest::collection::vec(any::<u8>(), 0..1500)) {
            let _ = parse_patch(&data);
            let _ = parse_raw_z(&data);
            let _ = parse_numstat_z(&data);
        }
    }
}
