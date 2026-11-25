//! Check runs and commit statuses: what shows beside a commit, and what a branch protection
//! rule's "required checks" looks at.

use crate::ctx::Ctx;
use crate::history::{self, Tiers};
use async_trait::async_trait;
use ch_core::events::DomainEvent;
use ch_core::model::{CheckRun, CombinedState, CommitStatus, SeenChecks};
use ch_core::services::Checks;
use ch_core::{Actor, Error, Permission, RepoId, Result};

type CheckTuple = (i64, i64, String, String, String, Option<String>, Option<String>);
type StatusTuple = (i64, i64, String, String, String, Option<String>, Option<String>);

pub struct ChecksImpl {
    pub ctx: Ctx,
}

/// A check run that counts as passing: a skipped or neutral one does not block a merge.
pub fn check_passes(status: &str, conclusion: Option<&str>) -> bool {
    status == "completed" && matches!(conclusion, Some("success" | "neutral" | "skipped"))
}

pub fn valid_state(s: &str) -> bool {
    matches!(s, "pending" | "success" | "failure" | "error")
}

/// Works out the state of a commit from its latest check runs and statuses.
///
/// Anything that failed makes it a failure. Otherwise anything still running makes it pending.
/// A commit with nothing on it is pending, not success, so "no checks yet" is never mistaken
/// for "all checks passed".
pub fn combine(checks: &[CheckRun], statuses: &[CommitStatus]) -> CombinedState {
    let failed = checks.iter().any(|c| {
        c.status == "completed" && matches!(c.conclusion.as_deref(), Some("failure" | "timed_out" | "cancelled" | "action_required"))
    }) || statuses.iter().any(|s| matches!(s.state.as_str(), "failure" | "error"));
    if failed {
        return CombinedState::Failure;
    }
    if checks.is_empty() && statuses.is_empty() {
        return CombinedState::Pending;
    }
    let running = checks.iter().any(|c| c.status != "completed") || statuses.iter().any(|s| s.state == "pending");
    if running { CombinedState::Pending } else { CombinedState::Success }
}

impl ChecksImpl {
    /// The latest attempt of each check name. With tiers, only attempts recorded at one of them
    /// count, so a viewer below Read sees the newest attempt they may see (ch-4pn.12).
    async fn latest_checks(&self, repo: RepoId, sha: &str, tiers: Tiers) -> Result<Vec<CheckRun>> {
        let rows: Vec<CheckTuple> = sqlx::query_as(
            "SELECT DISTINCT ON (name) id, repo_id, head_sha, name, status, conclusion, details_url
             FROM check_runs WHERE repo_id = $1 AND head_sha = $2 AND ($3::text[] IS NULL OR recorded_tier = ANY($3))
             ORDER BY name, id DESC",
        )
        .bind(repo.0)
        .bind(sha)
        .bind(history::bind(tiers))
        .fetch_all(&self.ctx.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, r, head_sha, name, status, conclusion, details_url)| CheckRun {
                id,
                repo_id: RepoId(r),
                head_sha,
                name,
                status,
                conclusion,
                details_url,
            })
            .collect())
    }

    async fn latest_statuses(&self, repo: RepoId, sha: &str, tiers: Tiers) -> Result<Vec<CommitStatus>> {
        let rows: Vec<StatusTuple> = sqlx::query_as(
            "SELECT DISTINCT ON (context) id, repo_id, sha, context, state, description, target_url
             FROM commit_statuses WHERE repo_id = $1 AND sha = $2 AND ($3::text[] IS NULL OR recorded_tier = ANY($3))
             ORDER BY context, created_at DESC, id DESC",
        )
        .bind(repo.0)
        .bind(sha)
        .bind(history::bind(tiers))
        .fetch_all(&self.ctx.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, r, sha, context, state, description, target_url)| CommitStatus {
                id,
                repo_id: RepoId(r),
                sha,
                context,
                state,
                description,
                target_url,
            })
            .collect())
    }

    /// A full sha is used as given. A shorter one must match exactly one commit that has checks
    /// or statuses, so a prefix cannot be used to read another repository's data.
    async fn full_sha(&self, repo: RepoId, sha: &str) -> Result<String> {
        if sha.len() == 40 || sha.len() == 64 {
            return Ok(sha.to_ascii_lowercase());
        }
        if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(sha.to_string());
        }
        let found: Vec<String> = sqlx::query_scalar(
            "SELECT head_sha FROM check_runs WHERE repo_id = $1 AND starts_with(head_sha, $2)
             UNION SELECT sha FROM commit_statuses WHERE repo_id = $1 AND starts_with(sha, $2) LIMIT 2",
        )
        .bind(repo.0)
        .bind(sha.to_ascii_lowercase())
        .fetch_all(&self.ctx.db)
        .await?;
        Ok(if found.len() == 1 { found[0].clone() } else { sha.to_string() })
    }
}

#[async_trait]
impl Checks for ChecksImpl {
    async fn check_runs_for_sha(&self, repo: RepoId, sha: &str) -> Result<Vec<CheckRun>> {
        let sha = self.full_sha(repo, sha).await?;
        self.latest_checks(repo, &sha, None).await
    }

    async fn statuses_for_sha(&self, repo: RepoId, sha: &str) -> Result<Vec<CommitStatus>> {
        let sha = self.full_sha(repo, sha).await?;
        self.latest_statuses(repo, &sha, None).await
    }

    async fn combined(&self, repo: RepoId, sha: &str) -> Result<CombinedState> {
        let sha = self.full_sha(repo, sha).await?;
        Ok(combine(&self.latest_checks(repo, &sha, None).await?, &self.latest_statuses(repo, &sha, None).await?))
    }

    async fn seen_for_sha(&self, actor: &Actor, repo: RepoId, sha: &str) -> Result<SeenChecks> {
        let tiers = history::tiers(&self.ctx, actor, repo).await?;
        let sha = self.full_sha(repo, sha).await?;
        let runs = self.latest_checks(repo, &sha, tiers).await?;
        let statuses = self.latest_statuses(repo, &sha, tiers).await?;
        let state = (!runs.is_empty() || !statuses.is_empty()).then(|| combine(&runs, &statuses));
        Ok(SeenChecks { runs, statuses, state })
    }

    async fn create_status(
        &self,
        actor: &Actor,
        repo: RepoId,
        sha: &str,
        state: &str,
        context: &str,
        description: Option<&str>,
        target_url: Option<&str>,
    ) -> Result<CommitStatus> {
        actor.deny_app("create a commit status")?;
        // A job may report on its own repository's commits when its token has `statuses: write`.
        // Everyone else needs write access.
        if let Actor::Job { repo_id, perms, .. } = actor {
            if *repo_id != repo {
                return Err(Error::NotFound);
            }
            if perms.statuses < Permission::Write {
                return Err(Error::forbidden(
                    "This job's token cannot write commit statuses. Add `statuses: write` to the workflow's `permissions:`.",
                ));
            }
        } else {
            self.ctx.authz.require(actor, repo, Permission::Write).await?;
        }
        if !valid_state(state) {
            return Err(Error::invalid("Status", "state", "The state must be pending, success, failure or error."));
        }
        let context = if context.trim().is_empty() { "default" } else { context.trim() };
        if context.len() > 255 || context.chars().any(|c| c.is_control()) {
            return Err(Error::invalid("Status", "context", "The context must be at most 255 characters."));
        }
        if description.is_some_and(|d| d.len() > 1024) {
            return Err(Error::invalid("Status", "description", "The description must be at most 1024 characters."));
        }
        if let Some(u) = target_url
            && (u.len() > 2048
                || !(u.starts_with("https://") || u.starts_with("http://"))
                || u.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            return Err(Error::invalid("Status", "target_url", "The target URL must be an http or https address."));
        }
        let full = self.ctx.source.resolve(repo, sha).await.map_err(|e| match e {
            Error::NotFound => Error::invalid("Status", "sha", "No commit found for that SHA."),
            o => o,
        })?;
        let mut tx = self.ctx.db.begin().await?;
        let row: (i64, i64, String, String, String, Option<String>, Option<String>) = sqlx::query_as(
            "INSERT INTO commit_statuses (repo_id, sha, context, state, description, target_url, creator_id) VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING id, repo_id, sha, context, state, description, target_url",
        )
        .bind(repo.0)
        .bind(&full)
        .bind(context)
        .bind(state)
        .bind(description)
        .bind(target_url)
        .bind(actor.user_id().map(|u| u.0))
        .fetch_one(&mut *tx)
        .await?;
        self.ctx
            .events
            .emit(
                &mut tx,
                DomainEvent::StatusCreated { repo_id: repo, sha: full.clone(), context: context.to_string(), state: state.to_string() },
            )
            .await?;
        tx.commit().await?;
        Ok(CommitStatus {
            id: row.0,
            repo_id: RepoId(row.1),
            sha: row.2,
            context: row.3,
            state: row.4,
            description: row.5,
            target_url: row.6,
        })
    }

    /// True when every context has a passing check run or status on `sha`, and none of the
    /// matching ones is failing or still running.
    async fn required_satisfied(&self, repo: RepoId, sha: &str, contexts: &[String]) -> Result<bool> {
        if contexts.is_empty() {
            return Ok(true);
        }
        let sha = self.full_sha(repo, sha).await?;
        let checks = self.latest_checks(repo, &sha, None).await?;
        let statuses = self.latest_statuses(repo, &sha, None).await?;
        for want in contexts {
            let mut seen = false;
            for c in checks.iter().filter(|c| &c.name == want) {
                seen = true;
                if !check_passes(&c.status, c.conclusion.as_deref()) {
                    return Ok(false);
                }
            }
            for s in statuses.iter().filter(|s| &s.context == want) {
                seen = true;
                if s.state != "success" {
                    return Ok(false);
                }
            }
            if !seen {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            id: 1,
            repo_id: RepoId(1),
            head_sha: "a".into(),
            name: "n".into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
            details_url: None,
        }
    }
    fn st(state: &str) -> CommitStatus {
        CommitStatus {
            id: 1,
            repo_id: RepoId(1),
            sha: "a".into(),
            context: "c".into(),
            state: state.into(),
            description: None,
            target_url: None,
        }
    }

    #[test]
    fn passing_conclusions() {
        assert!(check_passes("completed", Some("success")));
        assert!(check_passes("completed", Some("skipped")));
        assert!(check_passes("completed", Some("neutral")));
        assert!(!check_passes("completed", Some("failure")));
        assert!(!check_passes("completed", Some("cancelled")));
        assert!(!check_passes("completed", Some("timed_out")));
        assert!(!check_passes("in_progress", None));
        assert!(!check_passes("queued", Some("success")), "a conclusion without completion is not a pass");
    }

    #[test]
    fn combining() {
        assert_eq!(combine(&[], &[]), CombinedState::Pending, "nothing yet is not success");
        assert_eq!(combine(&[check("completed", Some("success"))], &[st("success")]), CombinedState::Success);
        assert_eq!(combine(&[check("completed", Some("success")), check("in_progress", None)], &[]), CombinedState::Pending);
        assert_eq!(
            combine(&[check("completed", Some("failure")), check("in_progress", None)], &[]),
            CombinedState::Failure,
            "a failure wins over running"
        );
        assert_eq!(combine(&[], &[st("error")]), CombinedState::Failure);
        assert_eq!(combine(&[], &[st("pending")]), CombinedState::Pending);
        assert_eq!(combine(&[check("completed", Some("cancelled"))], &[]), CombinedState::Failure);
        assert_eq!(combine(&[check("completed", Some("neutral")), check("completed", Some("skipped"))], &[]), CombinedState::Success);
    }

    #[test]
    fn states() {
        assert!(valid_state("pending") && valid_state("error"));
        assert!(!valid_state("ok") && !valid_state(""));
    }
}
