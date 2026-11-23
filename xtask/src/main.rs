//! `cargo xtask`: the repo's task runner. Runs inside the builder container
//! (see `deploy/dev/slot.sh`), so `DATABASE_URL` points at the throwaway `pg-test`.
//!
//! ```text
//! cargo xtask fmt         formatting check
//! cargo xtask lint        clippy with warnings denied
//! cargo xtask test [-p crate ...]   nextest, whole workspace by default
//! cargo xtask db-prepare  migrate pg-test, then refresh .sqlx/ (run after any query or migration change)
//! cargo xtask db-check    fail when .sqlx/ is stale
//! cargo xtask portable    fail when a name, address or path of one deployment is in the tree
//! cargo xtask vendor-check  check the vendored workspace files: LOCK, hashes, licences, budgets
//! cargo xtask ci          portable, vendor-check, fmt, lint, db-check, build, test: the gate every stream runs before a merge
//! ```

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Fmt,
    Lint {
        /// Limit to these crates.
        #[arg(short, long)]
        package: Vec<String>,
    },
    Test {
        #[arg(short, long)]
        package: Vec<String>,
        /// Extra arguments for nextest, after `--`.
        #[arg(last = true)]
        rest: Vec<String>,
    },
    DbPrepare,
    DbCheck,
    Portable,
    /// Check the vendored browser files: LOCK hashes, licences, size budgets, sink lint.
    VendorCheck,
    Ci,
}

fn run(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    eprintln!("$ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("starting {program}"))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

fn pkg_args(packages: &[String]) -> Vec<String> {
    if packages.is_empty() {
        vec!["--workspace".to_string()]
    } else {
        packages.iter().flat_map(|p| ["-p".to_string(), p.clone()]).collect()
    }
}

fn cargo(base: &[&str], extra: &[String], tail: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    let mut args: Vec<&str> = base.to_vec();
    args.extend(extra.iter().map(String::as_str));
    args.extend_from_slice(tail);
    run("cargo", &args, envs)
}

/// The portability guard: first thing in `ci`, because it is cheap and a leak is easy to fix early.
fn portable() -> Result<()> {
    run("python3", &["deploy/check-portable.py", "--self-test"], &[])?;
    run("python3", &["deploy/check-portable.py"], &[])
}

/// The vendored browser workspace and our own scripts beside it, both embedded in the binary.
const VENDOR_DIR: &str = "crates/ch-web/workspace/vendor";
const WORKSPACE_SRC_DIR: &str = "crates/ch-web/workspace/src";
/// The terminal frame's own script, beside the editor's src tree. The folder may not exist yet.
const WORKSPACE_FRAME_DIR: &str = "crates/ch-web/workspace/frame";

/// Size budgets in gzip bytes: the binary carries every one of these files.
const MONACO_BUDGET: u64 = 1_700_000;
const JS_BUDGET: u64 = 80 * 1024;
const CSS_BUDGET: u64 = 15 * 1024;
const XTERM_BUDGET: u64 = 135 * 1024;
const JUST_BASH_BUDGET: u64 = 400 * 1024;

/// Licences we may redistribute in a public MIT project.
const ALLOWED_LICENCES: [&str; 6] = ["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "CC-BY-4.0"];

/// Files in vendor/ that describe the vendoring and are not vendored code.
const NOT_VENDORED: [&str; 4] = ["LOCK", "THIRD_PARTY_NOTICES", "README.md", ".keep"];

/// Sinks that turn untrusted text into markup, plus the two classic escapes. The workspace builds
/// its DOM with textContent, so none of these may appear in a script of our own.
const SINK_WORDS: [&str; 6] = ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "eval(", "new Function"];

struct Lock {
    /// Every `component` line, in the order the LOCK names them.
    components: Vec<String>,
    /// (name, licence) of the component and of the third-party code bundled inside its files.
    licences: Vec<(String, String)>,
    /// (path, sha256) of every vendored file, relative to vendor/.
    files: Vec<(String, String)>,
}

/// Parses the LOCK written by deploy/workspace-vendor/vendor.py and terminal/build.sh:
/// `<word> <rest>` lines, `#` comments.
fn parse_lock(text: &str) -> Lock {
    let mut components = Vec::new();
    let mut licences = Vec::new();
    let mut files = Vec::new();
    // A `licence` line names the component it sits under. vendor/LOCK opens with
    // `component monaco-editor`, but the default keeps a LOCK that names no component at all
    // parsing the way the parser did before there was a second component to name.
    let mut component = "monaco-editor".to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((word, rest)) = line.split_once(' ') else { continue };
        let rest = rest.trim();
        match word {
            "component" => {
                component = rest.to_string();
                components.push(component.clone());
            }
            // `licence` is the current component itself; each `bundles` line names code inside its files.
            "licence" => licences.push((component.clone(), rest.to_string())),
            "bundles" => {
                // The licence is the last word; what sits between is the version, and is optional.
                let words: Vec<&str> = rest.split_whitespace().collect();
                if let (Some(&name), Some(&licence)) = (words.first(), words.last()) {
                    licences.push((name.to_string(), licence.to_string()));
                }
            }
            "file" => {
                if let Some((path, sha256)) = rest.split_once(' ') {
                    files.push((path.to_string(), sha256.trim().to_string()));
                }
            }
            _ => {}
        }
    }
    Lock { components, licences, files }
}

/// Every licence in LOCK must be one we may redistribute, and every component must state its own.
fn licence_problems(lock: &Lock) -> Vec<String> {
    let mut problems = Vec::new();
    // monaco-editor is the component of a LOCK that names no `component` line, the shape
    // vendor/LOCK had before the terminal added a second one; either way its licence must be stated.
    let monaco_listed = lock.components.iter().any(|name| name == "monaco-editor");
    if !monaco_listed && !lock.licences.iter().any(|(name, _)| name == "monaco-editor") {
        problems.push("monaco-editor: LOCK has no licence line".to_string());
    }
    for name in &lock.components {
        if !lock.licences.iter().any(|(licencee, _)| licencee == name) {
            problems.push(format!("{name}: LOCK has no licence line"));
        }
    }
    for (name, licence) in &lock.licences {
        if !ALLOWED_LICENCES.contains(&licence.as_str()) {
            problems.push(format!("{name}: licence {licence} is not allowed in a public MIT project"));
        }
    }
    problems
}

/// Every line of a workspace script that hands text to an HTML sink, or escapes into code.
fn sink_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().starts_with("//") {
            continue;
        }
        let number = index + 1;
        for word in SINK_WORDS {
            if line.contains(word) {
                problems.push(format!("{file}:{number}: {word} is not allowed in workspace code; use textContent"));
            }
        }
    }
    problems
}

/// Every module a shipped Monaco file loads must be shipped too. `files` maps each file's path under
/// vendor/monaco/ with the `.gz` removed (for example `vs/javascript-B_IYblwB.js`) to its text.
fn amd_problems(files: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, text) in files {
        if !file.ends_with(".js") {
            continue;
        }
        let dir: Vec<&str> = match file.rfind('/') {
            Some(slash) => file[..slash].split('/').collect(),
            None => Vec::new(),
        };
        let mut reported = std::collections::BTreeSet::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            let quote = i;
            let start = quote + 1;
            let mut end = start;
            while end < bytes.len() && is_module_path_byte(bytes[end]) {
                end += 1;
            }
            // Step past the literal whether it closed on a quote, broke on a character no module
            // path has, or ran to the end of the file.
            i = end + 1;
            let literal = &text[start..end];
            let closed = end < bytes.len() && bytes[end] == b'"';
            // A module names a file, so `"./"` (a base URL in the worker code) is not one.
            let module_path = (literal.starts_with("./") || literal.starts_with("../")) && !literal.ends_with('/');
            let worker_url = text[..quote].ends_with("toUrl(");
            if closed && module_path && !worker_url && !reported.contains(literal) && !resolves_to_shipped(files, &dir, literal) {
                reported.insert(literal.to_string());
                problems.push(format!("monaco/{file}: loads {literal}, which is not shipped"));
            }
        }
    }
    problems
}

/// The characters a module path is made of. Any other character before the closing quote means the
/// string is prose, a URL or a fragment, not a module reference.
fn is_module_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/')
}

/// A module reference is satisfied when the joined path names a shipped file, with or without `.js`.
/// A `..` that would climb above the top of `files` satisfies nothing, even when the joined path
/// happens to name a shipped file.
fn resolves_to_shipped(files: &std::collections::BTreeMap<String, String>, dir: &[&str], literal: &str) -> bool {
    let mut segments = dir.to_vec();
    let mut under_root = true;
    for segment in literal.split('/') {
        match segment {
            "." | "" => {}
            ".." => under_root &= segments.pop().is_some(),
            name => segments.push(name),
        }
    }
    let resolved = segments.join("/");
    under_root && (files.contains_key(resolved.as_str()) || files.contains_key(&format!("{resolved}.js")))
}

/// Every regular file under `dir`, sorted, so the problems come out the same way on every run.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return files };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files.extend(walk_files(&path)),
            Ok(kind) if kind.is_file() => files.push(path),
            _ => {}
        }
    }
    files.sort();
    files
}

/// The path of `path` under `root`, with `/` separators, the way LOCK writes them.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The size of `bytes` after a level 9 gzip, the encoding the server sends these files in.
fn gzip_size(bytes: &[u8]) -> u64 {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
    encoder.write_all(bytes).expect("gzip into memory");
    encoder.finish().expect("gzip into memory").len() as u64
}

/// Total gzip size of the `*.extension` files directly in `dir`: the workspace's own code.
fn gzip_total(dir: &Path, extension: &str) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return total };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != extension) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            total += gzip_size(&bytes);
        }
    }
    total
}

/// Decompresses one vendored file into the scratch tree.
fn decompress_file(gz: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut decoder = flate2::read::GzDecoder::new(std::fs::File::open(gz)?);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    std::fs::write(dest, bytes)
}

/// `cargo xtask vendor-check`: everything deploy/workspace-vendor/vendor.py and
/// deploy/workspace-vendor/terminal/build.sh write, checked.
/// Problems are collected first and reported together, so one run shows the whole picture.
fn vendor_check() -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    let vendor_dir = Path::new(VENDOR_DIR);
    let src_dir = Path::new(WORKSPACE_SRC_DIR);
    let frame_dir = Path::new(WORKSPACE_FRAME_DIR);

    // The LOCK is the spine of the check; without it everything below would only repeat itself.
    let lock_path = vendor_dir.join("LOCK");
    let lock_text = match std::fs::read_to_string(&lock_path) {
        Ok(text) => text,
        Err(err) => bail!(
            "vendor-check found problems:\n{}: cannot be read ({err}); run deploy/workspace-vendor/vendor.py first",
            lock_path.display()
        ),
    };
    let mut lock = parse_lock(&lock_text);
    // The terminal's LOCK sits in the terminal subfolder, because vendor.py rewrites vendor/LOCK
    // whole and so must never record what build.sh vendored. The two merge into one check.
    match std::fs::read_to_string(vendor_dir.join("terminal/LOCK")) {
        Ok(text) => {
            let terminal = parse_lock(&text);
            lock.components.extend(terminal.components);
            lock.licences.extend(terminal.licences);
            lock.files.extend(terminal.files);
        }
        Err(_) => problems.push("terminal/LOCK: missing".to_string()),
    }
    problems.extend(licence_problems(&lock));

    // Every file LOCK lists exists, is gzipped, and hashes to what was vendored.
    for (path, want) in &lock.files {
        if !path.ends_with(".gz") {
            problems.push(format!("{path}: only .gz files may be listed in LOCK"));
        }
        match std::fs::read(vendor_dir.join(path)) {
            Err(_) => problems.push(format!("{path}: listed in LOCK but missing")),
            Ok(bytes) => {
                if hex::encode(Sha256::digest(&bytes)) != *want {
                    problems.push(format!("{path}: hash does not match LOCK"));
                }
            }
        }
    }

    // And the other way round: nothing sits in vendor/ without a line in LOCK. The describing
    // files (LOCK, THIRD_PARTY_NOTICES, README.md) are matched by name, not by place, because
    // the terminal subfolder keeps a pair of its own.
    for path in walk_files(vendor_dir) {
        let rel = relative(vendor_dir, &path);
        if path.file_name().is_some_and(|name| NOT_VENDORED.contains(&name.to_str().unwrap_or_default())) {
            continue;
        }
        if !lock.files.iter().any(|(listed, _)| listed == &rel) {
            problems.push(format!("{rel}: not listed in LOCK"));
        }
    }

    // Every AMD module a shipped file loads must be shipped too: one that is missing is a 404 in
    // the browser that no build step would catch.
    let monaco_dir = vendor_dir.join("monaco");
    let mut texts = std::collections::BTreeMap::new();
    for path in walk_files(&monaco_dir) {
        let rel = relative(&monaco_dir, &path);
        let Some(key) = rel.strip_suffix(".gz") else { continue };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                problems.push(format!("monaco/{key}: cannot be read ({err})"));
                continue;
            }
        };
        let mut plain = Vec::new();
        if let Err(err) = flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut plain) {
            problems.push(format!("monaco/{key}: cannot be read ({err})"));
            continue;
        }
        texts.insert(key.to_string(), String::from_utf8_lossy(&plain).into_owned());
    }
    problems.extend(amd_problems(&texts));

    // The notices have to name everything we redistribute.
    match std::fs::read_to_string(vendor_dir.join("THIRD_PARTY_NOTICES")) {
        Err(_) => problems.push("THIRD_PARTY_NOTICES: missing".to_string()),
        Ok(notices) => {
            for word in ["Monaco Editor", "DOMPurify", "marked", "Codicons"] {
                if !notices.contains(word) {
                    problems.push(format!("THIRD_PARTY_NOTICES: does not mention {word}"));
                }
            }
        }
    }

    // The terminal's notices sit beside its LOCK, for the same reason, and carry their own list.
    match std::fs::read_to_string(vendor_dir.join("terminal/THIRD_PARTY_NOTICES")) {
        Err(_) => problems.push("terminal/THIRD_PARTY_NOTICES: missing".to_string()),
        Ok(notices) => {
            for word in ["xterm.js", "just-bash", "diff", "sprintf-js", "re2js", "file-type"] {
                if !notices.contains(word) {
                    problems.push(format!("terminal/THIRD_PARTY_NOTICES: does not mention {word}"));
                }
            }
        }
    }

    // Size budgets in gzip bytes.
    let monaco_gz: u64 =
        walk_files(&vendor_dir.join("monaco")).iter().filter_map(|path| std::fs::metadata(path).ok()).map(|metadata| metadata.len()).sum();
    println!("vendor-check: monaco/ is {monaco_gz} bytes gzipped (budget {MONACO_BUDGET})");
    if monaco_gz > MONACO_BUDGET {
        problems.push(format!("monaco/ is {monaco_gz} bytes gzipped, over the {MONACO_BUDGET} budget"));
    }
    let terminal_dir = vendor_dir.join("terminal");
    let xterm_gz: u64 = ["xterm.js.gz", "xterm.css.gz"]
        .into_iter()
        .filter_map(|name| std::fs::metadata(terminal_dir.join(name)).ok())
        .map(|metadata| metadata.len())
        .sum();
    println!("vendor-check: xterm is {xterm_gz} bytes gzipped (budget {XTERM_BUDGET})");
    if xterm_gz > XTERM_BUDGET {
        problems.push(format!("xterm is {xterm_gz} bytes gzipped, over the {XTERM_BUDGET} budget"));
    }
    let just_bash_gz = std::fs::metadata(terminal_dir.join("just-bash.js.gz")).map_or(0, |metadata| metadata.len());
    println!("vendor-check: just-bash is {just_bash_gz} bytes gzipped (budget {JUST_BASH_BUDGET})");
    if just_bash_gz > JUST_BASH_BUDGET {
        problems.push(format!("just-bash is {just_bash_gz} bytes gzipped, over the {JUST_BASH_BUDGET} budget"));
    }
    let js = gzip_total(src_dir, "js") + gzip_total(frame_dir, "js");
    println!("vendor-check: workspace JS is {js} bytes gzipped (budget {JS_BUDGET})");
    if js > JS_BUDGET {
        problems.push(format!("workspace JS is {js} bytes gzipped, over the {JS_BUDGET} budget"));
    }
    let css = gzip_total(src_dir, "css");
    println!("vendor-check: workspace CSS is {css} bytes gzipped (budget {CSS_BUDGET})");
    if css > CSS_BUDGET {
        problems.push(format!("workspace CSS is {css} bytes gzipped, over the {CSS_BUDGET} budget"));
    }

    // Our own scripts never turn untrusted text into markup, in the editor's src tree and in
    // the terminal frame's alike.
    let mut scripts = walk_files(src_dir);
    scripts.extend(walk_files(frame_dir));
    for path in scripts {
        if path.extension().is_none_or(|ext| ext != "js") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => problems.extend(sink_problems(&path.display().to_string(), &text)),
            Err(_) => problems.push(format!("{}: cannot be read", path.display())),
        }
    }

    // The vendored files, decompressed, go through the same portability scan as the rest of the tree.
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let name = format!("codehaven-vendor-check-{}-{}", std::process::id(), stamp.as_millis());
    let scratch = std::env::temp_dir().join(name);
    let mut decompressed = 0;
    for path in walk_files(vendor_dir) {
        let rel = relative(vendor_dir, &path);
        let Some(stem) = rel.strip_suffix(".gz") else { continue };
        match decompress_file(&path, &scratch.join(stem)) {
            Ok(()) => decompressed += 1,
            Err(_) => problems.push(format!("{rel}: cannot be decompressed")),
        }
    }
    if decompressed > 0 {
        let root = scratch.to_string_lossy().into_owned();
        if run("python3", &["deploy/check-portable.py", "--root", &root], &[]).is_err() {
            problems.push("the decompressed vendor files failed check-portable.py".to_string());
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    if problems.is_empty() {
        println!("vendor-check: ok");
        Ok(())
    } else {
        bail!("vendor-check found problems:\n{}", problems.join("\n"));
    }
}

fn migrate_test_db() -> Result<()> {
    if std::env::var("DATABASE_URL").is_err() {
        bail!("DATABASE_URL is not set. Run this inside the builder: make ci, or deploy/dev/slot.sh <stream> \"cargo xtask ...\"");
    }
    // `create` is a no-op when the database exists.
    run("sqlx", &["database", "create"], &[])?;
    run("sqlx", &["migrate", "run", "--source", "crates/ch-db/migrations"], &[])
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("xtask: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Fmt => run("cargo", &["fmt", "--all", "--", "--check"], &[]),
        Cmd::Lint { package } => cargo(&["clippy"], &pkg_args(&package), &["--all-targets", "--", "-D", "warnings"], &[]),
        Cmd::Test { package, rest } => {
            let mut tail: Vec<&str> = vec![];
            tail.extend(rest.iter().map(String::as_str));
            cargo(&["nextest", "run"], &pkg_args(&package), &tail, &[])
        }
        Cmd::DbPrepare => {
            migrate_test_db()?;
            // The macros check queries against the live database while preparing.
            run("cargo", &["sqlx", "prepare", "--workspace", "--", "--all-targets"], &[("SQLX_OFFLINE", "false")])
        }
        Cmd::DbCheck => {
            migrate_test_db()?;
            run("cargo", &["sqlx", "prepare", "--check", "--workspace", "--", "--all-targets"], &[("SQLX_OFFLINE", "false")])
        }
        Cmd::Portable => portable(),
        Cmd::VendorCheck => vendor_check(),
        Cmd::Ci => {
            portable()?;
            vendor_check()?;
            run("cargo", &["fmt", "--all", "--", "--check"], &[])?;
            migrate_test_db()?;
            run("cargo", &["sqlx", "prepare", "--check", "--workspace", "--", "--all-targets"], &[("SQLX_OFFLINE", "false")])?;
            run("cargo", &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"], &[])?;
            run("cargo", &["build", "--workspace"], &[])?;
            run("cargo", &["nextest", "run", "--workspace"], &[])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_licence_we_do_not_redistribute_is_refused() {
        let lock = parse_lock("component monaco-editor\nlicence GPL-3.0\n");
        assert_eq!(licence_problems(&lock), vec!["monaco-editor: licence GPL-3.0 is not allowed in a public MIT project"]);
    }

    #[test]
    fn the_bundles_licence_is_the_last_word() {
        let lock = parse_lock("licence MIT\nbundles DOMPurify 3.4.15 Apache-2.0\nbundles marked MIT\n");
        assert!(licence_problems(&lock).is_empty());
        assert!(lock.licences.contains(&("DOMPurify".to_string(), "Apache-2.0".to_string())));
        assert!(lock.licences.contains(&("marked".to_string(), "MIT".to_string())));
    }

    #[test]
    fn each_component_gets_its_own_licence() {
        let lock =
            parse_lock("component monaco-editor\nlicence MIT\ncomponent just-bash\nlicence Apache-2.0\nbundles diff 8.0.4 BSD-3-Clause\n");
        assert_eq!(lock.components, vec!["monaco-editor".to_string(), "just-bash".to_string()]);
        assert!(lock.licences.contains(&("monaco-editor".to_string(), "MIT".to_string())));
        assert!(lock.licences.contains(&("just-bash".to_string(), "Apache-2.0".to_string())));
        assert!(lock.licences.contains(&("diff".to_string(), "BSD-3-Clause".to_string())));
    }

    #[test]
    fn a_component_without_a_licence_line_is_reported() {
        let lock = parse_lock("component monaco-editor\nlicence MIT\ncomponent just-bash\n");
        assert_eq!(licence_problems(&lock), vec!["just-bash: LOCK has no licence line"]);
    }

    #[test]
    fn a_later_component_with_a_gpl_licence_is_refused() {
        let lock = parse_lock("component monaco-editor\nlicence MIT\ncomponent just-bash\nlicence GPL-3.0\n");
        assert_eq!(licence_problems(&lock), vec!["just-bash: licence GPL-3.0 is not allowed in a public MIT project"]);
    }

    #[test]
    fn the_sink_lint_refuses_innerhtml_but_not_a_comment() {
        let problems = sink_problems("app.js", "el.innerHTML = x;\n// innerHTML in a comment\n");
        assert_eq!(problems, vec!["app.js:1: innerHTML is not allowed in workspace code; use textContent"]);
    }

    #[test]
    fn a_module_that_is_not_shipped_is_reported() {
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            "vs/javascript-B.js".to_string(),
            r#"define("vs/javascript-B",["exports","./typescript-T"],function(){});"#.to_string(),
        );
        assert_eq!(amd_problems(&files), vec!["monaco/vs/javascript-B.js: loads ./typescript-T, which is not shipped"]);
    }

    #[test]
    fn a_shipped_module_resolves_with_or_without_js() {
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            "vs/javascript-B.js".to_string(),
            r#"define("vs/javascript-B",["exports","./typescript-T"],function(){});"#.to_string(),
        );
        files.insert("vs/typescript-T.js".to_string(), String::new());
        files.insert("vs/basic-languages/monaco.contribution.js".to_string(), r#"a(["../typescript-T"],e,s)"#.to_string());
        assert!(amd_problems(&files).is_empty());
    }

    #[test]
    fn a_worker_url_is_not_a_module() {
        let mut files = std::collections::BTreeMap::new();
        files.insert("vs/typescript-T.js".to_string(), r#"require.toUrl("./assets/ts.worker-g.js")"#.to_string());
        assert!(amd_problems(&files).is_empty());
    }

    #[test]
    fn a_bare_directory_is_not_a_module() {
        let mut files = std::collections::BTreeMap::new();
        files.insert("vs/assets/editor.worker-W.js".to_string(), r#"new URL("./",import.meta.url)"#.to_string());
        assert!(amd_problems(&files).is_empty());
    }

    #[test]
    fn a_dynamic_load_of_a_dropped_mode_is_reported() {
        let mut files = std::collections::BTreeMap::new();
        files.insert("vs/monaco.contribution-C.js".to_string(), r#"S(["./tsMode-D"],e,t)"#.to_string());
        assert_eq!(amd_problems(&files), vec!["monaco/vs/monaco.contribution-C.js: loads ./tsMode-D, which is not shipped"]);
    }
}
