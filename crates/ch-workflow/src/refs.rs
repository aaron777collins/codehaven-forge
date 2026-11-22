//! What a job reads from the outside: which secrets and variables, and which `needs`.
//! The server uses this to send a runner only the secrets a job names.

use crate::expr::{self, Refs};
use crate::model::*;

fn add(refs: &mut Refs, s: &str) {
    refs.merge(&expr::template_refs(s));
}

fn add_condition(refs: &mut Refs, c: &Option<String>) {
    if let Some(c) = c {
        // A bare condition has no `${{ }}`, so read it as an expression.
        if expr::has_expression(c) {
            add(refs, c);
        } else if let Ok(e) = expr::parse_expr(c) {
            e.collect_refs(refs);
        }
    }
}

/// Every reference in the job, including the workflow-level `env`, which every job inherits.
pub fn job_refs(wf: &Workflow, job: &Job) -> Refs {
    let mut r = Refs::default();
    for v in wf.env.values().chain(job.env.values()).chain(job.outputs.values()) {
        add(&mut r, v);
    }
    for l in &job.runs_on {
        add(&mut r, l);
    }
    if let Some(n) = &job.name {
        add(&mut r, n);
    }
    add_condition(&mut r, &job.condition);
    if let Some(c) = &job.container {
        add(&mut r, &c.image);
        for v in c.env.values() {
            add(&mut r, v);
        }
        if let Some(o) = &c.options {
            add(&mut r, o);
        }
    }
    for s in &job.steps {
        if let Some(n) = &s.name {
            add(&mut r, n);
        }
        add_condition(&mut r, &s.condition);
        for v in s.env.values() {
            add(&mut r, v);
        }
        match &s.kind {
            StepKind::Run { run, shell, working_directory } => {
                add(&mut r, run);
                for v in [shell, working_directory].into_iter().flatten() {
                    add(&mut r, v);
                }
            }
            StepKind::Uses { with, .. } => {
                for v in with.values() {
                    add(&mut r, v);
                }
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn collects_secrets_and_vars_from_everywhere() {
        let w = parse(
            "on: push
env:
  GLOBAL: ${{ secrets.G }}
jobs:
  a:
    runs-on: x
    env:
      J: ${{ vars.JV }}
    steps:
      - run: echo ${{ secrets.TOKEN }}
        env:
          E: ${{ secrets.E }}
      - if: secrets.COND != ''
        uses: actions/checkout@v4
        with:
          token: ${{ secrets.CO }}
  b:
    runs-on: x
    steps:
      - run: echo none
",
        )
        .unwrap();
        let a = job_refs(&w, w.job("a").unwrap());
        assert_eq!(a.secrets.iter().cloned().collect::<Vec<_>>(), vec!["CO", "COND", "E", "G", "TOKEN"]);
        assert_eq!(a.vars.iter().cloned().collect::<Vec<_>>(), vec!["JV"]);
        let b = job_refs(&w, w.job("b").unwrap());
        assert_eq!(b.secrets.iter().cloned().collect::<Vec<_>>(), vec!["G"], "workflow env reaches every job");
        assert!(!b.all_secrets);
    }
}
