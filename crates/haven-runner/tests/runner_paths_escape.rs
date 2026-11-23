//! ch-ctk.1, the whole sweep in one test: before each operation the runner does in a job
//! directory, the job swaps one path it can reach for a link to a stand-in for the runner's home.
//! Whatever the runner does then, nothing in that home may be written, replaced or read into
//! anything the job gets back (an output, a log, an artifact, a cache).
//!
//! In these tests a step's shell runs on the host, where it could follow its own links. A real
//! job runs in a container that cannot see the runner's home, so every step here is chosen to
//! write nothing through a link itself: what the test catches is the runner's doing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use haven_runner::testing::TestJob;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const MARK: &str = "runner-home-secret";

/// Every path below the job root a job can swap and the runner process opens. `{next}` is the
/// number of the step that follows the swap. `_runner/pids` is left out: only the step's own
/// shell writes there, inside the container (inventory B3).
const PATHS: &[&str] = &[
    "workspace",
    "workspace/sub",
    "workspace/c",
    "_runner",
    "_runner/temp",
    "_runner/tmp",
    "_runner/scripts",
    "_runner/files",
    "_runner/files/step-{next}",
    "_runner/scripts/step-{next}.sh",
];

/// A stand-in for the runner's home, full of names the runner might be tricked into writing or
/// reading: its credential, step files, and the directories the tests' patterns name.
fn seed_home(home: &Path) {
    let mut names = vec!["credential".to_string(), "config.toml".to_string(), "README.md".to_string()];
    for f in ["output", "env", "path", "summary"] {
        names.push(f.to_string());
        for n in 1..=8 {
            names.push(format!("step-{n}/{f}"));
        }
    }
    for d in ["c", "sub", "dl", "workspace", "workspace/c", "workspace/sub"] {
        names.push(format!("{d}/one.txt"));
        names.push(format!("{d}/credential"));
    }
    for n in names {
        let p = home.join(&n);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("leak={MARK}\n{MARK}\n")).unwrap();
    }
}

/// Every entry below `dir`: its kind, permission bits and, for a file, its contents.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(dir).unwrap().to_path_buf();
            let mode = m.permissions().mode();
            let what = if m.file_type().is_symlink() {
                format!("link {mode:o} -> {}", std::fs::read_link(&p).unwrap().display())
            } else if m.is_dir() {
                todo.push(p.clone());
                format!("dir {mode:o}")
            } else {
                format!("file {mode:o} {}", std::fs::read_to_string(&p).unwrap_or_default())
            };
            out.insert(rel, what);
        }
    }
    out
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository whose files share names with the runner's home, and its commit.
fn repo(dir: &Path) -> (PathBuf, String) {
    let work = dir.join("src");
    let bare = dir.join("remote.git");
    std::fs::create_dir_all(work.join("c")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    for f in ["credential", "output", "c/one.txt", "README.md"] {
        std::fs::write(work.join(f), "from the repository\n").unwrap();
    }
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(dir, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    (bare, sha)
}

/// An artifact whose files share names with the runner's home.
fn artifact_zip() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let o = zip::write::SimpleFileOptions::default();
        for f in ["credential", "output", "c/one.txt", "sub/credential"] {
            z.start_file(f, o).unwrap();
            z.write_all(b"pwned\n").unwrap();
        }
        z.finish().unwrap();
    }
    buf.into_inner()
}

fn zip_text(data: &[u8]) -> String {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).unwrap();
    let mut all = String::new();
    for i in 0..z.len() {
        let mut e = z.by_index(i).unwrap();
        all.push_str(e.name());
        let _ = e.read_to_string(&mut all);
    }
    all
}

fn tar_text(data: &[u8]) -> String {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data));
    let mut all = String::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        all.push_str(&e.path().unwrap().to_string_lossy());
        let _ = e.read_to_string(&mut all);
    }
    all
}

/// One runner operation: the steps that prepare it, and the steps that make the runner do it.
struct Op {
    name: &'static str,
    setup: fn(TestJob, i64) -> TestJob,
    setup_steps: i32,
    op: fn(TestJob, i64) -> TestJob,
}

fn setup_files(j: TestJob, _id: i64) -> TestJob {
    j.run("mkdir -p c sub && echo data > c/one.txt && echo data > sub/two.txt")
}

const OPS: &[Op] = &[
    Op { name: "run step (script, step files)", setup: |j, _| j, setup_steps: 0, op: |j, _| j.run("true") },
    Op { name: "hashFiles", setup: |j, _| j, setup_steps: 0, op: |j, _| j.run("echo h=${{ hashFiles('**') }}") },
    Op { name: "checkout", setup: |j, _| j, setup_steps: 0, op: |j, _| j.uses("actions/checkout@v4", &[]) },
    Op { name: "checkout into a path", setup: |j, _| j, setup_steps: 0, op: |j, _| j.uses("actions/checkout@v4", &[("path", "sub")]) },
    Op {
        name: "cache restore",
        setup: |j, id| {
            let key = format!("k-{id}");
            setup_files(j, id).uses("actions/cache/save@v4", &[("key", key.as_str()), ("path", "c")])
        },
        setup_steps: 2,
        op: |j, id| {
            let key = format!("k-{id}");
            j.uses("actions/cache/restore@v4", &[("key", key.as_str()), ("path", "c")])
        },
    },
    Op {
        name: "cache save",
        setup: setup_files,
        setup_steps: 1,
        op: |j, id| {
            let key = format!("s-{id}");
            j.uses("actions/cache/save@v4", &[("key", key.as_str()), ("path", "c\nsub\n.")])
        },
    },
    Op {
        name: "cache save after the job",
        setup: setup_files,
        setup_steps: 1,
        op: |j, id| {
            let key = format!("p-{id}");
            j.uses("actions/cache@v4", &[("key", key.as_str()), ("path", "c\nsub")])
        },
    },
    Op {
        name: "upload-artifact",
        setup: setup_files,
        setup_steps: 1,
        op: |j, _| j.uses("actions/upload-artifact@v4", &[("name", "up"), ("path", "."), ("if-no-files-found", "ignore")]),
    },
    Op { name: "download-artifact", setup: |j, _| j, setup_steps: 0, op: |j, _| j.uses("actions/download-artifact@v4", &[("name", "dl")]) },
    Op {
        name: "download-artifact into a path",
        setup: |j, _| j,
        setup_steps: 0,
        op: |j, _| j.uses("actions/download-artifact@v4", &[("name", "dl"), ("path", "sub")]),
    },
];

async fn sweep(op: &Op) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    seed_home(&home);
    let before = snapshot(&home);
    let (bare, sha) = repo(t.path());
    let h = Harness::new().await;
    let mut failures = Vec::new();
    for (i, path) in PATHS.iter().enumerate() {
        let id = 100 + i as i64;
        let next = op.setup_steps + 2;
        let path = path.replace("{next}", &next.to_string());
        let plant = format!(
            r#"set -e
R="$(cd "$RUNNER_TEMP/../.." && pwd)"
P="$R/{path}"
mkdir -p "$(dirname "$P")"
rm -rf "$P"
ln -s {home} "$P"
"#,
            home = home.display()
        );
        let mut job = TestJob::new(id).clone_url(&format!("file://{}", bare.display())).context({
            let sha = sha.clone();
            move |c| c.sha = sha
        });
        job = (op.setup)(job, id);
        job = job.run(&plant);
        job = (op.op)(job, id);
        h.server.put_artifact(id, "dl", artifact_zip());
        let r = h.run(job.build()).await;
        // The runner never opens `_runner/tmp`, so with it linked the operation must still work.
        // That shows each case really reached the operation it is meant to test.
        if path == "_runner/tmp" && r.conclusion != "success" {
            failures.push(format!("{} did not succeed with nothing in its way: {}", op.name, h.server.log_text(id)));
        }
        let now = snapshot(&home);
        if now != before {
            let changed: Vec<_> = now.iter().filter(|(k, v)| before.get(*k) != Some(*v)).map(|(k, _)| k.clone()).collect();
            let gone: Vec<_> = before.keys().filter(|k| !now.contains_key(*k)).cloned().collect();
            failures
                .push(format!("{} with {path} linked: wrote outside the job directory: changed {changed:?}, removed {gone:?}", op.name));
            // Put it back so the next case starts clean.
            std::fs::remove_dir_all(&home).unwrap();
            seed_home(&home);
        }
        let mut seen = h.server.log_text(id);
        for step in 1..=next + 1 {
            seen.push_str(&format!("{:?}", h.server.step_outputs(id, step)));
        }
        seen.push_str(&format!("{:?}", r.outputs));
        if let Some(z) = h.server.artifact(id, "up") {
            seen.push_str(&zip_text(&z));
        }
        for k in [format!("s-{id}"), format!("p-{id}")] {
            if let Some(c) = h.server.cache_data(&k) {
                seen.push_str(&tar_text(&c));
            }
        }
        if seen.contains(MARK) {
            failures.push(format!("{} with {path} linked: the runner read a file outside the job directory", op.name));
        }
        // Nothing in the workspace is really there to hash: a hash means the runner walked a link.
        if op.name == "hashFiles" && h.server.step_log(id, next).lines().any(|l| l.trim().strip_prefix("h=").is_some_and(|v| !v.is_empty()))
        {
            failures.push(format!("hashFiles with {path} linked hashed files outside the job directory"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(h.leftovers().is_empty(), "job directories left behind: {:?}", h.leftovers());
}

macro_rules! sweep_tests {
    ($($name:ident = $i:expr),* $(,)?) => {$(
        #[tokio::test]
        async fn $name() {
            sweep(&OPS[$i]).await;
        }
    )*};
}

sweep_tests! {
    run_step_follows_no_link = 0,
    hash_files_follows_no_link = 1,
    checkout_follows_no_link = 2,
    checkout_into_a_path_follows_no_link = 3,
    cache_restore_follows_no_link = 4,
    cache_save_follows_no_link = 5,
    cache_save_after_the_job_follows_no_link = 6,
    upload_artifact_follows_no_link = 7,
    download_artifact_follows_no_link = 8,
    download_artifact_into_a_path_follows_no_link = 9,
}

#[test]
fn every_operation_is_swept() {
    assert_eq!(OPS.len(), 10, "add a test above for each new operation");
}
