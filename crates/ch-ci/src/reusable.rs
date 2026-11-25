//! Finds the reusable workflows a run's workflow calls (ch-ctk.7), then inlines them
//! (`ch_workflow::reusable::expand`).
//!
//! A file in this repository is read at the run's own commit. A file in another repository is
//! used only when the run may use that repository at all, which is the rule for `haven_` actions
//! (`action_fetch::may_use_repo`): the caller's own, a public one, or one with the same owner that
//! is at least as visible and that the person who started the run can read. A pull request from a
//! fork and a scheduled run get public repositories only. A repository that may not be used
//! looks exactly like one that does not exist. Only a branch, a tag or a commit one of them
//! reaches is followed, never a pull request's ref, and the site's rule for `haven_` repositories
//! (`allow_codehaven_repos`) applies. What `./` means inside a called workflow is its own
//! repository and commit.

use crate::action_fetch::{Who, commit_of_ref, load_policy, may_use_repo};
use crate::ctx::Ctx;
use ch_core::{Actor, Error, RepoId, Result};
use ch_workflow::Workflow;
use ch_workflow::diag::Diagnostic;
use ch_workflow::reusable::{CallTarget, MAX_CALLED_FILES, MAX_DEPTH, Resolved, calls_of, parse_call_uses};
use std::collections::BTreeMap;

/// Where a workflow file sits: the repository and commit its own `./` calls are read from.
#[derive(Clone, Copy)]
struct Home<'a> {
    repo: RepoId,
    sha: &'a str,
}

/// The inlined workflow, or the diagnostics that say what is wrong with a call. `wf` is read from
/// `repo` at `sha`; a workflow with no calls comes back as it is, with no reads.
pub async fn inline_calls(
    ctx: &Ctx,
    repo: RepoId,
    sha: &str,
    path: &str,
    fork: bool,
    actor: Option<ch_core::UserId>,
    wf: &Workflow,
) -> Result<Workflow> {
    if wf.jobs.iter().all(|j| j.call.is_none()) {
        return Ok(wf.clone());
    }
    let who = Who { fork, actor };
    let mut budget = MAX_CALLED_FILES;
    // The file itself is on the stack, so a workflow that calls itself is caught at once.
    let mut stack: Vec<(i64, String)> = vec![(repo.0, format!("{sha}:{path}"))];
    let tree = resolve_level(ctx, repo, &who, Home { repo, sha }, wf, 0, &mut budget, &mut stack).await.map_err(to_error)?;
    ch_workflow::reusable::expand(wf, &tree).map_err(to_error)
}

fn to_error(d: Vec<Diagnostic>) -> Error {
    Error::Validation(d.iter().map(|d| ch_core::FieldError::custom("Workflow", "uses", d.to_string())).collect())
}

fn one(line: u32, msg: impl Into<String>) -> Vec<Diagnostic> {
    vec![Diagnostic::new(line, 1, msg)]
}

#[allow(clippy::too_many_arguments)]
async fn resolve_level(
    ctx: &Ctx,
    top: RepoId,
    who: &Who,
    home: Home<'_>,
    wf: &Workflow,
    depth: usize,
    budget: &mut usize,
    stack: &mut Vec<(i64, String)>,
) -> std::result::Result<BTreeMap<String, Resolved>, Vec<Diagnostic>> {
    let mut out = BTreeMap::new();
    if depth > MAX_DEPTH {
        // The expansion names the job that is too deep; stop reading here.
        return Ok(out);
    }
    for (uses, line) in calls_of(wf) {
        let target = parse_call_uses(&uses).map_err(|e| one(line, e))?;
        // Where the file is, and the commit its own relative calls read from.
        let (repo, sha, path, label) = match &target {
            CallTarget::Local { path } => (home.repo, home.sha.to_string(), path.clone(), uses.clone()),
            CallTarget::Repo { owner, repo, path, git_ref } => {
                let full = format!("haven_{owner}/{repo}");
                let missing = || one(line, format!("{full} was not found on this instance."));
                let policy = load_policy(ctx).await.map_err(|e| one(line, e.to_string()))?;
                if !policy.allow_codehaven_repos {
                    return Err(one(line, "this site does not allow workflows or actions from repositories on this instance".to_string()));
                }
                let target_repo = match ctx.repos.get_by_name(&Actor::System, owner, repo).await {
                    Ok(r) => r,
                    Err(Error::NotFound) => return Err(missing()),
                    Err(e) => return Err(one(line, e.to_string())),
                };
                let caller = ctx.repos.get(&Actor::System, top).await.map_err(|e| one(line, e.to_string()))?;
                if !may_use_repo(ctx, &caller, who, &target_repo).await.map_err(|e| one(line, e.to_string()))? {
                    return Err(missing());
                }
                let sha = match commit_of_ref(ctx, target_repo.id, &full, git_ref).await {
                    Ok(s) => s,
                    Err(Error::Validation(v)) => {
                        return Err(one(
                            line,
                            v.first().and_then(|f| f.message.clone()).unwrap_or_else(|| format!("{full}@{git_ref} was not found")),
                        ));
                    }
                    Err(Error::Forbidden(m)) => return Err(one(line, m)),
                    Err(e) => return Err(one(line, e.to_string())),
                };
                (target_repo.id, sha, path.clone(), uses.clone())
            }
        };
        if *budget == 0 {
            return Err(one(line, format!("a run may call at most {MAX_CALLED_FILES} reusable workflows")));
        }
        let at = (repo.0, format!("{sha}:{path}"));
        if stack.contains(&at) {
            return Err(one(line, format!("{label} calls itself, directly or through the workflows it calls")));
        }
        *budget -= 1;
        let text = match ctx.source.read(repo, &sha, &path, ch_workflow::parse::MAX_FILE_BYTES as u64).await {
            Ok(Some(t)) => t,
            Ok(None) => return Err(one(line, format!("{label} was not found"))),
            Err(Error::Validation(v)) => {
                return Err(one(line, format!("{label}: {}", v.first().and_then(|f| f.message.clone()).unwrap_or_default())));
            }
            Err(e) => return Err(one(line, e.to_string())),
        };
        let workflow = ch_workflow::parse::parse(&text).map_err(|d| {
            one(line, format!("{label} is not a valid workflow: {}", d.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")))
        })?;
        stack.push(at);
        let calls = Box::pin(resolve_level(ctx, top, who, Home { repo, sha: &sha }, &workflow, depth + 1, budget, stack)).await;
        stack.pop();
        out.insert(uses, Resolved { workflow, calls: calls? });
    }
    Ok(out)
}
