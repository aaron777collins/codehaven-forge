//! Which events start a workflow.

use crate::glob;
use crate::model::*;

pub const PR_TYPES: &[&str] = &[
    "opened",
    "edited",
    "closed",
    "reopened",
    "synchronize",
    "assigned",
    "unassigned",
    "labeled",
    "unlabeled",
    "ready_for_review",
    "converted_to_draft",
    "review_requested",
];

/// The types a `pull_request` trigger listens to when it names none.
pub const DEFAULT_PR_TYPES: &[&str] = &["opened", "synchronize", "reopened"];

/// What happened, in the terms a trigger filter speaks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventInfo {
    /// `push`, `pull_request`, `workflow_dispatch`, `schedule` or `release`.
    pub name: String,
    /// `refs/heads/main` or `refs/tags/v1`, for push events.
    pub git_ref: Option<String>,
    /// The activity type: `opened`, `synchronize`, `published` and so on.
    pub action: Option<String>,
    /// The branch a pull request targets, without `refs/heads/`.
    pub base_ref: Option<String>,
    /// Files the push or pull request changed. `None` when unknown, which never blocks a run.
    pub changed_paths: Option<Vec<String>>,
}

impl EventInfo {
    pub fn push(git_ref: &str, changed: Option<Vec<String>>) -> Self {
        Self { name: "push".into(), git_ref: Some(git_ref.into()), changed_paths: changed, ..Default::default() }
    }
    pub fn pull_request(action: &str, base_branch: &str, changed: Option<Vec<String>>) -> Self {
        Self {
            name: "pull_request".into(),
            action: Some(action.into()),
            base_ref: Some(base_branch.into()),
            changed_paths: changed,
            ..Default::default()
        }
    }
    pub fn simple(name: &str) -> Self {
        Self { name: name.into(), ..Default::default() }
    }
}

fn pick(include: &Option<Vec<String>>, ignore: &Option<Vec<String>>, text: &str) -> bool {
    match (include, ignore) {
        (Some(inc), _) => glob::filter(inc, text),
        (None, Some(ign)) => !glob::filter(ign, text),
        (None, None) => true,
    }
}

/// Applies `paths` and `paths-ignore` to the changed files. Unknown changes let the run through.
fn paths_pass(paths: &Option<Vec<String>>, ignore: &Option<Vec<String>>, changed: &Option<Vec<String>>) -> bool {
    let Some(files) = changed else { return true };
    match (paths, ignore) {
        (Some(p), _) => {
            let set = glob::FilterSet::new(p);
            files.iter().any(|f| set.matches(f))
        }
        // Skip only when every changed file is ignored. A change with no files counts as a change.
        (None, Some(ign)) => {
            let set = glob::FilterSet::new(ign);
            files.is_empty() || !files.iter().all(|f| set.matches(f))
        }
        (None, None) => true,
    }
}

impl PushTrigger {
    pub fn matches(&self, e: &EventInfo) -> bool {
        let Some(r) = &e.git_ref else { return false };
        let has_b = self.branches.is_some() || self.branches_ignore.is_some();
        let has_t = self.tags.is_some() || self.tags_ignore.is_some();
        if let Some(b) = r.strip_prefix("refs/heads/") {
            if has_t && !has_b {
                return false;
            }
            if has_b && !pick(&self.branches, &self.branches_ignore, b) {
                return false;
            }
            paths_pass(&self.paths, &self.paths_ignore, &e.changed_paths)
        } else if let Some(t) = r.strip_prefix("refs/tags/") {
            if has_b && !has_t {
                return false;
            }
            !has_t || pick(&self.tags, &self.tags_ignore, t)
        } else {
            false
        }
    }
}

impl PullRequestTrigger {
    pub fn matches(&self, e: &EventInfo) -> bool {
        let action = e.action.as_deref().unwrap_or("opened");
        let ok = if self.types.is_empty() { DEFAULT_PR_TYPES.contains(&action) } else { self.types.iter().any(|t| t == action) };
        if !ok {
            return false;
        }
        if let Some(base) = &e.base_ref
            && !pick(&self.branches, &self.branches_ignore, base)
        {
            return false;
        }
        paths_pass(&self.paths, &self.paths_ignore, &e.changed_paths)
    }
}

impl Workflow {
    /// True when `event` should start this workflow. `schedule` and `workflow_dispatch` match on
    /// the event name alone: which cron fired, and the chosen ref, are decided by the caller.
    pub fn matches(&self, e: &EventInfo) -> bool {
        let t = &self.triggers;
        match e.name.as_str() {
            "push" => t.push.as_ref().is_some_and(|p| p.matches(e)),
            "pull_request" => t.pull_request.as_ref().is_some_and(|p| p.matches(e)),
            "workflow_dispatch" => t.workflow_dispatch.is_some(),
            "schedule" => !t.schedule.is_empty(),
            "release" => t.release.as_ref().is_some_and(|r| {
                let a = e.action.as_deref().unwrap_or("published");
                r.types.is_empty() || r.types.iter().any(|x| x == a)
            }),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn wf(on: &str) -> Workflow {
        parse(&format!("on:\n{}\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: echo\n", indent(on)))
            .unwrap_or_else(|e| panic!("{e:?}"))
    }
    fn indent(s: &str) -> String {
        s.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
    }
    fn files(f: &[&str]) -> Option<Vec<String>> {
        Some(f.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn plain_push_matches_every_branch_and_tag() {
        let w = wf("push:");
        assert!(w.matches(&EventInfo::push("refs/heads/main", None)));
        assert!(w.matches(&EventInfo::push("refs/heads/feature/x", None)));
        assert!(w.matches(&EventInfo::push("refs/tags/v1", None)));
        assert!(!w.matches(&EventInfo::push("refs/pull/1/head", None)));
        assert!(!w.matches(&EventInfo::simple("pull_request")));
    }

    #[test]
    fn branch_filters() {
        let w = wf("push:\n  branches: [main, 'release/**']");
        assert!(w.matches(&EventInfo::push("refs/heads/main", None)));
        assert!(w.matches(&EventInfo::push("refs/heads/release/1.0", None)));
        assert!(!w.matches(&EventInfo::push("refs/heads/dev", None)));
        assert!(!w.matches(&EventInfo::push("refs/tags/v1", None)), "branches only: tags do not run it");
        let w = wf("push:\n  branches-ignore: ['wip/**']");
        assert!(w.matches(&EventInfo::push("refs/heads/main", None)));
        assert!(!w.matches(&EventInfo::push("refs/heads/wip/x", None)));
    }

    #[test]
    fn tag_filters() {
        let w = wf("push:\n  tags: ['v*']");
        assert!(w.matches(&EventInfo::push("refs/tags/v1.2", None)));
        assert!(!w.matches(&EventInfo::push("refs/tags/nightly", None)));
        assert!(!w.matches(&EventInfo::push("refs/heads/main", None)), "tags only: branches do not run it");
        let w = wf("push:\n  branches: [main]\n  tags: ['v*']");
        assert!(w.matches(&EventInfo::push("refs/heads/main", None)));
        assert!(w.matches(&EventInfo::push("refs/tags/v1", None)));
        assert!(!w.matches(&EventInfo::push("refs/heads/dev", None)));
        assert!(!w.matches(&EventInfo::push("refs/tags/x", None)));
        let w = wf("push:\n  tags-ignore: ['v0.*']");
        assert!(w.matches(&EventInfo::push("refs/tags/v1.0", None)));
        assert!(!w.matches(&EventInfo::push("refs/tags/v0.1", None)));
    }

    #[test]
    fn path_filters() {
        let w = wf("push:\n  paths: ['src/**', 'Cargo.toml']");
        assert!(w.matches(&EventInfo::push("refs/heads/main", files(&["src/a.rs"]))));
        assert!(w.matches(&EventInfo::push("refs/heads/main", files(&["README.md", "Cargo.toml"]))));
        assert!(!w.matches(&EventInfo::push("refs/heads/main", files(&["README.md"]))));
        assert!(w.matches(&EventInfo::push("refs/heads/main", None)), "unknown changes never block a run");
        let w = wf("push:\n  paths-ignore: ['docs/**', '*.md']");
        assert!(!w.matches(&EventInfo::push("refs/heads/main", files(&["docs/a.txt", "x.md"]))));
        assert!(w.matches(&EventInfo::push("refs/heads/main", files(&["docs/a.txt", "src/a.rs"]))));
        assert!(w.matches(&EventInfo::push("refs/heads/main", files(&[]))));
        let w = wf("push:\n  paths: ['src/**', '!src/gen/**']");
        assert!(!w.matches(&EventInfo::push("refs/heads/main", files(&["src/gen/x.rs"]))));
        assert!(w.matches(&EventInfo::push("refs/heads/main", files(&["src/gen/x.rs", "src/y.rs"]))));
    }

    #[test]
    fn pull_request_types_default_to_the_usual_three() {
        let w = wf("pull_request:");
        for a in ["opened", "synchronize", "reopened"] {
            assert!(w.matches(&EventInfo::pull_request(a, "main", None)), "{a}");
        }
        for a in ["closed", "labeled", "edited"] {
            assert!(!w.matches(&EventInfo::pull_request(a, "main", None)), "{a}");
        }
        let w = wf("pull_request:\n  types: [labeled, closed]");
        assert!(w.matches(&EventInfo::pull_request("labeled", "main", None)));
        assert!(!w.matches(&EventInfo::pull_request("opened", "main", None)));
    }

    #[test]
    fn pull_request_branch_filters_use_the_target_branch() {
        let w = wf("pull_request:\n  branches: [main]");
        assert!(w.matches(&EventInfo::pull_request("opened", "main", None)));
        assert!(!w.matches(&EventInfo::pull_request("opened", "dev", None)));
        let w = wf("pull_request:\n  paths: ['src/**']");
        assert!(!w.matches(&EventInfo::pull_request("opened", "main", files(&["a.md"]))));
    }

    #[test]
    fn other_events() {
        let w = wf("workflow_dispatch:\nschedule:\n  - cron: '0 3 * * *'\nrelease:\n  types: [published]");
        assert!(w.matches(&EventInfo::simple("workflow_dispatch")));
        assert!(w.matches(&EventInfo::simple("schedule")));
        let mut r = EventInfo::simple("release");
        assert!(w.matches(&r), "published is the default action");
        r.action = Some("created".into());
        assert!(!w.matches(&r));
        assert!(!w.matches(&EventInfo::push("refs/heads/main", None)));
        assert!(!wf("push:").matches(&EventInfo::simple("workflow_dispatch")));
    }

    #[test]
    fn on_can_be_a_name_or_a_list() {
        assert!(wf("push").triggers.push.is_some());
        let w = parse("on: [push, pull_request]\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: echo\n").unwrap();
        assert!(w.triggers.push.is_some() && w.triggers.pull_request.is_some());
        let w = parse("on: push\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: echo\n").unwrap();
        assert!(w.triggers.push.is_some());
    }
}
