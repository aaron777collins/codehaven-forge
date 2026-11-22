//! Which files in a repository are workflows.

use crate::{WORKFLOW_DIRS, is_workflow_file};

/// A directory listing entry: the directory (one of `WORKFLOW_DIRS`) and the file name in it.
pub type Listing<'a> = (&'a str, &'a str);

/// Picks the workflow files to read, as full paths, in a stable order. `.codehaven/workflows`
/// comes first and wins when the same file name is in both directories. `.github/workflows` is
/// only read when `read_github` is on.
pub fn select(files: &[Listing<'_>], read_github: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen_names: Vec<&str> = Vec::new();
    for dir in WORKFLOW_DIRS {
        if *dir == ".github/workflows" && !read_github {
            continue;
        }
        let mut names: Vec<&str> =
            files.iter().filter(|(d, n)| d == dir && is_workflow_file(n) && is_plain_name(n)).map(|(_, n)| *n).collect();
        names.sort_unstable();
        names.dedup();
        for n in names {
            if seen_names.contains(&n) {
                continue;
            }
            seen_names.push(n);
            out.push(format!("{dir}/{n}"));
        }
    }
    out
}

/// A name with no path tricks. Directory entries should never have these, but the names come
/// from a repository, which is user data.
fn is_plain_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && !n.contains(['/', '\\', '\0']) && n != "." && n != ".."
}

/// What the UI calls a workflow: its `name:`, else the file name without its extension.
pub fn display_name(name: Option<&str>, path: &str) -> String {
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".yml").or_else(|| file.strip_suffix(".yaml")).unwrap_or(file).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codehaven_wins_on_a_name_clash() {
        let files = [
            (".github/workflows", "ci.yml"),
            (".codehaven/workflows", "ci.yml"),
            (".github/workflows", "lint.yaml"),
            (".codehaven/workflows", "notes.md"),
        ];
        assert_eq!(select(&files, true), vec![".codehaven/workflows/ci.yml", ".github/workflows/lint.yaml"]);
    }

    #[test]
    fn github_directory_can_be_turned_off() {
        let files = [(".github/workflows", "ci.yml"), (".codehaven/workflows", "a.yml")];
        assert_eq!(select(&files, false), vec![".codehaven/workflows/a.yml"]);
    }

    #[test]
    fn only_workflow_files_in_known_directories() {
        let files = [
            ("src", "ci.yml"),
            (".codehaven/workflows", "x.txt"),
            (".codehaven/workflows", "../evil.yml"),
            (".codehaven/workflows", "a.yml"),
            (".codehaven/workflows", "a.yml"),
        ];
        assert_eq!(select(&files, true), vec![".codehaven/workflows/a.yml"]);
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name(Some("Build and test"), ".codehaven/workflows/ci.yml"), "Build and test");
        assert_eq!(display_name(None, ".github/workflows/release.yaml"), "release");
        assert_eq!(display_name(Some("  "), ".github/workflows/ci.yml"), "ci");
    }
}
