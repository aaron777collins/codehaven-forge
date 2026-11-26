//! `haven release`: create, list and upload.

use super::body_from;
use crate::cli::{ReleaseCmd, ReleaseCreateArgs, ReleaseListArgs, ReleaseUploadArgs};
use crate::{Ctx, fmt};
use anyhow::{Context, Result, bail};
use ch_api_types::releases::{CreateRelease, Release, ReleaseAsset};
use chrono::Utc;
use std::path::Path;

pub async fn run(ctx: &Ctx, cmd: ReleaseCmd) -> Result<i32> {
    match cmd {
        ReleaseCmd::Create(a) => create(ctx, a).await,
        ReleaseCmd::List(a) => list(ctx, a).await,
        ReleaseCmd::Upload(a) => upload(ctx, a).await,
    }
}

async fn create(ctx: &Ctx, a: ReleaseCreateArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    // Read the files first: a typo should not leave a release with nothing in it.
    let files = read_files(&a.files)?;
    let body = body_from(ctx, a.notes.as_deref(), a.notes_file.as_deref())?;
    let req = CreateRelease {
        tag_name: a.tag.clone(),
        target_commitish: a.target.clone(),
        name: a.title.clone(),
        body,
        draft: Some(a.draft),
        prerelease: Some(a.prerelease),
        generate_release_notes: if a.generate_notes { Some(true) } else { None },
        make_latest: None,
        discussion_category_name: None,
    };
    let api = ctx.api()?;
    let rel: Release = api.post(&format!("/repos/{owner}/{repo}/releases"), &req).await?;
    for (name, bytes) in files {
        upload_one(ctx, &rel, &name, bytes).await?;
    }
    ctx.println(&rel.html_url);
    Ok(0)
}

async fn list(ctx: &Ctx, a: ReleaseListArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let rels: Vec<Release> = ctx.api()?.get_paginated(&format!("/repos/{owner}/{repo}/releases"), &[], a.limit).await?;
    if a.json {
        ctx.print_json(&rels)?;
        return Ok(0);
    }
    if rels.is_empty() {
        ctx.println("No releases.");
        return Ok(0);
    }
    let now = Utc::now();
    let rows: Vec<Vec<String>> = rels
        .iter()
        .map(|r| {
            let flag = if r.draft {
                "draft"
            } else if r.prerelease {
                "pre-release"
            } else {
                ""
            };
            vec![
                r.tag_name.clone(),
                fmt::truncate(r.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&r.tag_name), 40),
                flag.to_string(),
                fmt::ago(r.published_at.unwrap_or(r.created_at), now),
            ]
        })
        .collect();
    ctx.print(fmt::table(&rows));
    Ok(0)
}

fn read_files(paths: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for p in paths {
        let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty());
        let Some(name) = name else { bail!("{p:?} is not a file") };
        let bytes = std::fs::read(p).with_context(|| format!("reading {p}"))?;
        out.push((name, bytes));
    }
    Ok(out)
}

pub fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "json" => "application/json",
        "txt" | "md" => "text/plain",
        "html" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// `https://host/api/v3/repos/o/r/releases/1/assets{?name,label}` into the URL to POST to.
pub fn upload_url(template: &str, name: &str) -> String {
    let base = template.split('{').next().unwrap_or(template);
    let mut enc = String::new();
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => enc.push(b as char),
            _ => enc.push_str(&format!("%{b:02X}")),
        }
    }
    format!("{base}?name={enc}")
}

async fn upload_one(ctx: &Ctx, rel: &Release, name: &str, bytes: Vec<u8>) -> Result<()> {
    let size = bytes.len() as i64;
    let url = upload_url(&rel.upload_url, name);
    let _: ReleaseAsset = ctx.api()?.upload(&url, content_type_for(name), bytes).await?;
    ctx.println(format!("Uploaded {name} ({})", fmt::bytes(size)));
    Ok(())
}

async fn upload(ctx: &Ctx, a: ReleaseUploadArgs) -> Result<i32> {
    let (owner, repo) = ctx.repo()?;
    let api = ctx.api()?;
    let files = read_files(&a.files)?;
    let rel: Release = api.get(&format!("/repos/{owner}/{repo}/releases/tags/{}", a.tag)).await?;
    // Check every name before changing anything, so a clash does not leave a half-uploaded release.
    let mut to_replace = Vec::new();
    for (name, _) in &files {
        if let Some(existing) = rel.assets.iter().find(|x| &x.name == name) {
            if !a.clobber {
                bail!("{name} is already on this release. Use --clobber to replace it.");
            }
            to_replace.push(existing.id);
        }
    }
    for id in to_replace {
        api.delete(&format!("/repos/{owner}/{repo}/releases/assets/{id}")).await?;
    }
    for (name, bytes) in files {
        upload_one(ctx, &rel, &name, bytes).await?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_urls_drop_the_template_and_encode_the_name() {
        assert_eq!(
            upload_url("https://h/api/v3/repos/o/r/releases/1/assets{?name,label}", "app v1.zip"),
            "https://h/api/v3/repos/o/r/releases/1/assets?name=app%20v1.zip"
        );
        assert_eq!(upload_url("https://h/x", "a.txt"), "https://h/x?name=a.txt");
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type_for("a.ZIP"), "application/zip");
        assert_eq!(content_type_for("a.tar.gz"), "application/gzip");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
        assert_eq!(content_type_for("x.png"), "image/png");
    }

    #[test]
    fn missing_files_are_an_error_before_anything_happens() {
        assert!(read_files(&["/definitely/not/here.bin".into()]).is_err());
        assert!(read_files(&[]).unwrap().is_empty());
    }
}
