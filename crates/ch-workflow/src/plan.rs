//! Turns a workflow into the jobs a run will contain: matrix expansion, job names, the
//! `needs` graph. Pure, so the server and the tests plan runs the same way.

use crate::diag::Diagnostic;
use crate::expr::{self, Context, Value};
use crate::model::*;
use crate::parse::MAX_MATRIX_JOBS;
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// Longest a job may run, in minutes, whatever the file says. Actions' default is 360.
pub const DEFAULT_TIMEOUT_MINUTES: u32 = 360;
pub const MAX_TIMEOUT_MINUTES: u32 = 1440;

/// One job in a run: a job with no matrix, or one cell of its matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedJob {
    pub key: String,
    /// Display name with expressions filled in, and the matrix values in brackets-less form,
    /// such as `build (linux, 3)`.
    pub name: String,
    pub matrix: Option<BTreeMap<String, Json>>,
    /// Position among the cells of this job and how many there are.
    pub index: usize,
    pub total: usize,
    /// Labels after expressions are evaluated.
    pub runs_on: Vec<String>,
    pub needs: Vec<String>,
    pub fail_fast: bool,
    pub max_parallel: Option<u32>,
    pub timeout_minutes: u32,
    pub continue_on_error: bool,
    /// A matrix built from `needs` outputs cannot be expanded until the jobs it needs have
    /// finished. The job is planned once, and the run expands it when it becomes ready.
    pub deferred_matrix: Option<Matrix>,
    /// What the page calls each step, expressions filled in where the planning contexts allow.
    pub step_names: Vec<String>,
    pub job: Job,
}

pub type Cell = BTreeMap<String, Json>;

fn json_matches(a: &Json, b: &Json) -> bool {
    a == b
}

/// Expands a matrix for a job that was waiting on `needs`: the same as [`expand_matrix`], with
/// the context the finished jobs provide.
pub fn expand_deferred(m: &Matrix, ctx: &Context<'_>) -> Result<Vec<Cell>, String> {
    expand_matrix(m, ctx)
}

/// Expands a matrix: the product of the axes, minus `exclude`, with `include` merged in.
pub fn expand_matrix(m: &Matrix, ctx: &Context<'_>) -> Result<Vec<Cell>, String> {
    let resolved;
    let m = if let Some(src) = &m.expression {
        resolved = from_value(expr::eval_typed(src, ctx).map_err(|e| format!("the matrix expression failed: {}", e.message))?)?;
        &resolved
    } else {
        m
    };
    let mut cells: Vec<Cell> = Vec::new();
    if !m.axes.is_empty() {
        cells.push(Cell::new());
        for (axis, values) in &m.axes {
            let mut next = Vec::with_capacity(cells.len().saturating_mul(values.len()));
            for c in &cells {
                for v in values {
                    let mut c2 = c.clone();
                    c2.insert(axis.clone(), v.clone());
                    next.push(c2);
                }
            }
            if next.len() > MAX_MATRIX_JOBS {
                return Err(format!("the matrix makes more than {MAX_MATRIX_JOBS} jobs, which is the limit"));
            }
            cells = next;
        }
    }
    for ex in &m.exclude {
        cells.retain(|c| !ex.iter().all(|(k, v)| c.get(k).is_some_and(|cv| json_matches(cv, v))));
    }
    let axis_names: Vec<&str> = m.axes.iter().map(|(a, _)| a.as_str()).collect();
    // Includes join the original cells only, never a cell another include made.
    let original = cells.len();
    for inc in &m.include {
        let mut merged = false;
        for c in cells.iter_mut().take(original) {
            // It joins a cell only if it agrees with every original axis value it names.
            let agrees =
                inc.iter().filter(|(k, _)| axis_names.contains(&k.as_str())).all(|(k, v)| c.get(k).is_some_and(|cv| json_matches(cv, v)));
            if agrees {
                for (k, v) in inc {
                    if !axis_names.contains(&k.as_str()) {
                        c.insert(k.clone(), v.clone());
                    }
                }
                merged = true;
            }
        }
        if !merged {
            cells.push(inc.clone());
        }
    }
    if cells.len() > MAX_MATRIX_JOBS {
        return Err(format!("the matrix makes {} jobs, and the limit is {MAX_MATRIX_JOBS}", cells.len()));
    }
    Ok(cells)
}

fn from_value(v: Value) -> Result<Matrix, String> {
    let Value::Object(o) = v else { return Err("the matrix expression must give an object such as {\"os\": [\"a\", \"b\"]}".into()) };
    let mut m = Matrix::default();
    for (k, v) in o {
        match (k.as_str(), v) {
            ("include", Value::Array(items)) | ("exclude", Value::Array(items)) => {
                let list: Vec<Cell> = items
                    .into_iter()
                    .filter_map(|i| match i.to_json() {
                        Json::Object(o) => Some(o.into_iter().collect()),
                        _ => None,
                    })
                    .collect();
                if k == "include" {
                    m.include = list;
                } else {
                    m.exclude = list;
                }
            }
            (_, Value::Array(vals)) => m.axes.push((k, vals.iter().map(Value::to_json).collect())),
            (axis, _) => return Err(format!("matrix axis `{axis}` must be a list of values")),
        }
    }
    Ok(m)
}

/// A step's name for the page: its expressions filled in from `github`, `inputs`, `vars`,
/// `matrix` and `strategy`. A name that reads `secrets`, or reads something not known yet (`env`,
/// `steps`), stays as written. Display only: the runner gets the step as written.
pub fn step_display_name(raw: &str, ctx: &Context<'_>) -> String {
    if !expr::has_expression(raw) {
        return raw.to_string();
    }
    let refs = expr::template_refs(raw);
    if refs.all_secrets
        || !refs.secrets.is_empty()
        || refs.names.iter().any(|n| !matches!(n.as_str(), "github" | "inputs" | "vars" | "matrix" | "strategy"))
    {
        return raw.to_string();
    }
    match expr::interpolate(raw, ctx) {
        Ok(s) if !s.trim().is_empty() => s.trim().chars().take(200).collect(),
        _ => raw.to_string(),
    }
}

fn cell_label(cell: &Cell) -> String {
    cell.values()
        .filter_map(|v| match v {
            Json::String(s) => Some(s.clone()),
            Json::Number(n) => Some(n.to_string()),
            Json::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Plans every job of the workflow. `base` carries `github`, `inputs`, `vars` and `env`. Each
/// cell adds `matrix` and `strategy`. Problems are reported per job so one bad matrix does not
/// hide the others.
pub fn plan(wf: &Workflow, base: &Context<'_>) -> Result<Vec<PlannedJob>, Vec<Diagnostic>> {
    let mut out = Vec::new();
    let mut diags = Vec::new();
    for job in &wf.jobs {
        let fail = |msg: String| Diagnostic::new(job.line, 1, format!("job `{}`: {msg}", job.key));
        let mut deferred: Option<Matrix> = None;
        let (cells, fail_fast, max_parallel): (Vec<Option<Cell>>, bool, Option<u32>) = match job.strategy.as_ref() {
            Some(s) => match s.matrix.as_ref() {
                Some(m) if m.expression.as_deref().is_some_and(|e| expr::template_refs(e).names.contains("needs")) => {
                    deferred = Some(m.clone());
                    (vec![None], s.fail_fast, s.max_parallel)
                }
                Some(m) => match expand_matrix(m, base) {
                    Ok(c) if c.is_empty() => {
                        diags.push(fail("the matrix has no combinations left after `exclude`".into()));
                        continue;
                    }
                    Ok(c) => (c.into_iter().map(Some).collect(), s.fail_fast, s.max_parallel),
                    Err(e) => {
                        diags.push(fail(e));
                        continue;
                    }
                },
                None => (vec![None], s.fail_fast, s.max_parallel),
            },
            None => (vec![None], true, None),
        };
        let total = cells.len();
        for (index, cell) in cells.into_iter().enumerate() {
            match plan_cell(job, cell, index, total, fail_fast, max_parallel, deferred.clone(), base) {
                Ok(p) => out.push(p),
                Err(mut d) => diags.append(&mut d),
            }
        }
    }
    if out.len() > MAX_MATRIX_JOBS {
        diags.push(Diagnostic::new(0, 0, format!("the run would have {} jobs, and the limit is {MAX_MATRIX_JOBS}", out.len())));
    }
    if diags.is_empty() { Ok(out) } else { Err(diags) }
}

/// Plans one job for one matrix cell: its name, labels, timeout and container. The server uses
/// this directly when it expands a matrix that had to wait for `needs`.
#[allow(clippy::too_many_arguments)]
pub fn plan_cell(
    job: &Job,
    cell: Option<Cell>,
    index: usize,
    total: usize,
    fail_fast: bool,
    max_parallel: Option<u32>,
    deferred: Option<Matrix>,
    base: &Context<'_>,
) -> Result<PlannedJob, Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let fail = |msg: String| Diagnostic::new(job.line, 1, format!("job `{}`: {msg}", job.key));
    let mut ctx = base.clone();
    ctx.set("matrix", Value::from_json(&Json::Object(cell.clone().unwrap_or_default().into_iter().collect())));
    ctx.set(
        "strategy",
        Value::from_json(&serde_json::json!({
            "fail-fast": fail_fast,
            "job-index": index,
            "job-total": total,
            "max-parallel": max_parallel,
        })),
    );
    let waiting_on_needs = deferred.is_some();
    let label = cell.as_ref().map(cell_label).unwrap_or_default();
    let name = match job.name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(n) => match expr::interpolate(n, &ctx) {
            // A name that does not read `matrix` gets the cell's values, as on GitHub, so the
            // cells of one job (and their checks) can be told apart.
            Ok(s) if !label.is_empty() && !expr::template_refs(n).names.contains("matrix") => format!("{s} ({label})"),
            Ok(s) => s,
            Err(e) => {
                if waiting_on_needs {
                    job.key.clone()
                } else {
                    diags.push(fail(format!("`name`: {}", e.message)));
                    job.key.clone()
                }
            }
        },
        None => match &cell {
            Some(c) if !cell_label(c).is_empty() => format!("{} ({})", job.key, cell_label(c)),
            _ => job.key.clone(),
        },
    };
    let mut runs_on = Vec::new();
    for l in &job.runs_on {
        match expr::eval_typed(l, &ctx) {
            Ok(Value::Array(items)) => runs_on.extend(items.iter().map(Value::to_display)),
            Ok(v) => runs_on.push(v.to_display()),
            Err(e) => diags.push(fail(format!("`runs-on`: {}", e.message))),
        }
    }
    runs_on.retain(|l| !l.trim().is_empty());
    if waiting_on_needs {
        // The labels may read `matrix`, which does not exist yet. They are worked out per cell.
        runs_on = job.runs_on.clone();
        diags.retain(|d| !d.message.contains("`runs-on`"));
    } else if runs_on.is_empty() && diags.is_empty() && job.call_role.is_none() {
        diags.push(fail("`runs-on` has no label once its expressions are filled in".into()));
    }
    let eval_bool = |raw: &Option<String>| -> Result<bool, String> {
        match raw {
            None => Ok(false),
            Some(s) => expr::eval_typed(s, &ctx).map(|v| v.truthy() && v.to_display() != "false").map_err(|e| e.message),
        }
    };
    let coe = match eval_bool(&job.continue_on_error) {
        Ok(b) => b,
        Err(e) => {
            if !waiting_on_needs {
                diags.push(fail(format!("`continue-on-error`: {e}")));
            }
            false
        }
    };
    let timeout = match &job.timeout_minutes {
        None => DEFAULT_TIMEOUT_MINUTES,
        Some(raw) => match expr::eval_typed(raw, &ctx) {
            Ok(v) => {
                let n = v.to_display().trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 1.0);
                match n {
                    Some(n) => (n as u32).min(MAX_TIMEOUT_MINUTES),
                    None => {
                        if !waiting_on_needs {
                            diags.push(fail("`timeout-minutes` must be a number above zero".into()));
                        }
                        DEFAULT_TIMEOUT_MINUTES
                    }
                }
            }
            Err(e) => {
                if !waiting_on_needs {
                    diags.push(fail(format!("`timeout-minutes`: {}", e.message)));
                }
                DEFAULT_TIMEOUT_MINUTES
            }
        },
    };
    let mut resolved = job.clone();
    if let Some(c) = resolved.container.as_mut() {
        match expr::interpolate(&c.image, &ctx) {
            Ok(i) if !i.trim().is_empty() => c.image = i.trim().to_string(),
            Ok(_) if waiting_on_needs => {}
            Ok(_) => diags.push(fail("`container.image` is empty once its expressions are filled in".into())),
            Err(_) if waiting_on_needs => {}
            Err(e) => diags.push(fail(format!("`container.image`: {}", e.message))),
        }
        if let Some(o) = c.options.clone() {
            match expr::interpolate(&o, &ctx) {
                Ok(o) => {
                    // Checked again here because expressions could have written the flags.
                    if let Err(e) = crate::options::validate(&o, &crate::options::Limits::default()) {
                        diags.push(fail(format!("`container.options`: {e}")));
                    }
                    c.options = Some(o);
                }
                Err(_) if waiting_on_needs => {}
                Err(e) => diags.push(fail(format!("`container.options`: {}", e.message))),
            }
        }
    }
    if !diags.is_empty() {
        return Err(diags);
    }
    let step_names = job.steps.iter().map(|st| step_display_name(&st.display_name(), &ctx)).collect();
    Ok(PlannedJob {
        key: job.key.clone(),
        name,
        step_names,
        matrix: cell,
        index,
        total,
        runs_on,
        needs: job.needs.clone(),
        fail_fast,
        max_parallel,
        timeout_minutes: timeout,
        continue_on_error: coe,
        deferred_matrix: deferred,
        job: resolved,
    })
}

/// A loop in `needs`, as the keys along it, or `None`.
pub fn find_cycle(wf: &Workflow) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum C {
        White,
        Grey,
        Black,
    }
    let idx: BTreeMap<&str, usize> = wf.jobs.iter().enumerate().map(|(i, j)| (j.key.as_str(), i)).collect();
    let mut color = vec![C::White; wf.jobs.len()];
    let mut stack: Vec<usize> = Vec::new();
    fn visit(i: usize, wf: &Workflow, idx: &BTreeMap<&str, usize>, color: &mut Vec<C>, stack: &mut Vec<usize>) -> Option<Vec<String>> {
        color[i] = C::Grey;
        stack.push(i);
        for need in &wf.jobs[i].needs {
            let Some(&n) = idx.get(need.as_str()) else { continue };
            match color[n] {
                C::Grey => {
                    let start = stack.iter().position(|&s| s == n).unwrap_or(0);
                    let mut cyc: Vec<String> = stack[start..].iter().map(|&s| wf.jobs[s].key.clone()).collect();
                    cyc.push(wf.jobs[n].key.clone());
                    return Some(cyc);
                }
                C::White => {
                    if let Some(c) = visit(n, wf, idx, color, stack) {
                        return Some(c);
                    }
                }
                C::Black => {}
            }
        }
        stack.pop();
        color[i] = C::Black;
        None
    }
    for i in 0..wf.jobs.len() {
        if color[i] == C::White
            && let Some(c) = visit(i, wf, &idx, &mut color, &mut stack)
        {
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use proptest::prelude::*;
    use serde_json::json;

    fn m(axes: &[(&str, Vec<Json>)], include: Vec<Json>, exclude: Vec<Json>) -> Matrix {
        let to_cell = |j: Json| -> Cell { j.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect() };
        Matrix {
            axes: axes.iter().map(|(a, v)| (a.to_string(), v.clone())).collect(),
            include: include.into_iter().map(to_cell).collect(),
            exclude: exclude.into_iter().map(to_cell).collect(),
            expression: None,
        }
    }
    fn expand(mx: &Matrix) -> Vec<Cell> {
        expand_matrix(mx, &Context::new()).unwrap()
    }

    #[test]
    fn product_of_axes() {
        let c = expand(&m(&[("os", vec![json!("a"), json!("b")]), ("v", vec![json!(1), json!(2), json!(3)])], vec![], vec![]));
        assert_eq!(c.len(), 6);
        assert!(c.iter().all(|x| x.len() == 2));
        assert_eq!(c[0]["os"], "a");
        assert_eq!(c[0]["v"], 1);
    }

    #[test]
    fn exclude_removes_matching_cells() {
        let c = expand(&m(
            &[("os", vec![json!("a"), json!("b")]), ("v", vec![json!(1), json!(2)])],
            vec![],
            vec![json!({"os": "a", "v": 2}), json!({"os": "b"})],
        ));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["os"], "a");
        assert_eq!(c[0]["v"], 1);
    }

    #[test]
    fn include_adds_to_matching_cells_or_makes_a_new_one() {
        // The Actions documentation's examples.
        let c = expand(&m(
            &[("fruit", vec![json!("apple"), json!("pear")]), ("animal", vec![json!("cat"), json!("dog")])],
            vec![
                json!({"color": "green"}),
                json!({"color": "pink", "animal": "cat"}),
                json!({"fruit": "apple", "shape": "circle"}),
                json!({"fruit": "banana"}),
                json!({"fruit": "banana", "animal": "cat"}),
            ],
            vec![],
        ));
        // 4 original cells, plus two new ones for banana.
        assert_eq!(c.len(), 6);
        let find =
            |f: &str, a: &str| c.iter().find(|x| x.get("fruit") == Some(&json!(f)) && x.get("animal") == Some(&json!(a))).unwrap().clone();
        // `{color: green}` names no axis, so it joins every cell, and a later include overwrites it.
        assert_eq!(find("apple", "dog")["color"], "green");
        assert_eq!(find("pear", "cat")["color"], "pink");
        assert_eq!(find("pear", "dog")["color"], "green");
        assert_eq!(find("apple", "cat")["shape"], "circle");
        assert_eq!(find("apple", "dog")["shape"], "circle");
        assert!(!find("pear", "dog").contains_key("shape"));
        assert!(c.iter().any(|x| x["fruit"] == "banana" && x.get("animal").is_none()));
        assert!(c.iter().any(|x| x["fruit"] == "banana" && x.get("animal") == Some(&json!("cat"))));
    }

    #[test]
    fn include_cannot_overwrite_an_original_axis_value() {
        let c = expand(&m(&[("os", vec![json!("a")])], vec![json!({"os": "z", "extra": 1})], vec![]));
        assert_eq!(c.len(), 2, "it disagrees with the only cell, so it becomes its own");
        assert!(c.iter().any(|x| x["os"] == "a" && x.get("extra").is_none()));
        assert!(c.iter().any(|x| x["os"] == "z" && x["extra"] == 1));
    }

    #[test]
    fn include_only_matrix() {
        let c = expand(&m(&[], vec![json!({"a": 1}), json!({"a": 2})], vec![]));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn matrix_from_an_expression() {
        let mx = Matrix {
            expression: Some("${{ fromJSON('{\"os\": [\"a\", \"b\"], \"include\": [{\"os\": \"c\"}]}') }}".into()),
            ..Default::default()
        };
        let c = expand(&mx);
        assert_eq!(c.len(), 3);
        let bad = Matrix { expression: Some("${{ fromJSON('[1]') }}".into()), ..Default::default() };
        assert!(expand_matrix(&bad, &Context::new()).is_err());
    }

    #[test]
    fn the_cap_is_enforced() {
        let big: Vec<Json> = (0..20).map(|i| json!(i)).collect();
        let mx = m(&[("a", big.clone()), ("b", big)], vec![], vec![]);
        assert!(expand_matrix(&mx, &Context::new()).unwrap_err().contains("256"));
        let ok: Vec<Json> = (0..16).map(|i| json!(i)).collect();
        assert_eq!(expand(&m(&[("a", ok.clone()), ("b", ok)], vec![], vec![])).len(), 256);
    }

    #[test]
    fn a_matrix_from_needs_outputs_waits_until_its_jobs_finish() {
        let w = parse(
            "on: push
jobs:
  setup:
    runs-on: x
    outputs:
      m: ${{ steps.s.outputs.m }}
    steps:
      - id: s
        run: echo
  test:
    needs: setup
    runs-on: x
    strategy:
      matrix: ${{ fromJSON(needs.setup.outputs.m) }}
    steps:
      - run: echo
",
        )
        .unwrap();
        let jobs = plan(&w, &Context::new()).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].deferred_matrix.is_none());
        assert!(jobs[1].deferred_matrix.is_some());
        assert_eq!(jobs[1].total, 1);
        let ctx = Context::new().with("needs", Value::from(json!({"setup": {"outputs": {"m": "{\"os\": [\"a\", \"b\"]}"}}})));
        assert_eq!(expand_deferred(jobs[1].deferred_matrix.as_ref().unwrap(), &ctx).unwrap().len(), 2);
    }

    #[test]
    fn plan_names_and_labels() {
        let w = parse(
            "on: push
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: echo
  test:
    needs: lint
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      max-parallel: 2
      matrix:
        os: [a, b]
        n: [1, 2]
        exclude:
          - os: b
            n: 2
    steps:
      - run: echo
  named:
    name: Build ${{ github.event_name }}
    runs-on: [self, linux]
    timeout-minutes: 15
    continue-on-error: true
    steps:
      - run: echo
",
        )
        .unwrap();
        let base = Context::new().with("github", Value::from(json!({"event_name": "push"})));
        let jobs = plan(&w, &base).unwrap();
        assert_eq!(jobs.len(), 5);
        assert_eq!(jobs[0].name, "lint");
        assert_eq!(jobs[1].key, "test");
        assert_eq!(jobs[1].name, "test (1, a)", "values in key order");
        assert_eq!(jobs[1].runs_on, vec!["a"]);
        assert_eq!((jobs[1].index, jobs[1].total), (0, 3));
        assert!(!jobs[1].fail_fast);
        assert_eq!(jobs[1].max_parallel, Some(2));
        assert_eq!(jobs[1].needs, vec!["lint"]);
        assert_eq!(jobs[4].name, "Build push");
        assert_eq!(jobs[4].runs_on, vec!["self", "linux"]);
        assert_eq!(jobs[4].timeout_minutes, 15);
        assert!(jobs[4].continue_on_error);
        assert_eq!(jobs[0].timeout_minutes, 360);
    }

    #[test]
    fn plan_reports_every_bad_job() {
        let w = parse(
            "on: push
jobs:
  a:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [x]
        exclude:
          - os: x
    steps:
      - run: echo
  b:
    runs-on: ${{ inputs.nothing }}
    steps:
      - run: echo
",
        )
        .unwrap();
        let errs = plan(&w, &Context::new().with("inputs", Value::Null)).unwrap_err();
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].message.contains("no combinations"));
        assert!(errs[1].message.contains("no label"));
    }

    #[test]
    fn container_options_are_rechecked_after_expressions() {
        let w = parse(
            "on: push
jobs:
  a:
    runs-on: x
    container:
      image: alpine
      options: ${{ github.event_name }}
    steps:
      - run: echo
",
        )
        .unwrap();
        let base = Context::new().with("github", Value::from(json!({"event_name": "--privileged"})));
        let errs = plan(&w, &base).unwrap_err();
        assert!(errs[0].message.contains("--privileged"), "{errs:?}");
    }

    #[test]
    fn cycles_are_found() {
        let w = |needs: &str| {
            parse(&format!(
                "on: push\njobs:\n  a:\n    {needs}\n    runs-on: x\n    steps:\n      - run: echo\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo\n"
            ))
        };
        assert!(w("needs: []").is_ok());
        let errs = w("needs: b").unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("loop") && d.message.contains("a -> b -> a")), "{errs:?}");
        let errs = w("needs: a").unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("needs itself")));
    }

    proptest! {
        // The product has exactly the product of the axis sizes, all cells distinct.
        #[test]
        fn product_size(sizes in prop::collection::vec(1usize..5, 1..4)) {
            let axes: Vec<(String, Vec<Json>)> = sizes.iter().enumerate().map(|(i, n)| (format!("a{i}"), (0..*n).map(|v| json!(v)).collect())).collect();
            let mx = Matrix { axes: axes.clone(), ..Default::default() };
            let cells = expand_matrix(&mx, &Context::new()).unwrap();
            let want: usize = sizes.iter().product();
            prop_assert_eq!(cells.len(), want);
            let uniq: std::collections::BTreeSet<String> = cells.iter().map(|c| serde_json::to_string(c).unwrap()).collect();
            prop_assert_eq!(uniq.len(), want);
        }

        // Excluding one full cell removes exactly that cell.
        #[test]
        fn excluding_a_full_cell_removes_one(n in 1usize..5, k in 1usize..5, pick_a in 0usize..5, pick_b in 0usize..5) {
            let (a, b) = (pick_a % n, pick_b % k);
            let axes = vec![("x".to_string(), (0..n).map(|v| json!(v)).collect()), ("y".to_string(), (0..k).map(|v| json!(v)).collect())];
            let mut mx = Matrix { axes, ..Default::default() };
            mx.exclude.push([("x".to_string(), json!(a)), ("y".to_string(), json!(b))].into_iter().collect());
            let cells = expand_matrix(&mx, &Context::new()).unwrap();
            prop_assert_eq!(cells.len(), n * k - 1);
            prop_assert!(!cells.iter().any(|c| c["x"] == json!(a) && c["y"] == json!(b)));
        }

        // Include never makes the original cells disappear.
        #[test]
        fn include_never_removes(n in 1usize..5, extra in 0usize..4) {
            let axes = vec![("x".to_string(), (0..n).map(|v| json!(v)).collect())];
            let mut mx = Matrix { axes, ..Default::default() };
            for i in 0..extra {
                mx.include.push([("z".to_string(), json!(i))].into_iter().collect());
            }
            let cells = expand_matrix(&mx, &Context::new()).unwrap();
            prop_assert!(cells.len() >= n);
            for v in 0..n {
                prop_assert!(cells.iter().any(|c| c.get("x") == Some(&json!(v))));
            }
        }
    }
}
