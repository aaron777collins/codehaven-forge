//! `haven repo`: create, clone, view, fork and list.

use crate::api::HttpError;
use crate::cli::{RepoCloneArgs, RepoCmd, RepoCreateArgs, RepoForkArgs, RepoListArgs, RepoViewArgs};
use crate::{Ctx, fmt};
use anyhow::{Result, bail};
use ch_api_types::repos::{CreateRepo, ForkRequest, Repository};
use ch_api_types::users::User;
use chrono::Utc;

pub async fn run(ctx: &Ctx, cmd: RepoCmd) -> Result<i32> {
    match cmd {
        RepoCmd::Create(a) => create(ctx, a).await,
        RepoCmd::Clone(a) => clone(ctx, a).await,
        RepoCmd::View(a) => view(ctx, a).await,
        RepoCmd::Fork(a) => fork(ctx, a).await,
        RepoCmd::List(a) => list(ctx, a).await,
    }
}

/// The URL `git clone` should use: ssh when `hosts.toml` says so for this host, https otherwise.
fn clone_url_for(ctx: &Ctx, r: &Repository) -> String {
    let proto = ctx.host().ok().and_then(|h| ctx.hosts().ok().and_then(|hs| hs.hosts.get(&h).and_then(|e| e.git_protocol.clone())));
    if proto.as_deref() == Some("ssh") { r.ssh_url.clone() } else { r.clone_url.clone() }
}

/// `owner/name` or `name`.
fn split_name(s: &str) -> (Option<&str>, &str) {
    match s.split_once('/') {
        Some((o, n)) => (Some(o), n),
        None => (None, s),
    }
}

async fn create(ctx: &Ctx, a: RepoCreateArgs) -> Result<i32> {
    if a.public {
        bail!("Make a repository public in the browser. Create it with --internal or without a flag, then open its settings.");
    }
    let api = ctx.api()?;
    let (owner, name) = split_name(&a.name);
    if name.is_empty() {
        bail!("the repository needs a name");
    }
    let req = CreateRepo {
        name: name.to_string(),
        description: a.description.clone(),
        private: None,
        visibility: Some(if a.internal { "internal" } else { "private" }.to_string()),
        auto_init: Some(a.add_readme),
        default_branch: None,
    };
    let me: User = api.get("/user").await?;
    let repo: Repository = match owner {
        Some(o) if !o.eq_ignore_ascii_case(&me.login) => api.post(&format!("/orgs/{o}/repos"), &req).await?,
        _ => api.post("/user/repos", &req).await?,
    };
    if a.json {
        ctx.print_json(&repo)?;
    } else {
        ctx.println(&repo.html_url);
    }
    if a.clone {
        let url = clone_url_for(ctx, &repo);
        super::git(ctx, &["clone", &url])?;
    }
    Ok(0)
}

async fn clone(ctx: &Ctx, a: RepoCloneArgs) -> Result<i32> {
    let (owner, name) = crate::repoctx::parse_owner_name(&a.repo)?;
    let repo: Repository = ctx.api()?.get(&format!("/repos/{owner}/{name}")).await?;
    let url = clone_url_for(ctx, &repo);
    let mut args: Vec<&str> = vec!["clone", &url];
    if let Some(d) = &a.directory {
        args.push(d);
    }
    for g in &a.git_args {
        args.push(g);
    }
    super::git(ctx, &args)?;
    Ok(0)
}

async fn view(ctx: &Ctx, a: RepoViewArgs) -> Result<i32> {
    let (owner, name) = ctx.repo_or(a.repo.as_deref())?;
    let r: Repository = ctx.api()?.get(&format!("/repos/{owner}/{name}")).await?;
    if a.json {
        ctx.print_json(&r)?;
        return Ok(0);
    }
    let now = Utc::now();
    ctx.println(format!("{} ({})", r.full_name, r.visibility));
    if let Some(d) = r.description.as_deref().filter(|d| !d.is_empty()) {
        ctx.println(d);
    }
    ctx.println("");
    ctx.println(format!("Default branch: {}", r.default_branch));
    ctx.println(format!("Clone (https): {}", r.clone_url));
    ctx.println(format!("Clone (ssh):   {}", r.ssh_url));
    if !r.topics.is_empty() {
        ctx.println(format!("Topics: {}", r.topics.join(", ")));
    }
    ctx.println(format!("Stars {}, forks {}, open issues {}", r.stargazers_count, r.forks_count, r.open_issues_count));
    if r.empty {
        ctx.println("This repository has no commits yet.");
    } else if let Some(p) = r.pushed_at {
        ctx.println(format!("Last push {}", fmt::ago(p, now)));
    }
    ctx.println(&r.html_url);
    Ok(0)
}

async fn fork(ctx: &Ctx, a: RepoForkArgs) -> Result<i32> {
    let (owner, name) = ctx.repo_or(a.repo.as_deref())?;
    let req = ForkRequest { organization: a.org.clone(), name: a.name.clone(), private: None, visibility: None };
    let r: Repository = ctx.api()?.post(&format!("/repos/{owner}/{name}/forks"), &req).await?;
    ctx.println(&r.html_url);
    if a.clone {
        let url = clone_url_for(ctx, &r);
        super::git(ctx, &["clone", &url])?;
    }
    Ok(0)
}

async fn list(ctx: &Ctx, a: RepoListArgs) -> Result<i32> {
    let api = ctx.api()?;
    let repos: Vec<Repository> = match &a.owner {
        None => api.get_paginated("/user/repos", &[], a.limit).await?,
        Some(o) => match api.get_paginated::<Repository>(&format!("/users/{o}/repos"), &[], a.limit).await {
            Ok(v) => v,
            Err(e) if e.downcast_ref::<HttpError>().is_some_and(|h| h.status == reqwest::StatusCode::NOT_FOUND) => {
                api.get_paginated(&format!("/orgs/{o}/repos"), &[], a.limit).await?
            }
            Err(e) => return Err(e),
        },
    };
    if a.json {
        ctx.print_json(&repos)?;
        return Ok(0);
    }
    if repos.is_empty() {
        ctx.println("No repositories.");
        return Ok(0);
    }
    let now = Utc::now();
    let rows: Vec<Vec<String>> = repos
        .iter()
        .map(|r| {
            vec![
                r.full_name.clone(),
                r.visibility.clone(),
                fmt::truncate(r.description.as_deref().unwrap_or(""), 50),
                r.pushed_at.map(|p| fmt::ago(p, now)).unwrap_or_default(),
            ]
        })
        .collect();
    ctx.print(fmt::table(&rows));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_split_on_the_slash() {
        assert_eq!(split_name("demo"), (None, "demo"));
        assert_eq!(split_name("acme/demo"), (Some("acme"), "demo"));
    }
}
