//! File patterns for `upload-artifact`, `cache` and `hashFiles`, evaluated inside a workspace.
//!
//! The rules follow what people expect from GitHub's toolkit: `*` stays inside one path
//! segment, `**` crosses them, a pattern that names a directory means everything under it, a
//! leading `!` excludes, and several patterns may be given one per line.
//!
//! These run in the runner's own process on directories a job controls, so the rules are
//! strict about where they may look: a pattern may not contain `..`, an absolute pattern must
//! be inside the workspace, and symbolic links are never followed (a job could otherwise make
//! `link -> /home/codehaven/.runner` and read the runner's credential).

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum GlobError {
    #[error("the pattern {0:?} goes outside the workspace")]
    Outside(String),
    #[error("the pattern {0:?} is not valid: {1}")]
    Invalid(String, String),
    #[error("{0}")]
    Io(String),
}

/// Splits a multi-line input into patterns, dropping blanks and `#` comments.
pub fn split_patterns(input: &str) -> Vec<String> {
    input.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
}

struct Compiled {
    include: GlobSet,
    exclude: GlobSet,
    has_include: bool,
}

fn compile(root: &Path, patterns: &[String]) -> Result<Compiled, GlobError> {
    let mut inc = GlobSetBuilder::new();
    let mut exc = GlobSetBuilder::new();
    let mut has_include = false;
    for raw in patterns {
        let (negated, body) = match raw.strip_prefix('!') {
            Some(b) => (true, b.trim()),
            None => (false, raw.as_str()),
        };
        let rel = relative_pattern(root, body).map_err(|_| GlobError::Outside(raw.clone()))?;
        if rel.is_empty() {
            return Err(GlobError::Invalid(raw.clone(), "it is empty".into()));
        }
        // A pattern may name a file, a directory (then everything below it) or a glob.
        for variant in [rel.clone(), format!("{}/**", rel.trim_end_matches('/'))] {
            let g = GlobBuilder::new(&variant)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map_err(|e| GlobError::Invalid(raw.clone(), e.kind().to_string()))?;
            if negated {
                exc.add(g);
            } else {
                inc.add(g);
            }
        }
        if !negated {
            has_include = true;
        }
    }
    let build = |b: GlobSetBuilder| b.build().map_err(|e| GlobError::Invalid(String::new(), e.to_string()));
    Ok(Compiled { include: build(inc)?, exclude: build(exc)?, has_include })
}

/// A pattern as a path relative to `root` with `/` separators, or an error when it would leave
/// the workspace.
fn relative_pattern(root: &Path, pattern: &str) -> Result<String, ()> {
    let p = pattern.trim();
    let p = p.strip_prefix("./").unwrap_or(p);
    let rel: String = if p.starts_with('/') {
        let root_s = root.to_string_lossy();
        let root_s = root_s.trim_end_matches('/');
        match p.strip_prefix(root_s) {
            Some(rest) if rest.starts_with('/') || rest.is_empty() => rest.trim_start_matches('/').to_string(),
            _ => return Err(()),
        }
    } else {
        p.to_string()
    };
    if rel.split('/').any(|seg| seg == "..") {
        return Err(());
    }
    Ok(if rel == "." { "**".to_string() } else { rel })
}

/// Every regular file under `root` that the patterns select, as paths relative to `root`,
/// sorted. Symbolic links are skipped, whether to files or directories.
pub fn expand(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>, GlobError> {
    let c = compile(root, patterns)?;
    if !c.has_include {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            // A file that vanishes mid-walk, or a directory we cannot read, is not fatal.
            Err(e)
                if e.io_error()
                    .is_some_and(|io| matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied)) =>
            {
                continue;
            }
            Err(e) => return Err(GlobError::Io(e.to_string())),
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if c.include.is_match(&rel_str) && !c.exclude.is_match(&rel_str) {
            out.push(rel.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// The deepest directory that contains every path (relative paths in, relative path out).
/// A single file's parent; the empty path when the files share nothing.
pub fn common_ancestor(files: &[PathBuf]) -> PathBuf {
    let mut it = files.iter();
    let Some(first) = it.next() else { return PathBuf::new() };
    let mut common: Vec<_> = first.parent().map(|p| p.components().collect()).unwrap_or_default();
    for f in it {
        let comps: Vec<_> = f.parent().map(|p| p.components().collect()).unwrap_or_default();
        let n = common.iter().zip(comps.iter()).take_while(|(a, b)| a == b).count();
        common.truncate(n);
    }
    common.iter().collect()
}

/// A stable digest of a set of files, as `hashFiles()` defines it: the SHA-256 of the
/// concatenated SHA-256 digests of the files in sorted path order. No files, no hash.
pub fn hash_files(root: &Path, patterns: &[String]) -> Result<String, GlobError> {
    use sha2::{Digest, Sha256};
    let files = expand(root, patterns)?;
    if files.is_empty() {
        return Ok(String::new());
    }
    let mut outer = Sha256::new();
    for f in files {
        let mut file = std::fs::File::open(root.join(&f)).map_err(|e| GlobError::Io(format!("{}: {e}", f.display())))?;
        let mut h = Sha256::new();
        std::io::copy(&mut file, &mut h).map_err(|e| GlobError::Io(format!("{}: {e}", f.display())))?;
        outer.update(h.finalize());
    }
    Ok(hex::encode(outer.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (p, c) in [
            ("a.txt", "A"),
            ("b.log", "B"),
            ("src/main.rs", "fn main(){}"),
            ("src/lib.rs", "pub fn x(){}"),
            ("src/deep/mod.rs", "mod"),
            ("target/debug/out", "bin"),
            (".hidden/x", "h"),
        ] {
            let path = d.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        d
    }

    fn names(v: Vec<PathBuf>) -> Vec<String> {
        v.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }

    fn pats(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn single_star_stays_in_one_segment() {
        let d = tree();
        assert_eq!(names(expand(d.path(), &pats(&["*.txt"])).unwrap()), ["a.txt"]);
        assert_eq!(names(expand(d.path(), &pats(&["src/*.rs"])).unwrap()), ["src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn double_star_crosses_directories() {
        let d = tree();
        assert_eq!(names(expand(d.path(), &pats(&["src/**/*.rs"])).unwrap()), ["src/deep/mod.rs", "src/lib.rs", "src/main.rs"]);
        assert_eq!(names(expand(d.path(), &pats(&["**/mod.rs"])).unwrap()), ["src/deep/mod.rs"]);
    }

    #[test]
    fn a_directory_name_means_everything_under_it() {
        let d = tree();
        assert_eq!(names(expand(d.path(), &pats(&["src"])).unwrap()), ["src/deep/mod.rs", "src/lib.rs", "src/main.rs"]);
        assert_eq!(names(expand(d.path(), &pats(&["src/"])).unwrap()), ["src/deep/mod.rs", "src/lib.rs", "src/main.rs"]);
        assert_eq!(names(expand(d.path(), &pats(&["target/debug"])).unwrap()), ["target/debug/out"]);
    }

    #[test]
    fn negation_excludes() {
        let d = tree();
        assert_eq!(names(expand(d.path(), &pats(&["src", "!src/deep"])).unwrap()), ["src/lib.rs", "src/main.rs"]);
        assert_eq!(names(expand(d.path(), &pats(&["**/*.rs", "!**/main.rs"])).unwrap()), ["src/deep/mod.rs", "src/lib.rs"]);
        assert!(expand(d.path(), &pats(&["!a.txt"])).unwrap().is_empty(), "only excludes selects nothing");
    }

    #[test]
    fn dot_means_everything_and_hidden_files_are_included() {
        let d = tree();
        let all = names(expand(d.path(), &pats(&["."])).unwrap());
        assert_eq!(all.len(), 7);
        assert!(all.contains(&".hidden/x".to_string()));
    }

    #[test]
    fn absolute_patterns_must_be_inside_the_workspace() {
        let d = tree();
        let inside = format!("{}/src/lib.rs", d.path().display());
        assert_eq!(names(expand(d.path(), &[inside]).unwrap()), ["src/lib.rs"]);
        assert_eq!(expand(d.path(), &pats(&["/etc/passwd"])), Err(GlobError::Outside("/etc/passwd".into())));
        assert_eq!(expand(d.path(), &pats(&["/home/codehaven/.runner"])), Err(GlobError::Outside("/home/codehaven/.runner".into())));
        // A sibling that merely starts with the same text is not inside.
        let sibling = format!("{}-other/x", d.path().display());
        assert!(matches!(expand(d.path(), &[sibling]), Err(GlobError::Outside(_))));
    }

    #[test]
    fn parent_references_are_refused() {
        let d = tree();
        assert!(matches!(expand(d.path(), &pats(&["../x"])), Err(GlobError::Outside(_))));
        assert!(matches!(expand(d.path(), &pats(&["src/../../x"])), Err(GlobError::Outside(_))));
        assert!(matches!(expand(d.path(), &pats(&["!../x", "a.txt"])), Err(GlobError::Outside(_))));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        let d = tree();
        let secret = tempfile::tempdir().unwrap();
        std::fs::write(secret.path().join(".runner"), "credential").unwrap();
        std::os::unix::fs::symlink(secret.path().join(".runner"), d.path().join("link-to-file")).unwrap();
        std::os::unix::fs::symlink(secret.path(), d.path().join("link-to-dir")).unwrap();
        let all = names(expand(d.path(), &pats(&["**"])).unwrap());
        assert!(!all.iter().any(|n| n.contains("link-to")), "got {all:?}");
        assert!(!all.iter().any(|n| n.contains(".runner")));
        let direct = names(expand(d.path(), &pats(&["link-to-file", "link-to-dir/.runner"])).unwrap());
        assert!(direct.is_empty(), "got {direct:?}");
    }

    #[test]
    fn invalid_globs_are_errors_not_panics() {
        let d = tree();
        assert!(matches!(expand(d.path(), &pats(&["src/[abc"])), Err(GlobError::Invalid(..))));
        assert!(matches!(expand(d.path(), &pats(&["{a,b"])), Err(GlobError::Invalid(..))));
    }

    #[test]
    fn pattern_lists_split_on_lines() {
        assert_eq!(split_patterns("a\n\n  b  \n# note\n!c\n"), ["a", "b", "!c"]);
        assert!(split_patterns("  \n").is_empty());
    }

    #[test]
    fn common_ancestors() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(common_ancestor(&[p("a/b/c.txt"), p("a/b/d/e.txt")]), p("a/b"));
        assert_eq!(common_ancestor(&[p("a/x"), p("b/y")]), p(""));
        assert_eq!(common_ancestor(&[p("only/one/file")]), p("only/one"));
        assert_eq!(common_ancestor(&[p("top")]), p(""));
        assert_eq!(common_ancestor(&[]), p(""));
    }

    #[test]
    fn file_hashes_are_stable_and_change_with_content() {
        let d = tree();
        let h1 = hash_files(d.path(), &pats(&["src/**"])).unwrap();
        assert_eq!(h1.len(), 64);
        assert_eq!(h1, hash_files(d.path(), &pats(&["src/**"])).unwrap());
        assert_eq!(hash_files(d.path(), &pats(&["nothing/**"])).unwrap(), "");
        std::fs::write(d.path().join("src/lib.rs"), "changed").unwrap();
        assert_ne!(h1, hash_files(d.path(), &pats(&["src/**"])).unwrap());
    }

    #[test]
    fn the_hash_matches_the_documented_construction() {
        use sha2::{Digest, Sha256};
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), "one").unwrap();
        std::fs::write(d.path().join("b"), "two").unwrap();
        let mut outer = Sha256::new();
        outer.update(Sha256::digest(b"one"));
        outer.update(Sha256::digest(b"two"));
        assert_eq!(hash_files(d.path(), &pats(&["*"])).unwrap(), hex::encode(outer.finalize()));
    }
}
