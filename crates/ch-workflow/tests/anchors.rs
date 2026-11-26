//! YAML anchors and aliases in a workflow file (ch-ctk.7).

#![allow(clippy::unwrap_used)]

use ch_workflow::StepKind;
use ch_workflow::parse::parse;

const WITH_ANCHORS: &str = "name: ci
on: push
env: &base
  A: '1'
  GREETING: hello world
jobs:
  one:
    runs-on: ubuntu-latest
    env: *base
    steps:
      - &hello
        name: Say hello
        run: echo hi
  two:
    runs-on: ubuntu-latest
    steps:
      - *hello
      - run: echo ${{ env.A }}
";

#[test]
fn a_file_with_anchors_reads_as_the_file_with_the_values_written_out() {
    let w = parse(WITH_ANCHORS).unwrap_or_else(|d| panic!("{d:?}"));
    assert_eq!(w.jobs.len(), 2);
    assert_eq!(w.jobs[0].env["A"], "1");
    assert_eq!(w.jobs[0].env["GREETING"], "hello world");
    assert_eq!(w.env["A"], "1");
    let two = &w.jobs[1];
    assert_eq!(two.steps.len(), 2);
    assert_eq!(two.steps[0].name.as_deref(), Some("Say hello"));
    let StepKind::Run { run, .. } = &two.steps[0].kind else { panic!() };
    assert_eq!(run, "echo hi");
    let StepKind::Run { run, .. } = &two.steps[1].kind else { panic!() };
    assert_eq!(run, "echo ${{ env.A }}");
    // The same file with the values written out is the same workflow.
    let plain = WITH_ANCHORS
        .replace("env: &base\n  A: '1'\n  GREETING: hello world\n", "env:\n  A: '1'\n  GREETING: hello world\n")
        .replace("    env: *base\n", "    env:\n      A: '1'\n      GREETING: hello world\n")
        .replace("      - &hello\n        name: Say hello\n        run: echo hi\n", "      - name: Say hello\n        run: echo hi\n")
        .replace("      - *hello\n", "      - name: Say hello\n        run: echo hi\n");
    let p = parse(&plain).unwrap();
    assert_eq!(serde_json::to_value(&w.jobs[0].env).unwrap(), serde_json::to_value(&p.jobs[0].env).unwrap());
    assert_eq!(w.jobs[1].steps.len(), p.jobs[1].steps.len());
}

#[test]
fn a_file_that_expands_without_end_is_refused_with_a_reason() {
    let mut t = String::from("name: bomb\non: push\nx0: &l0 [x, x, x, x, x, x, x, x, x, x]\n");
    for i in 1..8 {
        let prev = format!("*l{}", i - 1);
        t.push_str(&format!("x{i}: &l{i} [{}]\n", vec![prev; 10].join(", ")));
    }
    t.push_str("jobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo\n");
    let e = parse(&t).unwrap_err();
    assert!(e.iter().any(|d| d.message.contains("expand to more than")), "{e:?}");
}

#[test]
fn merge_keys_and_undefined_aliases_are_refused_with_a_reason() {
    let merge =
        "name: m\non: push\ndefaults: &d\n  run: {shell: bash}\njobs:\n  a:\n    <<: *d\n    runs-on: x\n    steps:\n      - run: echo\n";
    let e = parse(merge).unwrap_err();
    assert!(e.iter().any(|d| d.message.contains("merge keys")), "{e:?}");
    let undefined = "name: u\non: push\njobs:\n  a:\n    runs-on: *nowhere\n    steps:\n      - run: echo\n";
    assert!(parse(undefined).is_err());
}
