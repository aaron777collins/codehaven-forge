//! Reusable workflows (ch-ctk.7): `on: workflow_call` in the called file, `uses:` on a job of the
//! calling one.
//!
//! A call is inlined when the run is made. The called workflow's jobs take the place of the
//! calling job, so there is one run, one set of jobs and nothing new for the runner to learn:
//!
//! ```text
//! build:                      build__entry   stands for the caller's `needs` and `if`
//!   uses: ./x.yml             build__test    the called job `test`, its `needs` renamed
//!   with: {v: 3}              build__lint    another called job
//!                             build          waits for them all, gives back the result and outputs
//! ```
//!
//! The caller's key goes to the last job, so `needs.build.result` and `needs.build.outputs.x`
//! elsewhere in the caller mean what they mean on GitHub. The two server-side jobs (`CallRole`)
//! have no steps; the server finishes them itself.
//!
//! What the called jobs may read is decided here, not at run time. Every `inputs.x` in them is
//! replaced by what the caller passed, and every `secrets.X` by what the caller passed under that
//! name (nothing, when it passed nothing), so a called workflow never sees a secret it was not
//! given, and `needs` and `jobs` names move to the new keys. `with:` and `secrets:` values may
//! read `github`, `inputs` and `vars` only: they are pasted into jobs that are not the caller.
//!
//! This module is pure. The server finds each called file (checking who may read it) and passes
//! the files in as [`Resolved`], a tree with one node per `uses:`.

use crate::diag::Diagnostic;
use crate::model::*;
use crate::permissions::{Ceiling, effective};
use crate::rewrite::{FragType, Rewrite, contexts_read, fragment, rewrite_condition, rewrite_text};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// How many reusable workflows may be nested: a workflow that calls one that calls one ...
pub const MAX_DEPTH: usize = 4;
/// The most jobs one expansion may make. The run's own limit on jobs still applies after it.
pub const MAX_EXPANDED_JOBS: usize = 200;
/// The most distinct workflows one run may call, nested ones included.
pub const MAX_CALLED_FILES: usize = 20;
/// The longest `with:` or `secrets:` value of a call, once read.
pub const MAX_FRAGMENT: usize = 16 * 1024;
/// The most text all the rewrites of one expansion may make together. Every called workflow's
/// jobs are rewritten again at each level of nesting, so this bounds the memory an expansion uses.
pub const MAX_PASTED_TOTAL: usize = 8 * 1024 * 1024;
/// What separates a caller's key from a called job's key.
pub const SEP: &str = "__";

/// Where a `uses:` on a job points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// `./.codehaven/workflows/x.yml`: the repository and commit of the file that says it.
    Local { path: String },
    /// `haven_owner/repo/.codehaven/workflows/x.yml@ref`: a repository on this instance.
    Repo { owner: String, repo: String, path: String, git_ref: String },
}

fn workflow_path_ok(path: &str) -> bool {
    let Some((dir, file)) = path.rsplit_once('/') else { return false };
    crate::WORKFLOW_DIRS.contains(&dir)
        && crate::is_workflow_file(file)
        && file.len() > 4
        && !file.starts_with('.')
        && file.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && s != "."
        && s != ".."
}

/// Reads the text after `uses:` on a job. `Err` is a sentence for the person who wrote it.
pub fn parse_call_uses(uses: &str) -> Result<CallTarget, String> {
    let u = uses.trim();
    if u.is_empty() || u != uses || u.chars().any(char::is_control) || u.contains("${{") {
        return Err("`uses` must name a workflow file, with no spaces or expressions".into());
    }
    if let Some(path) = u.strip_prefix("./") {
        if path.contains('@') {
            return Err("a workflow in this repository is named without `@ref`, like `./.codehaven/workflows/build.yml`".into());
        }
        if !workflow_path_ok(path) {
            return Err(format!(
                "`{u}` is not a workflow file. It must be directly in {}, and end in .yml or .yaml",
                crate::WORKFLOW_DIRS.join(" or ")
            ));
        }
        return Ok(CallTarget::Local { path: path.to_string() });
    }
    let Some((name, git_ref)) = u.split_once('@') else {
        return Err("a workflow in another repository needs `@ref`, like `haven_owner/repo/.codehaven/workflows/build.yml@v1`".into());
    };
    if git_ref.is_empty()
        || git_ref.len() > 200
        || git_ref.contains("..")
        || git_ref.contains('@')
        || git_ref.chars().any(|c| c.is_whitespace())
    {
        return Err(format!("`@{git_ref}` is not a branch, tag or commit"));
    }
    let mut parts = name.splitn(3, '/');
    let (owner, repo, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let Some(owner) = owner.strip_prefix("haven_") else {
        return Err(
            "a workflow from another repository is named `haven_owner/repo/.codehaven/workflows/file.yml@ref`: the `haven_` marks a repository on this instance, and workflows are not fetched from GitHub".into(),
        );
    };
    if !name_ok(owner) || !name_ok(repo) || !workflow_path_ok(path) {
        return Err(format!("`{u}` is not `haven_owner/repo/<{}>/file.yml@ref`", crate::WORKFLOW_DIRS.join(" or ")));
    }
    Ok(CallTarget::Repo { owner: owner.to_string(), repo: repo.to_string(), path: path.to_string(), git_ref: git_ref.to_string() })
}

/// A called workflow, with what it calls in turn, by the `uses:` text it was named with.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub workflow: Workflow,
    pub calls: BTreeMap<String, Resolved>,
}

/// The `uses:` texts of the jobs of a workflow that call one, each once, with the first line.
pub fn calls_of(wf: &Workflow) -> Vec<(String, u32)> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for j in &wf.jobs {
        if let Some(c) = &j.call
            && seen.insert(c.uses.clone())
        {
            out.push((c.uses.clone(), c.line));
        }
    }
    out
}

/// How many workflows a resolved tree holds, not counting its root.
pub fn count_called(calls: &BTreeMap<String, Resolved>) -> usize {
    calls.values().map(|r| 1 + count_called(&r.calls)).sum()
}

fn diag(job: &Job, msg: impl Into<String>) -> Diagnostic {
    let line = job.call.as_ref().map(|c| c.line).unwrap_or(job.line);
    Diagnostic::new(line, 1, format!("job `{}`: {}", job.key, msg.into()))
}

/// What one expansion has used so far.
struct Budget {
    pasted: usize,
}

impl Budget {
    fn charge(&mut self, text: &str) -> Result<(), String> {
        self.pasted = self.pasted.saturating_add(text.len());
        if self.pasted > MAX_PASTED_TOTAL {
            return Err(format!(
                "the called workflows make more than {} MB of text when their inputs are pasted in, which is too large",
                MAX_PASTED_TOTAL / (1024 * 1024)
            ));
        }
        Ok(())
    }
}

/// How many jobs inlining the calls of `wf` makes, counted without building them and never more
/// than one past the limit. `memo` holds the count of each called workflow already counted.
fn count_jobs(wf: &Workflow, calls: &BTreeMap<String, Resolved>, depth: usize, memo: &mut BTreeMap<usize, usize>) -> usize {
    let cap = MAX_EXPANDED_JOBS + 1;
    let mut total = 0usize;
    for job in &wf.jobs {
        total += match &job.call {
            None => 1,
            // Refused when it is built, with a sentence that says why.
            Some(_) if depth >= MAX_DEPTH => 1,
            Some(call) => match calls.get(&call.uses) {
                None => 1,
                Some(r) => {
                    let key = std::ptr::from_ref(r) as usize;
                    let inner = match memo.get(&key) {
                        Some(n) => *n,
                        None => {
                            let n = count_jobs(&r.workflow, &r.calls, depth + 1, memo);
                            memo.insert(key, n);
                            n
                        }
                    };
                    2 + inner
                }
            },
        };
        if total >= cap {
            return cap;
        }
    }
    total
}

/// Inlines every call in `wf`. A workflow with no calls comes back as it is.
pub fn expand(wf: &Workflow, calls: &BTreeMap<String, Resolved>) -> Result<Workflow, Vec<Diagnostic>> {
    if wf.jobs.iter().all(|j| j.call.is_none()) {
        return Ok(wf.clone());
    }
    // The size is known before anything is built: a call that calls many calls would otherwise
    // make millions of jobs, and the memory for them, before the limit was looked at.
    if count_jobs(wf, calls, 0, &mut BTreeMap::new()) > MAX_EXPANDED_JOBS {
        return Err(vec![Diagnostic::new(
            0,
            0,
            format!("the reusable workflows make more than {MAX_EXPANDED_JOBS} jobs, and the limit is {MAX_EXPANDED_JOBS}"),
        )]);
    }
    let jobs = expand_jobs(wf, calls, 0, &mut Budget { pasted: 0 })?;
    if jobs.len() > MAX_EXPANDED_JOBS {
        return Err(vec![Diagnostic::new(
            0,
            0,
            format!("the reusable workflows make {} jobs, and the limit is {MAX_EXPANDED_JOBS}", jobs.len()),
        )]);
    }
    let mut out = wf.clone();
    out.jobs = jobs;
    Ok(out)
}

/// The jobs of `wf` with every call inlined, still in `wf`'s own terms (its `inputs` and `secrets`
/// are the caller's business, and are replaced when `wf` is itself inlined).
fn expand_jobs(wf: &Workflow, calls: &BTreeMap<String, Resolved>, depth: usize, budget: &mut Budget) -> Result<Vec<Job>, Vec<Diagnostic>> {
    let mut out: Vec<Job> = Vec::new();
    let mut diags = Vec::new();
    let taken: BTreeSet<&str> = wf.jobs.iter().map(|j| j.key.as_str()).collect();
    for job in &wf.jobs {
        let Some(call) = &job.call else {
            out.push(job.clone());
            continue;
        };
        if depth >= MAX_DEPTH {
            diags.push(diag(job, format!("reusable workflows may be nested {MAX_DEPTH} deep at most")));
            continue;
        }
        let Some(r) = calls.get(&call.uses) else {
            diags.push(diag(job, format!("{} was not found", call.uses)));
            continue;
        };
        match inline_one(wf, job, call, r, depth, &taken, budget) {
            Ok(mut jobs) => out.append(&mut jobs),
            Err(mut d) => diags.append(&mut d),
        }
    }
    if !diags.is_empty() {
        return Err(diags);
    }
    let mut seen = BTreeSet::new();
    for j in &out {
        if !seen.insert(j.key.clone()) {
            diags.push(Diagnostic::new(
                j.line,
                1,
                format!("job `{}` is made twice: a job is already named like a called job, so rename one of them", j.key),
            ));
        }
    }
    if diags.is_empty() { Ok(out) } else { Err(diags) }
}

/// The jobs one call turns into.
fn inline_one(
    caller_wf: &Workflow,
    caller: &Job,
    call: &Call,
    r: &Resolved,
    depth: usize,
    taken: &BTreeSet<&str>,
    budget: &mut Budget,
) -> Result<Vec<Job>, Vec<Diagnostic>> {
    let callee = &r.workflow;
    let Some(wc) = &callee.triggers.workflow_call else {
        return Err(vec![diag(caller, format!("{} cannot be called: it has no `on: workflow_call`", call.uses))]);
    };
    // The called workflow's own calls first, so what it passes down is in its own terms.
    let inner = expand_jobs(callee, &r.calls, depth + 1, budget).map_err(|d| {
        d.into_iter().map(|x| Diagnostic::new(call.line, 1, format!("job `{}`: in {}: {}", caller.key, call.uses, x))).collect::<Vec<_>>()
    })?;

    let mut errs: Vec<Diagnostic> = Vec::new();
    let mut rw = Rewrite { inherit_secrets: matches!(call.secrets, CallSecrets::Inherit), ..Default::default() };
    bind_inputs(caller, call, wc, &mut rw, &mut errs);
    bind_secrets(caller, call, wc, &mut rw, &mut errs);
    let k = caller.key.as_str();
    let entry_key = format!("{k}{SEP}entry");
    for j in &inner {
        let new = format!("{k}{SEP}{}", j.key);
        rw.jobs.insert(j.key.to_ascii_lowercase(), new);
    }
    for new in rw.jobs.values().chain(std::iter::once(&entry_key)) {
        if taken.contains(new.as_str()) {
            errs.push(diag(
                caller,
                format!("a job is already named `{new}`, which a job of the called workflow would take. Rename one of them"),
            ));
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }

    let caller_name = caller.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| k.to_string());
    // What the caller's token may do is the most any called job may do.
    let ceiling = effective(caller_wf.permissions.as_ref(), caller.permissions.as_ref(), Ceiling::Write, false);

    let mut out = Vec::with_capacity(inner.len() + 2);
    out.push(Job {
        key: entry_key.clone(),
        line: caller.line,
        name: Some(format!("{caller_name} (start)")),
        runs_on: Vec::new(),
        needs: caller.needs.clone(),
        condition: caller.condition.clone(),
        call: None,
        call_role: Some(CallRole::Entry),
        ..empty_job()
    });
    let mut all_keys = vec![entry_key.clone()];
    for j in &inner {
        let mut v = serde_json::to_value(j).map_err(|e| vec![diag(caller, e.to_string())])?;
        // The called workflow's own `env` and `defaults` apply to its jobs, under the job's own.
        merge_workflow_defaults(&mut v, callee);
        let mut failed = Vec::new();
        walk_strings(&mut v, None, &mut |text, is_condition| {
            let r = if is_condition { rewrite_condition(text, &rw) } else { rewrite_text(text, &rw) };
            let out = r.map_err(|e| e.to_string())?;
            budget.charge(&out)?;
            Ok(out)
        })
        .unwrap_or_else(|e| failed.push(diag(caller, format!("in {}, job `{}`: {e}", call.uses, j.key))));
        if !failed.is_empty() {
            return Err(failed);
        }
        let mut nj: Job = serde_json::from_value(v).map_err(|e| vec![diag(caller, e.to_string())])?;
        let new_key = format!("{k}{SEP}{}", j.key);
        let own = nj.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| j.key.clone());
        nj.name = Some(format!("{caller_name} / {own}"));
        nj.key = new_key.clone();
        nj.needs = j.needs.iter().map(|n| rw.jobs.get(&n.to_ascii_lowercase()).cloned().unwrap_or_else(|| n.clone())).collect();
        nj.needs.push(entry_key.clone());
        // Behind the entry job, so a call the caller would have skipped runs nothing, not even a
        // job of the called workflow that says `if: always()`.
        let gate = format!("needs.{entry_key}.result == 'success'");
        nj.condition = Some(match nj.condition.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => format!("{gate} && ({})", unwrap_expression(c)),
            None => gate,
        });
        // The called job's token may only be narrower than the caller's.
        let own_perms = effective(callee.permissions.as_ref(), nj.permissions.as_ref(), Ceiling::Write, false);
        nj.permissions = Some(own_perms.intersect(&ceiling).to_declared());
        nj.call = None;
        nj.called = true;
        all_keys.push(new_key);
        out.push(nj);
    }

    // The exit job: the caller's key, so what needs the caller needs this.
    let mut outputs = BTreeMap::new();
    for o in &wc.outputs {
        match rewrite_text(&o.value, &rw).and_then(|v| budget.charge(&v).map(|()| v)) {
            Ok(v) => {
                outputs.insert(o.name.clone(), v);
            }
            Err(e) => errs.push(Diagnostic::new(call.line, 1, format!("job `{}`: in {}, output `{}`: {e}", caller.key, call.uses, o.name))),
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    out.push(Job {
        key: caller.key.clone(),
        line: caller.line,
        name: Some(caller_name),
        runs_on: Vec::new(),
        needs: all_keys,
        condition: Some("always()".into()),
        outputs,
        call: None,
        call_role: Some(CallRole::Exit),
        ..empty_job()
    });
    Ok(out)
}

/// The expression inside an `if:` written as `${{ ... }}`, or the text as it is.
fn unwrap_expression(c: &str) -> String {
    let t = c.trim();
    if let Some(inner) = t.strip_prefix("${{").and_then(|x| x.strip_suffix("}}"))
        && !inner.contains("${{")
    {
        return inner.trim().to_string();
    }
    t.to_string()
}

fn empty_job() -> Job {
    Job {
        key: String::new(),
        line: 0,
        name: None,
        runs_on: Vec::new(),
        container: None,
        strategy: None,
        needs: Vec::new(),
        condition: None,
        timeout_minutes: None,
        continue_on_error: None,
        env: BTreeMap::new(),
        outputs: BTreeMap::new(),
        defaults: Defaults::default(),
        concurrency: None,
        permissions: None,
        services: BTreeMap::new(),
        call: None,
        call_role: None,
        called: false,
        steps: Vec::new(),
    }
}

/// Puts the called workflow's `env` and `defaults` under a job's own, in the job's JSON.
fn merge_workflow_defaults(v: &mut Json, callee: &Workflow) {
    let Some(o) = v.as_object_mut() else { return };
    let mut env: serde_json::Map<String, Json> = callee.env.iter().map(|(k, v)| (k.clone(), Json::String(v.clone()))).collect();
    if let Some(own) = o.get("env").and_then(Json::as_object) {
        for (k, v) in own {
            env.insert(k.clone(), v.clone());
        }
    }
    o.insert("env".into(), Json::Object(env));
    let mut d = serde_json::Map::new();
    if let Some(s) = &callee.defaults.shell {
        d.insert("shell".into(), Json::String(s.clone()));
    }
    if let Some(s) = &callee.defaults.working_directory {
        d.insert("working_directory".into(), Json::String(s.clone()));
    }
    if let Some(own) = o.get("defaults").and_then(Json::as_object) {
        for (k, v) in own {
            if !v.is_null() {
                d.insert(k.clone(), v.clone());
            }
        }
    }
    o.insert("defaults".into(), Json::Object(d));
}

/// Calls `f` on every string in `v` (not on object keys), with whether the string sits under a
/// `condition` key, and replaces it with the answer.
fn walk_strings(v: &mut Json, key: Option<&str>, f: &mut dyn FnMut(&str, bool) -> Result<String, String>) -> Result<(), String> {
    match v {
        Json::String(s) => {
            if s.contains("${{") || key == Some("condition") {
                *s = f(s, key == Some("condition"))?;
            }
            Ok(())
        }
        Json::Array(a) => a.iter_mut().try_for_each(|x| walk_strings(x, key, f)),
        Json::Object(o) => o.iter_mut().try_for_each(|(k, x)| walk_strings(x, Some(k.as_str()), f)),
        _ => Ok(()),
    }
}

/// The contexts a `with:` or `secrets:` value of a call may read.
const CALL_CONTEXTS: &[&str] = &["github", "inputs", "vars"];

fn check_contexts(caller: &Job, what: &str, frag: &str, allowed: &[&str], errs: &mut Vec<Diagnostic>) -> bool {
    let bad: Vec<String> = contexts_read(frag).into_iter().filter(|c| !allowed.contains(&c.as_str())).collect();
    if bad.is_empty() {
        return true;
    }
    errs.push(diag(
        caller,
        format!(
            "{what} reads `{}`, which a call cannot use here: it may read {} only, because its value is passed into the called jobs",
            bad.join("`, `"),
            allowed.join(", ")
        ),
    ));
    false
}

fn json_literal(v: &Json, ty: CallInputType) -> String {
    let text = match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    };
    match ty {
        CallInputType::String => format!("'{}'", text.replace('\'', "''")),
        CallInputType::Boolean => (matches!(v, Json::Bool(true)) || text.eq_ignore_ascii_case("true")).to_string(),
        CallInputType::Number => {
            if text.parse::<f64>().is_ok_and(f64::is_finite) {
                text
            } else {
                "0".into()
            }
        }
    }
}

fn bind_inputs(caller: &Job, call: &Call, wc: &WorkflowCall, rw: &mut Rewrite, errs: &mut Vec<Diagnostic>) {
    let declared: BTreeSet<&str> = wc.inputs.iter().map(|i| i.name.as_str()).collect();
    for name in call.with.keys() {
        if !declared.contains(name.as_str()) {
            errs.push(diag(caller, format!("`with.{name}` is not an input of {}", call.uses)));
        }
    }
    for i in &wc.inputs {
        let ty = match i.kind {
            CallInputType::String => FragType::String,
            CallInputType::Boolean => FragType::Boolean,
            CallInputType::Number => FragType::Number,
        };
        let frag = match call.with.get(&i.name) {
            Some(v) => match fragment(v, ty) {
                Ok(f) => {
                    if f.len() > MAX_FRAGMENT {
                        errs.push(diag(caller, format!("`with.{}` is too long", i.name)));
                        continue;
                    }
                    if !check_contexts(caller, &format!("`with.{}`", i.name), &f, CALL_CONTEXTS, errs) {
                        continue;
                    }
                    f
                }
                Err(e) => {
                    errs.push(diag(caller, format!("`with.{}`: {e}", i.name)));
                    continue;
                }
            },
            None => match &i.default {
                Some(d) => json_literal(d, i.kind),
                None if i.required => {
                    errs.push(diag(caller, format!("{} needs the input `{}`, so add it under `with`", call.uses, i.name)));
                    continue;
                }
                None => json_literal(&Json::Null, i.kind),
            },
        };
        rw.inputs.insert(i.name.to_ascii_lowercase(), frag);
    }
}

fn bind_secrets(caller: &Job, call: &Call, wc: &WorkflowCall, rw: &mut Rewrite, errs: &mut Vec<Diagnostic>) {
    let declared: BTreeSet<&str> = wc.secrets.iter().map(|s| s.name.as_str()).collect();
    match &call.secrets {
        CallSecrets::Inherit => {}
        CallSecrets::Map(m) => {
            for (name, v) in m {
                if !declared.contains(name.as_str()) {
                    errs.push(diag(caller, format!("`secrets.{name}` is not a secret of {}", call.uses)));
                    continue;
                }
                match fragment(v, FragType::String) {
                    Ok(f) if f.len() > MAX_FRAGMENT => errs.push(diag(caller, format!("`secrets.{name}` is too long"))),
                    Ok(f) => {
                        if check_contexts(caller, &format!("`secrets.{name}`"), &f, &["github", "inputs", "vars", "secrets"], errs) {
                            rw.secrets.insert(name.to_ascii_lowercase(), f);
                        }
                    }
                    Err(e) => errs.push(diag(caller, format!("`secrets.{name}`: {e}"))),
                }
            }
            for s in &wc.secrets {
                if s.required && !m.contains_key(&s.name) {
                    errs.push(diag(
                        caller,
                        format!("{} needs the secret `{}`, so pass it under `secrets` (or use `secrets: inherit`)", call.uses, s.name),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn wf(src: &str) -> Workflow {
        parse(src).unwrap_or_else(|d| panic!("{}", crate::diag::render(&d)))
    }

    const CALLEE: &str = "name: build
on:
  workflow_call:
    inputs:
      version: {type: string, required: true}
      deploy: {type: boolean, default: false}
      count: {type: number, default: 2}
    secrets:
      token: {required: true}
    outputs:
      artifact:
        value: ${{ jobs.pack.outputs.name }}
env:
  FROM_CALLEE: yes
jobs:
  test:
    runs-on: linux
    steps:
      - run: echo ${{ inputs.version }} ${{ inputs.count }}
  pack:
    runs-on: linux
    needs: test
    outputs:
      name: ${{ steps.p.outputs.name }}
    steps:
      - id: p
        run: echo name=x >> $GITHUB_OUTPUT
        env:
          T: ${{ secrets.token }}
          OTHER: ${{ secrets.unrelated }}
  ship:
    runs-on: linux
    needs: pack
    if: inputs.deploy && needs.pack.result == 'success'
    steps:
      - run: echo ship ${{ needs.pack.outputs.name }}
";

    fn caller(with: &str) -> Workflow {
        wf(&format!(
            "name: ci
on: push
jobs:
  prep:
    runs-on: linux
    steps:
      - run: echo
  build:
    needs: prep
    uses: ./.codehaven/workflows/build.yml
{with}
  after:
    runs-on: linux
    needs: build
    steps:
      - run: echo ${{{{ needs.build.outputs.artifact }}}}
"
        ))
    }

    fn tree() -> BTreeMap<String, Resolved> {
        BTreeMap::from([("./.codehaven/workflows/build.yml".to_string(), Resolved { workflow: wf(CALLEE), calls: BTreeMap::new() })])
    }

    fn job<'a>(w: &'a Workflow, key: &str) -> &'a Job {
        w.job(key).unwrap_or_else(|| panic!("no job {key} in {:?}", w.jobs.iter().map(|j| &j.key).collect::<Vec<_>>()))
    }

    #[test]
    fn uses_text_is_read_strictly() {
        assert_eq!(parse_call_uses("./.codehaven/workflows/a.yml"), Ok(CallTarget::Local { path: ".codehaven/workflows/a.yml".into() }));
        assert_eq!(
            parse_call_uses("haven_aaron/tools/.github/workflows/ci.yaml@v1"),
            Ok(CallTarget::Repo {
                owner: "aaron".into(),
                repo: "tools".into(),
                path: ".github/workflows/ci.yaml".into(),
                git_ref: "v1".into()
            })
        );
        for bad in [
            "",
            "./a.yml",
            "./.codehaven/workflows/../x.yml",
            "./.codehaven/workflows/a.yml@v1",
            "./.codehaven/workflows/sub/a.yml",
            "octo/tools/.github/workflows/ci.yml@v1",
            "haven_aaron/tools/.github/workflows/ci.yml",
            "haven_aaron/tools/ci.yml@v1",
            "haven_aaron/tools/.github/workflows/ci.yml@a..b",
            "haven_aaron/tools/.github/workflows/ci.yml@ v1",
            "./.codehaven/workflows/${{ x }}.yml",
            " ./.codehaven/workflows/a.yml",
        ] {
            assert!(parse_call_uses(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_call_becomes_an_entry_the_called_jobs_and_an_exit_with_the_callers_key() {
        let out =
            expand(&caller("    with:\n      version: '1.2'\n    secrets:\n      token: ${{ secrets.DEPLOY_TOKEN }}"), &tree()).unwrap();
        let keys: Vec<&str> = out.jobs.iter().map(|j| j.key.as_str()).collect();
        assert_eq!(keys, ["prep", "build__entry", "build__test", "build__pack", "build__ship", "build", "after"]);
        // The entry stands for the caller's needs and if.
        let entry = job(&out, "build__entry");
        assert_eq!((entry.needs.as_slice(), entry.call_role), (["prep".to_string()].as_slice(), Some(CallRole::Entry)));
        // Called jobs wait for it, and keep their own order.
        assert_eq!(job(&out, "build__test").needs, ["build__entry"]);
        assert_eq!(job(&out, "build__pack").needs, ["build__test", "build__entry"]);
        assert_eq!(job(&out, "build__pack").name.as_deref(), Some("build / pack"));
        assert_eq!(job(&out, "build__test").condition.as_deref(), Some("needs.build__entry.result == 'success'"));
        // Inputs, secrets and `needs` names are pasted in; a secret that was not passed is empty.
        let StepKind::Run { run, .. } = &job(&out, "build__test").steps[0].kind else { panic!() };
        assert_eq!(run, "echo ${{ '1.2' }} ${{ 2 }}");
        let pack = job(&out, "build__pack");
        assert_eq!(pack.steps[0].env["T"], "${{ (secrets.DEPLOY_TOKEN) }}");
        assert_eq!(pack.steps[0].env["OTHER"], "${{ '' }}");
        assert_eq!(pack.env["FROM_CALLEE"], "yes");
        let ship = job(&out, "build__ship");
        assert_eq!(
            ship.condition.as_deref(),
            Some("needs.build__entry.result == 'success' && (false && needs.build__pack.result == 'success')")
        );
        let StepKind::Run { run, .. } = &ship.steps[0].kind else { panic!() };
        assert_eq!(run, "echo ship ${{ needs.build__pack.outputs.name }}");
        // The exit job waits for everything, and its outputs read the called jobs.
        let exit = job(&out, "build");
        assert_eq!(exit.call_role, Some(CallRole::Exit));
        assert_eq!(exit.needs, ["build__entry", "build__test", "build__pack", "build__ship"]);
        assert_eq!(exit.outputs["artifact"], "${{ needs.build__pack.outputs.name }}");
        assert_eq!(exit.condition.as_deref(), Some("always()"));
        // Nothing is left that asks for a runner it does not need.
        assert!(out.jobs.iter().all(|j| j.call.is_none()));
        assert_eq!(job(&out, "after").needs, ["build"]);
    }

    #[test]
    fn what_a_call_does_not_pass_a_called_job_does_not_see() {
        let mut t = tree();
        // `inherit` passes every secret under its own name.
        let out = expand(&caller("    with:\n      version: '1'\n    secrets: inherit"), &t).unwrap();
        assert_eq!(job(&out, "build__pack").steps[0].env["OTHER"], "${{ secrets.unrelated }}");
        // Without it a secret the call did not pass is an empty string, whatever its name.
        let out = expand(&caller("    with:\n      version: '1'\n    secrets:\n      token: x"), &t).unwrap();
        assert_eq!(job(&out, "build__pack").steps[0].env["OTHER"], "${{ '' }}");
        // A called job that reads the whole secrets context is refused.
        let leaky = CALLEE.replace("OTHER: ${{ secrets.unrelated }}", "OTHER: ${{ toJSON(secrets) }}");
        t.insert("./.codehaven/workflows/build.yml".into(), Resolved { workflow: wf(&leaky), calls: BTreeMap::new() });
        let e = expand(&caller("    with:\n      version: '1'\n    secrets:\n      token: x"), &t).unwrap_err();
        assert!(e[0].message.contains("whole `secrets` context"), "{e:?}");
    }

    #[test]
    fn a_bad_call_is_refused_with_the_line_of_the_call() {
        let t = tree();
        for (with, want) in [
            ("    secrets:\n      token: x", "needs the input `version`"),
            ("    with:\n      version: '1'\n      nope: 1", "is not an input"),
            ("    with:\n      version: '1'", "needs the secret `token`"),
            ("    with:\n      version: '1'\n      deploy: maybe\n    secrets: inherit", "expected true or false"),
            ("    with:\n      version: ${{ needs.prep.outputs.v }}\n    secrets: inherit", "cannot use here"),
            ("    with:\n      version: ${{ secrets.X }}\n    secrets: inherit", "cannot use here"),
            ("    with:\n      version: '1'\n    secrets:\n      token: x\n      extra: y", "is not a secret"),
        ] {
            let e = expand(&caller(with), &t).unwrap_err();
            assert!(e.iter().any(|d| d.message.contains(want) && d.message.contains("job `build`")), "{with:?} gave {e:?}");
            assert!(e.iter().all(|d| d.line > 0), "{e:?}");
        }
        // A workflow that is not callable, and one that is missing.
        let mut plain = BTreeMap::new();
        plain.insert(
            "./.codehaven/workflows/build.yml".to_string(),
            Resolved { workflow: wf("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo\n"), calls: BTreeMap::new() },
        );
        let e = expand(&caller("    secrets: inherit"), &plain).unwrap_err();
        assert!(e[0].message.contains("no `on: workflow_call`"), "{e:?}");
        let e = expand(&caller("    secrets: inherit"), &BTreeMap::new()).unwrap_err();
        assert!(e[0].message.contains("was not found"), "{e:?}");
    }

    #[test]
    fn the_called_workflow_can_only_narrow_the_token() {
        let src = CALLEE.replace("env:\n  FROM_CALLEE: yes\n", "permissions:\n  contents: write\n  issues: write\n");
        let t = BTreeMap::from([("./.codehaven/workflows/build.yml".to_string(), Resolved { workflow: wf(&src), calls: BTreeMap::new() })]);
        let c =
            caller("    with:\n      version: '1'\n    secrets: inherit\n    permissions:\n      contents: read\n      statuses: write");
        let out = expand(&c, &t).unwrap();
        let eff = |k: &str| effective(None, job(&out, k).permissions.as_ref(), Ceiling::Write, false);
        use crate::permissions::{Level, TokenPermissions};
        // It asked for contents write and issues write; the caller gave contents read and statuses.
        assert_eq!(eff("build__test"), TokenPermissions { contents: Level::Read, ..Default::default() });
    }

    #[test]
    fn calls_nest_and_a_name_that_is_taken_is_refused() {
        // outer calls middle, which calls the leaf.
        let leaf = "on:\n  workflow_call:\n    inputs:\n      v: {type: string}\n    outputs:\n      o:\n        value: ${{ jobs.j.outputs.r }}\njobs:\n  j:\n    runs-on: x\n    outputs:\n      r: ${{ steps.s.outputs.r }}\n    steps:\n      - id: s\n        run: echo ${{ inputs.v }}\n";
        let middle = "on:\n  workflow_call:\n    inputs:\n      m: {type: string}\n    outputs:\n      o:\n        value: ${{ jobs.c.outputs.o }}\njobs:\n  c:\n    uses: ./.codehaven/workflows/leaf.yml\n    with:\n      v: pre-${{ inputs.m }}\n";
        let top = wf("on: push\njobs:\n  a:\n    uses: ./.codehaven/workflows/middle.yml\n    with:\n      m: ${{ github.ref }}\n");
        let tree = BTreeMap::from([(
            "./.codehaven/workflows/middle.yml".to_string(),
            Resolved {
                workflow: wf(middle),
                calls: BTreeMap::from([(
                    "./.codehaven/workflows/leaf.yml".to_string(),
                    Resolved { workflow: wf(leaf), calls: BTreeMap::new() },
                )]),
            },
        )]);
        let out = expand(&top, &tree).unwrap();
        let keys: Vec<&str> = out.jobs.iter().map(|j| j.key.as_str()).collect();
        assert_eq!(keys, ["a__entry", "a__c__entry", "a__c__j", "a__c", "a"]);
        let StepKind::Run { run, .. } = &job(&out, "a__c__j").steps[0].kind else { panic!() };
        assert_eq!(run, "echo ${{ format('pre-{0}', (github.ref)) }}");
        assert_eq!(job(&out, "a").outputs["o"], "${{ needs.a__c.outputs.o }}");
        assert_eq!(job(&out, "a__c__j").name.as_deref(), Some("a / c / j"));

        // A job of the caller already has the name a called job would take.
        let clash = wf(
            "on: push\njobs:\n  a:\n    uses: ./.codehaven/workflows/middle.yml\n    with:\n      m: x\n  a__entry:\n    runs-on: x\n    steps:\n      - run: echo\n",
        );
        let e = expand(&clash, &tree).unwrap_err();
        assert!(e.iter().any(|d| d.message.contains("already named")), "{e:?}");
    }

    #[test]
    fn calls_nest_only_so_deep() {
        let leaf = "on:\n  workflow_call:\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: echo\n";
        let mut node = Resolved { workflow: wf(leaf), calls: BTreeMap::new() };
        for _ in 0..MAX_DEPTH + 1 {
            let w = wf("on:\n  workflow_call:\njobs:\n  c:\n    uses: ./.codehaven/workflows/n.yml\n");
            node = Resolved { workflow: w, calls: BTreeMap::from([("./.codehaven/workflows/n.yml".to_string(), node)]) };
        }
        let top = wf("on: push\njobs:\n  a:\n    uses: ./.codehaven/workflows/n.yml\n");
        let e = expand(&top, &BTreeMap::from([("./.codehaven/workflows/n.yml".to_string(), node)])).unwrap_err();
        assert!(e.iter().any(|d| d.message.contains("nested")), "{e:?}");
    }

    /// `levels` files in a chain: the first has `fan` jobs that each call the next, and so on; the
    /// last has one plain job. Returns the top workflow that calls the first, and the tree.
    fn chain(fan: usize, levels: usize, with: &str) -> (Workflow, BTreeMap<String, Resolved>) {
        let name = |i: usize| format!("./.codehaven/workflows/l{i}.yml");
        let mut node = Resolved {
            workflow: wf(
                "on:\n  workflow_call:\n    inputs:\n      x: {type: string}\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: echo ${{ inputs.x }}\n",
            ),
            calls: BTreeMap::new(),
        };
        for i in (1..levels).rev() {
            let mut src = String::from("on:\n  workflow_call:\n    inputs:\n      x: {type: string}\njobs:\n");
            for n in 0..fan {
                src.push_str(&format!("  c{n}:\n    uses: {}\n    with:\n      x: {with}\n", name(i + 1)));
            }
            node = Resolved { workflow: wf(&src), calls: BTreeMap::from([(name(i + 1), node)]) };
        }
        let top = wf(&format!("on: push\njobs:\n  a:\n    uses: {}\n    with:\n      x: go\n", name(1)));
        (top, BTreeMap::from([(name(1), node)]))
    }

    #[test]
    fn a_fan_out_is_refused_before_it_is_built() {
        // 30 calls at each of three levels is about 1.4 million jobs. The count is made first, so
        // this answers at once and builds nothing.
        let (top, tree) = chain(30, 4, "x");
        let e = expand(&top, &tree).unwrap_err();
        assert!(e.iter().any(|d| d.message.contains("more than 200 jobs")), "{e:?}");
        // Small enough is still fine.
        let (top, tree) = chain(3, 3, "x");
        assert!(expand(&top, &tree).is_ok());
    }

    #[test]
    fn a_value_that_repeats_an_input_cannot_grow_without_end() {
        // Each level pastes the input forty times, so three levels would make 64,000 copies.
        let with = "${{ inputs.x }}".repeat(40);
        let (top, tree) = chain(1, 4, &format!("'{with}'"));
        let e = expand(&top, &tree).unwrap_err();
        assert!(e.iter().any(|d| d.message.contains("too long")), "{e:?}");
    }

    #[test]
    fn what_the_called_workflows_paste_in_has_a_total_budget() {
        // 12 calls of a workflow with 100 steps that each take a 10 KB input: 12 MB in all.
        let steps = "      - run: echo ${{ inputs.v }}\n".repeat(100);
        let callee =
            format!("on:\n  workflow_call:\n    inputs:\n      v: {{type: string}}\njobs:\n  j:\n    runs-on: x\n    steps:\n{steps}");
        let mut top = String::from("on: push\njobs:\n");
        for n in 0..12 {
            top.push_str(&format!("  c{n}:\n    uses: ./.codehaven/workflows/big.yml\n    with:\n      v: {}\n", "a".repeat(10_000)));
        }
        let tree =
            BTreeMap::from([("./.codehaven/workflows/big.yml".to_string(), Resolved { workflow: wf(&callee), calls: BTreeMap::new() })]);
        let e = expand(&wf(&top), &tree).unwrap_err();
        assert!(e.iter().any(|d| d.message.contains("too large")), "{e:?}");
    }

    #[test]
    fn called_jobs_are_marked_and_the_callers_are_not() {
        let out = expand(&caller("    with:\n      version: '1'\n    secrets: inherit"), &tree()).unwrap();
        for k in ["build__test", "build__pack", "build__ship"] {
            assert!(job(&out, k).called, "{k}");
        }
        for k in ["prep", "after", "build__entry", "build"] {
            assert!(!job(&out, k).called, "{k}");
        }
    }

    #[test]
    fn a_workflow_with_no_calls_is_returned_as_it_is() {
        let w = wf("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo\n");
        assert_eq!(expand(&w, &BTreeMap::new()).unwrap(), w);
    }
}
