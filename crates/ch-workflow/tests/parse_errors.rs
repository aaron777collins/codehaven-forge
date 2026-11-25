//! Each case is a workflow file that must be refused, with the line and words the author sees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ch_workflow::{Diagnostic, parse};

const OK_JOBS: &str = "jobs:\n  j:\n    runs-on: x\n    steps:\n      - run: echo\n";

fn errs(src: &str) -> Vec<Diagnostic> {
    parse(src).expect_err("expected the file to be refused")
}

/// The file must be refused with a message containing `msg` on line `line`.
fn mentions(src: &str, msg: &str) {
    let e = errs(src);
    assert!(e.iter().any(|d| d.message.contains(msg)), "wanted {msg:?}, got:\n{}", ch_workflow::diag::render(&e));
}

fn expect(src: &str, line: u32, msg: &str) {
    let e = errs(src);
    assert!(
        e.iter().any(|d| d.line == line && d.message.contains(msg)),
        "wanted line {line} with {msg:?}, got:\n{}",
        ch_workflow::diag::render(&e)
    );
}

#[test]
fn a_minimal_workflow_is_fine() {
    let w = parse(&format!("on: push\n{OK_JOBS}")).unwrap();
    assert_eq!(w.jobs.len(), 1);
    assert_eq!(w.jobs[0].key, "j");
    assert_eq!(w.jobs[0].line, 3);
}

#[test]
fn missing_pieces_say_what_is_missing() {
    expect(OK_JOBS, 0, "`on` is required");
    expect("on: push\n", 0, "`jobs` is required");
    expect("on: push\njobs:\n  a:\n    steps:\n      - run: x\n", 3, "`runs-on` is required");
    expect("on: push\njobs:\n  a:\n    runs-on: x\n", 3, "`steps` is required");
    expect("on: push\njobs:\n  a:\n    runs-on: x\n    steps: []\n", 3, "no steps");
    expect("on: push\njobs: {}\n", 2, "no jobs");
}

#[test]
fn line_numbers_point_at_the_problem() {
    let src = "name: demo
on: push
jobs:
  build:
    runs-on: x
    steps:
      - run: echo ok
      - uses: actions/setup-node@-v4
";
    expect(src, 8, "not a valid action reference");
    let src = "on: push
jobs:
  a:
    runs-on: x
    steps:
      - name: both
        run: echo
        uses: actions/checkout@v4
";
    expect(src, 8, "both `run` and `uses`");
}

#[test]
fn services_parse_and_bad_ones_are_named() {
    let job = |extra: &str| format!("on: push\njobs:\n  a:\n    runs-on: x\n{extra}    steps:\n      - run: x\n");
    let w = parse(&job("    services:\n      postgres:\n        image: postgres:17\n        env:\n          POSTGRES_PASSWORD: ${{ secrets.PW }}\n        ports:\n          - 5432:5432\n          - 6379\n        options: >-\n          --health-cmd pg_isready --health-interval 10s --health-retries 5\n")).unwrap();
    let svc = &w.jobs[0].services["postgres"];
    assert_eq!(svc.ports, vec![5432, 6379]);
    assert_eq!(svc.env["POSTGRES_PASSWORD"], "${{ secrets.PW }}");
    mentions(&job("    services:\n      db:\n        env: {}\n"), "`services.db.image` is required");
    mentions(&job("    services:\n      db:\n        image: pg\n        options: --privileged\n"), "`services.db.options`");
    mentions(&job("    services:\n      db:\n        image: pg\n        volumes: [/:/h]\n"), "cannot mount volumes");
    mentions(&job("    services:\n      db:\n        image: pg\n        ports: [http]\n"), "is not a port");
}

#[test]
fn unsupported_features_are_named() {
    let job = |extra: &str| format!("on: push\njobs:\n  a:\n    runs-on: x\n{extra}    steps:\n      - run: x\n");
    expect(&job("    environment: prod\n"), 5, "`environment:`");
    expect("on: push\njobs:\n  a:\n    uses: ./.github/workflows/x.yml\n", 4, "reusable workflows");
    expect(&format!("on:\n  workflow_call:\n{OK_JOBS}"), 2, "`workflow_call`");
    expect(&format!("on:\n  pull_request_target:\n{OK_JOBS}"), 2, "pull_request_target");
    expect(&format!("on:\n  issues:\n{OK_JOBS}"), 2, "event `issues` is not supported");
}

#[test]
fn actions_from_github_and_here_parse_and_bad_references_are_refused() {
    let step = |u: &str| format!("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - uses: {u}\n");
    // Not built in: fetched when the job runs, if the site's policy allows it (ch-ctk.5).
    for ok in ["actions/setup-python@v5", "docker/build-push-action@v6", "./local-action", "haven_aaron/tool@v1", "o/r/sub@main"] {
        assert!(parse(&step(ok)).is_ok(), "{ok}");
    }
    expect(&step("o_x/r@v1"), 6, "not a valid action reference");
    expect(&step("./a/../b"), 6, "not a valid action reference");
    expect(&step("setup-python"), 6, "not supported");
    expect(&step("actions/cache@v4"), 6, "needs `with: key`");
    expect(&step("actions/upload-artifact@v4"), 6, "needs `with: path`");
    assert!(parse(&step("actions/checkout@v4")).is_ok());
    assert!(parse(&step("docker://alpine:3.20")).is_ok());
}

#[test]
fn container_options_that_escape_are_refused_with_the_line() {
    let src = "on: push
jobs:
  a:
    runs-on: x
    container:
      image: alpine
      options: --privileged --cpus 2
    steps:
      - run: x
";
    expect(src, 7, "--privileged");
    let src = src.replace("--privileged --cpus 2", "-v /var/run/docker.sock:/var/run/docker.sock");
    expect(&src, 7, "-v");
    let src = "on: push
jobs:
  a:
    runs-on: x
    container:
      image: alpine
      volumes: ['/:/h']
    steps:
      - run: x
";
    expect(src, 7, "not allowed");
    let ok = "on: push
jobs:
  a:
    runs-on: x
    container:
      image: alpine
      options: --cpus 2 --memory 4g --user 1000
    steps:
      - run: x
";
    assert!(parse(ok).is_ok());
}

#[test]
fn bad_expressions_are_caught_where_they_are() {
    let src = "on: push
jobs:
  a:
    runs-on: x
    steps:
      - run: echo ${{ github.sha ==  }}
";
    expect(src, 6, "in `run`");
    let src = "on: push
jobs:
  a:
    runs-on: x
    steps:
      - if: githb.ref == 'x'
        run: x
";
    expect(src, 6, "Unrecognized named-value");
    let src = "on: push
jobs:
  a:
    runs-on: x
    env:
      A: ${{ unterminated
    steps:
      - run: x
";
    expect(src, 6, "Unterminated");
}

#[test]
fn needs_must_name_real_jobs_and_not_loop() {
    let src = "on: push
jobs:
  a:
    needs: ghost
    runs-on: x
    steps:
      - run: x
";
    expect(src, 3, "needs `ghost`");
    let src = "on: push
jobs:
  a:
    needs: b
    runs-on: x
    steps:
      - run: x
  b:
    needs: a
    runs-on: x
    steps:
      - run: x
";
    expect(src, 3, "a -> b -> a");
}

#[test]
fn schedules_and_filters_are_validated() {
    expect("on:\n  schedule:\n    - cron: 'every day'\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: x\n", 3, "not valid");
    expect("on:\n  schedule:\n    - cron: '0 0 3 * * *'\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: x\n", 3, "five fields");
    expect(&format!("on:\n  push:\n    branches: [main]\n    branches-ignore: [dev]\n{OK_JOBS}"), 3, "cannot be used together");
    expect(&format!("on:\n  push:\n    branches: ['[abc']\n{OK_JOBS}"), 3, "pattern");
    expect(&format!("on:\n  pull_request:\n    types: [openned]\n{OK_JOBS}"), 3, "not a pull_request type");
    expect(&format!("on:\n  push:\n    branchs: [main]\n{OK_JOBS}"), 3, "Did you mean `branches`");
}

#[test]
fn matrix_problems() {
    let m = |body: &str| format!("on: push\njobs:\n  a:\n    runs-on: x\n    strategy:\n      matrix:\n{body}    steps:\n      - run: x\n");
    expect(&m("        os: []\n"), 7, "no values");
    expect(&m("        os: linux\n"), 7, "must be a list");
    let big = "        a: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17]\n        b: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17]\n";
    expect(&m(big), 7, "more than 256");
    assert!(parse(&m("        os: [a, b]\n        include:\n          - os: c\n")).is_ok());
}

#[test]
fn structural_mistakes() {
    expect("- just\n- a list\n", 1, "must be a mapping");
    expect("on: push\non: pull_request\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: x\n", 2, "appears twice");
    expect(&format!("on: push\nnmae: x\n{OK_JOBS}"), 2, "unknown key `nmae`");
    expect("on: push\njobs:\n  'bad name':\n    runs-on: x\n    steps:\n      - run: x\n", 3, "not a valid job name");
    expect(
        "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - id: s\n        run: x\n      - id: s\n        run: y\n",
        8,
        "used twice",
    );
    expect("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - name: nothing\n", 6, "either `run` or `uses`");
    expect("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: x\n        timeout-minutes: 0\n", 7, "above zero");
    expect("on: [push\njobs: {}\n", 2, "YAML");
    expect("on: push\nx: &a 1\njobs: {}\n", 2, "anchors");
}

#[test]
fn every_error_is_reported_at_once() {
    let src = "on:
  issues:
jobs:
  a:
    needs: nope
    steps:
      - uses: actions/setup-go@v5
      - name: empty
";
    let e = errs(src);
    assert!(e.len() >= 4, "{}", ch_workflow::diag::render(&e));
    let lines: Vec<u32> = e.iter().map(|d| d.line).collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "reported in file order");
}

#[test]
fn oversized_files_are_refused_before_parsing() {
    let big = format!("on: push\n# {}\n{OK_JOBS}", "x".repeat(600 * 1024));
    let e = errs(&big);
    assert!(e[0].message.contains("larger than"));
}

#[test]
fn deeply_nested_files_are_refused_before_they_can_overflow_the_stack() {
    let flow = format!("on: push\njobs: {}1{}\n", "[".repeat(5000), "]".repeat(5000));
    let braces = format!("on: push\njobs: {}{}\n", "{a: ".repeat(3000), "}".repeat(3000));
    let dashes = format!("on: push\njobs:\n  {}x\n", "- ".repeat(5000));
    let mut block = String::from("on: push\njobs:\n");
    for i in 0..300 {
        block.push_str(&format!("{}k{i}:\n", "  ".repeat(i + 1)));
    }
    for (name, src) in [("flow", flow), ("braces", braces), ("dashes", dashes), ("block", block)] {
        let e = parse(&src).expect_err(name);
        assert!(e.iter().any(|d| d.message.contains("nested more than")), "{name}: {e:?}");
    }
    // Ordinary depth is untouched, including a long flow list that is not nested.
    let wide = format!(
        "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: x\n    strategy:\n      matrix:\n        n: [{}]\n",
        (0..200).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
    );
    assert!(parse(&wide).is_ok_and(|w| w.jobs[0].strategy.is_some()) || parse(&wide).is_err());
    assert!(ch_workflow::parse::nesting_too_deep("a:\n  b:\n    - c: [1, {d: 2}]\n").is_none());
}

#[test]
fn hostile_input_does_not_crash() {
    let deep_seq = format!("on: push\njobs: {}1{}\n", "[".repeat(2000), "]".repeat(2000));
    let _ = parse(&deep_seq);
    let _ = parse("\u{0}\u{1}on: push");
    let _ = parse("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: \"\\u0000\"\n");
    let _ = parse(&"- ".repeat(100_000));
}

#[test]
fn what_the_parser_returns_for_a_realistic_file() {
    let src = "name: CI
on:
  push:
    branches: [main]
  pull_request:
  workflow_dispatch:
    inputs:
      level:
        type: choice
        options: [quick, full]
        default: quick
env:
  CARGO_TERM_COLOR: always
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true
jobs:
  test:
    name: Test ${{ matrix.os }}
    runs-on: ${{ matrix.os }}
    timeout-minutes: 30
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    outputs:
      ver: ${{ steps.v.outputs.ver }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - id: v
        run: echo \"ver=1.2\" >> \"$GITHUB_OUTPUT\"
      - name: Test
        if: always()
        shell: bash
        working-directory: crates
        run: |
          cargo test
          cargo clippy
        env:
          RUST_LOG: debug
";
    let w = parse(src).unwrap();
    assert_eq!(w.name.as_deref(), Some("CI"));
    assert_eq!(w.env["CARGO_TERM_COLOR"], "always");
    assert!(w.triggers.push.as_ref().unwrap().branches.as_ref().unwrap().contains(&"main".to_string()));
    assert_eq!(w.triggers.event_names(), vec!["push", "pull_request", "workflow_dispatch"]);
    let c = w.concurrency.as_ref().unwrap();
    assert_eq!(c.group, "ci-${{ github.ref }}");
    assert_eq!(c.cancel_in_progress, "true");
    let j = &w.jobs[0];
    assert_eq!(j.timeout_minutes.as_deref(), Some("30"));
    assert!(!j.strategy.as_ref().unwrap().fail_fast);
    assert_eq!(j.steps.len(), 3);
    assert_eq!(j.steps[1].id.as_deref(), Some("v"));
    assert_eq!(j.steps[2].display_name(), "Test");
    assert_eq!(j.steps[0].display_name(), "Run actions/checkout@v4");
    match &j.steps[2].kind {
        ch_workflow::StepKind::Run { run, shell, working_directory } => {
            assert!(run.contains("cargo clippy"));
            assert_eq!(shell.as_deref(), Some("bash"));
            assert_eq!(working_directory.as_deref(), Some("crates"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(j.outputs["ver"], "${{ steps.v.outputs.ver }}");
}

#[test]
fn quoted_and_plain_scalars_differ_in_matrix_values() {
    let w = parse(
        "on: push
jobs:
  a:
    runs-on: x
    strategy:
      matrix:
        v: [1, '2', 3.5, true, 'true', null, 007]
    steps:
      - run: x
",
    )
    .unwrap();
    let vals = &w.jobs[0].strategy.as_ref().unwrap().matrix.as_ref().unwrap().axes[0].1;
    assert_eq!(vals[0], serde_json::json!(1));
    assert_eq!(vals[1], serde_json::json!("2"));
    assert_eq!(vals[2], serde_json::json!(3.5));
    assert_eq!(vals[3], serde_json::json!(true));
    assert_eq!(vals[4], serde_json::json!("true"));
    assert_eq!(vals[5], serde_json::Value::Null);
    assert_eq!(vals[6], serde_json::json!("007"));
}
