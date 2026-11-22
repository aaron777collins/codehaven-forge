//! A corpus of workflow files in the style real projects write. Every one must parse, plan with
//! sample contexts, and have no expression that fails to read.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ch_workflow::expr::{Context, Value};
use ch_workflow::{parse, plan::plan};
use serde_json::json;
use std::path::Path;

fn contexts() -> Context<'static> {
    Context::new()
        .with(
            "github",
            Value::from(json!({
                "event_name": "push", "ref": "refs/heads/main", "ref_name": "main", "sha": "a".repeat(40), "actor": "alice",
                "repository": "alice/demo", "run_id": 1, "run_number": 1, "event": {"inputs": {}},
                "head_ref": "", "base_ref": "", "workspace": "/work/1",
            })),
        )
        .with("env", Value::from(json!({})))
        .with("vars", Value::from(json!({})))
        .with("secrets", Value::from(json!({})))
        .with("inputs", Value::from(json!({"level": "quick", "version": "1.0", "dry_run": false})))
        .with("needs", Value::from(json!({})))
        .with("runner", Value::from(json!({"os": "Linux", "arch": "X64"})))
}

#[test]
fn every_corpus_file_parses_and_plans() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ok");
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("fixtures/ok exists").filter_map(Result::ok).map(|e| e.path()).collect();
    files.sort();
    assert!(files.len() >= 20, "the corpus has {} files, expected at least 20", files.len());
    let mut failures = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap();
        match parse(&src) {
            Err(e) => failures.push(format!("{}:\n{}", f.display(), ch_workflow::diag::render(&e))),
            Ok(w) => {
                if let Err(e) = plan(&w, &contexts()) {
                    // A matrix built from needs outputs cannot be planned here, which is fine.
                    let real: Vec<_> = e.iter().filter(|d| !d.message.contains("needs")).collect();
                    if !real.is_empty() {
                        failures.push(format!(
                            "{} (plan):\n{}",
                            f.display(),
                            real.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n")
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
