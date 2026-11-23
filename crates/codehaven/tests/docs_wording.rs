//! Wording that the three visibility tiers made false must not come back.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

const BANNED: &[&str] =
    &["no anonymous clone", "Nobody else can", "Nothing is readable without an account", "Every account on this server can read"];

/// Every file under `dir`, recursively. A directory that cannot be read adds nothing.
fn collect_all(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_all(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Every `.rs` file under `dir`, recursively. A directory that cannot be read adds nothing.
fn collect_rust(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_file_repeats_the_old_visibility_wording() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = fs::canonicalize(&root).unwrap_or(root);

    let mut files: Vec<PathBuf> = ["ARCHITECTURE.md", "README.md", "ROADMAP.md"].iter().map(|name| root.join(name)).collect();
    collect_all(&root.join("docs"), &mut files);
    collect_all(&root.join("crates/ch-web/templates"), &mut files);
    if let Ok(crates) = fs::read_dir(root.join("crates")) {
        for crate_dir in crates.flatten() {
            collect_rust(&crate_dir.path().join("src"), &mut files);
        }
    }

    let mut failures = Vec::new();
    for path in &files {
        if path.to_string_lossy().contains("/migrations/") {
            continue;
        }
        let Ok(bytes) = fs::read(path) else { continue };
        let Ok(text) = String::from_utf8(bytes) else { continue };
        let lower = text.to_lowercase();
        for phrase in BANNED {
            let needle = phrase.to_lowercase();
            if lower.contains(needle.as_str()) {
                failures.push(format!("{}: {:?}", path.display(), phrase));
            }
        }
    }
    assert!(failures.is_empty(), "old visibility wording is back:\n  {}", failures.join("\n  "));
}
