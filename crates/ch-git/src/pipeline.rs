//! The push pipeline: one set of rules for every way a ref can change (ARCHITECTURE.md 6.4).
//!
//! A `git push` reaches [`authorize`] from the `pre-receive` hook and [`record`] from
//! `post-receive`. A change made by the server itself (a merge, a web edit, a new branch from
//! the API) goes through [`GitCore::apply_ref_change`], which runs the same two steps around a
//! compare-and-swap `git update-ref`. So permissions, protection, events and the audit trail
//! are identical wherever a change came from.

use crate::command::GitCommand;
use crate::core::{GitCore, ZERO_SHA, audit_action};
use crate::hooks::{PushCtx, RefUpdate};
use crate::protect::{Origin, UpdateKind, check_branch_update};
use ch_core::audit::Outcome;
use ch_core::model::{ClientInfo, Repo};
use ch_core::{Actor, DomainEvent, Error, Permission, RepoId, Result};
use std::path::Path;

/// Refs people may create, move and delete. Everything else (`refs/pull/*`, `refs/ch/*`) is
/// written only by the server.
pub(crate) fn namespace_allowed(name: &str) -> bool {
    name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
}

pub(crate) fn kind_of(u: &RefUpdate) -> UpdateKind {
    if u.old == ZERO_SHA {
        UpdateKind::Create
    } else if u.new == ZERO_SHA {
        UpdateKind::Delete
    } else {
        UpdateKind::Update { forced: u.forced }
    }
}

/// Decides a push. `Err` is the text git shows, one problem per line. `quarantine` is where a
/// client's push keeps its new objects until this says yes: a checked directory inside the
/// repository's objects (see `GitCore::quarantine_dir`), or `None` when the objects are already
/// in the repository, as for a change the server makes.
pub async fn authorize(core: &GitCore, ctx: &PushCtx, updates: &[RefUpdate], quarantine: Option<&Path>) -> std::result::Result<(), String> {
    let repo = core.repo_row(ctx.repo).await.map_err(|_| "This repository no longer exists.".to_string())?;
    let objects = Objects { repo: repo.id, quarantine };
    match check(core, ctx, &repo, updates, objects).await {
        Ok(()) => Ok(()),
        Err(msg) => {
            core.audit_standalone(
                &ctx.actor,
                &ctx.client,
                audit_action::PUSH_REJECTED,
                Outcome::Failure,
                repo.id,
                serde_json::json!({ "refs": updates.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), "reason": msg }),
            )
            .await;
            Err(msg)
        }
    }
}

async fn check(core: &GitCore, ctx: &PushCtx, repo: &Repo, updates: &[RefUpdate], objects: Objects<'_>) -> std::result::Result<(), String> {
    if repo.archived {
        return Err("This repository is archived and read-only.".into());
    }
    match ctx.deploy_write {
        Some(true) => {}
        Some(false) => return Err("This deploy key is read-only.".into()),
        None => {
            if !ctx.actor.is_system() {
                match core.authz.require(&ctx.actor, repo.id, Permission::Write).await {
                    Ok(()) => {}
                    Err(Error::Forbidden(_)) => return Err("You do not have write access to this repository.".into()),
                    Err(_) => return Err("Repository not found.".into()),
                }
            }
        }
    }
    let protections = core.protections_of(repo.id).await.map_err(|e| format!("Could not read branch protection: {e}"))?;
    let mut problems = Vec::new();
    for u in updates {
        // Only the server itself writes `refs/pull/*` and friends, never a push, even one made
        // with a deploy key (which carries `Actor::System` but `Origin::Push`).
        if !namespace_allowed(&u.name) && ctx.origin != Origin::System {
            problems.push(format!("{} is managed by Codehaven and cannot be pushed to.", u.name));
            continue;
        }
        if let Actor::App { grant, .. } = &ctx.actor
            && let Err(msg) = app_ref_rules(core, repo, grant, u, objects).await
        {
            problems.push(msg);
            continue;
        }
        // A personal access token changes workflow files only with the `workflow` scope (ch-s7r).
        if let Actor::Token { scopes, .. } = &ctx.actor
            && !scopes.has("workflow")
            && u.new != ZERO_SHA
            && (u.name.starts_with("refs/heads/") || u.name.starts_with("refs/tags/"))
            && let Err(dir) = workflow_dirs_unchanged(core, u, objects).await
        {
            problems.push(format!("This changes {dir}, which runs as CI. A token needs the workflow scope to do that."));
            continue;
        }
        if let Some(branch) = u.name.strip_prefix("refs/heads/")
            && let Err(msg) = check_branch_update(&protections, branch, &repo.default_branch, kind_of(u), ctx.origin)
        {
            problems.push(msg);
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems.join("\n")) }
}

/// The two directories whose contents run as CI. A connected app without the `workflows`
/// permission may not change anything in them, by any route: an edit, a rename, a parent
/// replacement, a symlink, a merge, an update-branch or a first commit.
const WORKFLOW_DIRS: [&str; 2] = [".codehaven/workflows", ".github/workflows"];

/// What a connected app may do to a ref. Only branches, never a deletion, and no change to the
/// workflow directories without the `workflows` permission.
async fn app_ref_rules(
    core: &GitCore,
    repo: &Repo,
    grant: &ch_core::AppGrant,
    u: &RefUpdate,
    objects: Objects<'_>,
) -> std::result::Result<(), String> {
    if !u.name.starts_with("refs/heads/") {
        return Err("A connected app can change branches only, not tags or other references.".into());
    }
    if u.new == ZERO_SHA {
        return Err("A connected app cannot delete a branch.".into());
    }
    if grant.workflows.has(repo.id) {
        return Ok(());
    }
    let branch = u.name.trim_start_matches("refs/heads/");
    if branch == repo.default_branch {
        let scheduled: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM schedules WHERE repo_id = $1)")
            .bind(repo.id.0)
            .fetch_one(&core.db)
            .await
            .map_err(|_| "Could not check this repository's schedules.".to_string())?;
        if scheduled {
            return Err("This repository runs scheduled workflows from its default branch, so a connection without workflows access cannot change that branch. Use another branch and a pull request, or give the connection workflows access.".into());
        }
    }
    workflow_dirs_unchanged(core, u, objects).await.map_err(|dir| {
        format!("This changes {dir}, which runs as CI. A connection needs the workflows permission on this repository to do that.")
    })
}

/// The objects a push check reads: the repository's own, plus the pushed ones still in quarantine.
#[derive(Clone, Copy)]
struct Objects<'a> {
    repo: RepoId,
    quarantine: Option<&'a Path>,
}

impl Objects<'_> {
    /// A git command that sees the pushed objects as well as the repository's. git does the same
    /// for its own hooks: the quarantine is the object directory and the repository's objects
    /// (with any `alternates` they have) come after it. Nothing is written.
    fn git(&self, core: &GitCore) -> GitCommand {
        let cmd = core.git(self.repo);
        match self.quarantine {
            Some(q) => cmd
                .env("GIT_OBJECT_DIRECTORY", q.to_string_lossy())
                .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", core.store.repo_path(self.repo).join("objects").to_string_lossy()),
            None => cmd,
        }
    }

    /// The output of a git command that must succeed, as text.
    async fn read(&self, core: &GitCore, args: &[&str]) -> std::result::Result<String, ()> {
        let out = core.exec_any(self.git(core).args(args)).await.map_err(|_| ())?;
        if out.code == 0 { Ok(out.stdout_str()) } else { Err(()) }
    }

    /// The object id at `dir` in `commit`, or `None` when there is nothing there. A tree, a
    /// file or a symlink each have their own id, so any replacement shows as a difference.
    async fn dir_id(&self, core: &GitCore, commit: &str, dir: &str) -> std::result::Result<Option<String>, ()> {
        let out =
            core.exec_any(self.git(core).args(["rev-parse", "--verify", "--quiet", &format!("{commit}:{dir}")])).await.map_err(|_| ())?;
        Ok(if out.code == 0 { Some(out.stdout_str()) } else { None })
    }
}

/// `Err(dir)` when the update changes a workflow directory. The new tip is compared with the old
/// tip, or for a new ref with the commit it grows from: the nearest first-parent ancestor that a
/// branch or tag already reaches, or nothing for a history that shares none. Anything git
/// cannot read, such as a pushed commit it cannot see, counts as a change.
async fn workflow_dirs_unchanged(core: &GitCore, u: &RefUpdate, objects: Objects<'_>) -> std::result::Result<(), String> {
    let unreadable = |_| WORKFLOW_DIRS[0].to_string();
    // Both ends are object ids by now. Anything else is not passed to git at all.
    let id = |s: &str| (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !id(&u.new) || !id(&u.old) {
        return Err(unreadable(()));
    }
    // A tag may name a tag object: what runs is the commit under it.
    let peel = |sha: &str| format!("{sha}^{{commit}}");
    let new = objects.read(core, &["rev-parse", "--verify", "--quiet", &peel(&u.new)]).await.map_err(unreadable)?;
    let baseline: Option<String> = if u.old != ZERO_SHA {
        Some(objects.read(core, &["rev-parse", "--verify", "--quiet", &peel(&u.old)]).await.map_err(unreadable)?)
    } else {
        // The commits only this ref has, along its first parents, newest first. None means the
        // tip is already on a branch or tag.
        let fresh = objects.read(core, &["rev-list", "--first-parent", &new, "--not", "--branches", "--tags"]).await.map_err(unreadable)?;
        match fresh.lines().last() {
            None => Some(new.clone()),
            Some(oldest) => {
                let line = objects.read(core, &["rev-list", "--parents", "-n", "1", oldest]).await.map_err(unreadable)?;
                // A first commit, or one whose parent is new too, is compared with nothing.
                line.split_whitespace().nth(1).map(str::to_string)
            }
        }
    };
    for dir in WORKFLOW_DIRS {
        let now = objects.dir_id(core, &new, dir).await.map_err(|_| dir.to_string())?;
        let before = match &baseline {
            Some(b) => objects.dir_id(core, b, dir).await.map_err(|_| dir.to_string())?,
            None => None,
        };
        if now != before {
            return Err(dir.to_string());
        }
    }
    Ok(())
}

/// Records what a push changed: one transaction for the repository row and the events.
pub async fn record(core: &GitCore, ctx: &PushCtx, updates: &[RefUpdate]) -> Result<()> {
    let repo = core.repo_row(ctx.repo).await?;
    let pusher = ctx.actor.author_id();
    let created_branches: Vec<&str> =
        updates.iter().filter(|u| u.old == ZERO_SHA && u.new != ZERO_SHA).filter_map(|u| u.name.strip_prefix("refs/heads/")).collect();
    let touched_branch = updates.iter().any(|u| u.new != ZERO_SHA && u.name.starts_with("refs/heads/"));

    // The first branch pushed to an empty repository decides its default branch when the
    // configured one was not pushed.
    let mut new_default = None;
    if repo.is_empty && !created_branches.is_empty() && !created_branches.contains(&repo.default_branch.as_str()) {
        let pick = ["main", "master"].iter().find(|c| created_branches.contains(c)).copied().or_else(|| {
            let mut sorted = created_branches.clone();
            sorted.sort_unstable();
            sorted.first().copied()
        });
        new_default = pick.map(str::to_string);
    }

    let mut tx = core.db.begin().await?;
    sqlx::query(
        "UPDATE repositories SET pushed_at = now(), updated_at = now(), \
         is_empty = CASE WHEN $2 THEN false ELSE is_empty END, \
         default_branch = COALESCE($3, default_branch) WHERE id = $1",
    )
    .bind(repo.id.0)
    .bind(touched_branch)
    .bind(new_default.as_deref())
    .execute(&mut *tx)
    .await?;
    for u in updates {
        if u.new != ZERO_SHA {
            core.events
                .emit(
                    &mut tx,
                    DomainEvent::Push {
                        repo_id: repo.id,
                        git_ref: u.name.clone(),
                        before: u.old.clone(),
                        after: u.new.clone(),
                        pusher,
                        forced: u.forced,
                        app: ctx.actor.app_origin(repo.id),
                        server_built: ctx.server_built,
                    },
                )
                .await?;
        }
        if u.old == ZERO_SHA {
            core.events
                .emit(&mut tx, DomainEvent::RefCreated { repo_id: repo.id, git_ref: u.name.clone(), sha: u.new.clone(), actor: pusher })
                .await?;
        } else if u.new == ZERO_SHA {
            core.events
                .emit(&mut tx, DomainEvent::RefDeleted { repo_id: repo.id, git_ref: u.name.clone(), sha: u.old.clone(), actor: pusher })
                .await?;
        }
    }
    tx.commit().await?;

    if let Some(branch) = &new_default {
        let target = format!("refs/heads/{branch}");
        if let Err(e) = core.exec(core.git(repo.id).args(["symbolic-ref", "HEAD", &target])).await {
            tracing::warn!(repo = repo.id.0, error = %e, "could not point HEAD at the new default branch");
        }
    }
    core.note_push(repo.id);
    Ok(())
}

/// A change the server makes to one ref.
#[derive(Debug, Clone)]
pub struct RefChange {
    /// The full ref name, such as `refs/heads/main`.
    pub name: String,
    /// What the ref must point at now. `None` means it must not exist yet.
    pub old: Option<String>,
    /// What it should point at. `None` deletes it.
    pub new: Option<String>,
    /// True when the server wrote `new` on top of whatever the ref held, for the actor: a file
    /// edit or Update branch. The push event says so, so CI does not count the actor for the
    /// commits underneath (ch-x12). False when the actor named the commit.
    pub server_built: bool,
}

impl GitCore {
    /// True when `new` descends from `old`.
    pub(crate) async fn is_fast_forward(&self, repo: &Repo, old: &str, new: &str) -> Result<bool> {
        let out = self.exec_any(self.git(repo.id).args(["merge-base", "--is-ancestor", old, new])).await?;
        match out.code {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(Error::invalid("Ref", "sha", "that commit does not exist in this repository")),
        }
    }

    /// Applies one ref change with the same checks and events as a push. `force` lets a
    /// non-fast-forward through the force-push rule of the caller; protection still decides.
    pub(crate) async fn apply_ref_change(
        &self,
        actor: &Actor,
        repo: &Repo,
        change: RefChange,
        origin: Origin,
        client: &ClientInfo,
    ) -> Result<()> {
        if !namespace_allowed(&change.name) && !actor.is_system() {
            return Err(Error::invalid("Reference", "ref", "only refs/heads/ and refs/tags/ can be changed"));
        }
        let check = self.exec_any(self.store.bare_cmd().args(["check-ref-format", &change.name])).await?;
        // `check-ref-format` allows a component that starts with a dash; a branch named `-x`
        // would be an option to anything that forgets `--`, so it is refused here.
        if check.code != 0 || change.name.split('/').any(|c| c.starts_with('-')) || change.name == "refs/heads/HEAD" {
            return Err(Error::invalid("Reference", "ref", "not a valid reference name"));
        }
        let old = change.old.clone().unwrap_or_else(|| ZERO_SHA.to_string());
        let new = change.new.clone().unwrap_or_else(|| ZERO_SHA.to_string());
        let forced = match (&change.old, &change.new) {
            (Some(o), Some(n)) => !self.is_fast_forward(repo, o, n).await?,
            _ => false,
        };
        let update = RefUpdate { old: old.clone(), new: new.clone(), name: change.name.clone(), forced };
        let ctx = PushCtx {
            actor: actor.clone(),
            repo: repo.id,
            client: client.clone(),
            origin,
            deploy_write: None,
            server_built: change.server_built,
        };
        // `require` has already run for the caller, and archived is checked in `check`.
        if let Err(msg) = authorize(self, &ctx, std::slice::from_ref(&update), None).await {
            return Err(Error::forbidden(msg));
        }
        // The ref move and its push event finish together in their own task. If the request
        // were dropped between them, the branch would move with no event: no workflow would
        // start and nobody would be told. Dropping the handle below leaves the task running.
        let core = self.me.upgrade().ok_or_else(|| Error::unavailable("The server is shutting down."))?;
        let ticket = self.writes.also(repo.id);
        let deleting = change.new.is_none();
        let task = tokio::spawn(async move {
            let _ticket = ticket;
            let mut cmd = core.git(ctx.repo).arg("update-ref");
            cmd = if deleting { cmd.args(["-d", "--", &update.name, &old]) } else { cmd.args(["--", &update.name, &new, &old]) };
            match core.exec(cmd).await {
                Ok(_) => {}
                Err(e) => {
                    let text = e.stderr().to_string();
                    if text.contains("cannot lock ref") || text.contains("but expected") || text.contains("reference already exists") {
                        return Err(Error::conflict("The reference changed while this was running. Reload and try again."));
                    }
                    if text.contains("non-commit") || text.contains("not a valid SHA1") || text.contains("nonexistent object") {
                        return Err(Error::invalid("Reference", "sha", "that is not a commit in this repository"));
                    }
                    return Err(e.into());
                }
            }
            let recorded = record(&core, &ctx, std::slice::from_ref(&update)).await;
            if let Err(e) = &recorded {
                tracing::error!(repo = ctx.repo.0, git_ref = %update.name, error = %e, "a ref moved but its push event was not recorded");
            }
            recorded
        });
        match task.await {
            Ok(r) => r,
            Err(e) => Err(Error::internal(anyhow::anyhow!("the ref update task failed: {e}"))),
        }
    }
}
