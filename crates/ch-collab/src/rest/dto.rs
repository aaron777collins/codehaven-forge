//! Turns collab views into the `ch-api-types` shapes, adding the URLs.

use ch_api_types::common::SimpleUser;
use ch_api_types::issues as api_i;
use ch_api_types::pulls as api_p;
use ch_api_types::releases as api_r;
use ch_core::collab::*;
use ch_core::model::*;

/// The place URLs are built from: the instance and one repository.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub base: String,
    pub owner: String,
    pub repo: String,
}

impl Ctx {
    pub fn new(base: &str, owner: &str, repo: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_string(), owner: owner.into(), repo: repo.into() }
    }

    /// For lists that span repositories, from `owner/name`.
    pub fn from_full_name(base: &str, full: &str) -> Self {
        let (o, r) = full.split_once('/').unwrap_or(("", full));
        Self::new(base, o, r)
    }

    pub fn api(&self, path: &str) -> String {
        format!("{}/api/v3/repos/{}/{}{}", self.base, self.owner, self.repo, path)
    }

    pub fn html(&self, path: &str) -> String {
        format!("{}/{}/{}{}", self.base, self.owner, self.repo, path)
    }

    pub fn user(&self, u: &UserRef) -> SimpleUser {
        SimpleUser {
            login: u.username.clone(),
            id: u.id.0,
            avatar_url: ch_api_types::avatar_url(&self.base, &u.username),
            url: format!("{}/api/v3/users/{}", self.base, u.username),
            html_url: format!("{}/{}", self.base, u.username),
            kind: match u.kind {
                UserKind::User => "User",
                UserKind::Org => "Organization",
                UserKind::Bot => "Bot",
            }
            .into(),
            site_admin: false,
        }
    }

    pub fn label(&self, l: &LabelInfo) -> api_i::Label {
        api_i::Label {
            id: l.id.0,
            name: l.name.clone(),
            color: l.color.clone(),
            description: if l.description.is_empty() { None } else { Some(l.description.clone()) },
            default: l.is_default,
            url: self.api(&format!("/labels/{}", enc(&l.name))),
        }
    }

    pub fn milestone(&self, m: &Milestone) -> api_i::Milestone {
        api_i::Milestone {
            id: m.id.0,
            number: m.number,
            title: m.title.clone(),
            description: if m.description.is_empty() { None } else { Some(m.description.clone()) },
            state: if m.open { "open" } else { "closed" }.into(),
            open_issues: m.open_issues.max(0) as u32,
            closed_issues: m.closed_issues.max(0) as u32,
            due_on: m.due_on,
            closed_at: m.closed_at,
            created_at: m.created_at,
            updated_at: m.closed_at.unwrap_or(m.created_at),
            url: self.api(&format!("/milestones/{}", m.number)),
            html_url: self.html(&format!("/issues?milestone={}", m.id.0)),
            creator: None,
        }
    }

    pub fn reactions(&self, path: &str, r: &ReactionCounts) -> api_i::ReactionRollup {
        api_i::ReactionRollup {
            url: self.api(path),
            total_count: r.total(),
            plus_one: r.plus_one,
            minus_one: r.minus_one,
            laugh: r.laugh,
            confused: r.confused,
            heart: r.heart,
            hooray: r.hooray,
            rocket: r.rocket,
            eyes: r.eyes,
        }
    }

    pub fn issue(&self, v: &IssueView) -> api_i::Issue {
        let n = v.issue.number;
        let assignees: Vec<SimpleUser> = v.assignees.iter().map(|u| self.user(u)).collect();
        api_i::Issue {
            id: v.issue.id.0,
            number: n,
            title: v.issue.title.clone(),
            body: if v.issue.body.is_empty() { None } else { Some(v.issue.body.clone()) },
            state: if v.issue.open { "open" } else { "closed" }.into(),
            state_reason: v.issue.state_reason.clone(),
            locked: v.issue.locked,
            user: self.user(&v.author),
            labels: v.labels.iter().map(|l| self.label(l)).collect(),
            assignee: assignees.first().cloned(),
            assignees,
            milestone: v.milestone.as_ref().map(|m| self.milestone(m)),
            comments: v.issue.comments.max(0) as u32,
            created_at: v.issue.created_at,
            updated_at: v.issue.updated_at,
            closed_at: v.issue.closed_at,
            closed_by: v.closed_by.as_ref().map(|u| self.user(u)),
            html_url: self.html(&format!("/{}/{}", if v.issue.is_pull { "pulls" } else { "issues" }, n)),
            url: self.api(&format!("/issues/{n}")),
            repository_url: self.api(""),
            comments_url: self.api(&format!("/issues/{n}/comments")),
            pull_request: v.issue.is_pull.then(|| api_i::PullRequestRef {
                url: self.api(&format!("/pulls/{n}")),
                html_url: self.html(&format!("/pulls/{n}")),
                merged_at: v.merged_at,
            }),
            reactions: Some(self.reactions(&format!("/issues/{n}/reactions"), &v.reactions)),
        }
    }

    pub fn comment(&self, c: &CommentView, issue_number: i64) -> api_i::IssueComment {
        let id = c.comment.id.0;
        api_i::IssueComment {
            id,
            body: c.comment.body.clone(),
            user: self.user(&c.author),
            created_at: c.comment.created_at,
            updated_at: c.comment.updated_at,
            html_url: self.html(&format!("/issues/{issue_number}#comment-{id}")),
            url: self.api(&format!("/issues/comments/{id}")),
            issue_url: self.api(&format!("/issues/{issue_number}")),
            reactions: Some(self.reactions(&format!("/issues/comments/{id}/reactions"), &c.reactions)),
        }
    }

    pub fn reaction(&self, r: &ReactionRow) -> api_i::Reaction {
        api_i::Reaction { id: r.id, user: self.user(&r.user), content: r.content.clone(), created_at: r.created_at }
    }

    pub fn timeline_item(&self, t: &TimelineItem) -> api_i::TimelineEvent {
        let label = t.data.get("label").map(|l| api_i::Label {
            id: 0,
            name: l.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            color: l.get("color").and_then(|v| v.as_str()).unwrap_or("000000").to_string(),
            description: None,
            default: false,
            url: self.api(&format!("/labels/{}", enc(l.get("name").and_then(|v| v.as_str()).unwrap_or("")))),
        });
        let assignee = t.data.get("assignee").and_then(|a| {
            let login = a.get("username")?.as_str()?.to_string();
            let id = a.get("id")?.as_i64()?;
            Some(SimpleUser {
                login: login.clone(),
                id,
                avatar_url: ch_api_types::avatar_url(&self.base, &login),
                url: format!("{}/api/v3/users/{login}", self.base),
                html_url: format!("{}/{login}", self.base),
                kind: "User".into(),
                site_admin: false,
            })
        });
        api_i::TimelineEvent {
            id: t.id,
            event: if t.kind == "comment" { "commented".into() } else { t.kind.clone() },
            actor: t.actor.as_ref().map(|u| self.user(u)),
            created_at: t.at,
            label,
            assignee,
            source: t.data.get("source").cloned().map(|s| serde_json::json!({ "issue": s })),
            commit_id: t
                .data
                .get("sha")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| t.data.get("after").and_then(|v| v.as_str()).map(String::from)),
            body: t
                .comment
                .as_ref()
                .map(|c| c.comment.body.clone())
                .or_else(|| t.data.get("message").and_then(|v| v.as_str()).map(String::from)),
        }
    }

    pub fn issue_event(&self, t: &TimelineItem) -> Option<api_i::IssueEvent> {
        let actor = t.actor.as_ref().map(|u| self.user(u))?;
        Some(api_i::IssueEvent {
            id: t.id,
            event: t.kind.clone(),
            actor,
            created_at: t.at,
            commit_id: t.data.get("sha").and_then(|v| v.as_str()).map(String::from),
        })
    }

    // ------------------------------------------------------------ pulls

    pub fn pull(
        &self,
        v: &PullView,
        head_repo: Option<ch_api_types::repos::Repository>,
        base_repo: Option<ch_api_types::repos::Repository>,
    ) -> api_p::PullRequest {
        let n = v.pull.issue.number;
        let base_owner = v.base_repo.owner.clone();
        let (head_owner, head_name) = match &v.head_repo {
            Some(h) => (h.owner.clone(), h.name.clone()),
            None => (v.base_repo.owner.clone(), v.base_repo.name.clone()),
        };
        let _ = head_name;
        let user = self.user(&v.issue.author);
        let issue = self.issue(&v.issue);
        let mergeable = match v.pull.mergeable {
            MergeableState::Clean | MergeableState::Behind | MergeableState::Blocked => Some(true),
            MergeableState::Conflict => Some(false),
            MergeableState::Unknown => None,
        };
        api_p::PullRequest {
            url: self.api(&format!("/pulls/{n}")),
            id: v.pull.issue.id.0,
            number: n,
            html_url: self.html(&format!("/pulls/{n}")),
            diff_url: self.html(&format!("/pulls/{n}/files")),
            patch_url: self.html(&format!("/pulls/{n}/files")),
            state: if v.pull.issue.open { "open" } else { "closed" }.into(),
            locked: v.pull.issue.locked,
            title: v.pull.issue.title.clone(),
            body: issue.body.clone(),
            user: user.clone(),
            labels: issue.labels,
            milestone: issue.milestone,
            assignees: issue.assignees,
            requested_reviewers: v.requested.users.iter().map(|u| self.user(u)).collect(),
            head: api_p::PullBranch {
                // A head repository the caller cannot read, or one that was deleted, shows as
                // its branch name only.
                label: if head_repo.is_none() && (v.head_repo.is_some() || v.pull.from_fork) {
                    v.pull.head_ref.clone()
                } else {
                    format!("{}:{}", head_owner, v.pull.head_ref)
                },
                git_ref: v.pull.head_ref.clone(),
                sha: v.pull.head_sha.clone(),
                user: user.clone(),
                repo: head_repo,
            },
            base: api_p::PullBranch {
                label: format!("{}:{}", base_owner, v.pull.base_ref),
                git_ref: v.pull.base_ref.clone(),
                sha: v.pull.base_sha.clone(),
                user: SimpleUser { login: base_owner.clone(), ..user },
                repo: base_repo,
            },
            draft: v.pull.draft,
            merged: v.pull.merged_at.is_some(),
            mergeable,
            rebaseable: mergeable,
            mergeable_state: match v.pull.mergeable {
                MergeableState::Clean => "clean",
                MergeableState::Conflict => "dirty",
                MergeableState::Behind => "behind",
                MergeableState::Blocked => "blocked",
                MergeableState::Unknown => "unknown",
            }
            .into(),
            merged_by: v.merged_by.as_ref().map(|u| self.user(u)),
            merge_commit_sha: v.pull.merge_commit_sha.clone(),
            comments: v.pull.issue.comments.max(0) as u32,
            review_comments: v.review_comments,
            commits: v.stats.commits,
            additions: v.stats.additions,
            deletions: v.stats.deletions,
            changed_files: v.stats.changed_files,
            created_at: v.pull.issue.created_at,
            updated_at: v.pull.issue.updated_at,
            closed_at: v.pull.issue.closed_at,
            merged_at: v.pull.merged_at,
            auto_merge: v.auto_merge.as_ref().map(|a| api_p::AutoMerge {
                merge_method: crate::pulls::method_str(a.method).into(),
                enabled_by: self.user(&a.enabled_by),
            }),
            links: Some(api_p::PullLinks {
                self_link: api_p::Link { href: self.api(&format!("/pulls/{n}")) },
                html: api_p::Link { href: self.html(&format!("/pulls/{n}")) },
                issue: api_p::Link { href: self.api(&format!("/issues/{n}")) },
                comments: api_p::Link { href: self.api(&format!("/issues/{n}/comments")) },
                review_comments: api_p::Link { href: self.api(&format!("/pulls/{n}/comments")) },
                review_comment: api_p::Link { href: self.api("/pulls/comments{/number}") },
                commits: api_p::Link { href: self.api(&format!("/pulls/{n}/commits")) },
                statuses: api_p::Link { href: self.api(&format!("/statuses/{}", v.pull.head_sha)) },
            }),
        }
    }

    pub fn requested_teams(&self, teams: &[TeamRef]) -> Vec<ch_api_types::users::Team> {
        teams
            .iter()
            .map(|t| ch_api_types::users::Team {
                id: t.id.0,
                name: t.name.clone(),
                slug: t.name.clone(),
                description: None,
                permission: t.permission.clone().unwrap_or_else(|| "pull".into()),
                url: format!("{}/api/v3/orgs/{}/teams/{}", self.base, t.org, t.name),
                html_url: format!("{}/orgs/{}/teams/{}", self.base, t.org, t.name),
                members_count: 0,
                repos_count: 0,
                organization: None,
            })
            .collect()
    }

    pub fn pull_file(&self, f: &FileDiff, head_sha: &str) -> api_p::PullFile {
        let path = f.new_path.clone().or_else(|| f.old_path.clone()).unwrap_or_default();
        api_p::PullFile {
            sha: head_sha.to_string(),
            filename: path.clone(),
            status: match f.status {
                FileStatus::Added => "added",
                FileStatus::Modified => "modified",
                FileStatus::Removed => "removed",
                FileStatus::Renamed => "renamed",
                FileStatus::Copied => "copied",
            }
            .into(),
            additions: f.additions,
            deletions: f.deletions,
            changes: f.additions + f.deletions,
            patch: patch_text(f),
            previous_filename: if f.status == FileStatus::Renamed { f.old_path.clone() } else { None },
            blob_url: self.html(&format!("/blob/{head_sha}/{path}")),
            raw_url: self.html(&format!("/raw/{head_sha}/{path}")),
        }
    }

    pub fn commit(&self, c: &CommitInfo) -> ch_api_types::repos::Commit {
        let ident = |s: &Signature| ch_api_types::repos::GitIdentity { name: s.name.clone(), email: s.email.clone(), date: s.at };
        ch_api_types::repos::Commit {
            sha: c.sha.clone(),
            url: self.api(&format!("/commits/{}", c.sha)),
            html_url: self.html(&format!("/commit/{}", c.sha)),
            commit: ch_api_types::repos::CommitDetail {
                author: ident(&c.author),
                committer: ident(&c.committer),
                message: c.message.clone(),
                tree: ch_api_types::common::ShaRef { sha: c.tree.clone(), url: None },
                comment_count: 0,
            },
            author: None,
            committer: None,
            parents: c
                .parents
                .iter()
                .map(|p| ch_api_types::common::ShaRef { sha: p.clone(), url: Some(self.api(&format!("/commits/{p}"))) })
                .collect(),
            stats: None,
            files: Vec::new(),
        }
    }

    pub fn review(&self, v: &ReviewView, number: i64) -> api_p::Review {
        let r = &v.review;
        api_p::Review {
            id: r.id.0,
            user: self.user(&v.author),
            body: if r.body.is_empty() { None } else { Some(r.body.clone()) },
            state: match r.state {
                ReviewState::Pending => "PENDING",
                ReviewState::Commented => "COMMENTED",
                ReviewState::Approved => "APPROVED",
                ReviewState::ChangesRequested => "CHANGES_REQUESTED",
                ReviewState::Dismissed => "DISMISSED",
            }
            .into(),
            commit_id: Some(r.commit_sha.clone()),
            submitted_at: r.submitted_at,
            html_url: self.html(&format!("/pulls/{number}#review-{}", r.id.0)),
            pull_request_url: self.api(&format!("/pulls/{number}")),
            author_association: v.association.clone(),
        }
    }

    pub fn review_comment(&self, c: &ReviewCommentView, number: i64) -> api_p::ReviewComment {
        let rc = &c.comment;
        api_p::ReviewComment {
            id: rc.id.0,
            pull_request_review_id: rc.review_id.map(|r| r.0),
            diff_hunk: rc.diff_hunk.clone(),
            path: rc.path.clone(),
            commit_id: rc.commit_sha.clone(),
            original_commit_id: rc.original_commit_sha.clone(),
            user: self.user(&c.author),
            body: rc.body.clone(),
            created_at: rc.created_at,
            updated_at: rc.updated_at,
            html_url: self.html(&format!("/pulls/{number}/files")),
            pull_request_url: self.api(&format!("/pulls/{number}")),
            line: if rc.outdated { None } else { rc.line },
            original_line: rc.original_line,
            start_line: rc.start_line,
            side: Some(rc.side.to_uppercase()),
            in_reply_to_id: rc.in_reply_to.map(|i| i.0),
            position: None,
            reactions: Some(self.reactions(&format!("/pulls/comments/{}/reactions", rc.id.0), &c.reactions)),
        }
    }

    // ------------------------------------------------------------ releases

    pub fn asset(&self, a: &AssetInfo, tag: &str) -> api_r::ReleaseAsset {
        api_r::ReleaseAsset {
            id: a.id,
            name: a.name.clone(),
            label: if a.label.is_empty() { None } else { Some(a.label.clone()) },
            content_type: a.content_type.clone(),
            size: a.size,
            download_count: a.download_count,
            created_at: a.created_at,
            updated_at: a.created_at,
            browser_download_url: self.html(&format!("/releases/download/{}/{}", enc(tag), enc(&a.name))),
            url: self.api(&format!("/releases/assets/{}", a.id)),
            uploader: a.uploader.as_ref().map(|u| self.user(u)),
            state: "uploaded".into(),
        }
    }

    pub fn release(&self, v: &ReleaseView) -> api_r::Release {
        let r = &v.release;
        api_r::Release {
            id: r.id.0,
            tag_name: r.tag_name.clone(),
            target_commitish: v.target_commitish.clone(),
            name: if r.name.is_empty() { None } else { Some(r.name.clone()) },
            body: if r.body.is_empty() { None } else { Some(r.body.clone()) },
            draft: r.draft,
            prerelease: r.prerelease,
            created_at: r.created_at,
            published_at: r.published_at,
            author: self.user(&v.author),
            assets: v.assets.iter().map(|a| self.asset(a, &r.tag_name)).collect(),
            html_url: self.html(&format!("/releases/tag/{}", enc(&r.tag_name))),
            url: self.api(&format!("/releases/{}", r.id.0)),
            tarball_url: self.api(&format!("/tarball/{}", enc(&r.tag_name))),
            zipball_url: self.api(&format!("/zipball/{}", enc(&r.tag_name))),
            upload_url: self.api(&format!("/releases/{}/assets{{?name,label}}", r.id.0)),
        }
    }
}

/// Percent-encodes a path segment.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A unified diff for one file, without the `diff --git` header lines, like GitHub's `patch`.
pub fn patch_text(f: &FileDiff) -> Option<String> {
    if f.binary || f.collapsed || f.hunks.is_empty() {
        return None;
    }
    let mut out = String::new();
    for h in &f.hunks {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&h.header);
        for l in &h.lines {
            out.push('\n');
            out.push(match l.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&l.text);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding() {
        assert_eq!(enc("good first issue"), "good%20first%20issue");
        assert_eq!(enc("a/b"), "a%2Fb");
        assert_eq!(enc("v1.0-rc_1~"), "v1.0-rc_1~");
    }

    #[test]
    fn urls() {
        let c = Ctx::new("https://h.example/", "octo", "haven");
        assert_eq!(c.api("/issues/1"), "https://h.example/api/v3/repos/octo/haven/issues/1");
        assert_eq!(c.html("/pulls/2"), "https://h.example/octo/haven/pulls/2");
        let c = Ctx::from_full_name("https://h", "a/b");
        assert_eq!((c.owner.as_str(), c.repo.as_str()), ("a", "b"));
    }

    #[test]
    fn patch_joins_hunks() {
        let f = FileDiff {
            status: FileStatus::Modified,
            old_path: Some("a".into()),
            new_path: Some("a".into()),
            binary: false,
            collapsed: false,
            additions: 1,
            deletions: 0,
            hunks: vec![Hunk {
                header: "@@ -1 +1,2 @@".into(),
                lines: vec![
                    DiffLine { kind: LineKind::Context, old_no: Some(1), new_no: Some(1), text: "x".into() },
                    DiffLine { kind: LineKind::Added, old_no: None, new_no: Some(2), text: "y".into() },
                ],
            }],
        };
        assert_eq!(patch_text(&f).unwrap(), "@@ -1 +1,2 @@\n x\n+y");
        let mut b = f.clone();
        b.binary = true;
        assert!(patch_text(&b).is_none());
    }
}
