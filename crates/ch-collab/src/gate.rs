//! The merge gate: every reason a merge is blocked, in one list.
//!
//! `evaluate` is pure so each rule is tested on its own. `Core::gate` gathers the facts
//! (reviews, checks, threads, code owners) and feeds it.

use crate::protect::Rules;
use ch_core::UserId;
use ch_core::model::*;
use chrono::{DateTime, Utc};

/// One submitted review, with whether its author counts toward required approvals.
#[derive(Debug, Clone)]
pub struct ReviewFact {
    pub id: i64,
    pub reviewer: UserId,
    pub name: String,
    pub state: ReviewState,
    pub commit_sha: String,
    pub submitted_at: Option<DateTime<Utc>>,
    /// The reviewer has write access, so their review counts.
    pub counts: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effective {
    /// Approvals that count right now.
    pub approvals: u32,
    /// Names of reviewers whose latest decision is "changes requested".
    pub changes_requested: Vec<String>,
    /// Reviewers whose approval counts, for the code owner rule.
    pub approvers: Vec<UserId>,
}

/// Works out what each reviewer's latest decision is.
///
/// - Pending reviews are ignored. A "comment" review never changes a decision.
/// - Per reviewer, the latest approval, change request or dismissal decides.
/// - A dismissed review no longer counts either way.
/// - The author's own approval never counts.
/// - With `dismiss_stale`, an approval for an older commit than `head_sha` does not count.
///   A change request stays until the reviewer reviews again or a maintainer dismisses it.
pub fn effective(reviews: &[ReviewFact], head_sha: &str, dismiss_stale: bool, author: UserId) -> Effective {
    let mut latest: std::collections::HashMap<UserId, &ReviewFact> = std::collections::HashMap::new();
    for r in reviews {
        if !matches!(r.state, ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Dismissed) {
            continue;
        }
        match latest.get(&r.reviewer) {
            Some(cur) if (cur.submitted_at, cur.id) >= (r.submitted_at, r.id) => {}
            _ => {
                latest.insert(r.reviewer, r);
            }
        }
    }
    let mut out = Effective::default();
    let mut decided: Vec<&&ReviewFact> = latest.values().collect();
    decided.sort_by_key(|r| (r.submitted_at, r.id));
    for r in decided {
        if !r.counts {
            continue;
        }
        match r.state {
            ReviewState::Approved if r.reviewer != author && (!dismiss_stale || r.commit_sha == head_sha) => {
                out.approvals += 1;
                out.approvers.push(r.reviewer);
            }
            ReviewState::ChangesRequested => out.changes_requested.push(r.name.clone()),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct GateInput {
    pub open: bool,
    pub merged: bool,
    pub draft: bool,
    pub mergeable: MergeableState,
    pub base_ref: String,
    /// The method the caller asked for, when asking, and whether the repository allows it.
    pub method: Option<(MergeMethod, bool)>,
    pub rules: Option<Rules>,
    pub effective: Effective,
    /// Required checks that are missing or not passing.
    pub failing_checks: Vec<String>,
    /// Required checks could not be read at all (the CI service is down).
    pub checks_unavailable: bool,
    /// Names of the checks and statuses that did report on the head commit, so a required name
    /// that never runs can point at the one the person probably meant.
    pub check_names: Vec<String>,
    /// Code owner groups with no approval, like `docs/** (@alice, @acme/docs)`.
    pub unowned_reviews: Vec<String>,
    pub unresolved_threads: u32,
    /// The fork the pull request came from was deleted, so there is no head branch to merge.
    pub head_gone: bool,
}

/// Why a pull request whose fork was deleted cannot be merged.
pub const HEAD_NOT_PUBLISHED: &str = "This repository is public and the fork this pull request came from is not. Its author has not agreed to publish the commits here. Ask them to open the pull request again and confirm";
pub const HEAD_GONE: &str = "The repository this pull request came from was deleted, so it cannot be merged";

pub fn evaluate(i: &GateInput) -> GateReport {
    let mut reasons: Vec<String> = Vec::new();
    if i.merged {
        reasons.push("This pull request is already merged".into());
    } else if !i.open {
        reasons.push("This pull request is closed".into());
    }
    if i.open && !i.merged {
        if i.head_gone {
            reasons.push(HEAD_GONE.into());
        }
        if i.draft {
            reasons.push("This pull request is a draft. Mark it ready for review first".into());
        }
        if let Some((m, allowed)) = i.method
            && !allowed
        {
            reasons.push(
                match m {
                    MergeMethod::Merge => "Merge commits are turned off for this repository",
                    MergeMethod::Squash => "Squash merging is turned off for this repository",
                    MergeMethod::Rebase => "Rebase merging is turned off for this repository",
                }
                .into(),
            );
        }
        match i.mergeable {
            MergeableState::Conflict => reasons.push("There are merge conflicts to resolve".into()),
            MergeableState::Unknown => reasons.push("Mergeability is still being checked. Try again in a moment".into()),
            _ => {}
        }
        if let Some(r) = &i.rules {
            if r.strict && i.mergeable == MergeableState::Behind {
                reasons.push(format!("The branch is out of date with {}. Update the branch first", i.base_ref));
            }
            if i.effective.approvals < r.required_approvals {
                let need = r.required_approvals - i.effective.approvals;
                reasons
                    .push(format!("Needs {need} more approving review{} from someone with write access", if need == 1 { "" } else { "s" }));
            }
            if !i.effective.changes_requested.is_empty() {
                reasons.push(format!("Changes requested by {}", i.effective.changes_requested.join(", ")));
            }
            if i.checks_unavailable && !r.required_checks.is_empty() {
                reasons.push("Required checks could not be verified right now".into());
            } else {
                for c in &i.failing_checks {
                    match nearest_check(c, &i.check_names) {
                        Some(near) => reasons.push(format!(
                            "Required check \"{c}\" has not passed. No check by that name ran on this commit. The nearest is \"{near}\""
                        )),
                        None => reasons.push(format!("Required check \"{c}\" has not passed")),
                    }
                }
            }
            if r.require_code_owner_review {
                for g in &i.unowned_reviews {
                    reasons.push(format!("Needs a review from a code owner: {g}"));
                }
            }
            if r.require_resolved_threads && i.unresolved_threads > 0 {
                reasons.push(format!(
                    "{} review conversation{} still need{} to be resolved",
                    i.unresolved_threads,
                    if i.unresolved_threads == 1 { "" } else { "s" },
                    if i.unresolved_threads == 1 { "s" } else { "" }
                ));
            }
        }
        // Without protection a change request is advice, not a block.
    }
    GateReport { mergeable: reasons.is_empty(), reasons }
}

/// The reported check a required name most likely meant, when no check of that exact name ran.
///
/// A name that contains the wanted one, or is contained by it, ignoring case, wins first (a
/// required `test` against `ci / test`). Otherwise the smallest edit distance, at most a third of
/// the longer name and at least 3. Ties go to the alphabetically first name.
pub fn nearest_check<'a>(want: &str, names: &'a [String]) -> Option<&'a str> {
    if want.is_empty() || names.iter().any(|n| n == want) {
        return None;
    }
    let w = want.to_lowercase();
    let mut sorted: Vec<&String> = names.iter().filter(|n| !n.is_empty()).collect();
    sorted.sort();
    sorted.dedup();
    if let Some(n) = sorted.iter().find(|n| {
        let l = n.to_lowercase();
        l.contains(&w) || w.contains(&l)
    }) {
        return Some(n.as_str());
    }
    let mut best: Option<(usize, &str)> = None;
    for n in sorted {
        let d = edit_distance(&w, &n.to_lowercase());
        let limit = (w.chars().count().max(n.chars().count()) / 3).max(3);
        if d <= limit && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, n.as_str()));
        }
    }
    best.map(|(_, n)| n)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> GateInput {
        GateInput {
            open: true,
            merged: false,
            draft: false,
            mergeable: MergeableState::Clean,
            base_ref: "main".into(),
            method: None,
            rules: None,
            effective: Effective::default(),
            failing_checks: vec![],
            checks_unavailable: false,
            check_names: vec![],
            unowned_reviews: vec![],
            unresolved_threads: 0,
            head_gone: false,
        }
    }

    #[test]
    fn a_pull_request_whose_fork_was_deleted_cannot_merge() {
        let mut i = input();
        i.head_gone = true;
        let r = evaluate(&i);
        assert!(!r.mergeable);
        assert_eq!(r.reasons, vec![HEAD_GONE]);
    }

    fn rules() -> Rules {
        Rules::default()
    }

    fn review(id: i64, who: i64, state: ReviewState, sha: &str, at: i64) -> ReviewFact {
        ReviewFact {
            id,
            reviewer: UserId(who),
            name: format!("u{who}"),
            state,
            commit_sha: sha.into(),
            submitted_at: DateTime::from_timestamp(1_700_000_000 + at, 0),
            counts: true,
        }
    }

    #[test]
    fn clean_unprotected_pull_is_mergeable() {
        let r = evaluate(&input());
        assert!(r.mergeable);
        assert!(r.reasons.is_empty());
    }

    #[test]
    fn closed_and_merged_block() {
        let mut i = input();
        i.open = false;
        assert_eq!(evaluate(&i).reasons, vec!["This pull request is closed"]);
        i.merged = true;
        assert_eq!(evaluate(&i).reasons, vec!["This pull request is already merged"]);
    }

    #[test]
    fn draft_conflict_unknown_and_disabled_method() {
        let mut i = input();
        i.draft = true;
        i.mergeable = MergeableState::Conflict;
        i.method = Some((MergeMethod::Squash, false));
        let r = evaluate(&i);
        assert!(!r.mergeable);
        assert_eq!(r.reasons.len(), 3);
        i.draft = false;
        i.method = None;
        i.mergeable = MergeableState::Unknown;
        assert!(evaluate(&i).reasons[0].contains("still being checked"));
    }

    #[test]
    fn approvals_count_down() {
        let mut i = input();
        let mut r = rules();
        r.required_approvals = 2;
        i.rules = Some(r);
        assert!(evaluate(&i).reasons[0].starts_with("Needs 2 more approving reviews"));
        i.effective.approvals = 1;
        assert!(evaluate(&i).reasons[0].starts_with("Needs 1 more approving review "));
        i.effective.approvals = 2;
        assert!(evaluate(&i).mergeable);
    }

    #[test]
    fn changes_requested_blocks_only_with_protection() {
        let mut i = input();
        i.effective.changes_requested = vec!["bob".into()];
        assert!(evaluate(&i).mergeable);
        i.rules = Some(rules());
        let r = evaluate(&i);
        assert!(!r.mergeable);
        assert_eq!(r.reasons, vec!["Changes requested by bob"]);
    }

    #[test]
    fn strict_needs_up_to_date_branch() {
        let mut i = input();
        let mut r = rules();
        r.strict = true;
        i.rules = Some(r);
        i.mergeable = MergeableState::Behind;
        assert!(evaluate(&i).reasons[0].contains("out of date with main"));
        i.mergeable = MergeableState::Clean;
        assert!(evaluate(&i).mergeable);
    }

    #[test]
    fn checks_threads_and_code_owners() {
        let mut i = input();
        let mut r = rules();
        r.required_checks = vec!["ci".into(), "lint".into()];
        r.require_resolved_threads = true;
        r.require_code_owner_review = true;
        i.rules = Some(r);
        i.failing_checks = vec!["ci".into(), "lint".into()];
        i.unresolved_threads = 2;
        i.unowned_reviews = vec!["docs/** (@alice)".into()];
        let rep = evaluate(&i);
        assert_eq!(rep.reasons.len(), 4);
        assert!(rep.reasons.iter().any(|r| r == "Required check \"ci\" has not passed"));
        assert!(rep.reasons.iter().any(|r| r.starts_with("2 review conversations still need")));
        assert!(rep.reasons.iter().any(|r| r.contains("code owner")));
    }

    #[test]
    fn a_required_name_that_never_ran_names_the_nearest_check() {
        let names: Vec<String> = vec!["ci / test (go 1.21.13)".into(), "ci / lint".into(), "deploy".into()];
        assert_eq!(nearest_check("test (go 1.21.13)", &names), Some("ci / test (go 1.21.13)"));
        assert_eq!(nearest_check("CI / Lint", &names), Some("ci / lint"));
        assert_eq!(nearest_check("deplyo", &names), Some("deploy"));
        assert_eq!(nearest_check("security scan", &names), None);
        assert_eq!(nearest_check("deploy", &names), None, "an exact name that ran is not a typo");
        assert_eq!(nearest_check("x", &[]), None);

        let mut i = input();
        let mut r = rules();
        r.required_checks = vec!["test (go 1.21.13)".into(), "lint".into()];
        i.rules = Some(r);
        i.failing_checks = vec!["test (go 1.21.13)".into(), "security".into()];
        i.check_names = names;
        let rep = evaluate(&i);
        assert_eq!(
            rep.reasons,
            vec![
                "Required check \"test (go 1.21.13)\" has not passed. No check by that name ran on this commit. The nearest is \"ci / test (go 1.21.13)\"",
                "Required check \"security\" has not passed",
            ]
        );
    }

    #[test]
    fn unreadable_checks_fail_closed() {
        let mut i = input();
        let mut r = rules();
        r.required_checks = vec!["ci".into()];
        i.rules = Some(r);
        i.checks_unavailable = true;
        let rep = evaluate(&i);
        assert_eq!(rep.reasons, vec!["Required checks could not be verified right now"]);
    }

    #[test]
    fn single_thread_grammar() {
        let mut i = input();
        let mut r = rules();
        r.require_resolved_threads = true;
        i.rules = Some(r);
        i.unresolved_threads = 1;
        assert_eq!(evaluate(&i).reasons, vec!["1 review conversation still needs to be resolved"]);
    }

    #[test]
    fn latest_decision_per_reviewer_wins() {
        let rs = vec![
            review(1, 2, ReviewState::ChangesRequested, "a", 1),
            review(2, 2, ReviewState::Approved, "a", 2),
            review(3, 3, ReviewState::Approved, "a", 3),
            review(4, 3, ReviewState::Commented, "a", 4),
        ];
        let e = effective(&rs, "a", false, UserId(1));
        assert_eq!(e.approvals, 2);
        assert!(e.changes_requested.is_empty());
    }

    #[test]
    fn comment_after_change_request_keeps_the_request() {
        let rs = vec![review(1, 2, ReviewState::ChangesRequested, "a", 1), review(2, 2, ReviewState::Commented, "a", 2)];
        let e = effective(&rs, "a", false, UserId(1));
        assert_eq!(e.changes_requested, vec!["u2"]);
    }

    #[test]
    fn dismissed_review_stops_counting() {
        let rs = vec![review(1, 2, ReviewState::Approved, "a", 1), review(2, 2, ReviewState::Dismissed, "a", 2)];
        assert_eq!(effective(&rs, "a", false, UserId(1)).approvals, 0);
        let rs = vec![review(1, 2, ReviewState::ChangesRequested, "a", 1), review(2, 2, ReviewState::Dismissed, "a", 2)];
        assert!(effective(&rs, "a", false, UserId(1)).changes_requested.is_empty());
    }

    #[test]
    fn stale_approvals_dropped_only_when_asked() {
        let rs = vec![review(1, 2, ReviewState::Approved, "old", 1)];
        assert_eq!(effective(&rs, "new", false, UserId(1)).approvals, 1);
        assert_eq!(effective(&rs, "new", true, UserId(1)).approvals, 0);
    }

    #[test]
    fn authors_and_non_writers_do_not_count() {
        let mut rs = vec![review(1, 1, ReviewState::Approved, "a", 1), review(2, 2, ReviewState::Approved, "a", 2)];
        rs[1].counts = false;
        let e = effective(&rs, "a", false, UserId(1));
        assert_eq!(e.approvals, 0);
        rs.push(review(3, 4, ReviewState::ChangesRequested, "a", 3));
        rs[2].counts = false;
        assert!(effective(&rs, "a", false, UserId(1)).changes_requested.is_empty());
    }

    #[test]
    fn pending_reviews_are_ignored() {
        let rs = vec![review(1, 2, ReviewState::Pending, "a", 1)];
        assert_eq!(effective(&rs, "a", false, UserId(1)), Effective::default());
    }

    #[test]
    fn mergeable_iff_no_reasons() {
        use ReviewState::*;
        for open in [true, false] {
            for draft in [true, false] {
                for state in [MergeableState::Clean, MergeableState::Behind, MergeableState::Conflict, MergeableState::Unknown] {
                    let mut i = input();
                    i.open = open;
                    i.draft = draft;
                    i.mergeable = state;
                    let r = evaluate(&i);
                    assert_eq!(r.mergeable, r.reasons.is_empty());
                    let _ = Approved;
                }
            }
        }
    }
}
