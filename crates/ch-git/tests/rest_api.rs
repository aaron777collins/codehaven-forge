//! The repository REST endpoints over real HTTP: shapes, status codes and permissions.
#![allow(clippy::unwrap_used)]

mod common;

use base64::Engine;
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;

struct Http {
    base: String,
    c: reqwest::Client,
}

struct Reply {
    status: u16,
    body: Value,
    link: Option<String>,
    raw: Vec<u8>,
}

impl Http {
    fn new(addr: std::net::SocketAddr) -> Http {
        Http { base: format!("http://{addr}/api/v3"), c: reqwest::Client::new() }
    }

    async fn send(&self, req: reqwest::RequestBuilder, who: Option<&str>) -> Reply {
        let req = match who {
            Some(u) => req.basic_auth(u, Some(format!("pw-{u}"))),
            None => req,
        };
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let link = r.headers().get("link").map(|v| v.to_str().unwrap().to_string());
        let raw = r.bytes().await.unwrap().to_vec();
        let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        Reply { status, body, link, raw }
    }

    async fn get(&self, who: &str, path: &str) -> Reply {
        self.send(self.c.get(format!("{}{path}", self.base)), Some(who)).await
    }
    async fn post(&self, who: &str, path: &str, body: Value) -> Reply {
        self.send(self.c.post(format!("{}{path}", self.base)).json(&body), Some(who)).await
    }
    async fn put(&self, who: &str, path: &str, body: Value) -> Reply {
        self.send(self.c.put(format!("{}{path}", self.base)).json(&body), Some(who)).await
    }
    async fn patch(&self, who: &str, path: &str, body: Value) -> Reply {
        self.send(self.c.patch(format!("{}{path}", self.base)).json(&body), Some(who)).await
    }
    async fn delete(&self, who: &str, path: &str) -> Reply {
        self.send(self.c.delete(format!("{}{path}", self.base)), Some(who)).await
    }
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

async fn setup(pool: PgPool) -> (Fixture, Http, ch_core::UserId) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let addr = f.serve_http().await;
    (f, Http::new(addr), alice)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn create_get_patch_delete_a_repository(pool: PgPool) {
    let (f, h, _alice) = setup(pool).await;
    let r = h.post("alice", "/user/repos", json!({"name": "demo", "description": "a demo", "auto_init": true})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["full_name"], "alice/demo");
    assert_eq!(r.body["private"], true, "private unless said otherwise");
    assert_eq!(r.body["default_branch"], "main");
    assert_eq!(r.body["owner"]["login"], "alice");
    assert_eq!(r.body["owner"]["type"], "User");
    assert!(r.body["clone_url"].as_str().unwrap().ends_with("/alice/demo.git"));
    assert_eq!(r.body["permissions"]["admin"], true);
    assert_eq!(r.body["description"], "a demo");
    assert!(r.body["url"].as_str().unwrap().ends_with("/api/v3/repos/alice/demo"));

    let g = h.get("alice", "/repos/alice/demo").await;
    assert_eq!(g.status, 200);
    assert_eq!(g.body["id"], r.body["id"]);
    assert_eq!(g.body["empty"], false);

    let p = h
        .patch("alice", "/repos/alice/demo", json!({"description": "changed", "private": false, "name": "renamed", "archived": false}))
        .await;
    assert_eq!(p.status, 200, "{}", p.body);
    // `private: false` on a private repository makes it internal, and internal still reads as
    // private in the legacy flag.
    assert_eq!(
        (p.body["name"].as_str(), p.body["description"].as_str(), p.body["private"].as_bool()),
        (Some("renamed"), Some("changed"), Some(true))
    );
    assert_eq!(p.body["visibility"], "internal");
    // The old name still works through the redirect.
    assert_eq!(h.get("alice", "/repos/alice/demo").await.status, 200);

    assert_eq!(h.delete("alice", "/repos/alice/renamed").await.status, 204);
    assert_eq!(h.get("alice", "/repos/alice/renamed").await.status, 404);
    assert_eq!(f.env.count("repositories").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn visibility_on_create_and_patch(pool: PgPool) {
    let (_f, h, _alice) = setup(pool).await;

    // Creating.
    let a = h.post("alice", "/user/repos", json!({"name": "a"})).await;
    assert_eq!(a.status, 201, "{}", a.body);
    assert_eq!(a.body["visibility"], "private");
    assert_eq!(a.body["private"], true);
    assert_eq!(a.body["contribution_policy"], "everyone");
    let b = h.post("alice", "/user/repos", json!({"name": "b", "private": false})).await;
    assert_eq!(b.status, 201, "{}", b.body);
    assert_eq!(b.body["visibility"], "internal");
    assert_eq!(b.body["private"], true);
    let c = h.post("alice", "/user/repos", json!({"name": "c", "visibility": "internal", "private": true})).await;
    assert_eq!(c.status, 201, "{}", c.body);
    assert_eq!(c.body["visibility"], "internal");
    let d = h.post("alice", "/user/repos", json!({"name": "d", "visibility": "public"})).await;
    assert_eq!(d.status, 403, "{}", d.body);
    assert_eq!(d.body["message"], "Public repositories are off on this server.");
    assert_eq!(h.post("alice", "/user/repos", json!({"name": "e", "visibility": "secret"})).await.status, 422);
    assert_eq!(h.post("alice", "/user/repos", json!({"name": "f", "visibility": "private", "private": false})).await.status, 422);

    // Editing.
    let p = h.patch("alice", "/repos/alice/a", json!({"private": false})).await;
    assert_eq!(p.status, 200, "{}", p.body);
    assert_eq!(p.body["visibility"], "internal");
    let again = h.patch("alice", "/repos/alice/a", json!({"private": false})).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.body["visibility"], "internal");
    let back = h.patch("alice", "/repos/alice/a", json!({"private": true})).await;
    assert_eq!(back.status, 200, "{}", back.body);
    assert_eq!(back.body["visibility"], "private");
    let up = h.patch("alice", "/repos/alice/a", json!({"visibility": "internal"})).await;
    assert_eq!(up.status, 200, "{}", up.body);
    assert_eq!(up.body["visibility"], "internal");
    assert_eq!(h.patch("alice", "/repos/alice/a", json!({"visibility": "public"})).await.status, 403);
    // A refusal leaves the repository as it was.
    assert_eq!(h.get("alice", "/repos/alice/a").await.body["visibility"], "internal");
    assert_eq!(h.patch("alice", "/repos/alice/a", json!({"visibility": "Private"})).await.status, 422);
    assert_eq!(h.get("alice", "/repos/alice/a").await.body["visibility"], "internal");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn contribution_policy_by_rest(pool: PgPool) {
    let (f, h, _alice) = setup(pool).await;
    let made = h.post("alice", "/user/repos", json!({"name": "a", "visibility": "internal"})).await;
    assert_eq!(made.status, 201, "{}", made.body);

    let p = h.patch("alice", "/repos/alice/a", json!({"contribution_policy": "grants"})).await;
    assert_eq!(p.status, 200, "{}", p.body);
    assert_eq!(p.body["contribution_policy"], "grants");
    assert_eq!(h.get("alice", "/repos/alice/a").await.body["contribution_policy"], "grants");
    assert_eq!(h.patch("alice", "/repos/alice/a", json!({"contribution_policy": "nobody"})).await.status, 422);

    // An admin collaborator still cannot change it: only the account owner.
    f.user("bob").await;
    let add = h.put("alice", "/repos/alice/a/collaborators/bob", json!({"permission": "admin"})).await;
    assert!(matches!(add.status, 201 | 204), "{}", add.body);
    assert_eq!(h.patch("bob", "/repos/alice/a", json!({"contribution_policy": "everyone"})).await.status, 403);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn forks_start_private_and_never_above_the_source(pool: PgPool) {
    let (f, h, _alice) = setup(pool).await;
    let made = h.post("alice", "/user/repos", json!({"name": "src", "visibility": "internal", "auto_init": true})).await;
    assert_eq!(made.status, 201, "{}", made.body);
    f.user("bob").await;

    let fork = h.post("bob", "/repos/alice/src/forks", json!({})).await;
    assert_eq!(fork.status, 202, "{}", fork.body);
    assert_eq!(fork.body["visibility"], "private");
    let named = h.post("bob", "/repos/alice/src/forks", json!({"name": "src2", "visibility": "internal"})).await;
    assert_eq!(named.status, 202, "{}", named.body);
    assert_eq!(named.body["visibility"], "internal");
    assert_eq!(h.post("bob", "/repos/alice/src/forks", json!({"name": "src3", "visibility": "public"})).await.status, 403);

    // A fork of a private repository cannot ask for internal, even with access to the source.
    let hidden = h.post("alice", "/user/repos", json!({"name": "hidden", "auto_init": true})).await;
    assert_eq!(hidden.status, 201, "{}", hidden.body);
    let add = h.put("alice", "/repos/alice/hidden/collaborators/bob", json!({"permission": "pull"})).await;
    assert!(matches!(add.status, 201 | 204), "{}", add.body);
    assert_eq!(h.post("bob", "/repos/alice/hidden/forks", json!({"name": "hidden2", "visibility": "internal"})).await.status, 422);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn errors_have_the_github_shape(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    f.user("mallory").await;
    f.repo(alice, "private-one", true).await;
    // No credentials: the stand-in gate says 401.
    let r = h.send(h.c.get(format!("{}/repos/alice/private-one", h.base)), None).await;
    assert_eq!(r.status, 401);
    // Not readable looks like not existing, with the same body.
    let hidden = h.get("mallory", "/repos/alice/private-one").await;
    let missing = h.get("mallory", "/repos/alice/never-made").await;
    assert_eq!((hidden.status, missing.status), (404, 404));
    assert_eq!(hidden.body, missing.body);
    assert_eq!(hidden.body["message"], "Not Found");
    // Validation.
    let bad = h.post("alice", "/user/repos", json!({"name": "bad name"})).await;
    assert_eq!(bad.status, 422);
    assert_eq!(bad.body["message"], "Validation Failed");
    assert_eq!(bad.body["errors"][0]["field"], "name");
    let dup = h.post("alice", "/user/repos", json!({"name": "private-one"})).await;
    assert_eq!(dup.status, 422);
    // A malformed body is the same shape, not a bare text error.
    let broken =
        h.send(h.c.post(format!("{}/user/repos", h.base)).header("content-type", "application/json").body("{nope"), Some("alice")).await;
    assert_eq!(broken.status, 422);
    assert!(broken.body["message"].is_string());
    // Forbidden when you can see it but may not change it.
    f.env.collaborator(ch_core::RepoId(1), f.user("reader").await, "read").await;
    let denied = h.patch("reader", "/repos/alice/private-one", json!({"description": "x"})).await;
    assert_eq!(denied.status, 403);
    assert_eq!(h.delete("reader", "/repos/alice/private-one").await.status, 403);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn listing_pages_with_link_headers(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    for n in ["a", "b", "c"] {
        f.repo(alice, n, false).await;
    }
    let r = h.get("alice", "/user/repos?per_page=2").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body.as_array().unwrap().len(), 2);
    let link = r.link.unwrap();
    assert!(link.contains("rel=\"next\"") && link.contains("page=2"), "{link}");
    let r2 = h.get("alice", "/user/repos?per_page=2&page=2").await;
    assert_eq!(r2.body.as_array().unwrap().len(), 1);
    assert!(r2.link.unwrap().contains("rel=\"prev\""));
    let by_user = h.get("alice", "/users/alice/repos").await;
    assert_eq!(by_user.body.as_array().unwrap().len(), 3);
    f.user("bob").await;
    assert_eq!(h.get("bob", "/users/alice/repos").await.body.as_array().unwrap().len(), 0, "bob sees none of alice's private repos");
    assert_eq!(h.get("alice", "/users/nobody/repos").await.status, 404);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn topics_collaborators_and_forks(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    f.public_repo(alice, "proj").await;
    f.user("bob").await;
    assert_eq!(
        h.put("alice", "/repos/alice/proj/topics", json!({"names": ["Rust", "forge"]})).await.body["names"],
        json!(["forge", "rust"])
    );
    assert_eq!(h.get("alice", "/repos/alice/proj/topics").await.body["names"], json!(["forge", "rust"]));
    assert_eq!(h.put("alice", "/repos/alice/proj/topics", json!({"names": ["not valid"]})).await.status, 422);

    // Collaborators.
    assert_eq!(h.put("alice", "/repos/alice/proj/collaborators/bob", json!({"permission": "push"})).await.status, 204);
    let list = h.get("alice", "/repos/alice/proj/collaborators").await;
    let logins: Vec<&str> = list.body.as_array().unwrap().iter().map(|c| c["login"].as_str().unwrap()).collect();
    assert_eq!(logins, vec!["alice", "bob"]);
    assert_eq!(list.body[1]["permissions"]["push"], true);
    assert_eq!(list.body[1]["role_name"], "write");
    assert_eq!(h.get("alice", "/repos/alice/proj/collaborators/bob").await.status, 204);
    assert_eq!(h.get("alice", "/repos/alice/proj/collaborators/stranger").await.status, 404);
    let perm = h.get("alice", "/repos/alice/proj/collaborators/bob/permission").await;
    assert_eq!(perm.body["permission"], "write");
    assert_eq!(h.put("alice", "/repos/alice/proj/collaborators/bob", json!({"permission": "godmode"})).await.status, 422);
    assert_eq!(h.delete("alice", "/repos/alice/proj/collaborators/bob").await.status, 204);
    assert_eq!(h.delete("alice", "/repos/alice/proj/collaborators/bob").await.status, 404);

    // Forks. An internal one, so the parent's owner can see it in the list.
    let fork = h.post("bob", "/repos/alice/proj/forks", json!({"visibility": "internal"})).await;
    assert_eq!(fork.status, 202, "{}", fork.body);
    assert_eq!(fork.body["full_name"], "bob/proj");
    assert_eq!(fork.body["fork"], true);
    assert_eq!(fork.body["parent"]["full_name"], "alice/proj");
    let forks = h.get("alice", "/repos/alice/proj/forks").await;
    assert_eq!(forks.body.as_array().unwrap().len(), 1);
    assert_eq!(h.get("alice", "/repos/alice/proj").await.body["forks_count"], 1);
    // A second fork under the same name is a validation error.
    assert_eq!(h.post("bob", "/repos/alice/proj/forks", json!({})).await.status, 422);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn branches_tags_and_protection(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    f.commit(&repo, alice, "main", "a.txt", "a\n", "Two").await;
    let sha = f.branch_sha(&repo, "main").await;
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/heads/feature/x", &sha, &f.client()).await.unwrap();
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/tags/v1", &sha, &f.client()).await.unwrap();

    let branches = h.get("alice", "/repos/alice/demo/branches").await;
    let names: Vec<&str> = branches.body.as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["feature/x", "main"]);
    assert_eq!(branches.body[1]["commit"]["sha"], sha);
    assert_eq!(branches.body[1]["protected"], false);
    assert_eq!(h.get("alice", "/repos/alice/demo/branches/feature/x").await.body["name"], "feature/x", "slashes are fine");
    assert_eq!(h.get("alice", "/repos/alice/demo/branches/ghost").await.status, 404);

    // Protection round trip.
    assert_eq!(h.get("alice", "/repos/alice/demo/branches/main/protection").await.status, 404);
    let put = h
        .put(
            "alice",
            "/repos/alice/demo/branches/main/protection",
            json!({"required_status_checks": {"strict": true, "contexts": ["ci"]}, "required_pull_request_reviews": {"required_approving_review_count": 2, "dismiss_stale_reviews": true, "require_code_owner_reviews": false}, "enforce_admins": true, "required_conversation_resolution": true, "allow_force_pushes": false, "allow_deletions": false, "allow_direct_push": false}),
        )
        .await;
    assert_eq!(put.status, 200, "{}", put.body);
    assert_eq!(put.body["required_pull_request_reviews"]["required_approving_review_count"], 2);
    let got = h.get("alice", "/repos/alice/demo/branches/main/protection").await;
    assert_eq!(got.body["required_status_checks"]["contexts"], json!(["ci"]));
    assert_eq!(got.body["allow_force_pushes"], false);
    assert_eq!(h.get("alice", "/repos/alice/demo/branches/main").await.body["protected"], true);
    assert_eq!(h.delete("alice", "/repos/alice/demo/branches/main/protection").await.status, 204);
    assert_eq!(h.get("alice", "/repos/alice/demo/branches/main/protection").await.status, 404);

    // GitHub's own body works as it is (ch-yci): nulls for what is not wanted, `checks` instead of
    // `contexts`, the review block with only a count, and no Codehaven-only fields.
    let gh = h
        .put(
            "alice",
            "/repos/alice/demo/branches/main/protection",
            json!({
                "required_status_checks": {"strict": true, "checks": [{"context": "ci / test", "app_id": null}]},
                "enforce_admins": null,
                "required_pull_request_reviews": {"required_approving_review_count": 1},
                "restrictions": null,
                "required_linear_history": false,
                "allow_force_pushes": null,
                "allow_deletions": false,
                "block_creations": false,
                "required_conversation_resolution": true,
                "lock_branch": false,
                "allow_fork_syncing": false
            }),
        )
        .await;
    assert_eq!(gh.status, 200, "{}", gh.body);
    assert_eq!(gh.body["required_status_checks"]["contexts"], json!(["ci / test"]));
    assert_eq!(gh.body["required_pull_request_reviews"]["required_approving_review_count"], 1);
    assert_eq!(gh.body["allow_force_pushes"], false);
    assert_eq!(gh.body["allow_direct_push"], false, "requiring reviews means no direct push, as on GitHub");
    assert_eq!(gh.body["required_conversation_resolution"], true);
    // What Codehaven cannot enforce is refused, not silently dropped.
    for (field, v) in [
        ("restrictions", json!({"users": ["bob"], "teams": [], "apps": []})),
        ("required_linear_history", json!(true)),
        ("lock_branch", json!(true)),
    ] {
        let mut body =
            json!({"required_status_checks": null, "enforce_admins": null, "required_pull_request_reviews": null, "restrictions": null});
        body[field] = v;
        let r = h.put("alice", "/repos/alice/demo/branches/main/protection", body).await;
        assert_eq!(r.status, 422, "{field}: {}", r.body);
        assert!(r.body.to_string().contains(field), "{field}: {}", r.body);
    }
    // The minimal GitHub body: no rules but the defaults.
    let bare = h
        .put(
            "alice",
            "/repos/alice/demo/branches/main/protection",
            json!({"required_status_checks": null, "enforce_admins": null, "required_pull_request_reviews": null, "restrictions": null}),
        )
        .await;
    assert_eq!(bare.status, 200, "{}", bare.body);
    assert_eq!(bare.body["allow_direct_push"], true);
    assert_eq!(h.delete("alice", "/repos/alice/demo/branches/main/protection").await.status, 204);

    let tags = h.get("alice", "/repos/alice/demo/tags").await;
    assert_eq!(tags.body[0]["name"], "v1");
    assert_eq!(tags.body[0]["commit"]["sha"], sha);
    assert!(tags.body[0]["zipball_url"].as_str().unwrap().ends_with("/zipball/v1"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn commits_and_compare(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    let base = f.branch_sha(&repo, "main").await;
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/heads/topic", &base, &f.client()).await.unwrap();
    let c2 = f.commit(&repo, alice, "topic", "src/a.rs", "fn a() {}\nfn b() {}\n", "Add a\n\nBody here.").await;
    f.commit(&repo, alice, "topic", "src/a.rs", "fn a() {}\nfn c() {}\n", "Change a").await;

    let list = h.get("alice", "/repos/alice/demo/commits?sha=topic&per_page=2").await;
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(list.body.as_array().unwrap().len(), 2);
    assert_eq!(list.body[0]["commit"]["message"], "Change a");
    assert!(list.link.unwrap().contains("rel=\"next\""));
    let only_path = h.get("alice", "/repos/alice/demo/commits?sha=topic&path=src/a.rs").await;
    assert_eq!(only_path.body.as_array().unwrap().len(), 2);

    let one = h.get("alice", &format!("/repos/alice/demo/commits/{c2}")).await;
    assert_eq!(one.status, 200);
    assert_eq!(one.body["commit"]["message"], "Add a\n\nBody here.");
    assert_eq!(one.body["files"][0]["filename"], "src/a.rs");
    assert_eq!(one.body["files"][0]["status"], "added");
    assert!(one.body["files"][0]["patch"].as_str().unwrap().starts_with("@@ -0,0 +1,2 @@"));
    assert_eq!(one.body["stats"]["additions"], 2);
    assert_eq!(one.body["parents"][0]["sha"], base);
    assert_eq!(h.get("alice", "/repos/alice/demo/commits/nope").await.status, 404);

    let cmp = h.get("alice", "/repos/alice/demo/compare/main...topic").await;
    assert_eq!(cmp.status, 200, "{}", cmp.body);
    assert_eq!(
        (cmp.body["status"].as_str(), cmp.body["ahead_by"].as_i64(), cmp.body["behind_by"].as_i64()),
        (Some("ahead"), Some(2), Some(0))
    );
    assert_eq!(cmp.body["total_commits"], 2);
    assert_eq!(cmp.body["commits"][0]["commit"]["message"], "Add a\n\nBody here.", "oldest first");
    assert_eq!(cmp.body["files"].as_array().unwrap().len(), 1);
    assert_eq!(cmp.body["merge_base_commit"]["sha"], base);
    assert_eq!(h.get("alice", "/repos/alice/demo/compare/main...ghost").await.status, 404);
    assert_eq!(h.get("alice", "/repos/alice/demo/compare/just-one").await.status, 422);

    // An empty repository has no commits to list.
    f.repo(alice, "blank", false).await;
    assert_eq!(h.get("alice", "/repos/alice/blank/commits").await.status, 409);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn contents_read_create_update_delete(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    f.commit(&repo, alice, "main", "docs/guide.md", "# guide\n", "Docs").await;

    let root = h.get("alice", "/repos/alice/demo/contents").await;
    let names: Vec<(&str, &str)> =
        root.body.as_array().unwrap().iter().map(|e| (e["name"].as_str().unwrap(), e["type"].as_str().unwrap())).collect();
    assert_eq!(names, vec![("docs", "dir"), ("README.md", "file")]);
    let dir = h.get("alice", "/repos/alice/demo/contents/docs").await;
    assert_eq!(dir.body[0]["path"], "docs/guide.md");
    let file = h.get("alice", "/repos/alice/demo/contents/docs/guide.md").await;
    assert_eq!((file.body["type"].as_str(), file.body["encoding"].as_str()), (Some("file"), Some("base64")));
    let decoded = base64::engine::general_purpose::STANDARD.decode(file.body["content"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, b"# guide\n");
    // The raw media type returns the bytes.
    let raw = h
        .send(
            h.c.get(format!("{}/repos/alice/demo/contents/docs/guide.md", h.base)).header("accept", "application/vnd.github.raw+json"),
            Some("alice"),
        )
        .await;
    assert_eq!(raw.raw, b"# guide\n");
    assert_eq!(h.get("alice", "/repos/alice/demo/contents/missing.txt").await.status, 404);
    assert_eq!(h.get("alice", "/repos/alice/demo/contents/docs/guide.md?ref=nope").await.status, 404);
    let readme = h.get("alice", "/repos/alice/demo/readme").await;
    assert_eq!(readme.body["name"], "README.md");

    // Create.
    let created = h.put("alice", "/repos/alice/demo/contents/src/new.txt", json!({"message": "Add new", "content": b64("fresh\n")})).await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.body["content"]["path"], "src/new.txt");
    assert_eq!(created.body["commit"]["commit"]["message"], "Add new");
    assert_eq!(created.body["commit"]["commit"]["author"]["name"], "alice");
    let blob_sha = created.body["content"]["sha"].as_str().unwrap().to_string();
    // Creating over an existing file needs its sha.
    assert_eq!(
        h.put("alice", "/repos/alice/demo/contents/src/new.txt", json!({"message": "again", "content": b64("x\n")})).await.status,
        422
    );
    // Update with the right sha, and with a stale one.
    let upd = h
        .put("alice", "/repos/alice/demo/contents/src/new.txt", json!({"message": "Edit", "content": b64("edited\n"), "sha": blob_sha}))
        .await;
    assert_eq!(upd.status, 200, "{}", upd.body);
    let stale = h
        .put("alice", "/repos/alice/demo/contents/src/new.txt", json!({"message": "Edit", "content": b64("late\n"), "sha": blob_sha}))
        .await;
    assert_eq!(stale.status, 409);
    // Bad input.
    assert_eq!(
        h.put("alice", "/repos/alice/demo/contents/a.txt", json!({"message": "x", "content": "!!!not base64!!!"})).await.status,
        422
    );
    assert_eq!(h.put("alice", "/repos/alice/demo/contents/.git/config", json!({"message": "x", "content": b64("x")})).await.status, 422);
    // To a new branch? The API takes an existing branch; a missing one is a 404.
    assert_eq!(
        h.put("alice", "/repos/alice/demo/contents/b.txt", json!({"message": "x", "content": b64("x"), "branch": "ghost"})).await.status,
        404
    );
    // Delete needs the sha as well.
    let now_sha = h.get("alice", "/repos/alice/demo/contents/src/new.txt").await.body["sha"].as_str().unwrap().to_string();
    assert_eq!(
        h.send(
            h.c.delete(format!("{}/repos/alice/demo/contents/src/new.txt", h.base)).json(&json!({"message": "Rm", "sha": "0".repeat(40)})),
            Some("alice")
        )
        .await
        .status,
        409,
        "a wrong sha is a conflict"
    );
    let del = h
        .send(
            h.c.delete(format!("{}/repos/alice/demo/contents/src/new.txt", h.base)).json(&json!({"message": "Remove", "sha": now_sha})),
            Some("alice"),
        )
        .await;
    assert_eq!(del.status, 200, "{}", del.body);
    assert_eq!(h.get("alice", "/repos/alice/demo/contents/src/new.txt").await.status, 404);
    // A reader cannot write.
    let bob = f.user("bob").await;
    f.env.collaborator(repo.id, bob, "read").await;
    assert_eq!(h.put("bob", "/repos/alice/demo/contents/x.txt", json!({"message": "x", "content": b64("x")})).await.status, 403);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn archives_download(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/tags/v1", &f.branch_sha(&repo, "main").await, &f.client()).await.unwrap();
    let z = h.get("alice", "/repos/alice/demo/zipball/v1").await;
    assert_eq!(z.status, 200);
    assert_eq!(&z.raw[..2], b"PK");
    let t = h.send(h.c.get(format!("{}/repos/alice/demo/tarball", h.base)), Some("alice")).await;
    assert_eq!(&t.raw[..2], &[0x1f, 0x8b]);
    assert_eq!(h.get("alice", "/repos/alice/demo/zipball/ghost").await.status, 404);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn git_data_refs_trees_and_blobs(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    let sha = f.commit(&repo, alice, "main", "a/b/c.txt", "deep\n", "Nested").await;
    let all = h.get("alice", "/repos/alice/demo/git/refs").await;
    assert_eq!(all.body[0]["ref"], "refs/heads/main");
    assert_eq!(all.body[0]["object"]["type"], "commit");
    assert_eq!(h.get("alice", "/repos/alice/demo/git/ref/heads/main").await.body["object"]["sha"], sha);
    assert_eq!(h.get("alice", "/repos/alice/demo/git/ref/heads/ghost").await.status, 404);

    let created = h.post("alice", "/repos/alice/demo/git/refs", json!({"ref": "refs/heads/topic", "sha": sha})).await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(h.post("alice", "/repos/alice/demo/git/refs", json!({"ref": "refs/heads/topic", "sha": sha})).await.status, 422);
    assert_eq!(h.post("alice", "/repos/alice/demo/git/refs", json!({"ref": "refs/pull/1/head", "sha": sha})).await.status, 422);
    let matching = h.get("alice", "/repos/alice/demo/git/matching-refs/heads/to").await;
    assert_eq!(matching.body.as_array().unwrap().len(), 1);

    // Move it forward, then try to move it back without force.
    let newer = f.commit(&repo, alice, "main", "z.txt", "z\n", "Newer").await;
    assert_eq!(h.patch("alice", "/repos/alice/demo/git/refs/heads/topic", json!({"sha": newer})).await.status, 200);
    assert_eq!(h.patch("alice", "/repos/alice/demo/git/refs/heads/topic", json!({"sha": sha})).await.status, 422);
    assert_eq!(h.patch("alice", "/repos/alice/demo/git/refs/heads/topic", json!({"sha": sha, "force": true})).await.status, 200);
    assert_eq!(h.delete("alice", "/repos/alice/demo/git/refs/heads/topic").await.status, 204);

    let tree = h.get("alice", &format!("/repos/alice/demo/git/trees/{sha}")).await;
    let top: Vec<&str> = tree.body["tree"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert_eq!(top, vec!["a", "README.md"]);
    let deep = h.get("alice", &format!("/repos/alice/demo/git/trees/{sha}?recursive=1")).await;
    let paths: Vec<&str> = deep.body["tree"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"a/b/c.txt") && paths.contains(&"a/b"), "{paths:?}");
    let blob_sha =
        deep.body["tree"].as_array().unwrap().iter().find(|e| e["path"] == "a/b/c.txt").unwrap()["sha"].as_str().unwrap().to_string();
    let blob = h.get("alice", &format!("/repos/alice/demo/git/blobs/{blob_sha}")).await;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(blob.body["content"].as_str().unwrap()).unwrap(), b"deep\n");
    assert_eq!(h.get("alice", &format!("/repos/alice/demo/git/blobs/{}", "0".repeat(40))).await.status, 404);
    assert_eq!(h.get("alice", &format!("/repos/alice/demo/git/commits/{sha}")).await.body["sha"], sha);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn raw_contents_have_a_ceiling(pool: PgPool) {
    // ch-ulq: the raw media type used to read a file of any size into memory.
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    let big = "0".repeat(50_000_001);
    f.commit(&repo, alice, "main", "big.bin", &big, "Big").await;
    let raw = h
        .send(
            h.c.get(format!("{}/repos/alice/demo/contents/big.bin", h.base)).header("accept", "application/vnd.github.raw+json"),
            Some("alice"),
        )
        .await;
    assert_eq!(raw.status, 403, "{}", String::from_utf8_lossy(&raw.raw[..raw.raw.len().min(300)]));
    assert!(raw.body["message"].as_str().unwrap_or("").contains("too large"));
}

/// ch-4pn.17: a raw file over REST streams from git with its exact length, including one too
/// large to return as JSON.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_large_raw_file_streams_with_its_length(pool: PgPool) {
    let (f, h, alice) = setup(pool).await;
    let repo = f.repo(alice, "demo", true).await;
    let big: String = "0123456789abcdef\n".repeat(700_000);
    f.commit(&repo, alice, "main", "big.txt", &big, "Big").await;
    let json = h.get("alice", "/repos/alice/demo/contents/big.txt").await;
    assert_eq!(json.status, 403, "too large for JSON");
    let raw = h
        .send(
            h.c.get(format!("{}/repos/alice/demo/contents/big.txt", h.base)).header("accept", "application/vnd.github.raw+json"),
            Some("alice"),
        )
        .await;
    assert_eq!(raw.status, 200);
    assert_eq!(raw.raw.len(), big.len());
    assert!(raw.raw == big.as_bytes());
}
