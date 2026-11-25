//! The merge gate loader, merging, "update branch" and auto-merge.

use crate::codeowners::{self, Owner};
use crate::core::{Core, IssueRow};
use crate::gate::{self, GateInput, ReviewFact};
use crate::protect::{self, Rules};
use crate::pulls::{PullRow, full_branch, method_str};
use crate::util::{as_user, writing_user};
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::text;
use ch_core::{Actor, Error, FieldError, IssueId, Permission, PullId, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashMap;

type ReviewFactRow = (i64, UserId, String, String, String, Option<DateTime<Utc>>);

const CODEOWNERS_PATHS: &[&str] = &[".codehaven/CODEOWNERS", ".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

impl Core {
    // ------------------------------------------------------------ the gate

    /// Reads CODEOWNERS from the base branch. Missing is not an error: it means no owners.
    pub async fn read_codeowners(&self, repo: RepoId, base_ref: &str) -> Vec<codeowners::Rule> {
        for path in CODEOWNERS_PATHS {
            if let Ok(blob) = self.d.git.blob(repo, &full_branch(base_ref), path, 256 * 1024).await
                && !blob.is_binary
            {
                return codeowners::parse(&String::from_utf8_lossy(&blob.content));
            }
        }
        Vec::new()
    }

    async fn review_facts(&self, issue: &IssueRow, base_repo: RepoId) -> Result<Vec<ReviewFact>> {
        let rows: Vec<ReviewFactRow> = sqlx::query_as(
            "SELECT r.id, r.author_id, u.username::text, r.state, r.commit_sha, r.submitted_at
             FROM reviews r JOIN users u ON u.id = r.author_id WHERE r.pull_id = $1 AND r.state <> 'pending' ORDER BY r.submitted_at, r.id",
        )
        .bind(issue.id)
        .fetch_all(self.db())
        .await?;
        let mut perms: HashMap<UserId, bool> = HashMap::new();
        let mut out = Vec::with_capacity(rows.len());
        for (id, reviewer, name, state, commit_sha, submitted_at) in rows {
            let counts = match perms.get(&reviewer) {
                Some(c) => *c,
                None => {
                    let c = self.permission(&as_user(reviewer), base_repo).await? >= Permission::Write;
                    perms.insert(reviewer, c);
                    c
                }
            };
            out.push(ReviewFact { id, reviewer, name, state: crate::reviews::parse_state(&state), commit_sha, submitted_at, counts });
        }
        Ok(out)
    }

    /// Only a team of the organization that owns the repository counts as a code owner. A team
    /// of any other organization is neither looked up nor shown (ch-uph).
    async fn is_team_member(&self, repo: RepoId, user: UserId, org: &str, team: &str) -> Result<bool> {
        let n: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM team_members tm JOIN teams t ON t.id = tm.team_id JOIN users o ON o.id = t.org_id \
             WHERE o.username = $1::citext AND t.name = $2::citext AND tm.user_id = $3 \
               AND t.org_id = (SELECT owner_id FROM repositories WHERE id = $4)",
        )
        .bind(org)
        .bind(team)
        .bind(user)
        .bind(repo)
        .fetch_optional(self.db())
        .await?;
        Ok(n.is_some())
    }

    /// The code owner groups of this pull request that have no counting approval yet.
    async fn unowned_reviews(&self, issue: &IssueRow, pull: &PullRow, approvers: &[UserId]) -> Result<Vec<String>> {
        let rules = self.read_codeowners(pull.repo_id, &pull.base_ref).await;
        if rules.is_empty() {
            return Ok(Vec::new());
        }
        let paths = self.pull_diff_paths(issue, pull).await?;
        let mut groups: Vec<(usize, &codeowners::Rule)> = Vec::new();
        for p in &paths {
            if let Some(rule) = codeowners::owners_for(&rules, p)
                && !groups.iter().any(|(l, _)| *l == rule.line)
            {
                groups.push((rule.line, rule));
            }
        }
        let approver_refs = self.user_refs(approvers).await?;
        let mut out = Vec::new();
        for (_, rule) in groups {
            // Owners who wrote the pull request cannot approve it, so they cannot be required.
            let mut owners: Vec<&Owner> = Vec::new();
            for o in &rule.owners {
                match o {
                    Owner::User(n) if self.user_id_by_name(n).await? == Some(issue.author_id) => {}
                    Owner::Email(_) => {}
                    other => owners.push(other),
                }
            }
            if owners.is_empty() {
                continue;
            }
            let mut ok = false;
            for o in &owners {
                for a in approvers {
                    let Some(u) = approver_refs.get(a) else { continue };
                    ok = match o {
                        Owner::User(n) => n.eq_ignore_ascii_case(&u.username),
                        Owner::Team { org, team } => self.is_team_member(pull.repo_id, *a, org, team).await?,
                        Owner::Email(_) => false,
                    };
                    if ok {
                        break;
                    }
                }
                if ok {
                    break;
                }
            }
            if !ok {
                let names: Vec<String> = owners
                    .iter()
                    .map(|o| match o {
                        Owner::User(n) => format!("@{n}"),
                        Owner::Team { org, team } => format!("@{org}/{team}"),
                        Owner::Email(e) => e.clone(),
                    })
                    .collect();
                out.push(format!("{} ({})", rule.pattern, names.join(", ")));
            }
        }
        Ok(out)
    }

    pub async fn rules_for_pull(&self, pull: &PullRow) -> Result<Option<Rules>> {
        // Fail closed: if protections cannot be read, the merge does not go ahead.
        let prots = self.d.repos.protections(&Actor::System, pull.repo_id).await?;
        Ok(protect::rules_for(&prots, &pull.base_ref))
    }

    /// Gathers every fact the gate needs and evaluates it.
    pub async fn gate_for(&self, issue: &IssueRow, pull: &PullRow, method: Option<MergeMethod>) -> Result<GateReport> {
        let repo = self.repo(pull.repo_id).await?;
        let rules = self.rules_for_pull(pull).await?;
        let facts = self.review_facts(issue, pull.repo_id).await?;
        let dismiss_stale = rules.as_ref().is_some_and(|r| r.dismiss_stale_reviews);
        let eff = gate::effective(&facts, &pull.head_sha, dismiss_stale, issue.author_id);

        let mut failing = Vec::new();
        let mut unavailable = false;
        let mut check_names = Vec::new();
        if let Some(r) = &rules {
            let checks = self.d.checks();
            for c in &r.required_checks {
                match checks.required_satisfied(pull.repo_id, &pull.head_sha, std::slice::from_ref(c)).await {
                    Ok(true) => {}
                    Ok(false) => failing.push(c.clone()),
                    Err(e) => {
                        tracing::warn!(error = %e, "could not read required checks");
                        unavailable = true;
                    }
                }
            }
            if !failing.is_empty() {
                // Only for the wording: a failure here leaves the plain message.
                if let Ok(runs) = checks.check_runs_for_sha(pull.repo_id, &pull.head_sha).await {
                    check_names.extend(runs.into_iter().map(|c| c.name));
                }
                if let Ok(sts) = checks.statuses_for_sha(pull.repo_id, &pull.head_sha).await {
                    check_names.extend(sts.into_iter().map(|s| s.context));
                }
            }
        }
        let unowned = match &rules {
            Some(r) if r.require_code_owner_review => self.unowned_reviews(issue, pull, &eff.approvers).await?,
            _ => Vec::new(),
        };
        let threads: i64 = if rules.as_ref().is_some_and(|r| r.require_resolved_threads) {
            sqlx::query_scalar(
                "SELECT count(*) FROM review_threads t WHERE t.pull_id = $1 AND NOT t.resolved AND EXISTS (
                     SELECT 1 FROM review_comments c LEFT JOIN reviews r ON r.id = c.review_id WHERE c.thread_id = t.id AND (r.id IS NULL OR r.state <> 'pending'))",
            )
            .bind(issue.id)
            .fetch_one(self.db())
            .await?
        } else {
            0
        };
        let allowed = |m: MergeMethod| match m {
            MergeMethod::Merge => repo.allow_merge_commit,
            MergeMethod::Squash => repo.allow_squash_merge,
            MergeMethod::Rebase => repo.allow_rebase_merge,
        };
        let mut report = gate::evaluate(&GateInput {
            open: issue.open,
            merged: pull.merged_at.is_some(),
            draft: pull.draft,
            mergeable: pull.mergeable(),
            base_ref: pull.base_ref.clone(),
            method: method.map(|m| (m, allowed(m))),
            rules,
            effective: eff,
            failing_checks: failing,
            checks_unavailable: unavailable,
            check_names,
            unowned_reviews: unowned,
            unresolved_threads: threads as u32,
            head_gone: pull.head_gone(),
        });
        // A head its author did not agree to publish does not go into a public base (ch-4pn.14).
        if issue.open && pull.merged_at.is_none() && self.head_withheld(issue.id, pull).await? {
            report.mergeable = false;
            report.reasons.push(gate::HEAD_NOT_PUBLISHED.into());
        }
        Ok(report)
    }

    pub async fn pull_merge_gate(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<GateReport> {
        self.need_read(actor, repo).await?;
        let (issue, mut pull) = self.pull_pair(repo, number).await?;
        if issue.open && pull.mergeable() == MergeableState::Unknown {
            self.refresh_pull(&issue, &pull).await;
            pull = self.pull_row(issue.id).await?;
        }
        self.gate_for(&issue, &pull, None).await
    }

    // ------------------------------------------------------------ merging

    pub async fn pull_merge(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        method: MergeMethod,
        client: &ClientInfo,
    ) -> Result<PullRequest> {
        let opts = MergeOptions { method: Some(method), ..Default::default() };
        Ok(self.pull_merge_with(actor, repo, number, opts, client).await?.pull)
    }

    pub async fn pull_merge_with(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        options: MergeOptions,
        client: &ClientInfo,
    ) -> Result<PullView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        let (issue, _) = self.pull_pair(repo, number).await?;
        self.merge_pull(actor, user, issue.id, options, client).await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        self.hydrate_pull(issue, pull).await
    }

    /// The default method: the first one the repository allows, merge commit first.
    fn default_method(repo: &crate::core::RepoRow) -> Option<MergeMethod> {
        if repo.allow_merge_commit {
            Some(MergeMethod::Merge)
        } else if repo.allow_squash_merge {
            Some(MergeMethod::Squash)
        } else if repo.allow_rebase_merge {
            Some(MergeMethod::Rebase)
        } else {
            None
        }
    }

    /// Does the merge in `merger`'s name. The caller has checked permission.
    pub async fn merge_pull(
        &self,
        actor: &Actor,
        merger: UserId,
        issue_id: IssueId,
        options: MergeOptions,
        client: &ClientInfo,
    ) -> Result<()> {
        let issue = self.issue_row_by_id(issue_id).await?;
        let mut pull = self.pull_row(issue_id).await?;
        let repo = self.repo(pull.repo_id).await?;
        Self::ensure_writable(&repo)?;
        let method = match options.method.or_else(|| Self::default_method(&repo)) {
            Some(m) => m,
            None => return Err(Error::MergeBlocked(vec!["Every merge method is turned off for this repository".into()])),
        };
        if issue.open && pull.merged_at.is_none() {
            // Decide on what the branches are now, not on what we saw when the page loaded.
            self.refresh_pull(&issue, &pull).await;
            pull = self.pull_row(issue_id).await?;
        }
        if let Some(expected) = &options.expected_head_sha
            && *expected != pull.head_sha
        {
            return Err(Error::conflict("The head branch was modified. Review the changes and try again"));
        }
        let gate = self.gate_for(&issue, &pull, Some(method)).await?;
        if !gate.mergeable {
            return Err(Error::MergeBlocked(gate.reasons));
        }

        let (title, message) = self.merge_text(&issue, &pull, method, &options).await?;
        let (name, email) = self.identity(merger).await?;
        let now = Utc::now();
        let committer = Signature { name: name.clone(), email: email.clone(), at: now };
        let author = if method == MergeMethod::Squash {
            let (n, e) = self.identity(issue.author_id).await?;
            Signature { name: n, email: e, at: now }
        } else {
            committer.clone()
        };
        let req = MergeRequest {
            repo_id: pull.repo_id,
            base_ref: full_branch(&pull.base_ref),
            // The gate refuses a pull request whose fork is gone. This is the backstop, so the
            // base's branch of the same name is never merged in its place.
            head_repo_id: pull.head_repo().ok_or_else(|| Error::MergeBlocked(vec![gate::HEAD_GONE.into()]))?,
            head_ref: full_branch(&pull.head_ref),
            method,
            title: title.clone(),
            message: message.clone(),
            author,
            committer,
            expected_base_sha: pull.base_sha.clone(),
            update_branch: false,
            // The head the gate just passed, and nothing pushed after it (ch-oz8).
            expected_head_sha: Some(pull.head_sha.clone()),
        };
        let outcome = self.d.git.merge(actor, req, client).await?;
        let sha = match outcome {
            MergeOutcome::Merged { sha } => sha,
            MergeOutcome::Conflict { paths } => {
                sqlx::query("UPDATE pull_requests SET mergeable_state = 'conflict' WHERE issue_id = $1")
                    .bind(issue_id)
                    .execute(self.db())
                    .await?;
                let mut why = String::from("There are merge conflicts to resolve");
                if !paths.is_empty() {
                    why.push_str(&format!(": {}", paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ")));
                    if paths.len() > 5 {
                        why.push_str(&format!(" and {} more", paths.len() - 5));
                    }
                }
                return Err(Error::MergeBlocked(vec![why]));
            }
            MergeOutcome::BaseMoved { .. } => {
                // The base changed between our check and the update. Nothing was written.
                return Err(Error::conflict("The base branch was modified. Try again"));
            }
        };

        let mut tx = self.db().begin().await?;
        sqlx::query(
            "UPDATE issues SET open = false, state_reason = NULL, closed_at = now(), closed_by = $2, updated_at = now() WHERE id = $1",
        )
        .bind(issue_id)
        .bind(merger)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE pull_requests SET merged_at = now(), merged_by = $2, merge_commit_sha = $3, merge_method = $4, auto_merge_method = NULL, auto_merge_by = NULL,
                    mergeable_state = 'clean' WHERE issue_id = $1",
        )
        .bind(issue_id)
        .bind(merger)
        .bind(&sha)
        .bind(method_str(method))
        .execute(&mut *tx)
        .await?;
        Self::add_event(
            &mut tx,
            issue_id,
            Some(merger),
            "merged",
            json!({ "sha": sha, "method": method_str(method), "base": pull.base_ref }),
        )
        .await?;
        self.d
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::PULL_MERGED, Outcome::Success)
                    .target("pull", issue_id)
                    .details(json!({ "repo_id": pull.repo_id.0, "number": issue.number, "method": method_str(method), "sha": sha })),
            )
            .await?;
        let pid = PullId(issue_id.0);
        self.d
            .events
            .emit(
                &mut tx,
                DomainEvent::PrClosed { repo_id: pull.repo_id, pull_id: pid, number: issue.number, merged: true, actor: Some(merger) },
            )
            .await?;
        self.d
            .events
            .emit(
                &mut tx,
                DomainEvent::PrMerged { repo_id: pull.repo_id, pull_id: pid, number: issue.number, merge_sha: sha.clone(), actor: merger },
            )
            .await?;
        tx.commit().await?;

        // Housekeeping. None of it can undo the merge, so failures are logged and ignored.
        self.after_merge(&repo, &issue, &pull, merger, &options, &message, &title, actor.is_app()).await;
        Ok(())
    }

    async fn merge_text(&self, issue: &IssueRow, pull: &PullRow, method: MergeMethod, o: &MergeOptions) -> Result<(String, String)> {
        let head_label = match pull.head_repo_id {
            Some(h) if h != pull.repo_id => {
                let r = self.repo(h).await.ok();
                format!("{}:{}", r.map(|r| r.owner_name).unwrap_or_default(), pull.head_ref)
            }
            _ => pull.head_ref.clone(),
        };
        let (default_title, default_body) = match method {
            MergeMethod::Merge => (format!("Merge pull request #{} from {}", issue.number, head_label), issue.title.clone()),
            MergeMethod::Squash => {
                let commits = self.pull_compare_commits(issue, pull).await.unwrap_or_default();
                let body =
                    commits.iter().map(|c| format!("* {}", c.message.lines().next().unwrap_or("").trim())).collect::<Vec<_>>().join("\n");
                (format!("{} (#{})", issue.title, issue.number), body)
            }
            MergeMethod::Rebase => (issue.title.clone(), String::new()),
        };
        let title = o.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or(default_title);
        let message = o.message.clone().unwrap_or(default_body);
        Ok((title, message))
    }

    async fn pull_compare_commits(&self, _issue: &IssueRow, pull: &PullRow) -> Result<Vec<CommitInfo>> {
        Ok(self.compare_pull(pull, &pull.base_sha, &pull.head_sha).await?.commits)
    }

    #[allow(clippy::too_many_arguments)]
    async fn after_merge(
        &self,
        repo: &crate::core::RepoRow,
        issue: &IssueRow,
        pull: &PullRow,
        merger: UserId,
        options: &MergeOptions,
        message: &str,
        title: &str,
        app: bool,
    ) {
        // Delete the head branch when asked, or when the repository always does. Only a branch
        // of this same repository: a fork's branch is not here, and once a fork is deleted the
        // branch of the same name here is someone else's (`is_fork` stays true for it).
        let delete = options.delete_branch.unwrap_or(repo.delete_branch_on_merge) && !app;
        if delete && !pull.is_fork() && pull.head_ref != repo.default_branch {
            let protected = self
                .d
                .repos
                .protections(&Actor::System, repo.id)
                .await
                .map(|p| protect::rules_for(&p, &pull.head_ref).is_some())
                .unwrap_or(true);
            let others = self.open_pulls_by_head(repo.id, &pull.head_ref).await.map(|v| v.len()).unwrap_or(1);
            if !protected && others == 0 {
                match self.d.git.delete_ref(&as_user(merger), repo.id, &full_branch(&pull.head_ref), &ClientInfo::local()).await {
                    Ok(()) => {
                        let mut tx = match self.db().begin().await {
                            Ok(t) => t,
                            Err(_) => return,
                        };
                        let _ = Self::add_event(&mut tx, issue.id, Some(merger), "head_ref_deleted", json!({ "ref": pull.head_ref })).await;
                        let _ = tx.commit().await;
                    }
                    Err(e) => tracing::warn!(error = %e, "could not delete the head branch after a merge"),
                }
            }
        }
        // Issues this pull request closes, when it lands on the default branch.
        if pull.base_ref == repo.default_branch {
            let mut texts = vec![issue.title.clone(), issue.body.clone(), message.to_string(), title.to_string()];
            if let Ok(commits) = self.pull_compare_commits(issue, pull).await {
                texts.extend(commits.into_iter().map(|c| c.message));
            }
            self.close_by_keywords(repo, merger, &texts, json!({ "pull": issue.number }), app).await;
        }
        // Other pull requests aimed at the same branch may now conflict or be behind.
        if let Ok(others) = self.open_pulls_by_base(repo.id, &pull.base_ref).await {
            for (i, p) in others.iter().filter(|(i, _)| i.id != issue.id) {
                self.refresh_pull(i, p).await;
            }
        }
    }

    /// Closes the open issues named after a closing keyword in `texts`. Pull requests are never
    /// closed this way, and an issue in another repository only if the closer has write access.
    pub async fn close_by_keywords(
        &self,
        repo: &crate::core::RepoRow,
        closer: UserId,
        texts: &[String],
        via: serde_json::Value,
        app: bool,
    ) {
        let mut seen: Vec<(RepoId, i64)> = Vec::new();
        for t in texts {
            for r in text::refs(t).closes {
                let target_repo = match &r.repo {
                    None => repo.id,
                    Some((o, n)) => match self.repo_by_name(o, n).await {
                        Ok(Some(x)) => x.id,
                        _ => continue,
                    },
                };
                if seen.contains(&(target_repo, r.number)) {
                    continue;
                }
                seen.push((target_repo, r.number));
                // A change made by a connected app never closes an issue in another repository: the
                // person's own write access there is not what the app was given.
                if target_repo != repo.id
                    && (app || self.permission(&as_user(closer), target_repo).await.unwrap_or(Permission::None) < Permission::Write)
                {
                    continue;
                }
                let Ok(row) = self.issue_row(target_repo, r.number).await else { continue };
                if row.is_pull || !row.open {
                    continue;
                }
                let Ok(mut tx) = self.db().begin().await else { continue };
                match self.set_open_in_tx(&mut tx, closer, &row, false, Some("completed"), Some(via.clone())).await {
                    Ok(_) => {
                        if let Err(e) = tx.commit().await {
                            tracing::warn!(error = %e, "could not close an issue by keyword");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "could not close an issue by keyword"),
                }
            }
        }
    }

    // ------------------------------------------------------------ update branch

    pub async fn pull_update_branch(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Read).await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        if !issue.open || pull.merged_at.is_some() {
            return Err(Error::invalid("PullRequest", "state", "Only an open pull request can be updated"));
        }
        // Writing to the head branch needs write access there.
        let Some(head_repo) = pull.head_repo() else {
            return Err(Error::invalid("PullRequest", "head", "The repository this pull request came from was deleted"));
        };
        let allowed = self.permission(actor, head_repo).await? >= Permission::Write
            || (pull.is_fork()
                && pull.maintainer_can_modify
                && self.permission(actor, repo).await? >= Permission::Write
                && self.permission(actor, head_repo).await? >= Permission::Read);
        if !allowed {
            return Err(Error::forbidden("You need write access to the head branch to update it"));
        }
        // The update is a direct write into the head branch, with no merge gate in front of it, so
        // a head branch that refuses direct pushes refuses this too (ch-jnm). Fail closed when the
        // protections cannot be read. The git layer checks the same rule again.
        let head_rules = protect::rules_for(&self.d.repos.protections(&Actor::System, head_repo).await?, &pull.head_ref);
        if head_rules.is_some_and(|r| !r.allow_direct_push) {
            return Err(Error::forbidden(format!("Branch '{}' is protected. Open a pull request to change it.", pull.head_ref)));
        }
        let base_sha = self.d.git.resolve(repo, &full_branch(&pull.base_ref)).await?;
        let head_sha = self.d.git.resolve(head_repo, &full_branch(&pull.head_ref)).await?;
        let (name, email) = self.identity(user).await?;
        let sig = Signature { name, email, at: Utc::now() };
        let req = MergeRequest {
            repo_id: head_repo,
            base_ref: full_branch(&pull.head_ref),
            head_repo_id: repo,
            head_ref: full_branch(&pull.base_ref),
            method: MergeMethod::Merge,
            title: format!("Merge branch '{}' into {}", pull.base_ref, pull.head_ref),
            message: String::new(),
            author: sig.clone(),
            committer: sig,
            expected_base_sha: head_sha.clone(),
            update_branch: true,
            expected_head_sha: Some(base_sha.clone()),
        };
        match self.d.git.merge(actor, req, client).await? {
            MergeOutcome::Merged { sha } => {
                let mut tx = self.db().begin().await?;
                sqlx::query("UPDATE pull_requests SET head_sha = $2, synced_head_sha = $2, base_sha = $3 WHERE issue_id = $1")
                    .bind(issue.id)
                    .bind(&sha)
                    .bind(&base_sha)
                    .execute(&mut *tx)
                    .await?;
                // Armed auto-merge follows the new head only when the person updating it may
                // write to the base; otherwise it is turned off (ch-m2y).
                if pull.auto_merge_method.is_some() {
                    if self.permission(actor, repo).await? >= Permission::Write {
                        sqlx::query("UPDATE pull_requests SET auto_merge_head_sha = $2 WHERE issue_id = $1")
                            .bind(issue.id)
                            .bind(&sha)
                            .execute(&mut *tx)
                            .await?;
                    } else {
                        sqlx::query(
                            "UPDATE pull_requests SET auto_merge_method = NULL, auto_merge_by = NULL, auto_merge_head_sha = NULL WHERE issue_id = $1",
                        )
                        .bind(issue.id)
                        .execute(&mut *tx)
                        .await?;
                        Self::add_event(
                            &mut tx,
                            issue.id,
                            Some(user),
                            "auto_merge_disabled",
                            json!({ "reason": "someone without write access updated the branch" }),
                        )
                        .await?;
                    }
                }
                Self::add_event(
                    &mut tx,
                    issue.id,
                    Some(user),
                    "head_ref_updated",
                    json!({ "before": head_sha, "after": sha, "reason": "update_branch" }),
                )
                .await?;
                self.d
                    .events
                    .emit(
                        &mut tx,
                        DomainEvent::PrSynchronized {
                            repo_id: repo,
                            pull_id: PullId(issue.id.0),
                            number: issue.number,
                            head_sha: sha,
                            app: actor.app_origin(repo),
                            pusher: Some(user),
                            // The server made this exact commit for this person, on top of a
                            // head they did not necessarily write. CI counts them only if that
                            // head was itself let run or approved (ch-x12).
                            origin_known: true,
                            built_on: Some(head_sha.clone()),
                        },
                    )
                    .await?;
                tx.commit().await?;
            }
            MergeOutcome::Conflict { paths } => {
                return Err(Error::MergeBlocked(vec![format!(
                    "The branch cannot be updated automatically. Conflicts in: {}",
                    paths.join(", ")
                )]));
            }
            MergeOutcome::BaseMoved { .. } => return Err(Error::conflict("The head branch was modified. Try again")),
        }
        let issue = self.issue_row_by_id(issue.id).await?;
        let pull = self.pull_row(issue.id).await?;
        self.refresh_pull(&issue, &pull).await;
        let pull = self.pull_row(issue.id).await?;
        self.hydrate_pull(issue, pull).await
    }

    // ------------------------------------------------------------ auto-merge

    pub async fn pull_set_auto_merge(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        method: Option<MergeMethod>,
        client: &ClientInfo,
    ) -> Result<PullView> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        Self::ensure_writable(&self.repo(repo).await?)?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        if !issue.open || pull.merged_at.is_some() {
            return Err(Error::invalid("PullRequest", "auto_merge", "Auto-merge needs an open pull request"));
        }
        let mut tx = self.db().begin().await?;
        match method {
            Some(m) => {
                let report = self.gate_for(&issue, &pull, Some(m)).await?;
                if report.mergeable {
                    return Err(Error::Validation(vec![FieldError::custom(
                        "PullRequest",
                        "auto_merge",
                        "This pull request can already be merged. Merge it now",
                    )]));
                }
                if report
                    .reasons
                    .iter()
                    .any(|r| r.contains("turned off") || r.contains("conflicts") || r.contains("closed") || r.contains("merged"))
                {
                    return Err(Error::MergeBlocked(report.reasons));
                }
                // Armed for the head the person sees now, and no other (ch-m2y).
                sqlx::query(
                    "UPDATE pull_requests SET auto_merge_method = $2, auto_merge_by = $3, auto_merge_head_sha = $4 WHERE issue_id = $1",
                )
                .bind(issue.id)
                .bind(method_str(m))
                .bind(user)
                .bind(&pull.head_sha)
                .execute(&mut *tx)
                .await?;
                Self::add_event(&mut tx, issue.id, Some(user), "auto_merge_enabled", json!({ "method": method_str(m) })).await?;
            }
            None => {
                sqlx::query("UPDATE pull_requests SET auto_merge_method = NULL, auto_merge_by = NULL WHERE issue_id = $1")
                    .bind(issue.id)
                    .execute(&mut *tx)
                    .await?;
                if pull.auto_merge_method.is_some() {
                    Self::add_event(&mut tx, issue.id, Some(user), "auto_merge_disabled", json!({})).await?;
                }
            }
        }
        self.d
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::PULL_AUTO_MERGE, Outcome::Success)
                    .target("pull", issue.id)
                    .details(json!({ "number": number, "method": method.map(method_str) })),
            )
            .await?;
        tx.commit().await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        self.hydrate_pull(issue, pull).await
    }

    /// Merges the pull request if auto-merge is on and the gate is now clear. Returns whether
    /// it merged. A failure that may clear up (a push landed, checks still running) is `false`.
    pub async fn try_auto_merge(&self, issue_id: IssueId) -> Result<bool> {
        let issue = self.issue_row_by_id(issue_id).await?;
        let pull = self.pull_row(issue_id).await?;
        let (Some(method), Some(by)) = (pull.auto_merge_method.as_deref().and_then(crate::pulls::parse_method), pull.auto_merge_by) else {
            return Ok(false);
        };
        if !issue.open || pull.merged_at.is_some() {
            return Ok(false);
        }
        // The person who turned it on must still be allowed to merge.
        if self.permission(&as_user(by), pull.repo_id).await? < Permission::Write {
            sqlx::query("UPDATE pull_requests SET auto_merge_method = NULL, auto_merge_by = NULL WHERE issue_id = $1")
                .bind(issue_id)
                .execute(self.db())
                .await?;
            return Ok(false);
        }
        // Only the head it was armed for, or one a writer pushed since, ever merges (ch-m2y).
        let armed = pull.auto_merge_head_sha.clone().unwrap_or_else(|| pull.head_sha.clone());
        if armed != pull.head_sha {
            return Ok(false);
        }
        let report = self.gate_for(&issue, &pull, Some(method)).await?;
        if !report.mergeable {
            return Ok(false);
        }
        let opts = MergeOptions { method: Some(method), expected_head_sha: Some(armed), ..Default::default() };
        match self.merge_pull(&Actor::System, by, issue_id, opts, &ClientInfo::local()).await {
            Ok(()) => Ok(true),
            Err(Error::MergeBlocked(_)) | Err(Error::Conflict(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}
