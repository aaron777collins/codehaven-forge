//! `haven workflow`: list and start workflows.

use crate::cli::{WorkflowCmd, WorkflowListArgs, WorkflowRunArgs};
use crate::{Ctx, fmt};
use anyhow::{Result, anyhow};
use ch_api_types::ci::{DispatchRequest, Workflow, WorkflowsResponse};
use ch_api_types::repos::Repository;

pub async fn run(ctx: &Ctx, cmd: WorkflowCmd) -> Result<i32> {
    match cmd {
        WorkflowCmd::List(a) => list(ctx, a).await,
        WorkflowCmd::Run(a) => start(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, a: WorkflowListArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let ws: Vec<Workflow> = ctx
        .api()?
        .get_paginated_wrapped(&format!("/repos/{owner}/{repo}/actions/workflows"), &[], 0, |p: WorkflowsResponse| p.workflows)
        .await?;
    if a.json {
        ctx.print_json(&ws)?;
        return Ok(0);
    }
    if ws.is_empty() {
        ctx.println("No workflows. Add a file under .codehaven/workflows.");
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = ws.iter().map(|w| vec![w.id.to_string(), w.name.clone(), w.path.clone(), w.state.clone()]).collect();
    ctx.print(fmt::table(&rows));
    Ok(0)
}

/// `name=value` pairs into inputs. A repeated name keeps the last value.
fn parse_inputs(fields: &[String]) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut m = serde_json::Map::new();
    for f in fields {
        let (k, v) = f.split_once('=').ok_or_else(|| anyhow!("input {f:?} must be name=value"))?;
        if k.is_empty() {
            return Err(anyhow!("input {f:?} has no name"));
        }
        m.insert(k.to_string(), serde_json::Value::String(v.to_string()));
    }
    Ok(m)
}

async fn start(ctx: &Ctx, a: WorkflowRunArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    let inputs = parse_inputs(&a.field)?;
    let git_ref = match &a.r#ref {
        Some(r) => r.clone(),
        None => api.get::<Repository>(&format!("/repos/{owner}/{repo}")).await?.default_branch,
    };
    let req = DispatchRequest { git_ref: git_ref.clone(), inputs: if inputs.is_empty() { None } else { Some(inputs) } };
    api.post_empty(&format!("/repos/{owner}/{repo}/actions/workflows/{}/dispatches", a.workflow), &req).await?;
    ctx.println(format!("Started workflow {} on {git_ref}.", a.workflow));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_parse() {
        let m = parse_inputs(&["a=1".into(), "b=x=y".into(), "a=2".into()]).unwrap();
        assert_eq!(m["a"], "2");
        assert_eq!(m["b"], "x=y");
        assert!(parse_inputs(&["novalue".into()]).is_err());
        assert!(parse_inputs(&["=v".into()]).is_err());
        assert!(parse_inputs(&[]).unwrap().is_empty());
    }
}
