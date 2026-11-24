//! Fixed demo data: three people, an organisation, four repositories, one of them a small Rust
//! project with history, branches, issues, pull requests, reviews, releases and CI runs.
//! Times are relative to "now" so the pages read naturally.

use ch_core::actor::Permission;
use ch_core::model::*;
use ch_core::web_contract::*;
use ch_core::*;
use chrono::{DateTime, Duration, Utc};

pub const ADA: UserId = UserId(1);
pub const BOB: UserId = UserId(2);
pub const CLEO: UserId = UserId(3);
pub const HARBOR: UserId = UserId(10);

pub const LIGHTHOUSE: RepoId = RepoId(1);
pub const NOTES: RepoId = RepoId(2);
pub const DOCK: RepoId = RepoId(3);
pub const EMPTY: RepoId = RepoId(4);

pub fn ago(days: i64, hours: i64) -> DateTime<Utc> {
    Utc::now() - Duration::days(days) - Duration::hours(hours)
}

pub fn user(id: UserId, name: &str, display: &str, admin: bool, kind: UserKind) -> User {
    User {
        id,
        kind,
        username: name.into(),
        display_name: display.into(),
        email: if kind == UserKind::Org { None } else { Some(format!("{name}@example.test")) },
        is_admin: admin,
        can_run_ci: kind != UserKind::Org,
        suspended: false,
        has_2fa: admin || id == BOB,
        has_password: true,
        invited_by: if id == ADA { None } else { Some(ADA) },
        created_at: ago(120, 0),
    }
}

pub fn users() -> Vec<User> {
    vec![
        user(ADA, "ada", "Ada Marlow", true, UserKind::User),
        user(BOB, "bob", "Bob Okafor", false, UserKind::User),
        user(CLEO, "cleo", "Cleo Brandt", false, UserKind::User),
        user(HARBOR, "harbor", "Harbor Works", false, UserKind::Org),
    ]
}

fn repo(id: RepoId, owner: &User, name: &str, desc: &str, private: bool, empty: bool, topics: &[&str]) -> Repo {
    Repo {
        id,
        owner_id: owner.id,
        owner_name: owner.username.clone(),
        name: name.into(),
        description: desc.into(),
        visibility: if private { Visibility::Private } else { Visibility::Internal },
        default_branch: "main".into(),
        archived: false,
        is_empty: empty,
        fork_parent_id: None,
        topics: topics.iter().map(|s| s.to_string()).collect(),
        pushed_at: if empty { None } else { Some(ago(0, 3)) },
        created_at: ago(90, 0),
        contribution_policy: Default::default(),
    }
}

pub fn repos() -> Vec<Repo> {
    let u = users();
    vec![
        repo(
            LIGHTHOUSE,
            &u[0],
            "lighthouse",
            "A tiny signalling library and CLI for sending light patterns",
            false,
            false,
            &["rust", "cli", "signals"],
        ),
        repo(NOTES, &u[0], "notes", "Private working notes", true, false, &[]),
        repo(DOCK, &u[3], "dock", "Shared tooling for the harbor team", false, false, &["tooling"]),
        repo(EMPTY, &u[1], "sandbox", "", false, true, &[]),
    ]
}

// ------------------------------------------------------------------ files

pub const README: &str = "# Lighthouse\n\nA tiny signalling library and command line tool. It turns short messages into light patterns, so a keen eye can read them from across the water.\n\n## Install\n\n```sh\ncargo install --path .\n```\n\n## Use\n\n```rust\nuse lighthouse::Beam;\n\nfn main() {\n    let beam = Beam::new(\"harbour\");\n    println!(\"{}\", beam.flash(\"SOS\"));\n}\n```\n\n## Features\n\n- [x] Morse patterns\n- [x] Colour channels (see #12)\n- [ ] Fog mode\n\n| Pattern | Meaning |\n| --- | --- |\n| `...` | Short flashes |\n| `---` | Long flashes |\n\nSee the [guide](docs/guide.md) for more. Fixed in abc1234 by @bob.\n";

pub const MAIN_RS: &str = "use lighthouse::Beam;\nuse std::env;\n\n/// Sends the first argument as a flashing pattern.\nfn main() {\n    let message = env::args().nth(1).unwrap_or_else(|| String::from(\"HELLO\"));\n    let beam = Beam::new(\"harbour\");\n    // Print the pattern, one flash per symbol.\n    let pattern = beam.flash(&message);\n    println!(\"{pattern}\");\n    if pattern.len() > 80 {\n        eprintln!(\"warning: long message\");\n    }\n}\n";

pub const LIB_RS: &str = "//! Lighthouse turns text into light patterns.\n\npub mod beam;\n\npub use beam::Beam;\n\n#[derive(Debug, Clone, PartialEq)]\npub enum Colour {\n    White,\n    Red,\n    Green,\n}\n\nimpl Colour {\n    pub fn code(&self) -> u8 {\n        match self {\n            Colour::White => 0,\n            Colour::Red => 1,\n            Colour::Green => 2,\n        }\n    }\n}\n";

pub const BEAM_RS: &str = "use crate::Colour;\n\n/// A beam sends one message at a time.\npub struct Beam {\n    name: String,\n    colour: Colour,\n}\n\nimpl Beam {\n    pub fn new(name: &str) -> Self {\n        Beam { name: name.to_string(), colour: Colour::White }\n    }\n\n    pub fn flash(&self, message: &str) -> String {\n        message\n            .chars()\n            .map(|c| match c.to_ascii_uppercase() {\n                'S' => \"...\",\n                'O' => \"---\",\n                'E' => \".\",\n                _ => \"-.-\",\n            })\n            .collect::<Vec<_>>()\n            .join(\" \")\n    }\n\n    pub fn name(&self) -> &str {\n        &self.name\n    }\n}\n";

pub const CARGO_TOML: &str =
    "[package]\nname = \"lighthouse\"\nversion = \"0.2.0\"\nedition = \"2024\"\nlicense = \"MIT\"\n\n[dependencies]\n";

pub const GUIDE_MD: &str = "# Guide\n\n## Colours\n\nA beam can be white, red or green. Pick one with `Beam::colour`.\n\n> Keep messages short. Fog hides long ones.\n\n## Tips\n\n1. Test at night.\n2. Keep a spare bulb.\n";

pub const CI_YML: &str = "name: CI\non:\n  push:\n    branches: [main]\n  pull_request:\njobs:\n  test:\n    runs-on: codehaven\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo test\n  lint:\n    runs-on: codehaven\n    needs: test\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo clippy -- -D warnings\n";

pub const LICENSE: &str = "MIT License\n\nCopyright (c) 2026 Ada Marlow\n\nPermission is hereby granted, free of charge, to any person obtaining a copy of this software.\n";

pub fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (".codehaven/workflows/ci.yml", CI_YML.as_bytes().to_vec()),
        ("Cargo.toml", CARGO_TOML.as_bytes().to_vec()),
        ("LICENSE", LICENSE.as_bytes().to_vec()),
        ("README.md", README.as_bytes().to_vec()),
        ("assets/logo.png", vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]),
        ("docs/guide.md", GUIDE_MD.as_bytes().to_vec()),
        ("src/beam.rs", BEAM_RS.as_bytes().to_vec()),
        ("src/lib.rs", LIB_RS.as_bytes().to_vec()),
        ("src/main.rs", MAIN_RS.as_bytes().to_vec()),
    ]
}

// ------------------------------------------------------------------ commits

pub fn sha(n: u8) -> String {
    format!("{n:02x}{}", "a1b2c3d4e5f60718293a4b5c6d7e8f9012345".chars().cycle().take(38).collect::<String>())
}

fn sig(name: &str, days: i64, hours: i64) -> Signature {
    Signature {
        name: name.into(),
        email: format!("{}@example.test", name.to_lowercase().split(' ').next().unwrap_or("x")),
        at: ago(days, hours),
    }
}

fn commit(n: u8, parent: Option<u8>, who: &str, days: i64, msg: &str) -> CommitInfo {
    CommitInfo {
        sha: sha(n),
        parents: parent.map(sha).into_iter().collect(),
        tree: sha(n + 100),
        author: sig(who, days, 2),
        committer: sig(who, days, 2),
        message: msg.into(),
    }
}

/// Newest first.
pub fn history() -> Vec<CommitInfo> {
    vec![
        commit(
            6,
            Some(5),
            "Bob Okafor",
            1,
            "Add colour channels to Beam\n\nA beam can now be white, red or green. The pattern text is unchanged, so existing callers are not affected.\n\nCloses #12",
        ),
        commit(5, Some(4), "Ada Marlow", 4, "Print a warning for long messages"),
        commit(4, Some(3), "Cleo Brandt", 9, "Document colours in the guide"),
        commit(3, Some(2), "Ada Marlow", 21, "Add the flash method and a CLI"),
        commit(2, Some(1), "Ada Marlow", 40, "Set up CI"),
        commit(1, None, "Ada Marlow", 60, "First commit"),
    ]
}

pub fn feature_commits() -> Vec<CommitInfo> {
    vec![
        commit(21, Some(6), "Cleo Brandt", 0, "Colour the signal on the wire\n\nEach colour gets its own channel number."),
        commit(20, Some(6), "Cleo Brandt", 1, "Add a Signal type"),
    ]
}

fn line(kind: LineKind, o: Option<u32>, n: Option<u32>, t: &str) -> DiffLine {
    DiffLine { kind, old_no: o, new_no: n, text: t.into() }
}

pub fn sample_diff() -> Diff {
    use LineKind::*;
    Diff {
        files: vec![
            FileDiff {
                status: FileStatus::Modified,
                old_path: Some("src/beam.rs".into()),
                new_path: Some("src/beam.rs".into()),
                binary: false,
                collapsed: false,
                additions: 7,
                deletions: 2,
                hunks: vec![Hunk {
                    header: "@@ -1,12 +1,17 @@ pub struct Beam {".into(),
                    lines: vec![
                        line(Context, Some(1), Some(1), "use crate::Colour;"),
                        line(Context, Some(2), Some(2), ""),
                        line(Context, Some(3), Some(3), "/// A beam sends one message at a time."),
                        line(Removed, Some(4), None, "pub struct Beam {"),
                        line(Removed, Some(5), None, "    name: String,"),
                        line(Added, None, Some(4), "pub struct Beam {"),
                        line(Added, None, Some(5), "    name: String,"),
                        line(Added, None, Some(6), "    colour: Colour,"),
                        line(Context, Some(6), Some(7), "}"),
                        line(Context, Some(7), Some(8), ""),
                        line(Context, Some(8), Some(9), "impl Beam {"),
                        line(Added, None, Some(10), "    pub fn with_colour(mut self, colour: Colour) -> Self {"),
                        line(Added, None, Some(11), "        self.colour = colour;"),
                        line(Added, None, Some(12), "        self"),
                        line(Added, None, Some(13), "    }"),
                        line(Context, Some(9), Some(14), "    pub fn new(name: &str) -> Self {"),
                    ],
                }],
            },
            FileDiff {
                status: FileStatus::Added,
                old_path: None,
                new_path: Some("src/signal.rs".into()),
                binary: false,
                collapsed: false,
                additions: 4,
                deletions: 0,
                hunks: vec![Hunk {
                    header: "@@ -0,0 +1,4 @@".into(),
                    lines: vec![
                        line(Added, None, Some(1), "/// One flash on the wire."),
                        line(Added, None, Some(2), "pub struct Signal {"),
                        line(Added, None, Some(3), "    pub channel: u8,"),
                        line(Added, None, Some(4), "}"),
                    ],
                }],
            },
            FileDiff {
                status: FileStatus::Modified,
                old_path: Some("README.md".into()),
                new_path: Some("README.md".into()),
                binary: false,
                collapsed: false,
                additions: 1,
                deletions: 1,
                hunks: vec![Hunk {
                    header: "@@ -17,5 +17,5 @@ cargo install --path .".into(),
                    lines: vec![
                        line(Context, Some(17), Some(17), "- [x] Morse patterns"),
                        line(Removed, Some(18), None, "- [ ] Colour channels"),
                        line(Added, None, Some(18), "- [x] Colour channels (see #12)"),
                        line(Context, Some(19), Some(19), "- [ ] Fog mode"),
                    ],
                }],
            },
            FileDiff {
                status: FileStatus::Modified,
                old_path: Some("assets/logo.png".into()),
                new_path: Some("assets/logo.png".into()),
                binary: true,
                collapsed: false,
                additions: 0,
                deletions: 0,
                hunks: vec![],
            },
        ],
    }
}

// ------------------------------------------------------------------ issues

pub fn labels() -> Vec<Label> {
    let l = |id: i64, n: &str, c: &str, d: &str| Label {
        id: LabelId(id),
        repo_id: LIGHTHOUSE,
        name: n.into(),
        color: c.into(),
        description: d.into(),
    };
    vec![
        l(1, "bug", "d73a4a", "Something is broken"),
        l(2, "enhancement", "0075ca", "A new feature or improvement"),
        l(3, "good first issue", "0e8a16", "A friendly place to start"),
        l(4, "docs", "7057ff", "Words and examples"),
        l(5, "wontfix", "cccccc", "Not going to happen"),
    ]
}

pub fn milestones() -> Vec<Milestone> {
    vec![
        Milestone {
            id: MilestoneId(1),
            repo_id: LIGHTHOUSE,
            title: "0.3.0".into(),
            description: "Colours and fog mode.".into(),
            due_on: Some(Utc::now() + Duration::days(20)),
            open: true,
            open_issues: 3,
            closed_issues: 5,
        },
        Milestone {
            id: MilestoneId(2),
            repo_id: LIGHTHOUSE,
            title: "0.2.0".into(),
            description: String::new(),
            due_on: Some(ago(15, 0)),
            open: false,
            open_issues: 0,
            closed_issues: 9,
        },
    ]
}

#[allow(clippy::too_many_arguments)]
pub fn issue(
    n: i64,
    title: &str,
    body: &str,
    open: bool,
    author: UserId,
    labels: &[&str],
    assignees: &[UserId],
    comments: i64,
    days: i64,
) -> Issue {
    Issue {
        id: IssueId(100 + n),
        repo_id: LIGHTHOUSE,
        number: n,
        is_pull: false,
        author_id: author,
        title: title.into(),
        body: body.into(),
        open,
        state_reason: if open { None } else { Some("completed".into()) },
        locked: false,
        labels: labels.iter().map(|s| s.to_string()).collect(),
        assignees: assignees.to_vec(),
        milestone_id: Some(MilestoneId(1)),
        comments,
        created_at: ago(days, 3),
        updated_at: ago(days.saturating_sub(1), 1),
        closed_at: if open { None } else { Some(ago(days.saturating_sub(2), 0)) },
    }
}

pub fn issues() -> Vec<Issue> {
    vec![
        issue(
            12,
            "Support colour channels",
            "People want to send more than one signal at once.\n\n- [x] Add a `Colour` type\n- [ ] Show the colour in the CLI\n\nSee `src/lib.rs` and ada/lighthouse#9.",
            true,
            BOB,
            &["enhancement"],
            &[CLEO],
            3,
            6,
        ),
        issue(
            11,
            "Crash when the message is empty",
            "Running `lighthouse \"\"` panics with an index error.\n\n```\nthread 'main' panicked at src/beam.rs:19:21\n```",
            true,
            CLEO,
            &["bug", "good first issue"],
            &[],
            1,
            9,
        ),
        issue(
            10,
            "Guide is missing a section on fog",
            "The guide should say what to do when the fog comes in. @ada can you take this?",
            true,
            BOB,
            &["docs"],
            &[ADA],
            0,
            12,
        ),
        issue(9, "Add a Signal type", "Done in #14.", false, ADA, &["enhancement"], &[], 2, 20),
        issue(
            15,
            "<img src=x onerror=alert(1)> Escape me",
            "<script>alert('xss')</script>\n\n[click](javascript:alert(1)) ![pic](https://tracker.example/pixel.png)",
            true,
            CLEO,
            &["bug"],
            &[],
            0,
            14,
        ),
        issue(8, "Typo in the README install command", "It says `cargo instal`.", false, CLEO, &["bug", "docs"], &[], 0, 30),
    ]
}

pub fn issue_comments(number: i64) -> Vec<Comment> {
    let c = |id: i64, by: UserId, body: &str, days: i64| Comment {
        id: CommentId(id),
        issue_id: IssueId(100 + number),
        author_id: by,
        body: body.into(),
        created_at: ago(days, 1),
        updated_at: ago(days, 1),
    };
    match number {
        12 => vec![
            c(1, ADA, "Good idea. I think three channels is enough to start: white, red and green.", 5),
            c(2, CLEO, "I can take this. I will open a pull request this week.", 4),
            c(
                3,
                BOB,
                "Thanks! Fixed in abc1234. One question: should the CLI take a `--colour` flag?\n\n```rust\nlet beam = Beam::new(\"harbour\").with_colour(Colour::Red);\n```",
                2,
            ),
        ],
        11 => vec![c(4, ADA, "Reproduced. The `chars().nth(0)` should be a `match`.", 8)],
        _ => vec![],
    }
}

// ------------------------------------------------------------------ pull requests

#[allow(clippy::too_many_arguments)]
pub fn pull(n: i64, title: &str, body: &str, state: &str, author: UserId, head: &str, base: &str, days: i64) -> PullRequest {
    let open = state == "open" || state == "draft";
    let mut i = issue(n, title, body, open, author, &["enhancement"], &[], 2, days);
    i.is_pull = true;
    i.milestone_id = None;
    i.state_reason = None;
    i.closed_at = if open { None } else { Some(ago(days.saturating_sub(1), 0)) };
    PullRequest {
        issue: i,
        // The service stores no head repository for a branch of the same repository.
        head_repo_id: None,
        from_fork: false,
        head_ref: head.into(),
        head_sha: sha(21),
        base_ref: base.into(),
        base_sha: sha(6),
        merge_base: Some(sha(6)),
        mergeable: MergeableState::Clean,
        merge_commit_sha: if state == "merged" { Some(sha(6)) } else { None },
        merged_at: if state == "merged" { Some(ago(days.saturating_sub(1), 0)) } else { None },
        merged_by: if state == "merged" { Some(ADA) } else { None },
        draft: state == "draft",
    }
}

pub fn pulls() -> Vec<PullRequest> {
    vec![
        pull(
            14,
            "Colour the signal on the wire",
            "Adds a `Signal` type so each beam can carry a colour channel.\n\nFixes #12\n\n- [x] Tests\n- [x] Docs",
            "open",
            CLEO,
            "feature/signal-colours",
            "main",
            1,
        ),
        pull(13, "Fog mode (work in progress)", "Early sketch. Do not merge yet.", "draft", BOB, "feature/fog", "main", 3),
        pull(
            7,
            "Print a warning for long messages",
            "Warns when the pattern is over 80 characters.",
            "merged",
            ADA,
            "fix/long-messages",
            "main",
            5,
        ),
    ]
}

pub fn reviews() -> Vec<Review> {
    vec![
        Review {
            id: ReviewId(1),
            pull_id: PullId(14),
            author_id: ADA,
            state: ReviewState::Commented,
            body: "Nice start. A couple of small things inline.".into(),
            commit_sha: sha(21),
            submitted_at: Some(ago(0, 8)),
        },
        Review {
            id: ReviewId(2),
            pull_id: PullId(14),
            author_id: BOB,
            state: ReviewState::Approved,
            body: "Looks good to me.".into(),
            commit_sha: sha(21),
            submitted_at: Some(ago(0, 3)),
        },
    ]
}

/// The inline review threads of pull 14: one conversation on `src/beam.rs`, asked by Ada in
/// review 1 and answered by Cleo. `review_comments` is the flat view of the same data.
pub fn threads() -> Vec<ch_core::collab::ThreadView> {
    let who = |id: UserId| {
        let u = users().into_iter().find(|u| u.id == id).unwrap_or_else(|| user(ADA, "ada", "Ada Marlow", true, UserKind::User));
        ch_core::collab::UserRef { id, username: u.username.clone(), display_name: u.display_name.clone(), kind: u.kind }
    };
    let comment =
        |id: i64, review: Option<ReviewId>, author: UserId, reply: Option<CommentId>, at, body: &str| ch_core::collab::ReviewComment {
            id: CommentId(id),
            pull_id: PullId(14),
            review_id: review,
            thread_id: Some(41),
            author_id: author,
            in_reply_to: reply,
            path: "src/beam.rs".into(),
            side: "right".into(),
            line: Some(6),
            start_line: None,
            original_line: Some(6),
            commit_sha: sha(21),
            original_commit_sha: sha(21),
            diff_hunk: "@@ -2,7 +2,7 @@".into(),
            body: body.into(),
            outdated: false,
            pending: false,
            created_at: at,
            updated_at: at,
        };
    let view = |c: ch_core::collab::ReviewComment| {
        let author = who(c.author_id);
        let has_suggestion = c.body.contains("```suggestion") && c.side == "right";
        ch_core::collab::ReviewCommentView { comment: c, author, reactions: Default::default(), has_suggestion }
    };
    vec![ch_core::collab::ThreadView {
        id: 41,
        path: "src/beam.rs".into(),
        resolved: false,
        resolved_by: None,
        outdated: false,
        comments: vec![
            view(comment(
                31,
                Some(ReviewId(1)),
                ADA,
                None,
                ago(0, 8),
                "Should `colour` default to `White`? `new` sets it, so this is fine, just checking.",
            )),
            view(comment(32, None, CLEO, Some(CommentId(31)), ago(0, 6), "Yes, `new` sets it to white.")),
            view(comment(
                33,
                None,
                BOB,
                Some(CommentId(31)),
                ago(0, 4),
                "Or say it where the field is:\n\n```suggestion\n    colour: Colour::White,\n```",
            )),
        ],
    }]
}

pub fn review_comments() -> Vec<ReviewComment> {
    threads()
        .into_iter()
        .flat_map(|t| {
            let resolved = t.resolved;
            t.comments
                .into_iter()
                .map(|cv| ReviewComment {
                    id: cv.comment.id,
                    review_id: cv.comment.review_id,
                    author_id: cv.comment.author_id,
                    path: cv.comment.path.clone(),
                    side: cv.comment.side.clone(),
                    line: cv.comment.line.unwrap_or(0),
                    start_line: cv.comment.start_line,
                    body: cv.comment.body.clone(),
                    in_reply_to: cv.comment.in_reply_to,
                    resolved,
                    commit_sha: cv.comment.commit_sha.clone(),
                    created_at: cv.comment.created_at,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// ------------------------------------------------------------------ CI

pub fn runs() -> Vec<WorkflowRun> {
    let r = |id: i64, n: i64, ev: &str, status: &str, concl: Option<&str>, sha_n: u8, branch: &str, h: i64| WorkflowRun {
        id: RunId(id),
        repo_id: LIGHTHOUSE,
        workflow_id: Some(WorkflowId(1)),
        run_number: n,
        attempt: 1,
        title: "Colour the signal on the wire".into(),
        workflow_name: "ci".into(),
        event: ev.into(),
        head_sha: sha(sha_n),
        head_branch: Some(branch.into()),
        status: status.into(),
        conclusion: concl.map(str::to_string),
        created_at: ago(0, h),
    };
    vec![
        r(53, 53, "push", "in_progress", None, 21, "feature/signal-colours", 0),
        r(52, 52, "pull_request", "completed", Some("success"), 21, "feature/signal-colours", 3),
        r(51, 51, "push", "completed", Some("failure"), 5, "main", 30),
        r(50, 50, "push", "completed", Some("success"), 6, "main", 52),
    ]
}

pub fn jobs(run: i64) -> Vec<WorkflowJob> {
    let step = |n: u32, name: &str, st: &str, c: Option<&str>, from: u64, to: u64| WorkflowStep {
        number: n,
        name: name.into(),
        status: st.into(),
        conclusion: c.map(str::to_string),
        started_at: Some(ago(0, 2)),
        completed_at: if st == "completed" { Some(ago(0, 2) + Duration::seconds(20)) } else { None },
        log_from: from,
        log_to: Some(to),
    };
    let (ts, tc, ls, lc) = match run {
        53 => ("in_progress", None, "queued", None),
        51 => ("completed", Some("failure"), "completed", Some("skipped")),
        _ => ("completed", Some("success"), "completed", Some("success")),
    };
    vec![
        WorkflowJob {
            id: JobId(run * 10 + 1),
            run_id: RunId(run),
            name: "test".into(),
            status: ts.into(),
            conclusion: tc.map(str::to_string),
            needs: vec![],
            runner_name: Some("harbour-runner-1".into()),
            queue_note: None,
            started_at: Some(ago(0, 2)),
            completed_at: if ts == "completed" { Some(ago(0, 2) + Duration::seconds(95)) } else { None },
            steps: vec![
                step(1, "Set up job", "completed", Some("success"), 0, 70),
                step(2, "Run actions/checkout@v4", "completed", Some("success"), 70, 190),
                step(3, "cargo test", if ts == "in_progress" { "in_progress" } else { "completed" }, tc, 190, 10_000),
            ],
        },
        WorkflowJob {
            id: JobId(run * 10 + 2),
            run_id: RunId(run),
            name: "lint".into(),
            status: ls.into(),
            conclusion: lc.map(str::to_string),
            needs: vec!["test".into()],
            runner_name: None,
            queue_note: None,
            started_at: None,
            completed_at: None,
            steps: vec![],
        },
    ]
}

pub fn log_text() -> String {
    "##[group]Set up job\nRunner: harbour-runner-1\nImage: codehaven-job:trixie\n##[endgroup]\n\u{1b}[1mRun actions/checkout@v4\u{1b}[0m\nFetching the repository\nHEAD is now at 15f9a1b Colour the signal on the wire\n\u{1b}[36mRunning\u{1b}[0m cargo test\n   \u{1b}[32mCompiling\u{1b}[0m lighthouse v0.2.0\n    \u{1b}[32mFinished\u{1b}[0m test profile in 3.41s\n     Running unittests src/lib.rs\n\nrunning 4 tests\ntest beam::tests::flash_sos ... \u{1b}[32mok\u{1b}[0m\ntest beam::tests::empty ... \u{1b}[31mFAILED\u{1b}[0m\n##[error]test failed: index out of bounds\n".to_string()
}

pub fn workflows() -> Vec<WorkflowDef> {
    vec![
        WorkflowDef {
            path: ".codehaven/workflows/ci.yml".into(),
            name: "CI".into(),
            triggers: vec!["push".into(), "pull_request".into(), "workflow_dispatch".into()],
            error: None,
            can_dispatch: true,
            inputs: vec![
                WorkflowInput {
                    name: "verbose".into(),
                    description: "Print every test".into(),
                    default: Some("false".into()),
                    required: false,
                    kind: "boolean".into(),
                    options: vec![],
                },
                WorkflowInput {
                    name: "target".into(),
                    description: "Where to deploy".into(),
                    default: Some("staging".into()),
                    required: true,
                    kind: "choice".into(),
                    options: vec!["staging".into(), "production".into()],
                },
            ],
        },
        WorkflowDef {
            path: ".codehaven/workflows/release.yml".into(),
            name: "Release".into(),
            triggers: vec!["push".into()],
            error: Some("line 7: unknown key `runs_on`, did you mean `runs-on`?".into()),
            can_dispatch: false,
            inputs: vec![],
        },
    ]
}

pub fn runners() -> Vec<Runner> {
    vec![
        Runner {
            id: RunnerId(1),
            name: "harbour-runner-1".into(),
            labels: vec!["codehaven".into(), "linux".into(), "x64".into()],
            repo_id: None,
            status: "busy".into(),
            version: "0.1.0".into(),
            last_seen_at: Some(ago(0, 0)),
            created_at: ago(30, 0),
        },
        Runner {
            id: RunnerId(2),
            name: "spare".into(),
            labels: vec!["codehaven".into()],
            repo_id: Some(LIGHTHOUSE),
            status: "offline".into(),
            version: "0.1.0".into(),
            last_seen_at: Some(ago(2, 0)),
            created_at: ago(10, 0),
        },
    ]
}

pub fn releases() -> Vec<Release> {
    vec![
        Release {
            id: ReleaseId(2),
            repo_id: LIGHTHOUSE,
            tag_name: "v0.2.0".into(),
            name: "Lighthouse 0.2.0".into(),
            body: "## What is new\n\n- A `flash` method and a small CLI\n- CI on every push\n\nThanks to @cleo for the docs.".into(),
            draft: false,
            prerelease: false,
            author_id: ADA,
            created_at: ago(15, 0),
            published_at: Some(ago(15, 0)),
        },
        Release {
            id: ReleaseId(1),
            repo_id: LIGHTHOUSE,
            tag_name: "v0.1.0".into(),
            name: "First release".into(),
            body: "The first tagged version.".into(),
            draft: false,
            prerelease: true,
            author_id: ADA,
            created_at: ago(55, 0),
            published_at: Some(ago(55, 0)),
        },
    ]
}

pub fn notifications() -> Vec<Notification> {
    let n = |id: i64, reason: &str, ty: &str, subject: i64, title: &str, unread: bool, h: i64| Notification {
        id: NotificationId(id),
        user_id: ADA,
        repo_id: Some(LIGHTHOUSE),
        reason: reason.into(),
        subject_type: ty.into(),
        subject_id: subject,
        title: title.into(),
        unread,
        updated_at: ago(0, h),
        number: Some(subject),
        state: Some(if subject == 14 { "merged".into() } else { "open".into() }),
    };
    vec![
        n(1, "review_requested", "pull", 14, "Colour the signal on the wire", true, 2),
        n(2, "mention", "issue", 10, "Guide is missing a section on fog", true, 20),
        n(3, "assigned", "issue", 12, "Support colour channels", false, 60),
    ]
}

pub fn collaborators() -> Vec<Collaborator> {
    vec![
        Collaborator { user_id: BOB, username: "bob".into(), permission: Permission::Write },
        Collaborator { user_id: CLEO, username: "cleo".into(), permission: Permission::Triage },
    ]
}

pub fn protections() -> Vec<BranchProtection> {
    vec![BranchProtection {
        pattern: "main".into(),
        required_approvals: 1,
        dismiss_stale_reviews: true,
        required_checks: vec!["test".into()],
        strict: true,
        block_force_push: true,
        block_deletion: true,
        ..Default::default()
    }]
}
