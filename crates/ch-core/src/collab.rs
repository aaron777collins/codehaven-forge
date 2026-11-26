//! View models and request types for collaboration (issues, pull requests, reviews, releases).
//!
//! The plain models in `model.rs` hold ids. The web pages and the REST edge want names, label
//! colors, counts and reactions with them, and asking for those one at a time is an N+1 query
//! problem, so the collab service returns these "views" in one round trip.
//!
//! Added by the collab stream. Nothing here has URLs: the REST edge adds them.

use crate::ids::*;
use crate::model::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A person, org or bot as shown next to an issue, comment or review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRef {
    pub id: UserId,
    pub username: String,
    pub display_name: String,
    pub kind: UserKind,
}

/// A team named in a review request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRef {
    pub id: TeamId,
    pub org: String,
    pub name: String,
    /// The team's access to the repository in question, in GitHub's words (`pull`, `triage`,
    /// `push`, `maintain`, `admin`). `None` when it has no access there.
    #[serde(default)]
    pub permission: Option<String>,
}

/// A repository as shown in a pull request's head and base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub id: RepoId,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

// ---------------------------------------------------------------- labels and milestones

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelInfo {
    pub id: LabelId,
    pub name: String,
    /// Six hex digits, no `#`.
    pub color: String,
    pub description: String,
    /// One of the labels every new repository starts with.
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelDraft {
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LabelEdit {
    pub new_name: Option<String>,
    pub color: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub id: MilestoneId,
    pub repo_id: RepoId,
    /// Per repository, like issue numbers but a separate sequence.
    pub number: i64,
    pub title: String,
    pub description: String,
    pub open: bool,
    pub due_on: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub open_issues: i64,
    pub closed_issues: i64,
}

impl Milestone {
    /// 0 to 100, for the progress bar. A milestone with no issues is 0.
    pub fn percent_complete(&self) -> u32 {
        let total = self.open_issues + self.closed_issues;
        if total <= 0 { 0 } else { ((self.closed_issues * 100) / total) as u32 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneDraft {
    pub title: String,
    pub description: Option<String>,
    pub due_on: Option<DateTime<Utc>>,
    pub open: Option<bool>,
}

/// `due_on: Some(None)` clears the due date, `None` leaves it alone.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MilestoneEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub due_on: Option<Option<DateTime<Utc>>>,
    pub open: Option<bool>,
}

// ---------------------------------------------------------------- reactions

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReactionCounts {
    pub plus_one: i64,
    pub minus_one: i64,
    pub laugh: i64,
    pub confused: i64,
    pub heart: i64,
    pub hooray: i64,
    pub rocket: i64,
    pub eyes: i64,
}

impl ReactionCounts {
    pub const CONTENTS: [&'static str; 8] = ["+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes"];

    pub fn is_valid(content: &str) -> bool {
        Self::CONTENTS.contains(&content)
    }
    pub fn total(&self) -> i64 {
        self.plus_one + self.minus_one + self.laugh + self.confused + self.heart + self.hooray + self.rocket + self.eyes
    }
    pub fn add(&mut self, content: &str, n: i64) {
        match content {
            "+1" => self.plus_one += n,
            "-1" => self.minus_one += n,
            "laugh" => self.laugh += n,
            "confused" => self.confused += n,
            "heart" => self.heart += n,
            "hooray" => self.hooray += n,
            "rocket" => self.rocket += n,
            "eyes" => self.eyes += n,
            _ => {}
        }
    }
}

/// What a reaction is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReactionTarget {
    /// An issue or pull request, by its number in the repository.
    Issue(i64),
    IssueComment(CommentId),
    ReviewComment(CommentId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionRow {
    pub id: i64,
    pub user: UserRef,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------- issues

/// An issue, or the issue half of a pull request, with everything a page needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueView {
    pub issue: Issue,
    pub author: UserRef,
    pub assignees: Vec<UserRef>,
    pub labels: Vec<LabelInfo>,
    pub milestone: Option<Milestone>,
    pub closed_by: Option<UserRef>,
    pub reactions: ReactionCounts,
    pub lock_reason: Option<String>,
    pub pinned: bool,
    /// `Some` when this issue row is a pull request.
    pub merged_at: Option<DateTime<Utc>>,
    /// `owner/name`, so a cross-repository list can say where each item lives.
    pub repo_full_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentView {
    pub comment: Comment,
    pub author: UserRef,
    pub reactions: ReactionCounts,
}

/// One row of a timeline: a comment, or an event such as `labeled` or `closed`.
///
/// `kind` is `comment` or an event name: `labeled`, `unlabeled`, `assigned`, `unassigned`,
/// `closed`, `reopened`, `renamed`, `milestoned`, `demilestoned`, `locked`, `unlocked`, `pinned`,
/// `unpinned`, `cross-referenced`, `referenced`, `review_requested`, `review_request_removed`,
/// `review_dismissed`, `ready_for_review`, `convert_to_draft`, `head_ref_updated`,
/// `base_ref_changed`, `merged`, `auto_merge_enabled`, `auto_merge_disabled`. `data` holds the
/// details, for example `{"label": {"name": "bug", "color": "d73a4a"}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineItem {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor: Option<UserRef>,
    pub kind: String,
    pub data: serde_json::Value,
    /// Set when `kind` is `comment`.
    pub comment: Option<CommentView>,
}

/// The body of `PATCH /issues/{n}`. Every field is optional. `labels` and `assignees` replace
/// the whole list when present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssuePatch {
    pub title: Option<String>,
    pub body: Option<String>,
    /// `Some(true)` opens, `Some(false)` closes.
    pub open: Option<bool>,
    /// `completed` or `not_planned`, only with `open: Some(false)`.
    pub state_reason: Option<String>,
    pub labels: Option<Vec<String>>,
    pub assignees: Option<Vec<UserId>>,
    /// `Some(None)` clears the milestone. The number is the milestone's number in the repo.
    pub milestone: Option<Option<i64>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserIssueScope {
    Assigned,
    Created,
    Mentioned,
    ReviewRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueTemplate {
    pub name: String,
    pub about: String,
    pub title: String,
    pub labels: Vec<String>,
    pub body: String,
    /// Path in the repository, like `.codehaven/ISSUE_TEMPLATE/bug.md`.
    pub path: String,
    /// Set for a YAML issue form (`bug.yml`): its fields, which the new issue page draws. `body`
    /// is empty then.
    #[serde(default)]
    pub form: Option<crate::issue_form::IssueForm>,
}

/// Where a pull request stands in its repository's merge queue (ch-dxk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueView {
    /// 1 is the one being worked on.
    pub position: u32,
    pub total: u32,
    /// What it waits for, in words.
    pub note: String,
    pub method: MergeMethod,
}

// ---------------------------------------------------------------- pull requests

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PullStats {
    pub commits: u32,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoMergeInfo {
    pub method: MergeMethod,
    pub enabled_by: UserRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestedReviewers {
    pub users: Vec<UserRef>,
    pub teams: Vec<TeamRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullView {
    pub pull: PullRequest,
    pub issue: IssueView,
    pub head_repo: Option<RepoRef>,
    pub base_repo: RepoRef,
    pub merged_by: Option<UserRef>,
    pub merge_method: Option<MergeMethod>,
    pub stats: PullStats,
    pub requested: RequestedReviewers,
    /// Inline review comments, not counting pending ones.
    pub review_comments: u32,
    pub auto_merge: Option<AutoMergeInfo>,
    pub maintainer_can_modify: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullFilter {
    pub issue: IssueFilter,
    /// `branch` or `owner:branch`.
    pub head: Option<String>,
    pub base: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergeOptions {
    /// Defaults to the first method the repository allows, merge commit first.
    pub method: Option<MergeMethod>,
    pub title: Option<String>,
    pub message: Option<String>,
    /// The merge fails with a conflict when the head is no longer this commit.
    pub expected_head_sha: Option<String>,
    /// Delete the head branch afterwards (same repository only). `None` follows the repo setting.
    pub delete_branch: Option<bool>,
}

// ---------------------------------------------------------------- reviews

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewView {
    pub review: Review,
    pub author: UserRef,
    /// Whether the author may be counted toward required approvals (write access).
    pub counts: bool,
    pub comment_count: u32,
    /// GitHub's `author_association`: `OWNER`, `MEMBER`, `COLLABORATOR` or `NONE` (ch-yci).
    #[serde(default = "no_association")]
    pub association: String,
}

fn no_association() -> String {
    "NONE".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: CommentId,
    pub pull_id: PullId,
    pub review_id: Option<ReviewId>,
    pub thread_id: Option<i64>,
    pub author_id: UserId,
    pub in_reply_to: Option<CommentId>,
    pub path: String,
    /// `left` or `right`.
    pub side: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub commit_sha: String,
    pub original_commit_sha: String,
    pub diff_hunk: String,
    pub body: String,
    pub outdated: bool,
    /// True while the comment belongs to a review that has not been submitted.
    pub pending: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewCommentView {
    pub comment: ReviewComment,
    pub author: UserRef,
    pub reactions: ReactionCounts,
    /// The comment has a `suggestion` block the viewer could apply.
    pub has_suggestion: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub id: i64,
    pub path: String,
    pub resolved: bool,
    pub resolved_by: Option<UserRef>,
    pub outdated: bool,
    pub comments: Vec<ReviewCommentView>,
}

// ---------------------------------------------------------------- releases

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: i64,
    pub release_id: ReleaseId,
    pub name: String,
    pub label: String,
    pub content_type: String,
    pub size: i64,
    /// The SHA-256 of the file, as 64 lower case hex characters.
    pub sha256: String,
    pub download_count: i64,
    pub uploader: Option<UserRef>,
    pub created_at: DateTime<Utc>,
}

/// What `make_latest` asks for when a release is published or edited (GitHub's three values).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MakeLatest {
    /// Make it the latest release. The default for a published release that is not a prerelease.
    #[default]
    True,
    /// Never the latest.
    False,
    /// The latest only when its version is the highest published so far.
    Legacy,
}

impl MakeLatest {
    /// Reads the value of the REST field, which GitHub sends as a string.
    pub fn parse(s: &str) -> Option<MakeLatest> {
        match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(MakeLatest::True),
            "false" => Some(MakeLatest::False),
            "legacy" => Some(MakeLatest::Legacy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseView {
    pub release: Release,
    pub target_commitish: String,
    pub author: UserRef,
    pub assets: Vec<AssetInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseDraft {
    pub tag_name: String,
    pub target_commitish: Option<String>,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub generate_notes: bool,
    #[serde(default)]
    pub make_latest: MakeLatest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReleaseEdit {
    pub tag_name: Option<String>,
    pub target_commitish: Option<String>,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: Option<bool>,
    pub prerelease: Option<bool>,
    pub generate_notes: Option<bool>,
    pub make_latest: Option<MakeLatest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedNotes {
    pub name: String,
    pub body: String,
}
