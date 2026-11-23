//! `haven pr`: create, list, view, checkout, diff, review and merge.

use super::{body_from, git, git_output};
use crate::cli::{PrCheckoutArgs, PrCmd, PrCreateArgs, PrListArgs, PrMergeArgs, PrNumberArgs, PrReviewArgs, PrViewArgs};
use crate::{Ctx, fmt};
use anyhow::{Result, bail};
use ch_api_types::ci::CheckRunsResponse;
use ch_api_types::pulls::{CreatePull, CreateReview, MergePull, MergeResult, PullFile, PullRequest, Review};
use ch_api_types::repos::Repository;
use chrono::Utc;

pub async fn run(ctx: &Ctx, cmd: PrCmd) -> Result<i32> {
    match cmd {
        PrCmd::Create(a) => create(ctx, a).await,
        PrCmd::List(a) => list(ctx, a).await,
        PrCmd::View(a) => view(ctx, a).await,
        PrCmd::Checkout(a) => checkout(ctx, a).await,
        PrCmd::Diff(a) => diff(ctx, a).await,
        PrCmd::Review(a) => review(ctx, a).await,
        PrCmd::Merge(a) => merge(ctx, a).await,
    }
}

fn state_word(p: &PullRequest) -> &'static str {
    if p.merged {
        "merged"
    } else if p.draft && p.state == "open" {
        "draft"
    } else if p.state == "open" {
        "open"
    } else {
        "closed"
    }
}

async fn create(ctx: &Ctx, a: PrCreateArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    let head = match a.head.clone() {
        Some(h) => h,
        None => match git_output(ctx, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            Some(b) if b != "HEAD" && !b.is_empty() => b,
            _ => bail!("cannot tell which branch has your changes. Check one out, or pass --head."),
        },
    };
    let base = match a.base.clone() {
        Some(b) => b,
        None => api.get::<Repository>(&format!("/repos/{owner}/{repo}")).await?.default_branch,
    };
    let body = body_from(ctx, Some(a.body.as_str()), a.body_file.as_deref())?.filter(|b| !b.is_empty());
    let req = CreatePull {
        title: a.title,
        head,
        base,
        body,
        draft: if a.draft { Some(true) } else { None },
        head_repo: None,
        maintainer_can_modify: None,
    };
    let pr: PullRequest = api.post(&format!("/repos/{owner}/{repo}/pulls"), &req).await?;
    if a.json {
        ctx.print_json(&pr)?;
    } else {
        ctx.println(&pr.html_url);
    }
    Ok(0)
}

async fn list(ctx: &Ctx, a: PrListArgs) -> Result<i32> {
    if !["open", "closed", "merged", "all"].contains(&a.state.as_str()) {
        bail!("--state must be open, closed, merged or all");
    }
    let (owner, repo) = ctx.repo()?;
    // "merged" is not a state the server knows: it is closed pull requests that were merged.
    let server_state = if a.state == "merged" { "closed" } else { a.state.as_str() };
    let mut query = vec![("state", server_state.to_string())];
    if let Some(b) = &a.base {
        query.push(("base", b.clone()));
    }
    if let Some(h) = &a.head {
        query.push(("head", h.clone()));
    }
    // Filtering for merged happens here, so fetch every page and cut afterwards.
    let fetch = if a.state == "merged" { 0 } else { a.limit };
    let mut prs: Vec<PullRequest> = ctx.api()?.get_paginated(&format!("/repos/{owner}/{repo}/pulls"), &query, fetch).await?;
    if a.state == "merged" {
        prs.retain(|p| p.merged);
    }
    if a.limit != 0 {
        prs.truncate(a.limit);
    }
    if a.json {
        ctx.print_json(&prs)?;
        return Ok(0);
    }
    if prs.is_empty() {
        ctx.println(format!("No {} pull requests in {owner}/{repo}.", a.state));
        return Ok(0);
    }
    let now = Utc::now();
    let rows: Vec<Vec<String>> = prs
        .iter()
        .map(|p| {
            vec![
                format!("#{}", p.number),
                fmt::truncate(&p.title, 60),
                p.head.git_ref.clone(),
                state_word(p).to_string(),
                fmt::ago(p.updated_at, now),
            ]
        })
        .collect();
    ctx.print(fmt::table(&rows));
    Ok(0)
}

async fn view(ctx: &Ctx, a: PrViewArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    let pr: PullRequest = api.get(&format!("/repos/{owner}/{repo}/pulls/{}", a.number)).await?;
    let wants_more = a.comments || a.json;
    let reviews: Vec<Review> =
        if wants_more { api.get_paginated(&format!("/repos/{owner}/{repo}/pulls/{}/reviews", a.number), &[], 0).await? } else { vec![] };
    // Checks are a nicety: a server without them must not break the view.
    let checks: Option<CheckRunsResponse> =
        if wants_more { api.get(&format!("/repos/{owner}/{repo}/commits/{}/check-runs", pr.head.sha)).await.ok() } else { None };
    if a.json {
        ctx.print_json(&serde_json::json!({ "pull_request": pr, "reviews": reviews, "check_runs": checks }))?;
        return Ok(0);
    }
    let now = Utc::now();
    ctx.println(format!("#{} {}", pr.number, pr.title));
    ctx.println(format!("{}, {} -> {}", state_word(&pr), pr.head.label, pr.base.label));
    ctx.println(format!("Opened by {}, {}", pr.user.login, fmt::ago(pr.created_at, now)));
    ctx.println("");
    ctx.println(pr.body.as_deref().filter(|b| !b.trim().is_empty()).unwrap_or("No description."));
    ctx.println("");
    ctx.println(format!("{} commit(s), +{} -{}, {} file(s) changed", pr.commits, pr.additions, pr.deletions, pr.changed_files));
    if !pr.merged && pr.state == "open" {
        ctx.println(format!("Mergeable: {}", pr.mergeable_state));
    }
    if a.comments {
        for r in &reviews {
            ctx.println("");
            ctx.println(format!("{}: {}", r.user.login, r.state));
            if let Some(b) = r.body.as_deref().filter(|b| !b.trim().is_empty()) {
                ctx.println(b);
            }
        }
        if let Some(c) = &checks
            && !c.check_runs.is_empty()
        {
            ctx.println("");
            ctx.println("Checks:");
            for run in &c.check_runs {
                ctx.println(format!("  {}: {}", run.name, fmt::state_word(Some(&run.status), run.conclusion.as_deref())));
            }
        }
    }
    ctx.println("");
    ctx.println(&pr.html_url);
    Ok(0)
}

async fn checkout(ctx: &Ctx, a: PrCheckoutArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let pr: PullRequest = ctx.api()?.get(&format!("/repos/{owner}/{repo}/pulls/{}", a.number)).await?;
    let branch = a.branch.clone().unwrap_or_else(|| pr.head.git_ref.clone());
    let same_repo = match (&pr.head.repo, &pr.base.repo) {
        (Some(h), Some(b)) => h.full_name.eq_ignore_ascii_case(&b.full_name),
        (None, _) => true,
        _ => false,
    };
    if same_repo {
        git(ctx, &["fetch", "origin", &pr.head.git_ref])?;
    } else {
        let Some(h) = &pr.head.repo else { bail!("the pull request's source repository is gone") };
        git(ctx, &["fetch", &h.clone_url, &pr.head.git_ref])?;
    }
    git(ctx, &["checkout", "-B", &branch, "FETCH_HEAD"])?;
    ctx.println(format!("On branch {branch}, at pull request #{}.", pr.number));
    Ok(0)
}

fn diff_header(f: &PullFile) -> Vec<String> {
    let old = f.previous_filename.as_deref().unwrap_or(&f.filename);
    let (from, to) = match f.status.as_str() {
        "added" => ("/dev/null".to_string(), format!("b/{}", f.filename)),
        "removed" => (format!("a/{old}"), "/dev/null".to_string()),
        _ => (format!("a/{old}"), format!("b/{}", f.filename)),
    };
    vec![format!("diff --git a/{old} b/{}", f.filename), format!("--- {from}"), format!("+++ {to}")]
}

async fn diff(ctx: &Ctx, a: PrNumberArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let files: Vec<PullFile> = ctx.api()?.get_paginated(&format!("/repos/{owner}/{repo}/pulls/{}/files", a.number), &[], 0).await?;
    for f in &files {
        for l in diff_header(f) {
            ctx.println(l);
        }
        match f.patch.as_deref() {
            Some(p) if !p.is_empty() => ctx.println(p.trim_end_matches('\n')),
            _ => ctx.println("Binary or large file, no patch."),
        }
    }
    Ok(0)
}

async fn review(ctx: &Ctx, a: PrReviewArgs) -> Result<i32> {
    let (event, done) = match (a.approve, a.request_changes, a.comment) {
        (true, false, false) => ("APPROVE", "Approved"),
        (false, true, false) => ("REQUEST_CHANGES", "Requested changes on"),
        (false, false, true) => ("COMMENT", "Reviewed"),
        _ => bail!("choose one of --approve, --request-changes or --comment"),
    };
    let body = a.body.clone().filter(|b| !b.trim().is_empty());
    if event != "APPROVE" && body.is_none() {
        bail!("a review that {} needs a --body", if event == "COMMENT" { "comments" } else { "requests changes" });
    }
    let (owner, repo) = ctx.repo()?;
    let req = CreateReview { commit_id: None, body, event: Some(event.to_string()), comments: vec![] };
    let _: Review = ctx.api()?.post(&format!("/repos/{owner}/{repo}/pulls/{}/reviews", a.number), &req).await?;
    ctx.println(format!("{done} #{}", a.number));
    Ok(0)
}

async fn merge(ctx: &Ctx, a: PrMergeArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    if a.disable_auto {
        let _: PullRequest = ctx.api()?.delete_json(&format!("/repos/{owner}/{repo}/pulls/{}/auto-merge", a.number)).await?;
        ctx.println(format!("Auto-merge is off for #{}", a.number));
        return Ok(0);
    }
    let method = match (a.squash, a.rebase, a.merge) {
        (true, false, false) => "squash",
        (false, true, false) => "rebase",
        (false, false, _) => "merge",
        _ => bail!("choose one of --squash, --rebase or --merge"),
    };
    if a.auto {
        if a.subject.is_some() || a.body.is_some() {
            bail!("--subject and --body apply to merging now, not to --auto");
        }
        let p: PullRequest = ctx
            .api()?
            .put(&format!("/repos/{owner}/{repo}/pulls/{}/auto-merge", a.number), &serde_json::json!({ "merge_method": method }))
            .await?;
        let applied = p.auto_merge.as_ref().map(|m| m.merge_method.as_str()).unwrap_or(method);
        ctx.println(format!("Auto-merge is on for #{} ({applied}): it merges once the required checks and reviews pass.", a.number));
        return Ok(0);
    }
    let req =
        MergePull { merge_method: Some(method.to_string()), commit_title: a.subject.clone(), commit_message: a.body.clone(), sha: None };
    let r: MergeResult = ctx.api()?.put(&format!("/repos/{owner}/{repo}/pulls/{}/merge", a.number), &req).await?;
    if !r.merged {
        ctx.println(format!("Not merged: {}", r.message));
        return Ok(1);
    }
    ctx.println(format!("Merged #{} ({method}), {}", a.number, r.sha.chars().take(8).collect::<String>()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(status: &str, name: &str, prev: Option<&str>) -> PullFile {
        PullFile {
            sha: "s".into(),
            filename: name.into(),
            status: status.into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: None,
            previous_filename: prev.map(String::from),
            blob_url: String::new(),
            raw_url: String::new(),
        }
    }

    #[test]
    fn diff_headers() {
        assert_eq!(diff_header(&file("modified", "a.rs", None)), ["diff --git a/a.rs b/a.rs", "--- a/a.rs", "+++ b/a.rs"]);
        assert_eq!(diff_header(&file("added", "n.rs", None)), ["diff --git a/n.rs b/n.rs", "--- /dev/null", "+++ b/n.rs"]);
        assert_eq!(diff_header(&file("removed", "o.rs", None)), ["diff --git a/o.rs b/o.rs", "--- a/o.rs", "+++ /dev/null"]);
        assert_eq!(
            diff_header(&file("renamed", "new.rs", Some("old.rs"))),
            ["diff --git a/old.rs b/new.rs", "--- a/old.rs", "+++ b/new.rs"]
        );
    }
}
