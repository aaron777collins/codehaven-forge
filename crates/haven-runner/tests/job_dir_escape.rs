//! ch-ctk.1: what a job plants in its own directory must not reach the runner. The built-in
//! checkout runs no hook or command the job left behind, and the runner's own reads and writes
//! of step files never follow a symbolic link out of the job directory.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use haven_runner::testing::TestJob;
use std::path::Path;
use std::process::Command;

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

/// A bare repository with one commit, and that commit's id.
fn repo(dir: &Path) -> (std::path::PathBuf, String) {
    let work = dir.join("src");
    let bare = dir.join("remote.git");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "# demo\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(dir, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    (bare, sha)
}

#[tokio::test]
async fn checkout_runs_no_hook_fsmonitor_or_config_the_job_planted() {
    let t = tempfile::tempdir().unwrap();
    let (bare, sha) = repo(t.path());
    let marks = t.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let m = marks.display();
    // Step 1 is the job: a .git with hooks and an fsmonitor command, and a global git config in
    // the runner's part of the job directory, which the old checkout pointed GIT_CONFIG_GLOBAL at.
    let plant = format!(
        r#"set -e
git init -q .
mkdir -p .git/hooks evil-hooks
for h in post-checkout reference-transaction pre-auto-gc post-index-change; do
  printf '#!/bin/sh\ntouch {m}/hook-%s\n' "$h" > ".git/hooks/$h"; chmod +x ".git/hooks/$h"
  printf '#!/bin/sh\ntouch {m}/global-hook-%s\n' "$h" > "evil-hooks/$h"; chmod +x "evil-hooks/$h"
done
git config core.fsmonitor 'touch {m}/fsmonitor; false'
printf '[core]\n\tfsmonitor = touch {m}/global-fsmonitor; false\n\thooksPath = %s/evil-hooks\n' "$PWD" > "$RUNNER_TEMP/../gitconfig"
"#
    );
    let job = TestJob::new(1)
        .clone_url(&format!("file://{}", bare.display()))
        .context({
            let sha = sha.clone();
            move |c| c.sha = sha
        })
        .run(&plant)
        .uses("actions/checkout@v4", &[])
        .run("echo HEAD=$(git rev-parse HEAD)")
        .build();
    let h = Harness::new().await;
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(1));
    assert!(h.server.step_log(1, 3).contains(&format!("HEAD={sha}")), "{}", h.server.step_log(1, 3));
    let ran: Vec<String> = std::fs::read_dir(&marks).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(ran.is_empty(), "the checkout ran commands the job planted: {ran:?}");
}

#[tokio::test]
async fn step_files_and_scripts_never_follow_a_link_out_of_the_job_directory() {
    let t = tempfile::tempdir().unwrap();
    let victim = t.path().join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    for f in ["output", "env", "path", "summary", "script.sh"] {
        std::fs::write(victim.join(f), "keep\n").unwrap();
    }
    std::fs::write(victim.join("secret"), "leak=yes\n").unwrap();
    let v = victim.display();
    // Step 1 points step 2's file directory and script at the victim. Step 2 swaps its own
    // $GITHUB_OUTPUT and $GITHUB_ENV for links to a file outside, for the runner to read back.
    let plant = format!(
        r#"set -e
R="$RUNNER_TEMP/.."
rm -rf "$R/files/step-2"; ln -s {v} "$R/files/step-2"
ln -sf {v}/script.sh "$R/scripts/step-2.sh"
"#
    );
    let swap =
        format!(r#"rm -f "$GITHUB_OUTPUT" "$GITHUB_ENV"; ln -s {v}/secret "$GITHUB_OUTPUT"; ln -s {v}/secret "$GITHUB_ENV"; echo swapped"#);
    let h = Harness::new().await;
    let r = h.run(TestJob::new(2).run(&plant).run(&swap).run("echo leak=${leak:-none}").build()).await;
    for f in ["output", "env", "path", "summary", "script.sh"] {
        assert_eq!(std::fs::read_to_string(victim.join(f)).unwrap(), "keep\n", "{f} outside the job directory was written");
    }
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(2));
    assert!(h.server.step_log(2, 2).contains("swapped"), "step 2 ran its own script: {}", h.server.step_log(2, 2));
    assert!(!h.server.step_outputs(2, 2).contains_key("leak"), "the runner read a file outside the job directory");
    assert!(h.server.step_log(2, 3).contains("leak=none"), "{}", h.server.step_log(2, 3));
}

#[tokio::test]
async fn a_linked_runner_directory_stops_the_job_instead_of_writing_outside() {
    let t = tempfile::tempdir().unwrap();
    let victim = t.path().join("victim");
    std::fs::create_dir_all(victim.join("step-2")).unwrap();
    std::fs::write(victim.join("step-2/output"), "keep\n").unwrap();
    let v = victim.display();
    let plant = format!(r#"set -e; R="$RUNNER_TEMP/.."; mv "$R/files" "$R/files.old"; ln -s {v} "$R/files""#);
    let h = Harness::new().await;
    let r = h.run(TestJob::new(3).run(&plant).run("echo second").build()).await;
    assert_eq!(std::fs::read_to_string(victim.join("step-2/output")).unwrap(), "keep\n");
    assert_eq!(r.conclusion, "failure", "{}", h.server.log_text(3));
}

#[tokio::test]
async fn hash_files_and_uploads_do_not_walk_a_linked_workspace() {
    let t = tempfile::tempdir().unwrap();
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("credential"), "secret\n").unwrap();
    let o = outside.display();
    let plant = format!(r#"set -e; cd "$GITHUB_WORKSPACE/.."; mv workspace ws.old; ln -s {o} workspace"#);
    let h = Harness::new().await;
    let job = TestJob::new(4)
        .run(&plant)
        .run("echo hash=${{ hashFiles('**') }}")
        .uses("actions/upload-artifact@v4", &[("name", "loot"), ("path", "**"), ("if-no-files-found", "ignore")])
        .build();
    h.run(job).await;
    assert!(h.server.step_log(4, 2).lines().any(|l| l.trim() == "hash="), "{}", h.server.step_log(4, 2));
    if let Some(z) = h.server.artifact(4, "loot") {
        assert!(!String::from_utf8_lossy(&z).contains("credential"), "a file outside the job directory was uploaded");
    }
}

/// A bare repository whose `.gitattributes` names a filter for `*.txt`, the way the reviewer's
/// repository did, and that commit's id.
fn repo_with_filter(dir: &Path) -> (std::path::PathBuf, String) {
    let work = dir.join("fsrc");
    let bare = dir.join("filtered.git");
    std::fs::create_dir_all(work.join("docs")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    for i in 0..40 {
        std::fs::write(work.join(format!("docs/note-{i}.txt")), format!("note {i}\n")).unwrap();
    }
    std::fs::write(work.join("README.md"), "# demo\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(dir, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    (bare, sha)
}

/// The reviewer's escape (ch-ctk.1): a process the job started in an earlier step keeps running,
/// watches for the checkout's new `.git/config` and appends a smudge filter to it while the fetch
/// runs. The repository's `.gitattributes` then makes the runner's own `git checkout` run that
/// filter as the runner. The checkout must read no config the job can write.
#[tokio::test]
async fn a_job_process_cannot_add_a_filter_to_the_checkouts_git_config() {
    let t = tempfile::tempdir().unwrap();
    let (bare, sha) = repo_with_filter(t.path());
    let marks = t.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let m = marks.display();
    // Polls with shell builtins only, so it wins the window between `git init` and the checkout.
    // It ends when the job writes the stop file or the job directory goes away.
    let inject = format!(
        r#"W="$GITHUB_WORKSPACE"
(
  while [ -d "$W" ] && [ ! -e "$W/stop-injector" ]; do
    for c in "$W/.git/config" "$W"/*/.git/config; do
      if [ -f "$c" ] && ! grep -q evil "$c" 2>/dev/null; then
        printf '[filter "evil"]\n\tsmudge = "touch {m}/smudge-ran; cat"\n' >> "$c"
      fi
    done
  done
) >/dev/null 2>&1 </dev/null &
echo injector started
"#
    );
    let job = TestJob::new(5)
        .clone_url(&format!("file://{}", bare.display()))
        .context({
            let sha = sha.clone();
            move |c| c.sha = sha
        })
        .run(&inject)
        .uses("actions/checkout@v4", &[])
        .run("touch stop-injector; echo HEAD=$(git rev-parse HEAD); cat docs/note-7.txt")
        .build();
    let h = Harness::new().await;
    let r = h.run(job).await;
    let ran: Vec<String> = std::fs::read_dir(&marks).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(ran.is_empty(), "the runner's checkout ran a filter a job process wrote into its git config: {ran:?}");
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(5));
    let l = h.server.step_log(5, 3);
    assert!(l.contains(&format!("HEAD={sha}")) && l.contains("note 7"), "the job gets a working checkout: {l}");
}

/// The checkout replaces links the job left in the workspace instead of writing through them.
#[tokio::test]
async fn checkout_replaces_links_in_the_workspace_instead_of_following_them() {
    let t = tempfile::tempdir().unwrap();
    let (bare, sha) = repo_with_filter(t.path());
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("README.md"), "keep\n").unwrap();
    std::fs::write(outside.join("note-1.txt"), "keep\n").unwrap();
    let o = outside.display();
    let plant =
        format!(r#"set -e; ln -s {o}/README.md README.md; ln -s {o} docs; ln -s {o} .git; mkdir -p mine; echo kept > mine/untracked"#);
    let job = TestJob::new(6)
        .clone_url(&format!("file://{}", bare.display()))
        .context({
            let sha = sha.clone();
            move |c| c.sha = sha
        })
        .run(&plant)
        .uses("actions/checkout@v4", &[])
        .run("test ! -L README.md && test ! -L docs && test ! -L .git && cat README.md docs/note-1.txt mine/untracked; git rev-parse HEAD")
        .build();
    let h = Harness::new().await;
    let r = h.run(job).await;
    assert_eq!(std::fs::read_to_string(outside.join("README.md")).unwrap(), "keep\n");
    assert_eq!(std::fs::read_to_string(outside.join("note-1.txt")).unwrap(), "keep\n");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 2, "nothing was written outside the workspace");
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(6));
    let l = h.server.step_log(6, 3);
    assert!(l.contains("# demo") && l.contains("note 1") && l.contains("kept") && l.contains(&sha), "{l}");
}

/// The ch-ctk.1 cache finding, as reproduced on a throwaway stack: a job saves a cache, swaps
/// its workspace for a link to the runner's home and restores the cache. Nothing may land there.
#[tokio::test]
async fn a_cache_restore_does_not_follow_a_linked_workspace() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("credential"), "secret\n").unwrap();
    let hd = home.display();
    let job = TestJob::new(7)
        .run("mkdir -p c && echo pwned > c/pwned.txt && echo replaced > credential")
        .uses("actions/cache/save@v4", &[("key", "k1"), ("path", "c\ncredential")])
        .run(&format!(r#"cd /; rm -rf "$GITHUB_WORKSPACE"; ln -s {hd} "$GITHUB_WORKSPACE""#))
        .uses("actions/cache/restore@v4", &[("key", "k1"), ("path", "c\ncredential")])
        .build();
    let h = Harness::new().await;
    let r = h.run(job).await;
    assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), "secret\n", "the runner's credential was replaced");
    let left: Vec<_> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, vec![std::ffi::OsString::from("credential")], "the restore wrote into the runner's home");
    assert_eq!(r.conclusion, "failure", "{}", h.server.log_text(7));
    assert!(h.server.cache_keys().contains(&"k1".to_string()), "the save step really saved: {}", h.server.log_text(7));
}

/// A step's output file grown to any size (sparse, so it costs the job nothing) is not read into
/// the runner's memory: the step fails and says why.
#[tokio::test]
async fn an_oversized_step_file_is_refused_not_read() {
    let h = Harness::new().await;
    let job = TestJob::new(8).run(r#"echo ok=1 >> "$GITHUB_OUTPUT"; truncate -s 64M "$GITHUB_OUTPUT""#).run("echo after").build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure", "{}", h.server.log_text(8));
    let l = h.server.step_log(8, 1);
    assert!(l.contains("$GITHUB_OUTPUT is larger than 16 MiB"), "{l}");
    assert!(h.server.step_outputs(8, 1).is_empty());
}

/// A submodule whose URL is a local path (here, a repository standing in for something in the
/// runner's home) is never fetched: a fork's .gitmodules chooses that URL.
#[tokio::test]
async fn a_submodule_from_a_local_path_is_not_fetched() {
    let t = tempfile::tempdir().unwrap();
    // The "secret" repository on the runner's disk.
    let secret = t.path().join("home-repo");
    std::fs::create_dir_all(&secret).unwrap();
    git(&secret, &["init", "-q", "-b", "main"]);
    std::fs::write(secret.join("credential"), "secret\n").unwrap();
    git(&secret, &["add", "."]);
    git(&secret, &["commit", "-q", "-m", "s"]);
    // The repository the job checks out names it as a submodule.
    let work = t.path().join("src");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "# demo\n").unwrap();
    git(&work, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", secret.to_str().unwrap(), "leak"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "with a submodule"]);
    let bare = t.path().join("remote.git");
    git(t.path(), &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    let job = TestJob::new(9)
        .clone_url(&format!("file://{}", bare.display()))
        .context({
            let sha = sha.clone();
            move |c| c.sha = sha
        })
        .uses("actions/checkout@v4", &[("submodules", "true")])
        .build();
    let h = Harness::new().await;
    let r = h.run(job).await;
    assert!(!h.server.log_text(9).contains("secret"), "{}", h.server.log_text(9));
    assert_eq!(r.conclusion, "failure", "the submodule was fetched from a local path: {}", h.server.log_text(9));
}
