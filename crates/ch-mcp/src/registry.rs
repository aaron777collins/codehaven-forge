//! The tool registry: a static list of 46 tools in a fixed order, each with a title, a closed
//! input schema, a short factual description and all four annotations.
//!
//! Nothing here depends on who is calling or on anything a person wrote, so a description or
//! schema can never carry an instruction from a repository. A tool that the grant lacks the scope
//! for is still listed, so Claude can ask for more access (step-up) instead of never knowing.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Write,
    Workflows,
}

impl Scope {
    pub fn word(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Workflows => "workflows",
        }
    }
}

/// How a tool behaves, which sets its annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Read,
    /// Adds something new and changes nothing that exists.
    Additive,
    /// Can change or replace what exists.
    Changing,
}

#[derive(Debug, Clone, Copy)]
pub enum Ty {
    Str(usize),
    Int(i64, i64),
    Bool,
    Enum(&'static [&'static str]),
    StrList(usize, usize),
    /// An array of file operations, checked by the tool itself.
    Files,
    /// An object, checked by the tool itself.
    Object,
}

#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
    pub desc: &'static str,
}

const fn s(name: &'static str, max: usize, required: bool, desc: &'static str) -> Param {
    Param { name, ty: Ty::Str(max), required, desc }
}
const fn i(name: &'static str, min: i64, max: i64, required: bool, desc: &'static str) -> Param {
    Param { name, ty: Ty::Int(min, max), required, desc }
}
const fn b(name: &'static str, desc: &'static str) -> Param {
    Param { name, ty: Ty::Bool, required: false, desc }
}
const fn e(name: &'static str, vals: &'static [&'static str], required: bool, desc: &'static str) -> Param {
    Param { name, ty: Ty::Enum(vals), required, desc }
}

const OWNER: Param = s("owner", 100, true, "Repository owner: a user or organization name.");
const REPO: Param = s("repo", 100, true, "Repository name.");
const REF: Param = s("ref", 255, false, "Branch, tag or full commit id. Defaults to the default branch.");
const PAGE: Param = i("page", 1, 10_000, false, "Page number, starting at 1.");
const PER_PAGE: Param = i("per_page", 1, 50, false, "Items per page. Default 30, at most 50.");
const NUMBER: Param = i("number", 1, 1_000_000_000, true, "The issue or pull request number.");
const LIMIT: Param = i("limit", 1, 50, false, "How many results. Default 20, at most 50.");
const STATE_ENUM: &[&str] = &["open", "closed", "all"];
const NEW_STATE: &[&str] = &["open", "closed"];
const REVIEW_KIND: &[&str] = &["comment", "request_changes"];
const MERGE_METHOD: &[&str] = &["merge", "squash", "rebase"];
const SIDE: &[&str] = &["left", "right"];

#[derive(Debug, Clone, Copy)]
pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub scope: Scope,
    pub class: Class,
    pub params: &'static [Param],
}

macro_rules! tool {
    ($name:literal, $title:literal, $scope:ident, $class:ident, $desc:literal, [$($p:expr),* $(,)?]) => {
        Tool { name: $name, title: $title, description: $desc, scope: Scope::$scope, class: Class::$class, params: &[$($p),*] }
    };
}

/// The 46 tools, in the order they are listed.
pub static TOOLS: &[Tool] = &[
    // ---- connection and repositories
    tool!(
        "get_connection",
        "Show this connection",
        Read,
        Read,
        "Shows the signed-in username and what this connection may read, write and run. Never returns an email address.",
        []
    ),
    tool!(
        "list_repositories",
        "List repositories",
        Read,
        Read,
        "Lists repositories this connection can read, most recently pushed first.",
        [PAGE, PER_PAGE]
    ),
    tool!(
        "search_repositories",
        "Search repositories",
        Read,
        Read,
        "Searches repository names and descriptions among those this connection can read.",
        [s("query", 200, true, "Words to search for."), LIMIT]
    ),
    tool!(
        "get_repository",
        "Get a repository",
        Read,
        Read,
        "Shows one repository: description, default branch, visibility and topics.",
        [OWNER, REPO]
    ),
    // ---- files
    tool!(
        "list_files",
        "List files",
        Read,
        Read,
        "Lists the files and folders at a path in a repository.",
        [OWNER, REPO, REF, s("path", 1024, false, "Folder path. Empty means the top level.")]
    ),
    tool!(
        "get_file",
        "Read a file",
        Read,
        Read,
        "Reads a text file, or a window of its lines. A binary file returns only its size and checksum.",
        [
            OWNER,
            REPO,
            REF,
            s("path", 1024, true, "File path."),
            i("start_line", 1, 10_000_000, false, "First line to return, starting at 1."),
            i("end_line", 1, 10_000_000, false, "Last line to return.")
        ]
    ),
    tool!(
        "search_code",
        "Search code",
        Read,
        Read,
        "Searches the text of files at one revision of a repository.",
        [OWNER, REPO, s("query", 200, true, "Text to find."), REF, LIMIT]
    ),
    // ---- refs and commits
    tool!(
        "list_branches",
        "List branches",
        Read,
        Read,
        "Lists the branches of a repository with the commit each points at.",
        [OWNER, REPO]
    ),
    tool!("list_tags", "List tags", Read, Read, "Lists the tags of a repository with the commit each points at.", [OWNER, REPO]),
    tool!(
        "compare_refs",
        "Compare two revisions",
        Read,
        Read,
        "Shows the commits and the files that differ between a base and a head revision.",
        [OWNER, REPO, s("base", 255, true, "Base branch, tag or commit."), s("head", 255, true, "Head branch, tag or commit.")]
    ),
    tool!(
        "list_commits",
        "List commits",
        Read,
        Read,
        "Lists commits reachable from a revision, newest first, optionally only those touching a path.",
        [OWNER, REPO, REF, s("path", 1024, false, "Only commits that changed this path."), PAGE, PER_PAGE]
    ),
    tool!(
        "get_commit",
        "Get a commit",
        Read,
        Read,
        "Shows one commit: message, author name and date, parents and the files it changed.",
        [OWNER, REPO, s("sha", 64, true, "A full commit id, a branch or a tag.")]
    ),
    // ---- issues
    tool!(
        "list_issues",
        "List issues",
        Read,
        Read,
        "Lists issues in a repository.",
        [
            OWNER,
            REPO,
            e("state", STATE_ENUM, false, "Open, closed or all. Default open."),
            s("labels", 200, false, "Comma separated label names."),
            PAGE,
            PER_PAGE
        ]
    ),
    tool!(
        "search_issues",
        "Search issues",
        Read,
        Read,
        "Searches issues and pull requests among the repositories this connection can read.",
        [s("query", 200, true, "Words to search for."), LIMIT]
    ),
    tool!("get_issue", "Get an issue", Read, Read, "Shows one issue with its labels, state and counts.", [OWNER, REPO, NUMBER]),
    tool!(
        "list_issue_comments",
        "List issue comments",
        Read,
        Read,
        "Lists the comments on an issue or pull request, oldest first.",
        [OWNER, REPO, NUMBER]
    ),
    tool!("list_labels", "List labels", Read, Read, "Lists the labels defined in a repository.", [OWNER, REPO]),
    // ---- pull requests
    tool!(
        "list_pull_requests",
        "List pull requests",
        Read,
        Read,
        "Lists pull requests in a repository.",
        [OWNER, REPO, e("state", STATE_ENUM, false, "Open, closed or all. Default open."), PAGE, PER_PAGE]
    ),
    tool!(
        "get_pull_request",
        "Get a pull request",
        Read,
        Read,
        "Shows one pull request with its branches, state and the merge gate: what still blocks a merge.",
        [OWNER, REPO, NUMBER]
    ),
    tool!(
        "get_pull_request_diff",
        "Get a pull request diff",
        Read,
        Read,
        "Shows the changed files of a pull request with their patches, up to 100 files a page.",
        [OWNER, REPO, NUMBER, PAGE]
    ),
    tool!(
        "list_pull_request_commits",
        "List pull request commits",
        Read,
        Read,
        "Lists the commits of a pull request.",
        [OWNER, REPO, NUMBER]
    ),
    tool!(
        "get_pull_request_checks",
        "Get pull request checks",
        Read,
        Read,
        "Shows the status checks and check runs on the head commit of a pull request.",
        [OWNER, REPO, NUMBER]
    ),
    tool!("list_reviews", "List reviews", Read, Read, "Lists the reviews on a pull request.", [OWNER, REPO, NUMBER]),
    tool!(
        "list_review_threads",
        "List review threads",
        Read,
        Read,
        "Lists the inline comment threads on a pull request.",
        [OWNER, REPO, NUMBER]
    ),
    // ---- CI
    tool!(
        "get_commit_status",
        "Get commit status",
        Read,
        Read,
        "Shows the combined status, statuses and check runs of a commit.",
        [OWNER, REPO, s("sha", 64, true, "A full commit id, a branch or a tag.")]
    ),
    tool!(
        "list_workflows",
        "List workflows",
        Read,
        Read,
        "Lists the workflow files of a repository and the events that start them.",
        [OWNER, REPO, REF]
    ),
    tool!(
        "list_workflow_runs",
        "List workflow runs",
        Read,
        Read,
        "Lists recent workflow runs of a repository.",
        [OWNER, REPO, PAGE, PER_PAGE]
    ),
    tool!(
        "get_workflow_run",
        "Get a workflow run",
        Read,
        Read,
        "Shows one workflow run and its jobs.",
        [OWNER, REPO, i("run_id", 1, i64::MAX / 2, true, "The run id.")]
    ),
    tool!(
        "get_job_log",
        "Get a job log",
        Read,
        Read,
        "Reads the log of one job. By default the end of the log; pass a cursor to read from a byte position.",
        [
            OWNER,
            REPO,
            i("job_id", 1, i64::MAX / 2, true, "The job id."),
            i("cursor", 0, i64::MAX / 2, false, "Byte position to read from. Omit for the end of the log.")
        ]
    ),
    // ---- inbox
    tool!(
        "list_notifications",
        "List notifications",
        Read,
        Read,
        "Lists the notifications for repositories this connection can read.",
        [b("unread_only", "Only unread notifications."), PAGE, PER_PAGE]
    ),
    // ---- additive writes
    tool!(
        "create_repository",
        "Create a repository",
        Write,
        Additive,
        "Creates a new private repository in the signed-in person's own namespace.",
        [s("name", 100, true, "Repository name."), s("description", 500, false, "Short description.")]
    ),
    tool!(
        "create_branch",
        "Create a branch",
        Write,
        Additive,
        "Creates a new branch from an existing branch, tag or commit.",
        [
            OWNER,
            REPO,
            s("name", 255, true, "New branch name."),
            s("from", 255, false, "Branch, tag or full commit id to start from. Defaults to the default branch.")
        ]
    ),
    tool!(
        "create_issue",
        "Create an issue",
        Write,
        Additive,
        "Opens a new issue.",
        [
            OWNER,
            REPO,
            s("title", 256, true, "Issue title."),
            s("body", 60_000, false, "Issue text, Markdown."),
            Param { name: "labels", ty: Ty::StrList(20, 100), required: false, desc: "Label names to add." }
        ]
    ),
    tool!(
        "add_issue_comment",
        "Comment on an issue",
        Write,
        Additive,
        "Adds a comment to an issue or pull request.",
        [OWNER, REPO, NUMBER, s("body", 60_000, true, "Comment text, Markdown.")]
    ),
    tool!(
        "create_pull_request",
        "Open a pull request",
        Write,
        Additive,
        "Opens a pull request from a branch of this repository into another.",
        [
            OWNER,
            REPO,
            s("title", 256, true, "Pull request title."),
            s("head", 255, true, "The branch with the changes."),
            s("base", 255, false, "The branch to merge into. Defaults to the default branch."),
            s("body", 60_000, false, "Description, Markdown."),
            b("draft", "Open as a draft.")
        ]
    ),
    tool!(
        "add_review_comment",
        "Comment on a line",
        Write,
        Additive,
        "Adds an inline comment on a changed line of a pull request.",
        [
            OWNER,
            REPO,
            NUMBER,
            s("path", 1024, true, "File path."),
            i("line", 1, 10_000_000, true, "Line number."),
            e("side", SIDE, false, "Which side of the diff. Default right."),
            s("body", 60_000, true, "Comment text, Markdown.")
        ]
    ),
    // ---- changing writes
    tool!(
        "commit_files",
        "Commit file changes",
        Write,
        Changing,
        "Commits up to 50 file changes (create, replace_text, update, delete, rename) to a branch, or to a new branch made from it. The expected head must match the branch, so a concurrent change is never overwritten.",
        [
            OWNER,
            REPO,
            s("branch", 255, true, "Branch to commit to."),
            s("expected_head", 64, true, "The commit the branch must currently point at."),
            s("new_branch", 255, false, "Create this branch from the expected head and commit there instead."),
            s("message", 5_000, true, "Commit message."),
            Param {
                name: "changes",
                ty: Ty::Files,
                required: true,
                desc: "File operations: objects with op (create, replace_text, update, delete, rename), path, and content, old_text, new_text or from as the operation needs."
            }
        ]
    ),
    tool!(
        "update_issue",
        "Update an issue",
        Write,
        Changing,
        "Changes an issue's title, text, state or labels.",
        [
            OWNER,
            REPO,
            NUMBER,
            s("title", 256, false, "New title."),
            s("body", 60_000, false, "New text."),
            e("state", NEW_STATE, false, "Open or close it."),
            Param {
                name: "labels",
                ty: Ty::StrList(20, 100),
                required: false,
                desc: "The full list of label names. Replaces the current labels."
            }
        ]
    ),
    tool!(
        "update_pull_request",
        "Update a pull request",
        Write,
        Changing,
        "Changes a pull request's title, text or state (open or closed). It does not change the base branch.",
        [
            OWNER,
            REPO,
            NUMBER,
            s("title", 256, false, "New title."),
            s("body", 60_000, false, "New text."),
            e("state", NEW_STATE, false, "Open or close it.")
        ]
    ),
    tool!(
        "submit_review",
        "Submit a review",
        Write,
        Changing,
        "Submits a review as a comment or as a request for changes.",
        [
            OWNER,
            REPO,
            NUMBER,
            e("kind", REVIEW_KIND, true, "A plain comment, or request changes."),
            s("body", 60_000, false, "Review text, Markdown.")
        ]
    ),
    tool!(
        "update_pull_request_branch",
        "Update a pull request branch",
        Write,
        Changing,
        "Brings the base branch into the pull request's branch. The result is checked for changes to workflow files like any other commit.",
        [OWNER, REPO, NUMBER]
    ),
    tool!(
        "merge_pull_request",
        "Merge a pull request",
        Write,
        Changing,
        "Merges a pull request under the same gate and branch protection as the web page. The expected head must match.",
        [
            OWNER,
            REPO,
            NUMBER,
            s("expected_head", 64, true, "The head commit the merge is based on."),
            e("method", MERGE_METHOD, false, "Default: the first method the repository allows."),
            s("title", 256, false, "Merge commit title."),
            s("message", 5_000, false, "Merge commit message.")
        ]
    ),
    tool!(
        "mark_notification_read",
        "Mark a notification read",
        Write,
        Changing,
        "Marks one notification read, for a repository this connection may write to.",
        [i("id", 1, i64::MAX / 2, true, "The notification id.")]
    ),
    // ---- workflows scope
    tool!(
        "dispatch_workflow",
        "Run a workflow",
        Workflows,
        Changing,
        "Starts a workflow by hand. This runs code on the CI runner with the repository's secrets.",
        [
            OWNER,
            REPO,
            s("workflow", 255, true, "Workflow file path or name."),
            s("ref", 255, true, "Branch or tag to run it on."),
            Param { name: "inputs", ty: Ty::Object, required: false, desc: "Input values the workflow declares." }
        ]
    ),
    tool!(
        "rerun_workflow_run",
        "Rerun a workflow run",
        Workflows,
        Changing,
        "Runs a finished workflow run again, or only its failed jobs.",
        [OWNER, REPO, i("run_id", 1, i64::MAX / 2, true, "The run id."), b("failed_only", "Only rerun jobs that failed.")]
    ),
    tool!(
        "cancel_workflow_run",
        "Cancel a workflow run",
        Workflows,
        Changing,
        "Cancels a queued or running workflow run.",
        [OWNER, REPO, i("run_id", 1, i64::MAX / 2, true, "The run id.")]
    ),
];

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

impl Tool {
    /// The JSON schema of the arguments. Closed: no other key is accepted.
    pub fn input_schema(&self) -> Value {
        let mut props = Map::new();
        let mut required = Vec::new();
        for p in self.params {
            let mut o = Map::new();
            match p.ty {
                Ty::Str(max) => {
                    o.insert("type".into(), json!("string"));
                    o.insert("maxLength".into(), json!(max));
                }
                Ty::Int(min, max) => {
                    o.insert("type".into(), json!("integer"));
                    o.insert("minimum".into(), json!(min));
                    o.insert("maximum".into(), json!(max));
                }
                Ty::Bool => {
                    o.insert("type".into(), json!("boolean"));
                }
                Ty::Enum(vals) => {
                    o.insert("type".into(), json!("string"));
                    o.insert("enum".into(), json!(vals));
                }
                Ty::StrList(n, max) => {
                    o.insert("type".into(), json!("array"));
                    o.insert("maxItems".into(), json!(n));
                    o.insert("items".into(), json!({"type": "string", "maxLength": max}));
                }
                Ty::Files => {
                    o.insert("type".into(), json!("array"));
                    o.insert("minItems".into(), json!(1));
                    o.insert("maxItems".into(), json!(50));
                    o.insert(
                        "items".into(),
                        json!({
                            "type": "object",
                            "properties": {
                                "op": {"type": "string", "enum": ["create", "replace_text", "update", "delete", "rename"]},
                                "path": {"type": "string", "maxLength": 1024},
                                "content": {"type": "string"},
                                "old_text": {"type": "string"},
                                "new_text": {"type": "string"},
                                "from": {"type": "string", "maxLength": 1024}
                            },
                            "required": ["op", "path"],
                            "additionalProperties": false
                        }),
                    );
                }
                Ty::Object => {
                    o.insert("type".into(), json!("object"));
                    o.insert("additionalProperties".into(), json!({"type": "string", "maxLength": 1000}));
                }
            }
            o.insert("description".into(), json!(p.desc));
            props.insert(p.name.into(), Value::Object(o));
            if p.required {
                required.push(p.name);
            }
        }
        json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
    }

    pub fn annotations(&self) -> Value {
        let (read_only, destructive, open_world) = match self.class {
            Class::Read => (true, false, false),
            Class::Additive => (false, false, true),
            Class::Changing => (false, true, true),
        };
        json!({"readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": self.class == Class::Read, "openWorldHint": open_world})
    }

    /// One entry of `tools/list`.
    pub fn listing(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": self.input_schema(),
            "annotations": self.annotations(),
        })
    }

    pub fn is_write(&self) -> bool {
        self.class != Class::Read
    }

    /// Checks the arguments against the schema and returns them. `Err` is a sentence for the
    /// protocol error.
    pub fn check(&self, args: &Value) -> Result<Args, String> {
        let empty = Map::new();
        let obj = match args {
            Value::Null => &empty,
            Value::Object(m) => m,
            _ => return Err("arguments must be an object".into()),
        };
        for k in obj.keys() {
            if !self.params.iter().any(|p| p.name == k) {
                return Err(format!("unknown argument '{k}'"));
            }
        }
        for p in self.params {
            let v = obj.get(p.name);
            let Some(v) = v.filter(|v| !v.is_null()) else {
                if p.required {
                    return Err(format!("missing required argument '{}'", p.name));
                }
                continue;
            };
            let ok = match p.ty {
                Ty::Str(max) => v.as_str().is_some_and(|s| s.chars().count() <= max && !s.contains('\0')),
                Ty::Int(min, max) => v.as_i64().is_some_and(|n| n >= min && n <= max),
                Ty::Bool => v.is_boolean(),
                Ty::Enum(vals) => v.as_str().is_some_and(|s| vals.contains(&s)),
                Ty::StrList(n, max) => v.as_array().is_some_and(|a| {
                    a.len() <= n && a.iter().all(|x| x.as_str().is_some_and(|s| s.chars().count() <= max && !s.is_empty()))
                }),
                Ty::Files => v.as_array().is_some_and(|a| !a.is_empty() && a.len() <= 50 && a.iter().all(|x| x.is_object())),
                Ty::Object => v
                    .as_object()
                    .is_some_and(|m| m.len() <= 20 && m.values().all(|x| x.as_str().is_some_and(|s| s.chars().count() <= 1000))),
            };
            if !ok {
                return Err(format!("argument '{}' is not valid: {}", p.name, p.desc));
            }
        }
        Ok(Args { map: obj.clone() })
    }
}

/// Checked arguments. Every accessor can rely on the schema having been enforced.
#[derive(Debug, Clone)]
pub struct Args {
    map: Map<String, Value>,
}

impl Args {
    pub fn str(&self, name: &str) -> Option<&str> {
        self.map.get(name).and_then(|v| v.as_str())
    }
    pub fn req(&self, name: &str) -> &str {
        self.str(name).unwrap_or("")
    }
    pub fn int(&self, name: &str) -> Option<i64> {
        self.map.get(name).and_then(|v| v.as_i64())
    }
    pub fn bool(&self, name: &str) -> bool {
        self.map.get(name).and_then(|v| v.as_bool()).unwrap_or(false)
    }
    pub fn has(&self, name: &str) -> bool {
        self.map.get(name).is_some_and(|v| !v.is_null())
    }
    pub fn list(&self, name: &str) -> Vec<String> {
        self.map
            .get(name)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }
    pub fn array(&self, name: &str) -> Vec<Value> {
        self.map.get(name).and_then(|v| v.as_array()).cloned().unwrap_or_default()
    }
    pub fn object(&self, name: &str) -> Map<String, Value> {
        self.map.get(name).and_then(|v| v.as_object()).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_exactly_46_tools_with_the_right_split() {
        assert_eq!(TOOLS.len(), 46);
        let read = TOOLS.iter().filter(|t| t.class == Class::Read).count();
        let additive = TOOLS.iter().filter(|t| t.class == Class::Additive).count();
        let changing = TOOLS.iter().filter(|t| t.class == Class::Changing).count();
        assert_eq!((read, additive, changing), (30, 6, 10), "30 read, 6 additive, 7 changing plus 3 workflows");
        assert_eq!(TOOLS.iter().filter(|t| t.scope == Scope::Workflows).count(), 3);
        assert_eq!(TOOLS.iter().filter(|t| t.scope == Scope::Write).count(), 13);
        assert!(TOOLS.iter().filter(|t| t.class == Class::Read).all(|t| t.scope == Scope::Read));
    }

    #[test]
    fn names_are_valid_unique_and_listing_is_complete() {
        let mut seen = std::collections::HashSet::new();
        for t in TOOLS {
            assert!(!t.name.is_empty() && t.name.len() <= 64 && t.name.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'), "{}", t.name);
            assert!(seen.insert(t.name), "duplicate {}", t.name);
            assert!(!t.title.is_empty() && !t.description.is_empty());
            assert!(t.description.len() < 400, "{} description is long", t.name);
            let l = t.listing();
            assert_eq!(l["inputSchema"]["type"], "object");
            assert_eq!(l["inputSchema"]["additionalProperties"], false, "{} schema must be closed", t.name);
            for k in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
                assert!(l["annotations"][k].is_boolean(), "{} {k}", t.name);
            }
        }
    }

    #[test]
    fn annotations_follow_the_class() {
        let a = |n: &str| find(n).unwrap().annotations();
        assert_eq!(a("get_file"), json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}));
        assert_eq!(
            a("create_issue"),
            json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true})
        );
        for n in [
            "commit_files",
            "update_issue",
            "update_pull_request",
            "submit_review",
            "update_pull_request_branch",
            "merge_pull_request",
            "mark_notification_read",
            "dispatch_workflow",
            "rerun_workflow_run",
            "cancel_workflow_run",
        ] {
            assert_eq!(
                a(n),
                json!({"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true}),
                "{n}"
            );
        }
    }

    #[test]
    fn no_tool_exists_for_a_forbidden_area() {
        // Webhooks, secrets, variables, runner tokens, releases, tag writes, branch deletion,
        // auto-merge, dismissing a review, resolving a thread, approving, changing a base,
        // git transport and the registry are never reachable through a tool.
        let banned = [
            "webhook",
            "secret",
            "variable",
            "runner",
            "release",
            "delete",
            "auto_merge",
            "dismiss",
            "resolve",
            "approve",
            "registry",
            "transport",
            "invite",
            "account",
            "password",
            "credential",
            "ssh",
            "organization",
            "team",
            "collaborator",
            "permission",
            "visibility",
            "transfer",
            "rename_repo",
            "fork",
        ];
        for t in TOOLS {
            let hay = format!("{} {}", t.name, t.title).to_ascii_lowercase();
            for w in banned {
                assert!(!hay.contains(w), "tool {} mentions {w}", t.name);
            }
            // A description may say what a tool does not do, or warn about secrets, but it never
            // offers one of these areas.
            let desc = t.description.to_ascii_lowercase();
            for w in ["webhook", "registry", "invite", "password", "credential", "ssh key"] {
                assert!(!desc.contains(w), "tool {} description mentions {w}", t.name);
            }
        }
        assert!(find("request").is_none() && find("api").is_none(), "no generic request tool");
    }

    #[test]
    fn argument_checking() {
        let t = find("get_file").unwrap();
        assert!(t.check(&json!({"owner": "a", "repo": "b", "path": "x"})).is_ok());
        assert!(t.check(&json!({"owner": "a", "repo": "b"})).is_err(), "required");
        assert!(t.check(&json!({"owner": "a", "repo": "b", "path": "x", "extra": 1})).is_err(), "closed");
        assert!(t.check(&json!({"owner": "a", "repo": "b", "path": "x", "start_line": 0})).is_err(), "range");
        assert!(t.check(&json!({"owner": 1, "repo": "b", "path": "x"})).is_err(), "type");
        assert!(t.check(&json!({"owner": "a\u{0}", "repo": "b", "path": "x"})).is_err(), "nul");
        assert!(t.check(&json!("nope")).is_err());
        let m = find("merge_pull_request").unwrap();
        assert!(m.check(&json!({"owner": "a", "repo": "b", "number": 1, "expected_head": "abc"})).is_ok());
        assert!(m.check(&json!({"owner": "a", "repo": "b", "number": 1})).is_err(), "expected head is required");
        assert!(m.check(&json!({"owner": "a", "repo": "b", "number": 1, "expected_head": "abc", "method": "fast"})).is_err());
        let c = find("commit_files").unwrap();
        assert!(
            c.check(&json!({"owner": "a", "repo": "b", "branch": "x", "expected_head": "h", "message": "m", "changes": []})).is_err(),
            "at least one change"
        );
        let many: Vec<Value> = (0..51).map(|n| json!({"op": "create", "path": format!("f{n}")})).collect();
        assert!(
            c.check(&json!({"owner": "a", "repo": "b", "branch": "x", "expected_head": "h", "message": "m", "changes": many})).is_err(),
            "at most 50"
        );
        let r = find("submit_review").unwrap();
        assert!(r.check(&json!({"owner": "a", "repo": "b", "number": 1, "kind": "approve"})).is_err(), "there is no approving");
    }
}
