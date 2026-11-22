//! Renders demo pages to plain HTML files, plus the static files they link, so a headless
//! browser can screenshot them from any static file server.
//!
//!   cargo run -p ch-web --example snapshot -- <out dir> [names]
//!
//! `names` is a comma separated list of page names, each optionally ending in `*` for a prefix.
//!
//! For each page it writes `<name>.light.html` and `<name>.dark.html` (the theme is the same
//! cookie the real site uses) and copies every `/static/...` file the pages mention.

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt;
use std::collections::BTreeSet;
use std::path::PathBuf;
use tower::ServiceExt;

const PAGES: &[(&str, &str)] = &[
    ("home", "/"),
    ("repo-home", "/ada/lighthouse"),
    ("repo-empty", "/bob/sandbox"),
    ("tree", "/ada/lighthouse/tree/main/src"),
    ("blob-rust", "/ada/lighthouse/blob/main/src/beam.rs"),
    ("blob-markdown", "/ada/lighthouse/blob/main/README.md"),
    ("blame", "/ada/lighthouse/blame/main/src/beam.rs"),
    ("commits", "/ada/lighthouse/commits"),
    ("commit", "/ada/lighthouse/commit/main"),
    ("branches", "/ada/lighthouse/branches"),
    ("compare", "/ada/lighthouse/compare/main...feature/signal-colours"),
    ("compare-split", "/ada/lighthouse/compare/main...feature/signal-colours?diff=split"),
    ("edit", "/ada/lighthouse/edit/main/src/main.rs"),
    ("issues", "/ada/lighthouse/issues"),
    ("issue", "/ada/lighthouse/issues/12"),
    ("issue-new", "/ada/lighthouse/issues/new"),
    ("labels", "/ada/lighthouse/labels"),
    ("milestones", "/ada/lighthouse/milestones"),
    ("pulls", "/ada/lighthouse/pulls"),
    ("pull", "/ada/lighthouse/pulls/14"),
    ("pull-files", "/ada/lighthouse/pulls/14/files"),
    ("pull-checks", "/ada/lighthouse/pulls/14/checks"),
    ("pull-draft", "/ada/lighthouse/pulls/13"),
    ("pull-merged", "/ada/lighthouse/pulls/7"),
    ("pull-new", "/ada/lighthouse/pulls/new?head=feature/signal-colours"),
    ("actions", "/ada/lighthouse/actions"),
    ("run", "/ada/lighthouse/actions/runs/52"),
    ("job", "/ada/lighthouse/actions/runs/51/jobs/511"),
    ("job-live", "/ada/lighthouse/actions/runs/53/jobs/531"),
    ("releases", "/ada/lighthouse/releases"),
    ("settings-repo", "/ada/lighthouse/settings"),
    ("settings-collab", "/ada/lighthouse/settings/collaborators"),
    ("settings-branches", "/ada/lighthouse/settings/branches"),
    ("settings-hooks", "/ada/lighthouse/settings/hooks"),
    ("settings-secrets", "/ada/lighthouse/settings/secrets"),
    ("profile", "/ada"),
    ("org", "/harbor"),
    ("explore", "/explore"),
    ("notifications", "/notifications"),
    ("new-repo", "/new"),
    ("settings-profile", "/settings/profile"),
    ("settings-security", "/settings/security"),
    ("settings-sessions", "/settings/sessions"),
    ("settings-keys", "/settings/keys"),
    ("settings-tokens", "/settings/tokens"),
    ("settings-appearance", "/settings/appearance"),
    ("admin", "/admin"),
    ("admin-invites", "/admin/invites"),
    ("admin-users", "/admin/users"),
    ("admin-runners", "/admin/runners"),
    ("admin-audit", "/admin/audit"),
];

async fn fetch(app: &axum::Router, path: &str, cookie: &str) -> (u16, Vec<u8>) {
    let req = Request::builder().uri(path).header(header::COOKIE, cookie).body(Body::empty()).unwrap_or_default();
    match app.clone().oneshot(req).await {
        Ok(r) => {
            let status = r.status().as_u16();
            let bytes = r.into_body().collect().await.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
            (status, bytes)
        }
        Err(_) => (500, Vec::new()),
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| "snapshots".into()));
    let filter = args.next();
    let app = ch_web::demo::router();
    std::fs::create_dir_all(out.join("static")).ok();
    let mut statics: BTreeSet<String> = BTreeSet::new();
    // The sign-in pages need no cookie.
    let mut all: Vec<(&str, &str, bool)> = vec![
        ("login", "/login", false),
        ("invite", "/invite/chi_demoInviteToken0123456789abcdef", false),
        ("bad-invite", "/invite/nope", false),
    ];
    all.extend(PAGES.iter().map(|(n, p)| (*n, *p, true)));
    for (name, path, signed_in) in all {
        if filter
            .as_deref()
            .is_some_and(|f| !f.split(',').any(|x| x == name || (x.ends_with('*') && name.starts_with(x.trim_end_matches('*')))))
        {
            continue;
        }
        for theme in ["light", "dark"] {
            let cookie = format!("{}ch_theme={theme}", if signed_in { "ch_session=demo-session-ada; " } else { "" });
            let (status, body) = fetch(&app, path, &cookie).await;
            if status != 200 && status != 404 {
                eprintln!("{name}: status {status}");
            }
            let html = String::from_utf8_lossy(&body).to_string();
            for part in html.split(['"', '\'']) {
                if part.starts_with("/static/") || part.starts_with("/avatar/") {
                    statics.insert(part.to_string());
                }
            }
            std::fs::write(out.join(format!("{name}.{theme}.html")), &html).ok();
        }
    }
    for s in statics {
        let (status, body) = fetch(&app, &s, "ch_session=demo-session-ada").await;
        if status == 200 {
            let decoded = percent_decode(s.trim_start_matches('/'));
            let rel = decoded.as_str();
            if let Some(dir) = std::path::Path::new(rel).parent() {
                std::fs::create_dir_all(out.join(dir)).ok();
            }
            std::fs::write(out.join(rel), body).ok();
        } else {
            eprintln!("static {s}: {status}");
        }
    }
    println!("wrote snapshots to {}", out.display());
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
