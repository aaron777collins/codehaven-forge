//! Read paths against real repositories: refs, trees, blobs, history, compare, diff, blame,
//! grep and archives, plus the refusals that keep user input out of git's option position.
#![allow(clippy::unwrap_used)]

mod common;

use ch_core::model::*;
use ch_core::services::ArchiveFormat;
use ch_core::{Error, RepoId};
use common::*;
use futures::StreamExt;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;

fn sig() -> Signature {
    Signature {
        name: "Alice Example".into(),
        email: "alice@test.example".into(),
        at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    }
}

async fn change(f: &Fixture, repo: &Repo, user: ch_core::UserId, branch: &str, changes: Vec<FileChange>, message: &str) -> String {
    f.state
        .git
        .commit_file_change(
            &f.actor(user),
            CommitFileChange {
                repo_id: repo.id,
                branch: branch.into(),
                new_branch: None,
                expected_sha: None,
                changes,
                message: message.into(),
                author: sig(),
            },
            &f.client(),
        )
        .await
        .unwrap()
}

fn put(path: &str, content: &str) -> FileChange {
    FileChange { path: path.into(), content: Some(content.as_bytes().to_vec()), renamed_from: None }
}

async fn setup(pool: PgPool) -> (Fixture, ch_core::UserId, Repo) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "demo", true).await;
    (f, alice, repo)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn refs_resolve_and_peel_tags(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let c1 = f.branch_sha(&repo, "main").await;
    let c2 = f.commit(&repo, alice, "main", "a.txt", "a\n", "Second").await;
    let git = &f.state.git;
    git.create_ref(&f.actor(alice), repo.id, "refs/heads/feature", &c1, &f.client()).await.unwrap();
    git.create_ref(&f.actor(alice), repo.id, "refs/tags/light", &c1, &f.client()).await.unwrap();
    // An annotated tag is its own object; refs() shows the commit it points at.
    f.core
        .store
        .cmd(repo.id)
        .args(["tag", "-a", "annotated", "-m", "release", &c2])
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@x.test")
        .run()
        .await
        .unwrap();
    let all = git.refs(repo.id, None).await.unwrap();
    let by_name = |n: &str| all.iter().find(|r| r.name == n).map(|r| r.sha.clone());
    assert_eq!(by_name("refs/heads/main"), Some(c2.clone()));
    assert_eq!(by_name("refs/heads/feature"), Some(c1.clone()));
    assert_eq!(by_name("refs/tags/light"), Some(c1.clone()));
    assert_eq!(by_name("refs/tags/annotated"), Some(c2.clone()), "peeled to the commit");
    assert_eq!(git.refs(repo.id, Some("refs/tags/")).await.unwrap().len(), 2);
    for rev in ["main", "HEAD", "refs/heads/feature", "annotated", &c1, &c1[..10], "main~1", "main^"] {
        assert!(git.resolve(repo.id, rev).await.is_ok(), "{rev}");
    }
    assert_eq!(git.resolve(repo.id, "main~1").await.unwrap(), c1);
    assert!(matches!(git.resolve(repo.id, "nope").await, Err(Error::NotFound)));
    assert!(matches!(git.resolve(repo.id, &"0".repeat(40)).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_tag_named_like_a_branch_never_hides_the_branch(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let c1 = f.branch_sha(&repo, "main").await;
    let c2 = f.commit(&repo, alice, "main", "reviewed.txt", "reviewed\n", "Reviewed change").await;
    let git = &f.state.git;
    // Anyone with write access can push a tag; this one is named like the protected branch.
    git.create_ref(&f.actor(alice), repo.id, "refs/tags/main", &c1, &f.client()).await.unwrap();
    assert_eq!(git.resolve(repo.id, "main").await.unwrap(), c2, "the branch wins over the tag");
    assert_eq!(git.resolve(repo.id, "main~1").await.unwrap(), c1);
    assert_eq!(git.resolve(repo.id, "refs/tags/main").await.unwrap(), c1, "the tag is still reachable by its full name");
    let names: Vec<String> = git.tree(repo.id, "main", "").await.unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"reviewed.txt".to_string()), "{names:?}");
    let blob = git.blob(repo.id, "main", "reviewed.txt", 100).await.unwrap();
    assert_eq!(blob.content, b"reviewed\n");
    assert_eq!(git.log(repo.id, "main", None, 0, 10).await.unwrap()[0].sha, c2);
    let zip = collect(git.archive(repo.id, "main", ArchiveFormat::Zip).await.unwrap()).await;
    assert!(zip.windows(12).any(|w| w == b"reviewed.txt"), "the archive is of the branch");
    // A tag with no branch of that name still resolves.
    git.create_ref(&f.actor(alice), repo.id, "refs/tags/v1", &c1, &f.client()).await.unwrap();
    assert_eq!(git.resolve(repo.id, "v1").await.unwrap(), c1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn trees_list_folders_first_and_blobs_detect_binary_and_truncate(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    change(
        &f,
        &repo,
        alice,
        "main",
        vec![
            put("src/lib.rs", "fn main() {}\n"),
            put("src/util/mod.rs", "// util\n"),
            put("Zebra.txt", "z\n"),
            put("apple.txt", "a\n"),
            put("docs/readme.md", "# docs\n"),
        ],
        "Layout",
    )
    .await;
    let git = &f.state.git;
    let root = git.tree(repo.id, "main", "").await.unwrap();
    assert_eq!(root.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["docs", "src", "apple.txt", "README.md", "Zebra.txt"]);
    assert_eq!(root[0].kind, EntryKind::Tree);
    assert_eq!(root[2].kind, EntryKind::Blob);
    let src = git.tree(repo.id, "main", "src").await.unwrap();
    assert_eq!(src.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["src/util", "src/lib.rs"]);
    assert_eq!(src[1].size, Some(13));
    assert!(git.tree(repo.id, "main", "/src/").await.is_ok(), "slashes are tolerated");
    for bad in ["nothing", "src/lib.rs", "src/util/deeper"] {
        assert!(matches!(git.tree(repo.id, "main", bad).await, Err(Error::NotFound)), "{bad}");
    }
    let b = git.blob(repo.id, "main", "src/lib.rs", 1000).await.unwrap();
    assert_eq!((b.content.as_slice(), b.is_binary, b.truncated, b.size), (b"fn main() {}\n".as_slice(), false, false, 13));
    let cut = git.blob(repo.id, "main", "src/lib.rs", 5).await.unwrap();
    assert_eq!((cut.content.as_slice(), cut.truncated, cut.size), (b"fn ma".as_slice(), true, 13));
    f.state
        .git
        .commit_file_change(
            &f.actor(alice),
            CommitFileChange {
                repo_id: repo.id,
                branch: "main".into(),
                new_branch: None,
                expected_sha: None,
                changes: vec![FileChange { path: "img.bin".into(), content: Some(vec![0x89, b'P', 0, 1, 2, 3]), renamed_from: None }],
                message: "Binary".into(),
                author: sig(),
            },
            &f.client(),
        )
        .await
        .unwrap();
    assert!(git.blob(repo.id, "main", "img.bin", 100).await.unwrap().is_binary);
    // A directory is not a blob, a file is not a tree.
    assert!(matches!(git.blob(repo.id, "main", "src", 10).await, Err(Error::NotFound)));
    assert!(matches!(git.blob(repo.id, "main", "missing", 10).await, Err(Error::NotFound)));
    // Reading by blob sha alone works for the git data API.
    let sha = b.sha.clone();
    assert_eq!(git.blob(repo.id, &sha, "", 100).await.unwrap().content, b"fn main() {}\n");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn history_with_paths_skip_and_limit(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    f.commit(&repo, alice, "main", "a.txt", "1\n", "Add a").await;
    f.commit(&repo, alice, "main", "b.txt", "1\n", "Add b").await;
    f.commit(&repo, alice, "main", "a.txt", "2\n", "Change a\n\nWith a body.").await;
    let git = &f.state.git;
    let all = git.log(repo.id, "main", None, 0, 50).await.unwrap();
    assert_eq!(
        all.iter().map(|c| c.message.lines().next().unwrap()).collect::<Vec<_>>(),
        vec!["Change a", "Add b", "Add a", "Initial commit"]
    );
    assert_eq!(all[0].message, "Change a\n\nWith a body.");
    assert_eq!(all[0].parents.len(), 1);
    assert_eq!(all[3].parents.len(), 0);
    assert_eq!(all[0].author.email, "alice@test.example");
    let only_a = git.log(repo.id, "main", Some("a.txt"), 0, 50).await.unwrap();
    assert_eq!(only_a.len(), 2);
    assert_eq!(git.log(repo.id, "main", None, 1, 2).await.unwrap().len(), 2);
    assert_eq!(git.log(repo.id, "main", None, 3, 10).await.unwrap().len(), 1);
    assert_eq!(git.commit(repo.id, "main").await.unwrap().sha, all[0].sha);
    assert_eq!(git.commit(repo.id, &all[2].sha[..8]).await.unwrap().sha, all[2].sha);
    assert!(matches!(git.commit(repo.id, "nope").await, Err(Error::NotFound)));
    assert!(git.log(repo.id, "nope", None, 0, 5).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn compare_is_three_dot(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let base = f.branch_sha(&repo, "main").await;
    git.create_ref(&f.actor(alice), repo.id, "refs/heads/topic", &base, &f.client()).await.unwrap();
    f.commit(&repo, alice, "topic", "t1.txt", "t1\n", "Topic one").await;
    f.commit(&repo, alice, "topic", "t2.txt", "t2\n", "Topic two").await;
    f.commit(&repo, alice, "main", "m.txt", "m\n", "Main moved").await;
    let c = git.compare(repo.id, "main", "topic").await.unwrap();
    assert_eq!((c.ahead_by, c.behind_by), (2, 1));
    assert_eq!(c.merge_base, base);
    assert_eq!(c.commits.iter().map(|x| x.message.as_str()).collect::<Vec<_>>(), vec!["Topic one", "Topic two"], "oldest first");
    // Only what the topic changed: main's own commit does not show.
    let mut files: Vec<&str> = c.diff.files.iter().map(|d| d.new_path.as_deref().unwrap()).collect();
    files.sort();
    assert_eq!(files, vec!["t1.txt", "t2.txt"]);
    assert!(c.diff.files.iter().all(|d| d.status == FileStatus::Added));
    let same = git.compare(repo.id, "main", "main").await.unwrap();
    assert_eq!((same.ahead_by, same.behind_by, same.commits.len(), same.diff.files.len()), (0, 0, 0, 0));
    assert!(matches!(git.compare(repo.id, "main", "nope").await, Err(Error::NotFound)));
    // The exposed `diff` is the same three-dot diff.
    assert_eq!(git.diff(repo.id, "main", "topic").await.unwrap().files.len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn diffs_cover_add_modify_remove_rename_binary_and_big_files(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    change(&f, &repo, alice, "main", vec![put("keep.txt", &body), put("old-name.txt", &body), put("gone.txt", "bye\n")], "Setup").await;
    let before = f.branch_sha(&repo, "main").await;
    let edited = body.replace("line 10\n", "line ten\n").replace("line 25\n", "line 25\nnew line\n");
    f.state
        .git
        .commit_file_change(
            &f.actor(alice),
            CommitFileChange {
                repo_id: repo.id,
                branch: "main".into(),
                new_branch: None,
                expected_sha: None,
                changes: vec![
                    put("keep.txt", &edited),
                    FileChange {
                        path: "new-name.txt".into(),
                        content: Some(body.replace("line 3\n", "line three\n").into_bytes()),
                        renamed_from: Some("old-name.txt".into()),
                    },
                    FileChange { path: "gone.txt".into(), content: None, renamed_from: None },
                    put("fresh.txt", "a\nb\nc\n"),
                    FileChange { path: "pic.png".into(), content: Some(vec![0, 1, 2, 255]), renamed_from: None },
                    put("huge.txt", &"x\n".repeat(21_000)),
                ],
                message: "Everything".into(),
                author: sig(),
            },
            &f.client(),
        )
        .await
        .unwrap();
    let d = git.diff(repo.id, &before, "main").await.unwrap();
    let find = |name: &str| {
        d.files
            .iter()
            .find(|x| x.new_path.as_deref() == Some(name) || x.old_path.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    let keep = find("keep.txt");
    assert_eq!((keep.status, keep.additions, keep.deletions, keep.hunks.len()), (FileStatus::Modified, 2, 1, 2));
    let h = &keep.hunks[0];
    assert!(h.header.starts_with("@@ "));
    let removed: Vec<_> = h.lines.iter().filter(|l| l.kind == LineKind::Removed).collect();
    assert_eq!((removed[0].text.as_str(), removed[0].old_no, removed[0].new_no), ("line 10", Some(10), None));
    let added: Vec<_> = h.lines.iter().filter(|l| l.kind == LineKind::Added).collect();
    assert_eq!((added[0].text.as_str(), added[0].new_no), ("line ten", Some(10)));
    let renamed = find("new-name.txt");
    assert_eq!((renamed.status, renamed.old_path.as_deref()), (FileStatus::Renamed, Some("old-name.txt")));
    assert_eq!((renamed.additions, renamed.deletions), (1, 1));
    let gone = find("gone.txt");
    assert_eq!((gone.status, gone.new_path.clone(), gone.deletions), (FileStatus::Removed, None, 1));
    let fresh = find("fresh.txt");
    assert_eq!(
        (fresh.status, fresh.old_path.clone(), fresh.additions, fresh.hunks[0].lines[0].new_no),
        (FileStatus::Added, None, 3, Some(1))
    );
    let pic = find("pic.png");
    assert!(pic.binary && pic.hunks.is_empty());
    let huge = find("huge.txt");
    assert!(huge.collapsed && huge.hunks.is_empty() && huge.additions == 21_000, "counts survive the collapse");
    // A root commit is compared with the empty tree.
    let root = git.log(repo.id, "main", None, 0, 100).await.unwrap().last().unwrap().sha.clone();
    let root_diff = f.core.commit_diff(repo.id, &root).await.unwrap();
    assert_eq!(root_diff.files.len(), 1);
    assert_eq!(root_diff.files[0].new_path.as_deref(), Some("README.md"));
    // Cached results are the same results.
    assert_eq!(git.diff(repo.id, &before, "main").await.unwrap().files.len(), d.files.len());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn blame_attributes_lines_to_commits(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let c1 = f.commit(&repo, alice, "main", "poem.txt", "one\ntwo\nthree\n", "First draft").await;
    let c2 = f.commit(&repo, alice, "main", "poem.txt", "one\nTWO\nthree\nfour\n", "Edit two, add four").await;
    let b = f.state.git.blame(repo.id, "main", "poem.txt").await.unwrap();
    let shape: Vec<(u32, u32, &str, &str)> =
        b.ranges.iter().map(|r| (r.start_line, r.line_count, r.sha.as_str(), r.summary.as_str())).collect();
    assert_eq!(
        shape,
        vec![
            (1, 1, c1.as_str(), "First draft"),
            (2, 1, c2.as_str(), "Edit two, add four"),
            (3, 1, c1.as_str(), "First draft"),
            (4, 1, c2.as_str(), "Edit two, add four")
        ]
    );
    assert_eq!(b.ranges[0].orig_path, "poem.txt");
    assert!(matches!(f.state.git.blame(repo.id, "main", "missing.txt").await, Err(Error::NotFound)));
    assert!(f.state.git.blame(repo.id, "main", "").await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn grep_is_literal_case_insensitive_and_bounded(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    change(
        &f,
        &repo,
        alice,
        "main",
        vec![put("a.rs", "fn Hello() {}\nlet x = 1 + 1;\n"), put("b.txt", "say hello\nnothing\n"), put("c.bin", "hello\0binary")],
        "Files",
    )
    .await;
    let git = &f.state.git;
    let hits = git.grep(repo.id, "main", "HELLO", 50).await.unwrap();
    let mut got: Vec<(String, i64)> = hits.iter().map(|h| (h.title.clone(), h.id)).collect();
    got.sort();
    assert_eq!(got, vec![("a.rs".to_string(), 1), ("b.txt".to_string(), 1)], "binary files are skipped");
    assert_eq!(hits.iter().find(|h| h.title == "b.txt").unwrap().snippet, "say hello");
    // Regex characters are just characters.
    assert_eq!(git.grep(repo.id, "main", "1 + 1", 50).await.unwrap().len(), 1);
    assert!(git.grep(repo.id, "main", ".*", 50).await.unwrap().is_empty());
    assert_eq!(git.grep(repo.id, "main", "hello", 1).await.unwrap().len(), 1, "limit");
    assert!(git.grep(repo.id, "main", "zzzz", 50).await.unwrap().is_empty());
    for bad in ["", "two\nlines", &"x".repeat(201)] {
        assert!(git.grep(repo.id, "main", bad, 10).await.is_err(), "{bad:?}");
    }
    assert!(matches!(git.grep(repo.id, "nope", "x", 10).await, Err(Error::NotFound)));
}

async fn collect(mut s: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn archives_stream_as_zip_and_tar_gz(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    f.commit(&repo, alice, "main", "src/main.rs", "fn main() {}\n", "Add main").await;
    let zip = collect(f.state.git.archive(repo.id, "main", ArchiveFormat::Zip).await.unwrap()).await;
    assert_eq!(&zip[..4], b"PK\x03\x04");
    let text = String::from_utf8_lossy(&zip);
    assert!(text.contains("demo-") && text.contains("src/main.rs"), "prefix and paths in the zip");
    let tgz = collect(f.state.git.archive(repo.id, "main", ArchiveFormat::TarGz).await.unwrap()).await;
    assert_eq!(&tgz[..2], &[0x1f, 0x8b], "gzip magic");
    let mut tar = Vec::new();
    async_compression::tokio::bufread::GzipDecoder::new(tgz.as_slice()).read_to_end(&mut tar).await.unwrap();
    let t = String::from_utf8_lossy(&tar);
    assert!(t.contains("src/main.rs") && t.contains("README.md"));
    assert!(matches!(f.state.git.archive(repo.id, "nope", ArchiveFormat::Zip).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn user_input_never_reaches_git_as_an_option(pool: PgPool) {
    let (f, _alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let evil = ["--output=/tmp/pwned", "-p", "--upload-pack=touch /tmp/x", "main --all", "a\nb", "x*", "../../etc/passwd:x", ""];
    for rev in evil {
        assert!(git.resolve(repo.id, rev).await.is_err(), "resolve {rev:?}");
        assert!(git.tree(repo.id, rev, "").await.is_err(), "tree {rev:?}");
        assert!(git.log(repo.id, rev, None, 0, 5).await.is_err(), "log {rev:?}");
        assert!(git.compare(repo.id, rev, "main").await.is_err(), "compare {rev:?}");
        assert!(git.blob(repo.id, rev, "README.md", 10).await.is_err(), "blob {rev:?}");
    }
    for path in ["../x", "a/../../b", "a\0b"] {
        assert!(git.tree(repo.id, "main", path).await.is_err(), "tree path {path:?}");
        assert!(git.blob(repo.id, "main", path, 10).await.is_err(), "blob path {path:?}");
        assert!(git.log(repo.id, "main", Some(path), 0, 5).await.is_err(), "log path {path:?}");
    }
    assert!(git.refs(repo.id, Some("--all")).await.is_err());
    assert!(git.refs(repo.id, Some("heads/x")).await.is_err());
    assert!(!std::path::Path::new("/tmp/pwned").exists());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_empty_repository_reads_as_empty_not_as_an_error_page(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "blank", false).await;
    let git = &f.state.git;
    assert!(git.refs(repo.id, None).await.unwrap().is_empty());
    for r in [
        git.resolve(repo.id, "main").await.map(|_| ()),
        git.tree(repo.id, "HEAD", "").await.map(|_| ()),
        git.commit(repo.id, "HEAD").await.map(|_| ()),
    ] {
        assert!(matches!(r, Err(Error::NotFound)), "{r:?}");
    }
    let missing = git.refs(RepoId(999_999), None).await;
    assert!(missing.is_err(), "a repository with no files on disk is an error, not an empty list");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_tag_never_hides_a_branch_with_a_hex_name(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let c1 = f.branch_sha(&repo, "main").await;
    let git = &f.state.git;
    git.create_ref(&f.actor(alice), repo.id, "refs/heads/beef", &c1, &f.client()).await.unwrap();
    let c2 = f.commit(&repo, alice, "beef", "b.txt", "branch\n", "On beef").await;
    git.create_ref(&f.actor(alice), repo.id, "refs/tags/beef", &c1, &f.client()).await.unwrap();
    assert_eq!(git.resolve(repo.id, "beef").await.unwrap(), c2, "the branch wins over the tag");
    assert_eq!(git.resolve(repo.id, "refs/tags/beef").await.unwrap(), c1);
    // A full commit id still resolves as itself.
    assert_eq!(git.resolve(repo.id, &c1).await.unwrap(), c1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn grep_keeps_colons_in_paths_and_text(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    change(&f, &repo, alice, "main", vec![put("a:b.txt", "key: needle: value\n")], "Colons").await;
    let hits = f.state.git.grep(repo.id, "main", "needle", 50).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "a:b.txt");
    assert_eq!(hits[0].id, 1);
    assert_eq!(hits[0].snippet, "key: needle: value");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workspace_reads_are_pinned_exact_and_capped(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    change(
        &f,
        &repo,
        alice,
        "main",
        vec![put("src/a:b.rs", "fn x() {} // needle\n"), put("bin.dat", "a\0b"), put("big.txt", &"y".repeat(5000))],
        "Files",
    )
    .await;
    let git = &f.state.git;
    let head = git.workspace_branch_head(repo.id, "main").await.unwrap().unwrap();
    assert_eq!(head, f.branch_sha(&repo, "main").await);
    assert_eq!(git.workspace_branch_head(repo.id, "nope").await.unwrap(), None);
    // A tag never answers for a branch name.
    git.create_ref(&f.actor(alice), repo.id, "refs/tags/release", &head, &f.client()).await.unwrap();
    assert_eq!(git.workspace_branch_head(repo.id, "release").await.unwrap(), None);
    // Ancestry: the head and its parent are in, a made-up id and a short id are out.
    let parent = git.commit(repo.id, &head).await.unwrap().parents[0].clone();
    assert!(git.workspace_is_ancestor(repo.id, &head, &head).await.unwrap());
    assert!(git.workspace_is_ancestor(repo.id, &parent, &head).await.unwrap());
    assert!(!git.workspace_is_ancestor(repo.id, &"a".repeat(40), &head).await.unwrap());
    assert!(!git.workspace_is_ancestor(repo.id, &head[..12], &head).await.unwrap());
    assert!(!git.workspace_is_ancestor(repo.id, &head.to_uppercase(), &head).await.unwrap());
    // The tree lists everything, folders too, and a cap cuts it.
    let t = git.workspace_tree(repo.id, &head, Default::default()).await.unwrap();
    assert!(!t.cut);
    let paths: Vec<String> = t.entries.iter().filter_map(|e| e.path.clone()).collect();
    assert!(paths.contains(&"src".to_string()) && paths.contains(&"src/a:b.rs".to_string()), "{paths:?}");
    let small =
        git.workspace_tree(repo.id, &head, ch_core::model::WorkspaceTreeLimits { max_entries: 2, max_bytes: 1 << 20 }).await.unwrap();
    assert!(small.cut);
    assert_eq!(small.entries.len(), 2);
    // Blobs by raw path, size checked first.
    let b = git.workspace_blob(repo.id, &head, b"src/a:b.rs", 1000).await.unwrap();
    assert_eq!(b.bytes, b"fn x() {} // needle\n");
    assert_eq!(b.mode, "100644");
    let big = git.workspace_blob(repo.id, &head, b"big.txt", 100).await.unwrap();
    assert!(big.too_large && big.bytes.is_empty() && big.size == 5000);
    assert!(git.workspace_blob(repo.id, &head, b"src", 100).await.is_err(), "a folder is not a file");
    assert!(git.workspace_blob(repo.id, &head, b"*.rs", 100).await.is_err(), "no pathspec magic");
    // Search keeps colons, skips binary files and says when it is cut.
    let s = git.workspace_search(repo.id, &head, "NEEDLE", Default::default()).await.unwrap();
    assert_eq!(s.hits.len(), 1);
    assert_eq!(s.hits[0].path.as_deref(), Some("src/a:b.rs"));
    assert_eq!(s.hits[0].line, 1);
    assert!(!s.cut && !s.timed_out);
    let one = git
        .workspace_search(repo.id, &head, "y", ch_core::model::WorkspaceSearchLimits { max_hits: 1, ..Default::default() })
        .await
        .unwrap();
    assert!(one.hits.len() <= 1);
    assert!(git.workspace_search(repo.id, &head, "", Default::default()).await.is_err());
    // Direct commits follow branch protection.
    assert!(git.workspace_direct_commit_allowed(&f.actor(alice), repo.id, "main").await.unwrap());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workspace_snapshot_reads_every_file_once_and_is_capped(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    change(
        &f,
        &repo,
        alice,
        "main",
        vec![put("src/a:b.rs", "fn a() {}\n"), put("src/deep/c.txt", "c\n"), put("bin.dat", "a\0b"), put("docs/x.md", "x\n")],
        "Files",
    )
    .await;
    let git = &f.state.git;
    let head = git.workspace_branch_head(repo.id, "main").await.unwrap().unwrap();
    let all = git.workspace_snapshot(repo.id, &head, b"", Default::default()).await.unwrap();
    assert!(!all.too_large);
    let mut paths: Vec<&str> = all.files.iter().map(|x| x.path.as_str()).collect();
    paths.sort();
    // README.md comes from the repository's first commit (auto_init).
    assert_eq!(paths, vec!["README.md", "bin.dat", "docs/x.md", "src/a:b.rs", "src/deep/c.txt"]);
    let bin = all.files.iter().find(|x| x.path == "bin.dat").unwrap();
    assert_eq!(bin.bytes, b"a\0b");
    assert!(!bin.executable);
    assert_eq!(all.total_files, 5);
    // A folder: only what is inside it, and "sr" is not a prefix of "src".
    let src = git.workspace_snapshot(repo.id, &head, b"src", Default::default()).await.unwrap();
    let mut sp: Vec<&str> = src.files.iter().map(|x| x.path.as_str()).collect();
    sp.sort();
    assert_eq!(sp, vec!["src/a:b.rs", "src/deep/c.txt"]);
    assert!(git.workspace_snapshot(repo.id, &head, b"sr", Default::default()).await.unwrap().files.is_empty());
    // Over a cap nothing is read and the totals say why.
    let capped = git.workspace_snapshot(repo.id, &head, b"", WorkspaceSnapshotLimits { max_files: 2, max_bytes: 1 << 20 }).await.unwrap();
    assert!(capped.too_large && capped.files.is_empty() && capped.total_files == 5);
    let small = git.workspace_snapshot(repo.id, &head, b"", WorkspaceSnapshotLimits { max_files: 100, max_bytes: 5 }).await.unwrap();
    assert!(small.too_large && small.files.is_empty());
    // A short id or a bad folder is refused.
    assert!(git.workspace_snapshot(repo.id, &head[..12], b"", Default::default()).await.is_err());
    assert!(git.workspace_snapshot(repo.id, &head, b"/src", Default::default()).await.is_err());
    assert!(git.workspace_snapshot(repo.id, &head, b"src/", Default::default()).await.is_err());
}
