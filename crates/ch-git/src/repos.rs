//! Repository records: create, rename, transfer, fork, delete, collaborators, protections and
//! settings (ARCHITECTURE.md sections 5.1 and 7.2).
//!
//! Every change records an audit event and emits its domain event in the same transaction as
//! the database change. The bare repository on disk is made first and removed on failure, so a
//! row never points at nothing.

use crate::command::check_rev;
use crate::core::{GitCore, REPO_SELECT, RepoRow, audit_action};
use crate::tier::alternates_of;
use ch_core::actor::Scopes;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::*;
use ch_core::services::OrgRole;
use ch_core::{Actor, DomainEvent, Error, FieldError, OrgId, Permission, RepoId, Result, UserId};
use regex::Regex;
use std::sync::OnceLock;

fn name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,100}$").expect("static regex"))
}

fn topic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,49}$").expect("static regex"))
}

pub(crate) fn check_repo_name(name: &str) -> Result<()> {
    if !name_re().is_match(name) || name == "." || name == ".." || name.to_ascii_lowercase().ends_with(".git") {
        return Err(Error::Validation(vec![FieldError::custom(
            "Repository",
            "name",
            "use 1 to 100 letters, numbers, dots, dashes or underscores, and do not end in .git",
        )]));
    }
    Ok(())
}

fn already_exists() -> Error {
    Error::Validation(vec![FieldError::custom("Repository", "name", "name already exists on this account")])
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

fn vis_str(v: Visibility) -> &'static str {
    v.as_str()
}

fn client_audit(actor: &Actor, client: &ClientInfo, action: &str, repo: &Repo, details: serde_json::Value) -> AuditEvent {
    AuditEvent::new(actor, client, action, Outcome::Success).target("repository", repo.id.0).details(details)
}

impl GitCore {
    async fn user_kind(&self, id: UserId) -> Result<(UserKind, String)> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT kind, username::text FROM users WHERE id = $1").bind(id.0).fetch_optional(&self.db).await?;
        let (kind, name) = row.ok_or(Error::NotFound)?;
        Ok((UserKind::parse(&kind).unwrap_or(UserKind::User), name))
    }

    /// May this actor make repositories owned by `owner`?
    async fn may_create_under(&self, actor: &Actor, owner: UserId) -> Result<()> {
        let deny = || Error::forbidden("You cannot create repositories for that account.");
        match actor {
            Actor::System => return Ok(()),
            Actor::Token { scopes, .. } if !Scopes::has(scopes, "repo") => {
                return Err(Error::forbidden("This token needs the repo scope to create repositories."));
            }
            Actor::User { .. } | Actor::Token { .. } => {}
            // A connected app creates repositories only for the person who connected it, and only
            // when it was given read and write on all of their repositories, present and future.
            Actor::App { user_id, grant, .. } => {
                if *user_id != owner || !grant.read.is_all() || !grant.write.is_all() {
                    return Err(Error::forbidden(
                        "This connection can create repositories only in your own namespace, and only when it was given read and write on all repositories.",
                    ));
                }
                return Ok(());
            }
            _ => return Err(deny()),
        }
        if actor.user_id() == Some(owner) || actor.is_site_admin() {
            return Ok(());
        }
        let (kind, _) = self.user_kind(owner).await?;
        if kind == UserKind::Org && self.authz.org_role(actor, OrgId(owner.0)).await? == Some(OrgRole::Owner) {
            return Ok(());
        }
        Err(deny())
    }

    pub(crate) async fn do_create_repo(&self, actor: &Actor, new: NewRepo, client: &ClientInfo) -> Result<Repo> {
        self.may_create_under(actor, new.owner_id).await?;
        // A repository a connected app makes is private, whatever it asked for. Nothing is
        // created public: public is a second step, in the browser, after the store is checked.
        let mut new = new;
        if actor.is_app() {
            new.visibility = Visibility::Private;
        }
        new.visibility = new.visibility.min(Visibility::Internal);
        check_repo_name(&new.name)?;
        let branch = new.default_branch.clone().unwrap_or_else(|| "main".into());
        self.check_branch_name(&branch).await?;
        let (_, owner_name) = self.user_kind(new.owner_id).await?;

        let inserted: std::result::Result<(i64,), sqlx::Error> = sqlx::query_as(
            "INSERT INTO repositories (owner_id, name, description, access_tier, default_branch) VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(new.owner_id.0)
        .bind(&new.name)
        .bind(&new.description)
        .bind(vis_str(new.visibility))
        .bind(&branch)
        .fetch_one(&self.db)
        .await;
        let id = match inserted {
            Ok((id,)) => RepoId(id),
            Err(e) if is_unique_violation(&e) => return Err(already_exists()),
            Err(e) => return Err(e.into()),
        };
        let made = async {
            self.store.init_bare(id, &branch).await?;
            if new.auto_init {
                let (name, email) = self.commit_identity(new.owner_id).await?;
                let readme = if new.description.trim().is_empty() {
                    format!("# {}\n", new.name)
                } else {
                    format!("# {}\n\n{}\n", new.name, new.description.trim())
                };
                let req = CommitFileChange {
                    repo_id: id,
                    branch: branch.clone(),
                    new_branch: None,
                    expected_sha: None,
                    changes: vec![FileChange { path: "README.md".into(), content: Some(readme.into_bytes()), renamed_from: None }],
                    message: "Initial commit".into(),
                    author: Signature { name, email, at: chrono::Utc::now() },
                };
                self.do_commit_file_change(&Actor::System, req, client).await?;
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = made {
            let _ = sqlx::query("DELETE FROM repositories WHERE id = $1").bind(id.0).execute(&self.db).await;
            let _ = self.store.trash(id);
            return Err(e);
        }

        let mut tx = self.db.begin().await?;
        // A new repository takes over the name of any old redirect.
        sqlx::query("DELETE FROM repo_redirects WHERE old_owner = $1::citext AND old_name = $2::citext")
            .bind(&owner_name)
            .bind(&new.name)
            .execute(&mut *tx)
            .await?;
        let repo = self.repo_row(id).await?;
        self.audit
            .record(
                &mut tx,
                client_audit(
                    actor,
                    client,
                    audit_action::REPO_CREATED,
                    &repo,
                    serde_json::json!({"full_name": repo.full_name(), "visibility": vis_str(new.visibility)}),
                ),
            )
            .await?;
        self.events.emit(&mut tx, DomainEvent::RepoCreated { repo_id: id, actor: actor.author_id() }).await?;
        tx.commit().await?;
        Ok(repo)
    }

    /// A default branch name is one git accepts as `refs/heads/<name>`.
    pub(crate) async fn check_branch_name(&self, name: &str) -> Result<()> {
        check_rev(name).map_err(|_| Error::invalid("Repository", "default_branch", "not a valid branch name"))?;
        let full = format!("refs/heads/{name}");
        let out = self.exec_any(self.store.bare_cmd().args(["check-ref-format", &full])).await?;
        if out.code != 0 || name == "HEAD" || name.starts_with("refs/") {
            return Err(Error::invalid("Repository", "default_branch", "not a valid branch name"));
        }
        Ok(())
    }

    pub(crate) async fn do_get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo> {
        let repo = self.repo_by_name_unchecked(owner, name).await?;
        if actor.is_system() {
            return Ok(repo);
        }
        self.authz.require(actor, repo.id, Permission::View).await?;
        Ok(repo)
    }

    pub(crate) async fn do_get(&self, actor: &Actor, id: RepoId) -> Result<Repo> {
        let repo = self.repo_row(id).await?;
        if !actor.is_system() {
            self.authz.require(actor, id, Permission::View).await?;
        }
        Ok(repo)
    }

    pub(crate) async fn do_list_for_owner(&self, actor: &Actor, owner: UserId) -> Result<Vec<Repo>> {
        let rows: Vec<RepoRow> =
            sqlx::query_as(&format!("{REPO_SELECT} WHERE r.owner_id = $1 ORDER BY r.name")).bind(owner.0).fetch_all(&self.db).await?;
        let mut out = Vec::new();
        for r in rows {
            let repo = r.into_repo();
            if actor.is_system() || self.authz.repo_permission(actor, repo.id).await?.allows(Permission::View) {
                out.push(repo);
            }
        }
        Ok(out)
    }

    /// Every repository the actor can read, most recently pushed first (at most 2000).
    pub(crate) async fn accessible_all(&self, actor: &Actor) -> Result<Vec<Repo>> {
        if actor.is_anonymous() {
            // Signed out: public repositories while the switch is on, and nothing else.
            let rows: Vec<RepoRow> = sqlx::query_as(&format!(
                "{REPO_SELECT} WHERE r.access_tier = 'public' AND (SELECT s.allow_public_repos FROM visibility_settings s) \
                 ORDER BY r.pushed_at DESC NULLS LAST, r.id DESC LIMIT 2000"
            ))
            .fetch_all(&self.db)
            .await?;
            let mut out = Vec::new();
            for r in rows {
                let repo = r.into_repo();
                if self.authz.repo_permission(actor, repo.id).await?.allows(Permission::View) {
                    out.push(repo);
                }
            }
            return Ok(out);
        }
        let Some(user) = actor.author_id() else {
            return if actor.is_system() { self.all_repos().await } else { Ok(Vec::new()) };
        };
        // A cheap pre-filter in SQL; the permission service has the last word on each row.
        let rows: Vec<RepoRow> = sqlx::query_as(&format!(
            "{REPO_SELECT} WHERE r.access_tier <> 'private' OR r.owner_id = $1 \
             OR EXISTS (SELECT 1 FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $1) \
             OR EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = $1) \
             OR EXISTS (SELECT 1 FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id WHERE tr.repo_id = r.id AND tm.user_id = $1) \
             ORDER BY r.pushed_at DESC NULLS LAST, r.id DESC LIMIT 2000"
        ))
        .bind(user.0)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let repo = r.into_repo();
            if self.authz.repo_permission(actor, repo.id).await?.allows(Permission::View) {
                out.push(repo);
            }
        }
        Ok(out)
    }

    pub(crate) async fn do_list_accessible(&self, actor: &Actor, page: u32, per_page: u32) -> Result<Vec<Repo>> {
        let per = per_page.clamp(1, 100) as usize;
        let skip = (page.max(1) as usize - 1) * per;
        Ok(self.accessible_all(actor).await?.into_iter().skip(skip).take(per).collect())
    }

    async fn all_repos(&self) -> Result<Vec<Repo>> {
        let rows: Vec<RepoRow> = sqlx::query_as(&format!("{REPO_SELECT} ORDER BY r.pushed_at DESC NULLS LAST, r.id DESC LIMIT 2000"))
            .fetch_all(&self.db)
            .await?;
        Ok(rows.into_iter().map(RepoRow::into_repo).collect())
    }

    pub(crate) async fn do_rename(&self, actor: &Actor, id: RepoId, new_name: &str, client: &ClientInfo) -> Result<Repo> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Admin).await?;
        if repo.archived {
            return Err(Error::forbidden("This repository is archived and read-only."));
        }
        check_repo_name(new_name)?;
        if repo.name == new_name {
            return Ok(repo);
        }
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO repo_redirects (old_owner, old_name, repo_id) VALUES ($1, $2, $3) \
             ON CONFLICT (old_owner, old_name) DO UPDATE SET repo_id = EXCLUDED.repo_id",
        )
        .bind(&repo.owner_name)
        .bind(&repo.name)
        .bind(id.0)
        .execute(&mut *tx)
        .await?;
        match sqlx::query("UPDATE repositories SET name = $2, updated_at = now() WHERE id = $1")
            .bind(id.0)
            .bind(new_name)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => return Err(already_exists()),
            Err(e) => return Err(e.into()),
        }
        // The repository now owns its new name, so a redirect from that name goes.
        sqlx::query("DELETE FROM repo_redirects WHERE old_owner = $1::citext AND old_name = $2::citext")
            .bind(&repo.owner_name)
            .bind(new_name)
            .execute(&mut *tx)
            .await?;
        self.audit
            .record(
                &mut tx,
                client_audit(actor, client, audit_action::REPO_RENAMED, &repo, serde_json::json!({"from": repo.name, "to": new_name})),
            )
            .await?;
        tx.commit().await?;
        self.repo_row(id).await
    }

    pub(crate) async fn do_transfer(&self, actor: &Actor, id: RepoId, new_owner: UserId, client: &ClientInfo) -> Result<Repo> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Admin).await?;
        // Only the current account owner (or an owner of the owning org, or a site admin) moves
        // a repository. A collaborator with admin could otherwise move it to their own account
        // and publish it there.
        self.must_be_account_owner(actor, &repo).await?;
        self.may_create_under(actor, new_owner).await?;
        if repo.owner_id == new_owner {
            return Ok(repo);
        }
        let (_, new_owner_name) = self.user_kind(new_owner).await?;
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO repo_redirects (old_owner, old_name, repo_id) VALUES ($1, $2, $3) \
             ON CONFLICT (old_owner, old_name) DO UPDATE SET repo_id = EXCLUDED.repo_id",
        )
        .bind(&repo.owner_name)
        .bind(&repo.name)
        .bind(id.0)
        .execute(&mut *tx)
        .await?;
        match sqlx::query("UPDATE repositories SET owner_id = $2, updated_at = now() WHERE id = $1")
            .bind(id.0)
            .bind(new_owner.0)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => return Err(already_exists()),
            Err(e) => return Err(e.into()),
        }
        // Collaborators stay; the new owner is already admin through ownership.
        sqlx::query("DELETE FROM repo_collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(id.0)
            .bind(new_owner.0)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM repo_redirects WHERE old_owner = $1::citext AND old_name = $2::citext")
            .bind(&new_owner_name)
            .bind(&repo.name)
            .execute(&mut *tx)
            .await?;
        // What a person gave a connected app on this repository was given by the old owner's
        // say-so. The new owner decides again.
        sqlx::query("DELETE FROM oauth_grant_repos WHERE repo_id = $1").bind(id.0).execute(&mut *tx).await?;
        self.audit
            .record(
                &mut tx,
                client_audit(
                    actor,
                    client,
                    action::REPO_TRANSFERRED,
                    &repo,
                    serde_json::json!({"from": repo.owner_name, "to": new_owner_name}),
                ),
            )
            .await?;
        tx.commit().await?;
        self.repo_row(id).await
    }

    pub(crate) async fn do_delete(&self, actor: &Actor, id: RepoId, client: &ClientInfo) -> Result<()> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Admin).await?;
        let (forks,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM repositories WHERE (fork_root_id = $1 OR fork_parent_id = $1) AND id <> $1")
                .bind(id.0)
                .fetch_one(&self.db)
                .await?;
        if forks > 0 {
            return Err(Error::conflict("This repository has forks that share its storage. Delete or detach the forks first."));
        }
        let mut tx = self.db.begin().await?;
        self.audit
            .record(&mut tx, client_audit(actor, client, action::REPO_DELETED, &repo, serde_json::json!({"full_name": repo.full_name()})))
            .await?;
        self.events.emit(&mut tx, DomainEvent::RepoDeleted { repo_id: id, actor: actor.user_id() }).await?;
        sqlx::query("DELETE FROM repositories WHERE id = $1").bind(id.0).execute(&mut *tx).await?;
        tx.commit().await?;
        if let Err(e) = self.store.trash(id) {
            tracing::error!(repo = id.0, error = %e, "repository deleted but its files could not be moved to the trash");
        }
        Ok(())
    }

    /// A fork copies the branches and tags of its source and every object they need. It borrows
    /// nothing, so it holds exactly what its own readers may see. It starts private unless
    /// `visibility` asks for more, never above the source and never public.
    /// Stars, forks this person could see, and their own star. Forks are counted like the REST
    /// repository: public ones, and internal or their own when signed in. A private fork shared
    /// with them is left out rather than risk counting one they cannot read.
    pub(crate) async fn do_social(&self, actor: &Actor, id: RepoId) -> Result<RepoSocial> {
        self.do_get(actor, id).await?;
        let me = actor.user_id().map(|u| u.0).unwrap_or(0);
        let (stars, forks, starred): (i64, i64, bool) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM stars s WHERE s.repo_id = $1),
                    (SELECT count(*) FROM repositories f WHERE f.fork_parent_id = $1
                       AND (f.access_tier = 'public' OR ($2 > 0 AND (f.access_tier = 'internal' OR f.owner_id = $2)))),
                    EXISTS (SELECT 1 FROM stars s WHERE s.repo_id = $1 AND s.user_id = $2)",
        )
        .bind(id.0)
        .bind(me)
        .fetch_one(&self.db)
        .await?;
        Ok(RepoSocial { stars, forks, starred })
    }

    /// A star belongs to the person, so it needs an account that can see the repository and
    /// nothing more (ch-4pn section 3). A token needs the `repo` scope.
    pub(crate) async fn do_set_star(&self, actor: &Actor, id: RepoId, starred: bool) -> Result<()> {
        let user = match actor {
            Actor::User { id, .. } => *id,
            Actor::Token { user_id, scopes, .. } if Scopes::has(scopes, "repo") => *user_id,
            Actor::Token { .. } => return Err(Error::forbidden("This token needs the repo scope to star repositories.")),
            Actor::Anonymous => return Err(Error::unauthorized()),
            _ => return Err(Error::forbidden("Only a person can star a repository.")),
        };
        self.authz.require(actor, id, Permission::View).await?;
        if starred {
            sqlx::query("INSERT INTO stars (user_id, repo_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                .bind(user.0)
                .bind(id.0)
                .execute(&self.db)
                .await?;
        } else {
            sqlx::query("DELETE FROM stars WHERE user_id = $1 AND repo_id = $2").bind(user.0).bind(id.0).execute(&self.db).await?;
        }
        Ok(())
    }

    /// Direct forks the actor can read, newest first. The parent must be readable too.
    pub(crate) async fn do_forks(&self, actor: &Actor, id: RepoId) -> Result<Vec<Repo>> {
        self.do_get(actor, id).await?;
        let ids: Vec<(i64,)> =
            sqlx::query_as("SELECT id FROM repositories WHERE fork_parent_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1000")
                .bind(id.0)
                .fetch_all(&self.db)
                .await?;
        let mut visible = Vec::new();
        for (fid,) in ids {
            if let Ok(r) = self.do_get(actor, RepoId(fid)).await {
                visible.push(r);
            }
        }
        Ok(visible)
    }

    pub(crate) async fn do_fork(
        &self,
        actor: &Actor,
        id: RepoId,
        into: UserId,
        new_name: Option<&str>,
        client: &ClientInfo,
    ) -> Result<Repo> {
        self.do_fork_as(actor, id, into, new_name, Visibility::Private, client).await
    }

    pub(crate) async fn do_fork_as(
        &self,
        actor: &Actor,
        id: RepoId,
        into: UserId,
        new_name: Option<&str>,
        visibility: Visibility,
        client: &ClientInfo,
    ) -> Result<Repo> {
        let source = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Read).await?;
        if visibility > source.visibility || visibility == Visibility::Public {
            return Err(if visibility == Visibility::Public {
                Error::forbidden(crate::tier::MAKE_PUBLIC_IN_BROWSER)
            } else {
                Error::invalid("Repository", "visibility", crate::tier::FORK_CAP)
            });
        }
        self.may_create_under(actor, into).await?;
        let name = new_name.unwrap_or(&source.name).to_string();
        check_repo_name(&name)?;
        let (root,): (Option<i64>,) =
            sqlx::query_as("SELECT fork_root_id FROM repositories WHERE id = $1").bind(id.0).fetch_one(&self.db).await?;
        let root_id = RepoId(root.unwrap_or(id.0));
        let inserted: std::result::Result<(i64,), sqlx::Error> = sqlx::query_as(
            "INSERT INTO repositories (owner_id, name, description, access_tier, default_branch, fork_parent_id, fork_root_id, is_empty) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(into.0)
        .bind(&name)
        .bind(&source.description)
        .bind(visibility.as_str())
        .bind(&source.default_branch)
        .bind(id.0)
        .bind(root_id.0)
        .bind(source.is_empty)
        .fetch_one(&self.db)
        .await;
        let new_id = match inserted {
            Ok((v,)) => RepoId(v),
            Err(e) if is_unique_violation(&e) => return Err(already_exists()),
            Err(e) => return Err(e.into()),
        };
        let made = self.populate_fork(&source, new_id).await;
        if let Err(e) = made {
            let _ = sqlx::query("DELETE FROM repositories WHERE id = $1").bind(new_id.0).execute(&self.db).await;
            let _ = self.store.trash(new_id);
            return Err(e);
        }
        let mut tx = self.db.begin().await?;
        let repo = self.repo_row(new_id).await?;
        self.audit
            .record(&mut tx, client_audit(actor, client, audit_action::REPO_FORKED, &repo, serde_json::json!({"from": source.full_name()})))
            .await?;
        self.events.emit(&mut tx, DomainEvent::RepoCreated { repo_id: new_id, actor: actor.user_id() }).await?;
        tx.commit().await?;
        Ok(repo)
    }

    async fn populate_fork(&self, source: &Repo, new_id: RepoId) -> Result<()> {
        self.store.init_bare(new_id, &source.default_branch).await?;
        if source.is_empty {
            return Ok(());
        }
        let src = self.store.repo_path(source.id);
        let fetch = self
            .git(new_id)
            .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"])
            .arg(&src)
            .args(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"])
            .timeout(std::time::Duration::from_secs(600));
        self.exec(fetch).await?;
        if alternates_of(&self.store.repo_path(new_id)).is_some() {
            return Err(Error::internal(anyhow::anyhow!("a new fork must not borrow objects")));
        }
        Ok(())
    }

    /// The personal owner, an owner of the owning org, or a site admin with two-factor.
    pub(crate) async fn must_be_account_owner(&self, actor: &Actor, repo: &Repo) -> Result<()> {
        if actor.is_system() || actor.is_site_admin() || (actor.user_id() == Some(repo.owner_id) && !actor.is_app()) {
            return Ok(());
        }
        let (kind, _) = self.user_kind(repo.owner_id).await?;
        if kind == UserKind::Org && self.authz.org_role(actor, OrgId(repo.owner_id.0)).await? == Some(OrgRole::Owner) {
            return Ok(());
        }
        Err(Error::forbidden("Only the owner of this repository's account can do that."))
    }

    // ----------------------------------------------------------------- collaborators

    pub(crate) async fn do_collaborators(&self, actor: &Actor, id: RepoId) -> Result<Vec<Collaborator>> {
        let repo = self.repo_row(id).await?;
        self.require(actor, &repo, Permission::Read)
            .await
            .or_else(|e| if repo.archived && matches!(e, Error::Forbidden(_)) { Ok(()) } else { Err(e) })?;
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT c.user_id, u.username::text, c.permission FROM repo_collaborators c JOIN users u ON u.id = c.user_id \
             WHERE c.repo_id = $1 ORDER BY u.username",
        )
        .bind(id.0)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(uid, username, perm)| Collaborator {
                user_id: UserId(uid),
                username,
                permission: Permission::parse(&perm).unwrap_or(Permission::Read),
            })
            .collect())
    }

    pub(crate) async fn do_set_collaborator(
        &self,
        actor: &Actor,
        id: RepoId,
        user: UserId,
        permission: Permission,
        client: &ClientInfo,
    ) -> Result<()> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Admin).await?;
        if permission == Permission::None {
            return self.do_remove_collaborator(actor, id, user, client).await;
        }
        let (kind, username) = self.user_kind(user).await?;
        if kind == UserKind::Org {
            return Err(Error::invalid("Collaborator", "user", "an organization cannot be a collaborator"));
        }
        if user == repo.owner_id {
            return Err(Error::invalid("Collaborator", "user", "the owner already has full access"));
        }
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, $3) \
             ON CONFLICT (repo_id, user_id) DO UPDATE SET permission = EXCLUDED.permission",
        )
        .bind(id.0)
        .bind(user.0)
        .bind(permission.as_str())
        .execute(&mut *tx)
        .await?;
        self.audit
            .record(
                &mut tx,
                client_audit(
                    actor,
                    client,
                    audit_action::COLLABORATOR_ADDED,
                    &repo,
                    serde_json::json!({"user": username, "permission": permission.as_str()}),
                ),
            )
            .await?;
        self.events
            .emit(&mut tx, DomainEvent::MemberAdded { repo_id: Some(id), org_id: None, user_id: user, actor: actor.user_id() })
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn do_remove_collaborator(&self, actor: &Actor, id: RepoId, user: UserId, client: &ClientInfo) -> Result<()> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Admin).await?;
        let mut tx = self.db.begin().await?;
        let gone = sqlx::query("DELETE FROM repo_collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(id.0)
            .bind(user.0)
            .execute(&mut *tx)
            .await?;
        if gone.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.audit
            .record(&mut tx, client_audit(actor, client, audit_action::COLLABORATOR_REMOVED, &repo, serde_json::json!({"user_id": user.0})))
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ----------------------------------------------------------------- protections

    pub(crate) async fn do_protections(&self, actor: &Actor, id: RepoId) -> Result<Vec<BranchProtection>> {
        let repo = self.repo_row(id).await?;
        self.require(actor, &repo, Permission::Read)
            .await
            .or_else(|e| if repo.archived && matches!(e, Error::Forbidden(_)) { Ok(()) } else { Err(e) })?;
        self.protections_of(id).await
    }

    pub(crate) async fn do_set_protection(&self, actor: &Actor, id: RepoId, p: BranchProtection, client: &ClientInfo) -> Result<()> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Maintain).await?;
        let pattern = p.pattern.trim();
        if pattern.is_empty() || pattern.len() > 200 || pattern.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::invalid("BranchProtection", "pattern", "use a branch name or a pattern like release/*"));
        }
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO branch_protections (repo_id, pattern, required_approvals, dismiss_stale_reviews, require_code_owner_review, \
             required_checks, strict, require_resolved_threads, block_force_push, block_deletion, allow_direct_push) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (repo_id, pattern) DO UPDATE SET required_approvals = EXCLUDED.required_approvals, \
             dismiss_stale_reviews = EXCLUDED.dismiss_stale_reviews, require_code_owner_review = EXCLUDED.require_code_owner_review, \
             required_checks = EXCLUDED.required_checks, strict = EXCLUDED.strict, require_resolved_threads = EXCLUDED.require_resolved_threads, \
             block_force_push = EXCLUDED.block_force_push, block_deletion = EXCLUDED.block_deletion, allow_direct_push = EXCLUDED.allow_direct_push",
        )
        .bind(id.0)
        .bind(pattern)
        .bind(p.required_approvals as i32)
        .bind(p.dismiss_stale_reviews)
        .bind(p.require_code_owner_review)
        .bind(&p.required_checks)
        .bind(p.strict)
        .bind(p.require_resolved_threads)
        .bind(p.block_force_push)
        .bind(p.block_deletion)
        .bind(p.allow_direct_push)
        .execute(&mut *tx)
        .await?;
        self.audit
            .record(
                &mut tx,
                client_audit(actor, client, action::BRANCH_PROTECTION, &repo, serde_json::json!({"pattern": pattern, "change": "set"})),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn do_remove_protection(&self, actor: &Actor, id: RepoId, pattern: &str, client: &ClientInfo) -> Result<()> {
        let repo = self.repo_row(id).await?;
        self.authz.require(actor, id, Permission::Maintain).await?;
        let mut tx = self.db.begin().await?;
        let gone = sqlx::query("DELETE FROM branch_protections WHERE repo_id = $1 AND pattern = $2")
            .bind(id.0)
            .bind(pattern)
            .execute(&mut *tx)
            .await?;
        if gone.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.audit
            .record(
                &mut tx,
                client_audit(actor, client, action::BRANCH_PROTECTION, &repo, serde_json::json!({"pattern": pattern, "change": "removed"})),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ----------------------------------------------------------------- settings

    pub(crate) async fn do_update_settings(&self, actor: &Actor, id: RepoId, u: RepoSettingsUpdate, client: &ClientInfo) -> Result<Repo> {
        let repo = self.repo_row(id).await?;
        let needs_admin = u.visibility.is_some() || u.archived.is_some();
        self.authz.require(actor, id, if needs_admin { Permission::Admin } else { Permission::Maintain }).await?;
        if repo.archived && u.archived != Some(false) {
            return Err(Error::forbidden("This repository is archived and read-only."));
        }
        let topics = match &u.topics {
            None => None,
            Some(list) => {
                let mut clean: Vec<String> = list.iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
                clean.sort();
                clean.dedup();
                if clean.len() > 20 || clean.iter().any(|t| !topic_re().is_match(t)) {
                    return Err(Error::invalid("Repository", "topics", "up to 20 topics of lowercase letters, numbers and dashes"));
                }
                Some(clean)
            }
        };
        let mut new_default = None;
        if let Some(b) = &u.default_branch
            && *b != repo.default_branch
        {
            self.check_branch_name(b).await?;
            if !repo.is_empty {
                let found =
                    self.read_refs(id, Some(&format!("refs/heads/{b}"))).await?.into_iter().any(|r| r.name == format!("refs/heads/{b}"));
                if !found {
                    return Err(Error::invalid("Repository", "default_branch", "that branch does not exist"));
                }
            }
            new_default = Some(b.clone());
        }
        if let Some(d) = &u.description
            && d.len() > 1000
        {
            return Err(Error::invalid("Repository", "description", "keep it under 1000 characters"));
        }
        // The tier changes only through `set_tier`, which has its own rules and audit. A
        // narrowing goes first, so it holds even if the rest fails. A widening goes last.
        let tier = u.visibility.filter(|v| *v != repo.visibility);
        if let Some(v) = tier
            && v < repo.visibility
        {
            self.do_set_tier(actor, id, v, client).await?;
        }
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "UPDATE repositories SET description = COALESCE($2, description), website = COALESCE($3, website), \
             default_branch = COALESCE($4, default_branch), archived = COALESCE($5, archived), \
             topics = COALESCE($6, topics), has_issues = COALESCE($7, has_issues), has_pulls = COALESCE($8, has_pulls), \
             has_actions = COALESCE($9, has_actions), allow_merge_commit = COALESCE($10, allow_merge_commit), \
             allow_squash_merge = COALESCE($11, allow_squash_merge), allow_rebase_merge = COALESCE($12, allow_rebase_merge), \
             delete_branch_on_merge = COALESCE($13, delete_branch_on_merge), updated_at = now() WHERE id = $1",
        )
        .bind(id.0)
        .bind(u.description.as_deref())
        .bind(u.website.as_deref())
        .bind(new_default.as_deref())
        .bind(u.archived)
        .bind(topics.as_deref())
        .bind(u.has_issues)
        .bind(u.has_pulls)
        .bind(u.has_actions)
        .bind(u.allow_merge_commit)
        .bind(u.allow_squash_merge)
        .bind(u.allow_rebase_merge)
        .bind(u.delete_branch_on_merge)
        .execute(&mut *tx)
        .await?;
        if let Some(a) = u.archived
            && a != repo.archived
        {
            self.audit
                .record(&mut tx, client_audit(actor, client, audit_action::REPO_ARCHIVED, &repo, serde_json::json!({"archived": a})))
                .await?;
        }
        self.audit
            .record(
                &mut tx,
                client_audit(actor, client, audit_action::REPO_SETTINGS, &repo, serde_json::json!({"default_branch": new_default})),
            )
            .await?;
        tx.commit().await?;
        if let Some(b) = new_default {
            self.exec(self.git(id).args(["symbolic-ref", "HEAD", &format!("refs/heads/{b}")])).await?;
        }
        if let Some(v) = tier
            && v > repo.visibility
        {
            self.do_set_tier(actor, id, v, client).await?;
        }
        self.repo_row(id).await
    }
}
