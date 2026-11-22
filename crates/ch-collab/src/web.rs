//! What the web pages call: `ch_core::web_contract::Tracker`.
//!
//! The pages work with labels and milestones by id and with one flat list of inline review
//! comments. The five collaboration traits work with names, per-repository numbers and
//! threads. This is the translation, and nothing else: every call goes through those traits,
//! so permissions, audit records and events are the ones the REST edge gets.

use async_trait::async_trait;
use ch_core::collab::{IssuePatch, LabelDraft, MilestoneDraft, MilestoneEdit, ReleaseDraft};
use ch_core::model::{ClientInfo, Release};
use ch_core::services::{Issues, Releases, Reviews};
use ch_core::web_contract::{Label, Milestone, NewRelease, ReviewComment, Tracker};
use ch_core::{Actor, Error, MilestoneId, ReleaseId, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use std::sync::Arc;

pub struct TrackerSvc {
    pub issues: Arc<dyn Issues>,
    pub reviews: Arc<dyn Reviews>,
    pub releases: Arc<dyn Releases>,
}

fn milestone(repo: RepoId, m: ch_core::collab::Milestone) -> Milestone {
    Milestone {
        id: m.id,
        repo_id: repo,
        title: m.title,
        description: m.description,
        due_on: m.due_on,
        open: m.open,
        open_issues: m.open_issues,
        closed_issues: m.closed_issues,
    }
}

impl TrackerSvc {
    /// The milestone's per-repository number, which is what the issue service takes.
    async fn number_of(&self, actor: &Actor, repo: RepoId, id: MilestoneId) -> Result<i64> {
        let all = self.issues.milestones(actor, repo, None).await?;
        all.into_iter().find(|m| m.id == id).map(|m| m.number).ok_or(Error::NotFound)
    }
}

#[async_trait]
impl Tracker for TrackerSvc {
    async fn labels(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Label>> {
        Ok(self
            .issues
            .labels(actor, repo)
            .await?
            .into_iter()
            .map(|l| Label { id: l.id, repo_id: repo, name: l.name, color: l.color, description: l.description })
            .collect())
    }

    async fn create_label(
        &self,
        actor: &Actor,
        repo: RepoId,
        name: &str,
        color: &str,
        description: &str,
        client: &ClientInfo,
    ) -> Result<Label> {
        let draft = LabelDraft {
            name: name.to_string(),
            color: Some(color.trim_start_matches('#').to_string()).filter(|c| !c.is_empty()),
            description: Some(description.to_string()),
        };
        let l = self.issues.create_label(actor, repo, draft, client).await?;
        Ok(Label { id: l.id, repo_id: repo, name: l.name, color: l.color, description: l.description })
    }

    async fn delete_label(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        self.issues.delete_label(actor, repo, name, client).await
    }

    async fn milestones(&self, actor: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>> {
        Ok(self.issues.milestones(actor, repo, open).await?.into_iter().map(|m| milestone(repo, m)).collect())
    }

    async fn create_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        title: &str,
        description: &str,
        due_on: Option<DateTime<Utc>>,
        client: &ClientInfo,
    ) -> Result<Milestone> {
        let draft = MilestoneDraft { title: title.to_string(), description: Some(description.to_string()), due_on, open: Some(true) };
        Ok(milestone(repo, self.issues.create_milestone(actor, repo, draft, client).await?))
    }

    async fn set_milestone_state(&self, actor: &Actor, repo: RepoId, id: MilestoneId, open: bool, client: &ClientInfo) -> Result<()> {
        let number = self.number_of(actor, repo, id).await?;
        let edit = MilestoneEdit { open: Some(open), ..Default::default() };
        self.issues.update_milestone(actor, repo, number, edit, client).await?;
        Ok(())
    }

    async fn set_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        milestone: Option<MilestoneId>,
        client: &ClientInfo,
    ) -> Result<()> {
        let target = match milestone {
            Some(id) => Some(self.number_of(actor, repo, id).await?),
            None => None,
        };
        self.issues.set_milestone(actor, repo, number, target, client).await?;
        Ok(())
    }

    async fn set_assignees(&self, actor: &Actor, repo: RepoId, number: i64, users: Vec<UserId>, client: &ClientInfo) -> Result<()> {
        let patch = IssuePatch { assignees: Some(users), ..Default::default() };
        self.issues.update(actor, repo, number, patch, client).await?;
        Ok(())
    }

    async fn review_comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ReviewComment>> {
        let threads = self.reviews.threads(actor, repo, number).await?;
        let mut out: Vec<ReviewComment> = threads
            .into_iter()
            .flat_map(|t| {
                let resolved = t.resolved;
                t.comments.into_iter().map(move |c| {
                    let c = c.comment;
                    ReviewComment {
                        id: c.id,
                        review_id: c.review_id,
                        author_id: c.author_id,
                        path: c.path,
                        side: c.side,
                        line: c.line.or(c.original_line).unwrap_or(0),
                        start_line: c.start_line,
                        body: c.body,
                        in_reply_to: c.in_reply_to,
                        resolved,
                        commit_sha: c.commit_sha,
                        created_at: c.created_at,
                    }
                })
            })
            .collect();
        out.sort_by_key(|c| (c.created_at, c.id.0));
        Ok(out)
    }

    async fn create_release(&self, actor: &Actor, new: NewRelease, client: &ClientInfo) -> Result<Release> {
        let draft = ReleaseDraft {
            tag_name: new.tag_name,
            target_commitish: Some(new.target).filter(|t| !t.is_empty()),
            name: Some(new.name).filter(|n| !n.is_empty()),
            body: Some(new.body),
            draft: new.draft,
            prerelease: new.prerelease,
            generate_notes: false,
        };
        Ok(self.releases.create(actor, new.repo_id, draft, client).await?.release)
    }

    async fn delete_release(&self, actor: &Actor, repo: RepoId, id: ReleaseId, client: &ClientInfo) -> Result<()> {
        self.releases.delete(actor, repo, id, client).await
    }
}
