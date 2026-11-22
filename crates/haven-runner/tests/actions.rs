//! The built-in actions, driven through whole jobs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use haven_runner::testing::TestJob;
use std::io::Read;
use std::path::Path;

fn zip_entries(bytes: &[u8]) -> Vec<(String, String)> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).expect("a zip");
    let mut v = Vec::new();
    for i in 0..z.len() {
        let mut e = z.by_index(i).unwrap();
        let mut s = String::new();
        e.read_to_string(&mut s).unwrap();
        v.push((e.name().to_string(), s));
    }
    v.sort();
    v
}

fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        for (n, c) in files {
            z.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(c.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }
    buf.into_inner()
}

// ---- artifacts ----------------------------------------------------------------------------------

#[tokio::test]
async fn upload_artifact_sends_a_zip_of_the_matched_files() {
    let h = Harness::new().await;
    let job = TestJob::new(1)
        .run("mkdir -p target/report/sub && echo A > target/report/a.txt && echo B > target/report/sub/b.txt && echo ignored > target/other.txt")
        .uses("actions/upload-artifact@v4", &[("name", "report"), ("path", "target/report\n"), ("retention-days", "14")])
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(1));
    let z = h.server.artifact(1, "report").expect("the artifact arrived");
    assert_eq!(zip_entries(&z), vec![("a.txt".to_string(), "A\n".to_string()), ("sub/b.txt".to_string(), "B\n".to_string())]);
    assert_eq!(h.server.artifact_retention(1, "report"), Some(Some(14)));
    assert!(h.server.step_outputs(1, 2).contains_key("artifact-id"));
}

#[tokio::test]
async fn upload_artifact_globs_and_negation() {
    let h = Harness::new().await;
    let job = TestJob::new(2)
        .run("mkdir -p out/logs && echo 1 > out/a.bin && echo 2 > out/b.bin && echo 3 > out/logs/x.log && echo 4 > out/skip.bin")
        .uses("upload-artifact", &[("name", "bins"), ("path", "out/*.bin\n!out/skip.bin\nout/logs")])
        .build();
    h.run(job).await;
    let names: Vec<String> = zip_entries(&h.server.artifact(2, "bins").unwrap()).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["a.bin", "b.bin", "logs/x.log"], "paths are relative to the common directory");
}

#[tokio::test]
async fn upload_artifact_when_nothing_matches() {
    let h = Harness::new().await;
    let warn = h.run(TestJob::new(3).uses("upload-artifact", &[("name", "none"), ("path", "nope/**")]).build()).await;
    assert_eq!(warn.conclusion, "success");
    assert!(h.server.step_log(3, 1).contains("##[warning]No files were found"));
    assert!(h.server.artifact(3, "none").is_none());
    let err = h
        .run(TestJob::new(4).uses("upload-artifact", &[("name", "none"), ("path", "nope/**"), ("if-no-files-found", "error")]).build())
        .await;
    assert_eq!(err.conclusion, "failure");
    let ign = h
        .run(TestJob::new(5).uses("upload-artifact", &[("name", "none"), ("path", "nope/**"), ("if-no-files-found", "ignore")]).build())
        .await;
    assert_eq!(ign.conclusion, "success");
    assert!(!h.server.step_log(5, 1).contains("##[warning]"));
}

#[tokio::test]
async fn upload_artifact_cannot_read_outside_the_workspace() {
    let h = Harness::new().await;
    // A file the runner itself could read but the job must not be able to take.
    let secret = h.work.path().join(".runner");
    std::fs::write(&secret, "chr_runner_credential").unwrap();
    for (i, path) in [secret.to_string_lossy().into_owned(), "../../.runner".to_string(), "/etc/passwd".to_string()].iter().enumerate() {
        let id = 10 + i as i64;
        let r = h.run(TestJob::new(id).uses("upload-artifact", &[("name", "x"), ("path", path.as_str())]).build()).await;
        assert_eq!(r.conclusion, "failure", "{path}");
        assert!(h.server.step_log(id, 1).contains("outside the workspace"), "{path}: {}", h.server.step_log(id, 1));
        assert!(h.server.artifact(id, "x").is_none());
    }
    // And through a symlink the job planted.
    let job = TestJob::new(20)
        .run(&format!("ln -s '{}' stolen.txt && ln -s '{}' linkdir", secret.display(), h.work.path().display()))
        .uses("upload-artifact", &[("name", "sneaky"), ("path", "**")])
        .build();
    h.run(job).await;
    if let Some(z) = h.server.artifact(20, "sneaky") {
        let all = zip_entries(&z);
        assert!(all.iter().all(|(_, c)| !c.contains("chr_runner_credential")), "a symlink leaked the credential: {all:?}");
    }
}

#[tokio::test]
async fn upload_artifact_rejects_bad_names() {
    let h = Harness::new().await;
    let r = h.run(TestJob::new(6).run("echo x > f").uses("upload-artifact", &[("name", "a/b"), ("path", "f")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(6, 2).contains("not allowed"));
}

#[tokio::test]
async fn a_second_upload_of_the_same_name_is_the_servers_call() {
    let h = Harness::new().await;
    let job = TestJob::new(7)
        .run("echo x > f")
        .uses("upload-artifact", &[("name", "dup"), ("path", "f")])
        .uses("upload-artifact", &[("name", "dup"), ("path", "f")])
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(7, 3).contains("already exists"), "{}", h.server.step_log(7, 3));
}

#[tokio::test]
async fn download_artifact_unpacks_into_the_workspace() {
    let h = Harness::new().await;
    h.server.put_artifact(8, "bundle", make_zip(&[("top.txt", "T"), ("dir/inner.txt", "I")]));
    let job = TestJob::new(8)
        .uses("actions/download-artifact@v4", &[("name", "bundle"), ("path", "got")])
        .run("cat got/top.txt got/dir/inner.txt")
        .build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(8));
    assert!(h.server.step_log(8, 2).contains("TI"), "{}", h.server.step_log(8, 2));
}

#[tokio::test]
async fn download_artifact_refuses_to_escape_and_reports_a_missing_artifact() {
    let h = Harness::new().await;
    h.server.put_artifact(9, "evil", make_zip(&[("../escape.txt", "pwned"), ("fine.txt", "ok")]));
    let r = h
        .run(
            TestJob::new(9)
                .uses("download-artifact", &[("name", "evil")])
                .run("test -f fine.txt && echo fine-present; test ! -e ../escape.txt && echo no-escape")
                .build(),
        )
        .await;
    assert_eq!(r.conclusion, "success");
    assert!(h.server.step_log(9, 2).contains("fine-present") && h.server.step_log(9, 2).contains("no-escape"));
    assert!(!h.work.path().join("escape.txt").exists());

    let r = h.run(TestJob::new(10).uses("download-artifact", &[("name", "missing")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(10, 1).contains("downloading the artifact"));
    let r = h.run(TestJob::new(11).uses("download-artifact", &[("name", "x"), ("path", "../../etc")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(11, 1).contains("outside the workspace"));
}

// ---- cache ------------------------------------------------------------------------------------

fn cache_job(id: i64, key: &str, restore_keys: &str, build_cmd: &str) -> TestJob {
    TestJob::new(id)
        .uses_id("c", "actions/cache@v4", &[("key", key), ("restore-keys", restore_keys), ("path", "deps\nbuild/out")])
        .run(build_cmd)
}

#[tokio::test]
async fn cache_saves_after_the_job_then_restores_in_the_next_one() {
    let h = Harness::new().await;
    // First run: miss, build, and save at the end.
    let r = h.run(cache_job(1, "deps-v1-abc", "", "mkdir -p deps build/out && echo lib > deps/lib.txt && echo o > build/out/o.txt && echo cache-hit=${{ steps.c.outputs.cache-hit }}").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(1));
    assert!(h.server.step_log(1, 2).contains("cache-hit=false"));
    assert_eq!(h.server.cache_keys(), ["deps-v1-abc"]);
    assert!(h.server.log_text(1).contains("Saved 2 files"), "{}", h.server.log_text(1));

    // Second run: exact hit, files are back, and nothing is saved again.
    let r = h
        .run(cache_job(2, "deps-v1-abc", "", "cat deps/lib.txt build/out/o.txt; echo cache-hit=${{ steps.c.outputs.cache-hit }}").build())
        .await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(2));
    let l = h.server.step_log(2, 2);
    assert!(l.contains("lib") && l.contains("cache-hit=true"), "{l}");
    assert_eq!(h.server.cache_keys().len(), 1, "an exact hit saves nothing new");
}

#[tokio::test]
async fn a_restore_key_match_restores_but_still_saves_under_the_new_key() {
    let h = Harness::new().await;
    h.run(cache_job(3, "deps-v1-old", "", "mkdir -p deps build/out && echo old > deps/lib.txt && echo o > build/out/o.txt").build()).await;
    let r = h
        .run(
            cache_job(
                4,
                "deps-v1-new",
                "deps-v1-\ndeps-",
                "cat deps/lib.txt; echo hit=${{ steps.c.outputs.cache-hit }} matched=${{ steps.c.outputs.cache-matched-key }}",
            )
            .build(),
        )
        .await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(4));
    let l = h.server.step_log(4, 2);
    assert!(l.contains("old") && l.contains("hit=false") && l.contains("matched=deps-v1-old"), "{l}");
    assert_eq!(h.server.cache_keys(), ["deps-v1-old", "deps-v1-new"]);
}

#[tokio::test]
async fn a_failed_job_does_not_save_its_cache() {
    let h = Harness::new().await;
    let r = h.run(cache_job(5, "k-fail", "", "mkdir -p deps build/out && echo x > deps/f && exit 1").build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.cache_keys().is_empty());
}

#[tokio::test]
async fn cache_restore_only_and_save_only_variants() {
    let h = Harness::new().await;
    let save = TestJob::new(6)
        .run("mkdir -p deps && echo d > deps/x")
        .uses("actions/cache/save@v4", &[("key", "k-save"), ("path", "deps")])
        .run("echo after-save")
        .build();
    assert_eq!(h.run(save).await.conclusion, "success");
    assert_eq!(h.server.cache_keys(), ["k-save"], "save-only saves at once");
    let restore = TestJob::new(7).uses("actions/cache/restore@v4", &[("key", "k-save"), ("path", "deps")]).run("cat deps/x").build();
    assert_eq!(h.run(restore).await.conclusion, "success");
    assert!(h.server.step_log(7, 2).contains('d'));
    let miss = TestJob::new(8).uses("actions/cache/restore@v4", &[("key", "other"), ("path", "deps")]).build();
    assert_eq!(h.run(miss).await.conclusion, "success", "a miss is not an error");
    assert_eq!(h.server.cache_keys().len(), 1, "restore-only never saves");
}

#[tokio::test]
async fn a_cache_with_a_different_path_set_is_a_different_cache() {
    let h = Harness::new().await;
    h.run(
        TestJob::new(9)
            .run("mkdir -p a b && echo 1 > a/f && echo 2 > b/f")
            .uses("actions/cache/save", &[("key", "same-key"), ("path", "a")])
            .build(),
    )
    .await;
    let r = h
        .run(
            TestJob::new(10)
                .uses_id("c", "actions/cache/restore", &[("key", "same-key"), ("path", "a\nb")])
                .run("echo hit=${{ steps.c.outputs.cache-hit }}")
                .build(),
        )
        .await;
    assert_eq!(r.conclusion, "success");
    assert!(h.server.step_log(10, 2).contains("hit=false"), "{}", h.server.step_log(10, 2));
}

#[tokio::test]
async fn cache_inputs_are_validated_and_save_problems_only_warn() {
    let h = Harness::new().await;
    assert_eq!(h.run(TestJob::new(11).uses("actions/cache", &[("path", "x")]).build()).await.conclusion, "failure");
    assert_eq!(h.run(TestJob::new(12).uses("actions/cache", &[("key", "k")]).build()).await.conclusion, "failure");
    assert_eq!(h.run(TestJob::new(13).uses("actions/cache", &[("key", "a,b"), ("path", "x")]).build()).await.conclusion, "failure");
    // Nothing to cache: warning, job still passes.
    let r = h.run(TestJob::new(14).uses("actions/cache", &[("key", "empty"), ("path", "does-not-exist")]).build()).await;
    assert_eq!(r.conclusion, "success");
    assert!(h.server.log_text(14).contains("nothing matched the cache paths"));
    // A lookup the server refuses fails the cache step.
    h.server.fail_next("cache", &[400]);
    let r = h.run(TestJob::new(15).uses("actions/cache", &[("key", "k15"), ("path", "d")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(15, 1).contains("cache lookup failed"), "{}", h.server.step_log(15, 1));
    // A save the server refuses is only a warning: the build already passed.
    h.server.fail_next("cache", &[200, 400]); // the first answers the lookup (no hit), the second refuses the save
    let r = h.run(TestJob::new(16).run("mkdir -p d && echo x > d/f").uses("actions/cache", &[("key", "k16"), ("path", "d")]).build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(16));
    assert!(h.server.log_text(16).contains("The cache could not be saved"), "{}", h.server.log_text(16));
}

// ---- checkout ---------------------------------------------------------------------------------

struct Repo {
    _dir: tempfile::TempDir,
    bare: std::path::PathBuf,
    sha: String,
    work: std::path::PathBuf,
}

fn make_repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("src");
    let bare = dir.path().join("remote.git");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "# demo\n").unwrap();
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    std::fs::write(work.join("second.txt"), "2\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "second"]);
    git(&work, &["tag", "v1.0"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(work.join("feature.txt"), "f\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    git(&work, &["checkout", "-q", "main"]);
    git(dir.path(), &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    Repo { _dir: dir, bare, sha, work }
}

fn checkout_job(id: i64, repo: &Repo, inputs: &[(&str, &str)]) -> TestJob {
    let sha = repo.sha.clone();
    TestJob::new(id).clone_url(&format!("file://{}", repo.bare.display())).context(move |c| c.sha = sha).uses("actions/checkout@v4", inputs)
}

#[tokio::test]
async fn checkout_fetches_the_commit_the_run_is_for() {
    let repo = make_repo();
    let h = Harness::new().await;
    let r = h
        .run(
            checkout_job(1, &repo, &[])
                .run("git log --oneline | head -3; echo HEAD=$(git rev-parse HEAD); ls; git status --short | wc -l")
                .build(),
        )
        .await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(1));
    let l = h.server.step_log(1, 2);
    assert!(l.contains(&format!("HEAD={}", repo.sha)), "{l}");
    assert!(l.contains("README.md") && l.contains("second.txt"));
    assert!(!l.contains("first"), "fetch-depth 1 is the default, so there is no older history: {l}");
    assert_eq!(h.server.step_outputs(1, 1)["commit"], repo.sha);
}

#[tokio::test]
async fn checkout_fetch_depth_zero_gets_the_full_history() {
    let repo = make_repo();
    let h = Harness::new().await;
    let r = h.run(checkout_job(2, &repo, &[("fetch-depth", "0")]).run("git rev-list --count HEAD; git tag --list").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(2));
    assert!(h.server.step_log(2, 2).lines().any(|l| l.trim() == "2"), "{}", h.server.step_log(2, 2));
}

#[tokio::test]
async fn checkout_a_branch_a_tag_a_sha_and_a_subdirectory() {
    let repo = make_repo();
    let h = Harness::new().await;
    let feature_sha = git(&repo.work, &["rev-parse", "feature"]);
    let r = h.run(checkout_job(3, &repo, &[("ref", "feature")]).run("git rev-parse HEAD; git branch --show-current; ls").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(3));
    let l = h.server.step_log(3, 2);
    assert!(l.contains(&feature_sha) && l.contains("feature.txt") && l.lines().any(|x| x.trim() == "feature"), "{l}");

    let r = h.run(checkout_job(4, &repo, &[("ref", "v1.0")]).run("git rev-parse HEAD; ls").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(4));
    assert!(h.server.step_log(4, 2).contains("second.txt"));

    let first = git(&repo.work, &["rev-parse", "HEAD~1"]);
    let r = h.run(checkout_job(5, &repo, &[("ref", &first), ("fetch-depth", "0")]).run("git rev-parse HEAD; ls").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(5));
    let l = h.server.step_log(5, 2);
    assert!(l.contains(&first) && !l.contains("second.txt"), "{l}");

    let r =
        h.run(checkout_job(6, &repo, &[("path", "code/here")]).run("ls code/here; git -C code/here rev-parse --short HEAD").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(6));
    assert!(h.server.step_log(6, 2).contains("README.md"));
}

#[tokio::test]
async fn checkout_when_the_branch_moved_on_lands_on_the_runs_commit() {
    let repo = make_repo();
    let old_sha = repo.sha.clone();
    // The branch advances after the run was created.
    std::fs::write(repo.work.join("newer.txt"), "n\n").unwrap();
    git(&repo.work, &["add", "."]);
    git(&repo.work, &["commit", "-q", "-m", "newer"]);
    git(&repo.work, &["push", "-q", repo.bare.to_str().unwrap(), "main"]);
    git(&repo.bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let h = Harness::new().await;
    let r = h.run(checkout_job(7, &repo, &[("fetch-depth", "0")]).run("git rev-parse HEAD; ls").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(7));
    let l = h.server.step_log(7, 2);
    assert!(l.contains(&old_sha) && !l.contains("newer.txt"), "must be the run's commit, not the branch tip: {l}");
}

#[tokio::test]
async fn checkout_errors_are_clear_and_safe() {
    let repo = make_repo();
    let h = Harness::new().await;
    let r = h.run(checkout_job(8, &repo, &[("ref", "no-such-branch")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(8, 1).contains("##[error]"));
    let r = h.run(checkout_job(9, &repo, &[("path", "../escape")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(9, 1).contains("outside the workspace"));
    let r = h.run(checkout_job(10, &repo, &[("fetch-depth", "lots")]).build()).await;
    assert_eq!(r.conclusion, "failure");
    let mut bad = checkout_job(11, &repo, &[]).build();
    bad.repo.clone_url = "file:///definitely/not/there.git".into();
    assert_eq!(h.run(bad).await.conclusion, "failure");
}

#[tokio::test]
async fn checkout_never_prints_the_job_token() {
    let repo = make_repo();
    let h = Harness::new().await;
    h.run(checkout_job(12, &repo, &[]).run("echo done").build()).await;
    assert!(!h.server.log_text(12).contains("job_token_12"));
    assert!(!h.server.log_text(12).contains(Path::new("x-access-token").to_str().unwrap()));
}

#[tokio::test]
async fn steps_after_checkout_can_use_git_in_the_workspace() {
    // The workspace belongs to the runner's user, not the container's. git needs to be told
    // that is fine, which the step environment does with safe.directory.
    let repo = make_repo();
    let h = Harness::new().await;
    let r = h.run(checkout_job(13, &repo, &[]).run("git config --get safe.directory; git log -1 --format=%s").build()).await;
    assert_eq!(r.conclusion, "success", "{}", h.server.log_text(13));
    assert!(h.server.step_log(13, 2).contains("second"));
}
