//! Domain events. A single serde enum stored in the `outbox_events` table.
//!
//! Producers emit inside the same transaction as the change. Consumers each keep a cursor
//! in `event_cursors` and process in id order (ARCHITECTURE.md section 4.4).

use crate::ids::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    Push {
        repo_id: RepoId,
        git_ref: String,
        before: String,
        after: String,
        pusher: Option<UserId>,
        forced: bool,
        /// Set when a connected app made the push. Old events have none.
        #[serde(default)]
        app: Option<crate::actor::AppOrigin>,
        /// True when the server wrote `after` for the pusher on top of whatever the branch held
        /// (`before`): a file edit from the web, the contents API or a suggestion, or Update
        /// branch. The pusher vouches for their own change, not for `before` (ch-x12).
        #[serde(default)]
        server_built: bool,
    },
    RefCreated {
        repo_id: RepoId,
        git_ref: String,
        sha: String,
        actor: Option<UserId>,
    },
    RefDeleted {
        repo_id: RepoId,
        git_ref: String,
        sha: String,
        actor: Option<UserId>,
    },
    RepoCreated {
        repo_id: RepoId,
        actor: Option<UserId>,
    },
    RepoDeleted {
        repo_id: RepoId,
        actor: Option<UserId>,
    },
    IssueOpened {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        actor: UserId,
    },
    IssueClosed {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        actor: UserId,
    },
    IssueReopened {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        actor: UserId,
    },
    IssueEdited {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        actor: UserId,
    },
    IssueLabeled {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        label: String,
        actor: UserId,
    },
    IssueAssigned {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        assignee: UserId,
        actor: UserId,
    },
    CommentCreated {
        repo_id: RepoId,
        issue_id: IssueId,
        number: i64,
        comment_id: CommentId,
        actor: UserId,
    },
    PrOpened {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        actor: UserId,
        #[serde(default)]
        app: Option<crate::actor::AppOrigin>,
        /// The head commit the pull request was opened at. CI skips the event when the head
        /// has moved on since, because `pusher` describes this commit only. None on a reopen
        /// and in events stored before this field.
        #[serde(default)]
        head_sha: Option<String>,
        /// Who CI may count as having pushed `head_sha`: the person who opened the pull
        /// request. None on a reopen, because reopening is not pushing (ch-x12).
        #[serde(default)]
        pusher: Option<UserId>,
    },
    PrSynchronized {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        head_sha: String,
        #[serde(default)]
        app: Option<crate::actor::AppOrigin>,
        /// Who put the new head there. CI runs a fork's code at once only when both the pull
        /// request's author and this person have write access; unknown counts as no.
        #[serde(default)]
        pusher: Option<UserId>,
        /// True when `pusher` and `app` describe exactly `head_sha`. False when the branch moved
        /// on before the push was handled, so nobody can be named for the commit it holds now,
        /// and in events stored before this field. CI then waits for a person (ch-x12).
        #[serde(default)]
        origin_known: bool,
        /// Set when the server wrote `head_sha` for `pusher` on top of this commit: Update
        /// branch, an applied suggestion, a file edit from the web. The pusher vouches for their
        /// own change only, so CI counts them for `head_sha` only when this commit was itself
        /// let run or approved for the pull request. Otherwise CI waits for a person (ch-x12).
        #[serde(default)]
        built_on: Option<String>,
    },
    PrClosed {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        merged: bool,
        actor: Option<UserId>,
    },
    PrMerged {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        merge_sha: String,
        actor: UserId,
    },
    /// Added by the collab stream so notifications can say "review requested".
    ReviewRequested {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        reviewer: Option<UserId>,
        team: Option<TeamId>,
        actor: Option<UserId>,
    },
    ReviewSubmitted {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        review_id: ReviewId,
        state: String,
        actor: UserId,
    },
    ReviewCommentCreated {
        repo_id: RepoId,
        pull_id: PullId,
        number: i64,
        comment_id: CommentId,
        actor: UserId,
    },
    ReleasePublished {
        repo_id: RepoId,
        release_id: ReleaseId,
        tag: String,
        actor: UserId,
    },
    WorkflowRunUpdated {
        repo_id: RepoId,
        run_id: RunId,
        status: String,
        conclusion: Option<String>,
    },
    WorkflowJobUpdated {
        repo_id: RepoId,
        run_id: RunId,
        job_id: JobId,
        status: String,
        conclusion: Option<String>,
    },
    CheckRunUpdated {
        repo_id: RepoId,
        sha: String,
        name: String,
        status: String,
        conclusion: Option<String>,
    },
    StatusCreated {
        repo_id: RepoId,
        sha: String,
        context: String,
        state: String,
    },
    MemberAdded {
        repo_id: Option<RepoId>,
        org_id: Option<OrgId>,
        user_id: UserId,
        actor: Option<UserId>,
    },
    PackagePublished {
        package_id: PackageId,
        owner_id: UserId,
        tag: Option<String>,
    },
}

impl DomainEvent {
    /// A stable name, stored in `outbox_events.kind` so consumers can filter in SQL.
    pub fn kind(&self) -> &'static str {
        use DomainEvent::*;
        match self {
            Push { .. } => "push",
            RefCreated { .. } => "ref_created",
            RefDeleted { .. } => "ref_deleted",
            RepoCreated { .. } => "repo_created",
            RepoDeleted { .. } => "repo_deleted",
            IssueOpened { .. } => "issue_opened",
            IssueClosed { .. } => "issue_closed",
            IssueReopened { .. } => "issue_reopened",
            IssueEdited { .. } => "issue_edited",
            IssueLabeled { .. } => "issue_labeled",
            IssueAssigned { .. } => "issue_assigned",
            CommentCreated { .. } => "comment_created",
            PrOpened { .. } => "pr_opened",
            PrSynchronized { .. } => "pr_synchronized",
            PrClosed { .. } => "pr_closed",
            PrMerged { .. } => "pr_merged",
            ReviewRequested { .. } => "review_requested",
            ReviewSubmitted { .. } => "review_submitted",
            ReviewCommentCreated { .. } => "review_comment_created",
            ReleasePublished { .. } => "release_published",
            WorkflowRunUpdated { .. } => "workflow_run_updated",
            WorkflowJobUpdated { .. } => "workflow_job_updated",
            CheckRunUpdated { .. } => "check_run_updated",
            StatusCreated { .. } => "status_created",
            MemberAdded { .. } => "member_added",
            PackagePublished { .. } => "package_published",
        }
    }
}

/// An event as read back from the outbox.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub id: i64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub event: DomainEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_serde_tag() {
        let e = DomainEvent::PrMerged { repo_id: RepoId(1), pull_id: PullId(2), number: 3, merge_sha: "abc".into(), actor: UserId(4) };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.kind());
        let back: DomainEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn events_stored_before_connected_apps_still_decode() {
        // The shape written by a binary that had no `app` field.
        let old = r#"{"type":"push","repo_id":5,"git_ref":"refs/heads/main","before":"0","after":"a","pusher":9,"forced":false}"#;
        match serde_json::from_str::<DomainEvent>(old).unwrap() {
            DomainEvent::Push { app, .. } => assert_eq!(app, None),
            other => panic!("{other:?}"),
        }
        let old = r#"{"type":"pr_opened","repo_id":5,"pull_id":1,"number":2,"actor":9}"#;
        assert!(matches!(serde_json::from_str::<DomainEvent>(old).unwrap(), DomainEvent::PrOpened { app: None, .. }));
        let old = r#"{"type":"pr_synchronized","repo_id":5,"pull_id":1,"number":2,"head_sha":"a"}"#;
        assert!(matches!(serde_json::from_str::<DomainEvent>(old).unwrap(), DomainEvent::PrSynchronized { app: None, .. }));
        // Events stored before the head's origin was recorded name nobody for it.
        assert!(matches!(
            serde_json::from_str::<DomainEvent>(old).unwrap(),
            DomainEvent::PrSynchronized { pusher: None, origin_known: false, .. }
        ));
        let old = r#"{"type":"pr_opened","repo_id":5,"pull_id":1,"number":2,"actor":9}"#;
        assert!(matches!(serde_json::from_str::<DomainEvent>(old).unwrap(), DomainEvent::PrOpened { head_sha: None, pusher: None, .. }));
        let with = DomainEvent::Push {
            repo_id: RepoId(1),
            git_ref: "r".into(),
            before: "b".into(),
            after: "a".into(),
            pusher: None,
            forced: false,
            app: Some(crate::actor::AppOrigin { grant_id: crate::ids::GrantId(3), ci_allowed: false }),
            server_built: false,
        };
        assert_eq!(serde_json::from_str::<DomainEvent>(&serde_json::to_string(&with).unwrap()).unwrap(), with);
    }

    #[test]
    fn push_round_trip() {
        let e = DomainEvent::Push {
            repo_id: RepoId(5),
            git_ref: "refs/heads/main".into(),
            before: "0".repeat(40),
            after: "a".repeat(40),
            pusher: Some(UserId(9)),
            forced: false,
            app: None,
            server_built: false,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<DomainEvent>(&s).unwrap(), e);
        assert_eq!(e.kind(), "push");
    }
}
