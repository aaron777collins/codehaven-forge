//! `haven issue`. This module is the pattern the other command groups follow: parse flags
//! (`cli.rs`), call the REST client with a `ch-api-types` DTO, print a table or `--json`.

use super::body_from;
use crate::cli::{IssueCloseArgs, IssueCmd, IssueCommentArgs, IssueCreateArgs, IssueListArgs, IssueViewArgs};
use crate::{Ctx, fmt};
use anyhow::{Result, bail};
use ch_api_types::issues::{CreateComment, CreateIssue, Issue, IssueComment};
use chrono::Utc;

pub async fn run(ctx: &Ctx, cmd: IssueCmd) -> Result<i32> {
    match cmd {
        IssueCmd::List(a) => list(ctx, a).await,
        IssueCmd::View(a) => view(ctx, a).await,
        IssueCmd::Create(a) => create(ctx, a).await,
        IssueCmd::Close(a) => close(ctx, a).await,
        IssueCmd::Comment(a) => comment(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, a: IssueListArgs) -> Result<i32> {
    if !["open", "closed", "all"].contains(&a.state.as_str()) {
        bail!("--state must be open, closed or all");
    }
    let (owner, repo) = ctx.repo()?;
    let mut query = vec![("state", a.state.clone())];
    if !a.label.is_empty() {
        query.push(("labels", a.label.join(",")));
    }
    if let Some(who) = &a.assignee {
        query.push(("assignee", who.clone()));
    }
    // The issues endpoint also returns pull requests. Ask for more than `limit` and drop them.
    let fetch = if a.limit == 0 { 0 } else { a.limit.saturating_mul(2).max(30) };
    let items: Vec<Issue> = ctx.api()?.get_paginated(&format!("/repos/{owner}/{repo}/issues"), &query, fetch).await?;
    let mut issues: Vec<Issue> = items.into_iter().filter(|i| i.pull_request.is_none()).collect();
    if a.limit != 0 {
        issues.truncate(a.limit);
    }
    if a.json {
        ctx.print_json(&issues)?;
        return Ok(0);
    }
    if issues.is_empty() {
        ctx.println(format!("No {} issues in {owner}/{repo}.", if a.state == "all" { "" } else { a.state.as_str() }).replace("  ", " "));
        return Ok(0);
    }
    let now = Utc::now();
    let rows: Vec<Vec<String>> = issues
        .iter()
        .map(|i| {
            vec![
                format!("#{}", i.number),
                fmt::truncate(&i.title, 60),
                i.labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", "),
                fmt::ago(i.updated_at, now),
            ]
        })
        .collect();
    ctx.print(fmt::table(&rows));
    Ok(0)
}

async fn view(ctx: &Ctx, a: IssueViewArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    let issue: Issue = api.get(&format!("/repos/{owner}/{repo}/issues/{}", a.number)).await?;
    let comments: Vec<IssueComment> = if a.comments || a.json {
        api.get_paginated(&format!("/repos/{owner}/{repo}/issues/{}/comments", a.number), &[], 0).await?
    } else {
        vec![]
    };
    if a.json {
        ctx.print_json(&serde_json::json!({ "issue": issue, "comments": comments }))?;
        return Ok(0);
    }
    let now = Utc::now();
    ctx.println(format!("#{} {}", issue.number, issue.title));
    let mut meta = format!("{} by {}, {}", issue.state, issue.user.login, fmt::ago(issue.created_at, now));
    if let Some(r) = &issue.state_reason {
        meta.push_str(&format!(" ({r})"));
    }
    ctx.println(meta);
    if !issue.labels.is_empty() {
        ctx.println(format!("Labels: {}", issue.labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !issue.assignees.is_empty() {
        ctx.println(format!("Assignees: {}", issue.assignees.iter().map(|u| u.login.as_str()).collect::<Vec<_>>().join(", ")));
    }
    ctx.println("");
    ctx.println(issue.body.as_deref().filter(|b| !b.trim().is_empty()).unwrap_or("No description."));
    if a.comments {
        for c in &comments {
            ctx.println("");
            ctx.println(format!("{}, {}:", c.user.login, fmt::ago(c.created_at, now)));
            ctx.println(&c.body);
        }
    } else if issue.comments > 0 {
        ctx.println("");
        ctx.println(format!("{} comment(s). Add --comments to read them.", issue.comments));
    }
    ctx.println("");
    ctx.println(&issue.html_url);
    Ok(0)
}

async fn create(ctx: &Ctx, a: IssueCreateArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let body = body_from(ctx, Some(a.body.as_str()), a.body_file.as_deref())?.filter(|b| !b.is_empty());
    let req = CreateIssue { title: a.title, body, assignees: a.assignee, labels: a.label, milestone: None };
    let issue: Issue = ctx.api()?.post(&format!("/repos/{owner}/{repo}/issues"), &req).await?;
    if a.json {
        ctx.print_json(&issue)?;
    } else {
        ctx.println(&issue.html_url);
    }
    Ok(0)
}

async fn close(ctx: &Ctx, a: IssueCloseArgs) -> Result<i32> {
    if !["completed", "not_planned"].contains(&a.reason.as_str()) {
        bail!("--reason must be completed or not_planned");
    }
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    if let Some(c) = a.comment.filter(|c| !c.trim().is_empty()) {
        let _: IssueComment = api.post(&format!("/repos/{owner}/{repo}/issues/{}/comments", a.number), &CreateComment { body: c }).await?;
    }
    // A hand-built body, not `UpdateIssue`: that type always serializes `labels` and
    // `assignees`, and an empty list there would clear them.
    let body = serde_json::json!({ "state": "closed", "state_reason": a.reason });
    let issue: Issue = api.patch(&format!("/repos/{owner}/{repo}/issues/{}", a.number), &body).await?;
    ctx.println(format!("Closed #{} {}", issue.number, issue.title));
    Ok(0)
}

async fn comment(ctx: &Ctx, a: IssueCommentArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let Some(body) = body_from(ctx, a.body.as_deref(), a.body_file.as_deref())?.filter(|b| !b.trim().is_empty()) else {
        bail!("the comment is empty. Use --body or --body-file.");
    };
    let c: IssueComment = ctx.api()?.post(&format!("/repos/{owner}/{repo}/issues/{}/comments", a.number), &CreateComment { body }).await?;
    ctx.println(&c.html_url);
    Ok(0)
}
