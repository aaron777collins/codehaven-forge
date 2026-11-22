//! The `ch_core::services` traits, implemented over `Core`.
//!
//! Each trait gets a thin wrapper so a method name that two traits share (`update`,
//! `add_comment`) never collides. The work is in `Core`.

use crate::core::Core;
use async_trait::async_trait;
use bytes::Bytes;
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::services::{Issues, Pulls, Releases, Reviews, Search};
use ch_core::{Actor, CommentId, IssueId, ReleaseId, RepoId, Result, ReviewId, TeamId, UserId};
use futures::stream::BoxStream;

#[derive(Clone)]
pub struct IssuesSvc(pub Core);
#[derive(Clone)]
pub struct PullsSvc(pub Core);
#[derive(Clone)]
pub struct ReviewsSvc(pub Core);
#[derive(Clone)]
pub struct ReleasesSvc(pub Core);
#[derive(Clone)]
pub struct SearchSvc(pub Core);

#[async_trait]
impl Issues for IssuesSvc {
    async fn create(&self, actor: &Actor, new: NewIssue, client: &ClientInfo) -> Result<Issue> {
        Ok(self.0.issue_create(actor, new, client).await?.issue)
    }
    async fn get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Issue> {
        Ok(self.0.issue_view(actor, repo, number).await?.issue)
    }
    async fn list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<Issue>> {
        Ok(self.0.issue_views(actor, repo, filter).await?.into_iter().map(|v| v.issue).collect())
    }
    async fn set_state(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        open: bool,
        reason: Option<&str>,
        client: &ClientInfo,
    ) -> Result<Issue> {
        Ok(self.0.issue_set_state(actor, repo, number, open, reason, client).await?.issue)
    }
    async fn edit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        title: Option<String>,
        body: Option<String>,
        client: &ClientInfo,
    ) -> Result<Issue> {
        Ok(self.0.issue_edit(actor, repo, number, title, body, client).await?.issue)
    }
    async fn set_labels(&self, actor: &Actor, repo: RepoId, number: i64, labels: Vec<String>, client: &ClientInfo) -> Result<Issue> {
        Ok(self.0.issue_set_labels(actor, repo, number, labels, client).await?.issue)
    }
    async fn comments(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Comment>> {
        self.0.issue_comments(actor, repo, number).await
    }
    async fn add_comment(&self, actor: &Actor, repo: RepoId, number: i64, body: &str, client: &ClientInfo) -> Result<Comment> {
        self.0.issue_add_comment(actor, repo, number, body, client).await
    }

    async fn view(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<IssueView> {
        self.0.issue_view(actor, repo, number).await
    }
    async fn views(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<IssueView>> {
        self.0.issue_views(actor, repo, filter).await
    }
    async fn views_by_ids(&self, actor: &Actor, ids: &[IssueId]) -> Result<Vec<IssueView>> {
        self.0.issue_views_by_ids(actor, ids).await
    }
    async fn for_user(&self, actor: &Actor, scope: UserIssueScope, filter: IssueFilter) -> Result<Vec<IssueView>> {
        self.0.issue_for_user(actor, scope, filter).await
    }
    async fn timeline(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<TimelineItem>> {
        self.0.timeline(actor, repo, number).await
    }
    async fn comment_views(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<CommentView>> {
        self.0.comment_views(actor, repo, number).await
    }
    async fn comment_view(&self, actor: &Actor, repo: RepoId, comment: CommentId) -> Result<CommentView> {
        self.0.comment_view(actor, repo, comment).await
    }
    async fn edit_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, body: &str, client: &ClientInfo) -> Result<CommentView> {
        self.0.issue_edit_comment(actor, repo, comment, body, client).await
    }
    async fn delete_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<()> {
        self.0.issue_delete_comment(actor, repo, comment, client).await
    }
    async fn update(&self, actor: &Actor, repo: RepoId, number: i64, patch: IssuePatch, client: &ClientInfo) -> Result<IssueView> {
        self.0.issue_update(actor, repo, number, patch, client).await
    }
    async fn add_labels(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        labels: Vec<String>,
        client: &ClientInfo,
    ) -> Result<Vec<LabelInfo>> {
        self.0.issue_add_labels(actor, repo, number, labels, client).await
    }
    async fn remove_label(&self, actor: &Actor, repo: RepoId, number: i64, label: &str, client: &ClientInfo) -> Result<Vec<LabelInfo>> {
        self.0.issue_remove_label(actor, repo, number, label, client).await
    }
    async fn add_assignees(&self, actor: &Actor, repo: RepoId, number: i64, users: Vec<UserId>, client: &ClientInfo) -> Result<IssueView> {
        self.0.issue_add_assignees(actor, repo, number, users, client).await
    }
    async fn remove_assignees(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        self.0.issue_remove_assignees(actor, repo, number, users, client).await
    }
    async fn set_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        milestone: Option<i64>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        self.0.issue_set_milestone(actor, repo, number, milestone, client).await
    }
    async fn set_locked(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        locked: bool,
        reason: Option<String>,
        client: &ClientInfo,
    ) -> Result<IssueView> {
        self.0.issue_set_locked(actor, repo, number, locked, reason, client).await
    }
    async fn set_pinned(&self, actor: &Actor, repo: RepoId, number: i64, pinned: bool, client: &ClientInfo) -> Result<IssueView> {
        self.0.issue_set_pinned(actor, repo, number, pinned, client).await
    }
    async fn assignable(&self, actor: &Actor, repo: RepoId) -> Result<Vec<UserRef>> {
        self.0.assignable(actor, repo).await
    }
    async fn labels(&self, actor: &Actor, repo: RepoId) -> Result<Vec<LabelInfo>> {
        self.0.labels(actor, repo).await
    }
    async fn label(&self, actor: &Actor, repo: RepoId, name: &str) -> Result<LabelInfo> {
        self.0.label(actor, repo, name).await
    }
    async fn create_label(&self, actor: &Actor, repo: RepoId, draft: LabelDraft, client: &ClientInfo) -> Result<LabelInfo> {
        self.0.create_label(actor, repo, draft, client).await
    }
    async fn update_label(&self, actor: &Actor, repo: RepoId, name: &str, edit: LabelEdit, client: &ClientInfo) -> Result<LabelInfo> {
        self.0.update_label(actor, repo, name, edit, client).await
    }
    async fn delete_label(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        self.0.delete_label(actor, repo, name, client).await
    }
    async fn milestones(&self, actor: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>> {
        self.0.milestones(actor, repo, open).await
    }
    async fn milestone(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Milestone> {
        self.0.milestone(actor, repo, number).await
    }
    async fn create_milestone(&self, actor: &Actor, repo: RepoId, draft: MilestoneDraft, client: &ClientInfo) -> Result<Milestone> {
        self.0.create_milestone(actor, repo, draft, client).await
    }
    async fn update_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        edit: MilestoneEdit,
        client: &ClientInfo,
    ) -> Result<Milestone> {
        self.0.update_milestone(actor, repo, number, edit, client).await
    }
    async fn delete_milestone(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<()> {
        self.0.delete_milestone(actor, repo, number, client).await
    }
    async fn react(&self, actor: &Actor, repo: RepoId, target: ReactionTarget, content: &str, client: &ClientInfo) -> Result<ReactionRow> {
        self.0.react(actor, repo, target, content, client).await
    }
    async fn reactions(&self, actor: &Actor, repo: RepoId, target: ReactionTarget) -> Result<Vec<ReactionRow>> {
        self.0.reactions(actor, repo, target).await
    }
    async fn unreact(&self, actor: &Actor, repo: RepoId, target: ReactionTarget, reaction_id: i64, client: &ClientInfo) -> Result<()> {
        self.0.unreact(actor, repo, target, reaction_id, client).await
    }
    async fn subscription(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Option<bool>> {
        self.0.subscription(actor, repo, number).await
    }
    async fn set_subscription(&self, actor: &Actor, repo: RepoId, number: i64, subscribed: bool) -> Result<()> {
        self.0.set_subscription(actor, repo, number, subscribed).await
    }
    async fn templates(&self, actor: &Actor, repo: RepoId) -> Result<Vec<IssueTemplate>> {
        self.0.templates(actor, repo).await
    }
}

#[async_trait]
impl Pulls for PullsSvc {
    async fn create(&self, actor: &Actor, new: NewPull, client: &ClientInfo) -> Result<PullRequest> {
        Ok(self.0.pull_create(actor, new, client).await?.pull)
    }
    async fn get(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullRequest> {
        self.0.pull_get(actor, repo, number).await
    }
    async fn list(&self, actor: &Actor, repo: RepoId, filter: IssueFilter) -> Result<Vec<PullRequest>> {
        self.0.pull_list(actor, repo, filter).await
    }
    async fn merge_gate(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<GateReport> {
        self.0.pull_merge_gate(actor, repo, number).await
    }
    async fn merge(&self, actor: &Actor, repo: RepoId, number: i64, method: MergeMethod, client: &ClientInfo) -> Result<PullRequest> {
        self.0.pull_merge(actor, repo, number, method, client).await
    }
    async fn close(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullRequest> {
        self.0.pull_close(actor, repo, number, client).await
    }

    async fn view(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<PullView> {
        self.0.pull_view(actor, repo, number).await
    }
    async fn views(&self, actor: &Actor, repo: RepoId, filter: PullFilter) -> Result<Vec<PullView>> {
        self.0.pull_views(actor, repo, filter).await
    }
    async fn open_for_branch(
        &self,
        actor: &Actor,
        repo: RepoId,
        head_repo: RepoId,
        head_ref: &str,
        base_ref: &str,
    ) -> Result<Option<PullView>> {
        self.0.pull_open_for_branch(actor, repo, head_repo, head_ref, base_ref).await
    }
    async fn update(&self, actor: &Actor, repo: RepoId, number: i64, patch: PullPatch, client: &ClientInfo) -> Result<PullView> {
        if patch.base.is_some() {
            actor.deny_app("change a pull request's base branch")?;
        }
        self.0.pull_update(actor, repo, number, patch, client).await
    }
    async fn reopen(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullView> {
        self.0.pull_set_open(actor, repo, number, true, client).await
    }
    async fn set_draft(&self, actor: &Actor, repo: RepoId, number: i64, draft: bool, client: &ClientInfo) -> Result<PullView> {
        self.0.pull_set_draft(actor, repo, number, draft, client).await
    }
    async fn files(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Diff> {
        self.0.pull_files(actor, repo, number).await
    }
    async fn commits(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<CommitInfo>> {
        self.0.pull_commits(actor, repo, number).await
    }
    async fn update_branch(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<PullView> {
        self.0.pull_update_branch(actor, repo, number, client).await
    }
    async fn set_auto_merge(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        method: Option<MergeMethod>,
        client: &ClientInfo,
    ) -> Result<PullView> {
        actor.deny_app("change auto-merge")?;
        self.0.pull_set_auto_merge(actor, repo, number, method, client).await
    }
    async fn merge_with(&self, actor: &Actor, repo: RepoId, number: i64, options: MergeOptions, client: &ClientInfo) -> Result<PullView> {
        self.0.pull_merge_with(actor, repo, number, options, client).await
    }
}

#[async_trait]
impl Reviews for ReviewsSvc {
    async fn list(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<Review>> {
        self.0.review_list(actor, repo, number).await
    }
    async fn submit(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        state: ReviewState,
        body: &str,
        comments: Vec<NewReviewComment>,
        client: &ClientInfo,
    ) -> Result<Review> {
        if state == ReviewState::Approved {
            actor.deny_app("approve a pull request")?;
        }
        self.0.review_submit(actor, repo, number, state, body, comments, client).await
    }
    async fn request_review(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        client: &ClientInfo,
    ) -> Result<()> {
        self.0.review_request(actor, repo, number, users, teams, client).await
    }
    async fn resolve_thread(&self, actor: &Actor, repo: RepoId, thread_id: i64, resolved: bool, client: &ClientInfo) -> Result<()> {
        actor.deny_app("resolve a review thread")?;
        self.0.review_resolve_thread(actor, repo, thread_id, resolved, client).await
    }

    async fn views(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ReviewView>> {
        self.0.review_views(actor, repo, number).await
    }
    async fn threads(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Vec<ThreadView>> {
        self.0.review_threads(actor, repo, number).await
    }
    async fn add_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        comment: NewReviewComment,
        review: Option<ReviewId>,
        client: &ClientInfo,
    ) -> Result<ReviewCommentView> {
        self.0.review_add_comment(actor, repo, number, comment, review, client).await
    }
    async fn comment(&self, actor: &Actor, repo: RepoId, comment: CommentId) -> Result<ReviewCommentView> {
        self.0.review_comment(actor, repo, comment).await
    }
    async fn edit_comment(
        &self,
        actor: &Actor,
        repo: RepoId,
        comment: CommentId,
        body: &str,
        client: &ClientInfo,
    ) -> Result<ReviewCommentView> {
        self.0.review_edit_comment(actor, repo, comment, body, client).await
    }
    async fn delete_comment(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<()> {
        self.0.review_delete_comment(actor, repo, comment, client).await
    }
    async fn submit_pending(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        state: ReviewState,
        body: &str,
        client: &ClientInfo,
    ) -> Result<Review> {
        if state == ReviewState::Approved {
            actor.deny_app("approve a pull request")?;
        }
        self.0.review_submit_pending(actor, repo, number, review, state, body, client).await
    }
    async fn delete_pending(&self, actor: &Actor, repo: RepoId, number: i64, review: ReviewId, client: &ClientInfo) -> Result<()> {
        self.0.review_delete_pending(actor, repo, number, review, client).await
    }
    async fn dismiss(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        review: ReviewId,
        message: &str,
        client: &ClientInfo,
    ) -> Result<Review> {
        actor.deny_app("dismiss a review")?;
        self.0.review_dismiss(actor, repo, number, review, message, client).await
    }
    async fn requested(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<RequestedReviewers> {
        self.0.review_requested(actor, repo, number).await
    }
    async fn remove_request(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        users: Vec<UserId>,
        teams: Vec<TeamId>,
        client: &ClientInfo,
    ) -> Result<()> {
        self.0.review_remove_request(actor, repo, number, users, teams, client).await
    }
    async fn apply_suggestion(&self, actor: &Actor, repo: RepoId, comment: CommentId, client: &ClientInfo) -> Result<String> {
        actor.deny_app("apply a suggestion")?;
        self.0.review_apply_suggestion(actor, repo, comment, client).await
    }
}

#[async_trait]
impl Releases for ReleasesSvc {
    async fn list(&self, actor: &Actor, repo: RepoId) -> Result<Vec<Release>> {
        Ok(self.0.release_views(actor, repo).await?.into_iter().map(|v| v.release).collect())
    }
    async fn get_by_tag(&self, actor: &Actor, repo: RepoId, tag: &str) -> Result<Release> {
        Ok(self.0.release_view_by_tag(actor, repo, tag).await?.release)
    }
    async fn latest(&self, actor: &Actor, repo: RepoId) -> Result<Release> {
        Ok(self.0.release_latest_view(actor, repo).await?.release)
    }

    async fn views(&self, actor: &Actor, repo: RepoId) -> Result<Vec<ReleaseView>> {
        self.0.release_views(actor, repo).await
    }
    async fn view(&self, actor: &Actor, repo: RepoId, id: ReleaseId) -> Result<ReleaseView> {
        self.0.release_view(actor, repo, id).await
    }
    async fn view_by_tag(&self, actor: &Actor, repo: RepoId, tag: &str) -> Result<ReleaseView> {
        self.0.release_view_by_tag(actor, repo, tag).await
    }
    async fn latest_view(&self, actor: &Actor, repo: RepoId) -> Result<ReleaseView> {
        self.0.release_latest_view(actor, repo).await
    }
    async fn create(&self, actor: &Actor, repo: RepoId, draft: ReleaseDraft, client: &ClientInfo) -> Result<ReleaseView> {
        actor.deny_app("manage releases")?;
        self.0.release_create(actor, repo, draft, client).await
    }
    async fn update(&self, actor: &Actor, repo: RepoId, id: ReleaseId, edit: ReleaseEdit, client: &ClientInfo) -> Result<ReleaseView> {
        actor.deny_app("manage releases")?;
        self.0.release_update(actor, repo, id, edit, client).await
    }
    async fn delete(&self, actor: &Actor, repo: RepoId, id: ReleaseId, client: &ClientInfo) -> Result<()> {
        actor.deny_app("manage releases")?;
        self.0.release_delete(actor, repo, id, client).await
    }
    async fn generate_notes(
        &self,
        actor: &Actor,
        repo: RepoId,
        tag: &str,
        previous_tag: Option<&str>,
        target: Option<&str>,
    ) -> Result<GeneratedNotes> {
        self.0.release_generate_notes(actor, repo, tag, previous_tag, target).await
    }
    async fn upload_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        release: ReleaseId,
        name: &str,
        label: Option<&str>,
        content_type: &str,
        body: BoxStream<'static, std::io::Result<Bytes>>,
        client: &ClientInfo,
    ) -> Result<AssetInfo> {
        actor.deny_app("manage releases")?;
        self.0.release_upload_asset(actor, repo, release, name, label, content_type, body, client).await
    }
    async fn asset(&self, actor: &Actor, repo: RepoId, asset: i64) -> Result<AssetInfo> {
        self.0.release_asset(actor, repo, asset).await
    }
    async fn delete_asset(&self, actor: &Actor, repo: RepoId, asset: i64, client: &ClientInfo) -> Result<()> {
        actor.deny_app("manage releases")?;
        self.0.release_delete_asset(actor, repo, asset, client).await
    }
    async fn open_asset(
        &self,
        actor: &Actor,
        repo: RepoId,
        tag: &str,
        name: &str,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        self.0.release_open_asset(actor, repo, tag, name).await
    }
    async fn open_asset_by_id(
        &self,
        actor: &Actor,
        repo: RepoId,
        asset: i64,
    ) -> Result<(AssetInfo, BoxStream<'static, std::io::Result<Bytes>>)> {
        self.0.release_open_asset_by_id(actor, repo, asset).await
    }
}

#[async_trait]
impl Search for SearchSvc {
    async fn repos(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.0.search_repos(actor, query, limit).await
    }
    async fn issues(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.0.search_issues(actor, query, limit).await
    }
    async fn users(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.0.search_users(actor, query, limit).await
    }
    async fn code(&self, actor: &Actor, repo: RepoId, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.0.search_code(actor, repo, query, limit).await
    }
}
