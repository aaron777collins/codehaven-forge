//! Reading an action's `action.yml` (ch-ctk.4).

#![allow(clippy::unwrap_used)]

use ch_workflow::StepKind;
use ch_workflow::action_def::ActionRuns;
use ch_workflow::parse::parse_action;

#[test]
fn a_javascript_action_with_pre_and_post() {
    let d = parse_action(
        "name: Setup thing
description: ignored
branding: {icon: box}
inputs:
  token:
    default: ${{ github.token }}
  version:
    required: true
outputs:
  path:
    description: where
runs:
  using: node20
  main: dist/index.js
  post: dist/post.js
  post-if: success()
",
    )
    .unwrap();
    assert_eq!(d.name, "Setup thing");
    assert_eq!(d.inputs.len(), 2);
    assert_eq!(d.inputs[0].default.as_deref(), Some("${{ github.token }}"));
    assert!(d.inputs[1].required && !d.inputs[0].required);
    assert_eq!(d.outputs.get("path"), Some(&None));
    match d.runs {
        ActionRuns::Node { using, main, post, post_if, pre, .. } => {
            assert_eq!((using.as_str(), main.as_str()), ("node20", "dist/index.js"));
            assert_eq!(post.as_deref(), Some("dist/post.js"));
            assert_eq!(post_if.as_deref(), Some("success()"));
            assert!(pre.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_composite_action_keeps_its_steps_and_output_expressions() {
    let d = parse_action(
        "name: c
outputs:
  greeting:
    value: ${{ steps.say.outputs.text }}
runs:
  using: composite
  steps:
    - id: say
      run: echo text=hi >> $GITHUB_OUTPUT
      shell: bash
    - uses: ./inner
",
    )
    .unwrap();
    assert_eq!(d.outputs["greeting"].as_deref(), Some("${{ steps.say.outputs.text }}"));
    let ActionRuns::Composite { steps } = d.runs else { panic!() };
    assert_eq!(steps.len(), 2);
    assert!(matches!(&steps[1].kind, StepKind::Uses { uses, .. } if uses == "./inner"));
}

#[test]
fn what_cannot_run_is_named() {
    assert!(parse_action("name: x\n").unwrap_err()[0].message.contains("runs"));
    assert!(parse_action("runs:\n  using: node20\n").unwrap_err()[0].message.contains("runs.main"));
    assert!(parse_action("runs:\n  using: java\n").unwrap_err()[0].message.contains("not something the runner can run"));
    assert!(matches!(parse_action("runs:\n  using: docker\n  image: Dockerfile\n").unwrap().runs, ActionRuns::Docker { .. }));
}

/// Codehaven's own workflows (ch-d9s) parse, and its CI job runs in the builder with Postgres
/// beside it.
#[test]
fn codehavens_own_workflows_parse() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.codehaven/workflows");
    let mut seen = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let src = std::fs::read_to_string(&p).unwrap();
        let w = ch_workflow::parse(&src).unwrap_or_else(|d| panic!("{}: {d:?}", p.display()));
        if p.file_name().is_some_and(|n| n == "ci.yml") {
            let job = w.job("ci").unwrap();
            // The builder for the toolchain this tree pins, or the job would build with another.
            let toolchain = std::fs::read_to_string(dir.join("../../rust-toolchain.toml")).unwrap();
            let channel = toolchain
                .lines()
                .find_map(|l| l.trim().strip_prefix("channel"))
                .unwrap()
                .trim()
                .trim_start_matches('=')
                .trim()
                .trim_matches('"')
                .to_string();
            assert_eq!(job.container.as_ref().map(|c| c.image.clone()), Some(format!("codehaven-builder:{channel}")));
            assert_eq!(job.services["postgres"].ports, vec![5432]);
            assert!(job.steps.iter().any(|s| matches!(&s.kind, StepKind::Run { run, .. } if run.contains("make ci CH_NATIVE=1"))));
        }
        seen += 1;
    }
    assert!(seen >= 2);
}
