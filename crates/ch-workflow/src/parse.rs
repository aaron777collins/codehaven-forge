//! Reads a workflow file into the typed model, collecting every problem with its line.
//!
//! The parser keeps going after an error so a person sees all of them at once. It is strict
//! about keys it does not know (a typo should not silently turn a job off) and specific about
//! the Actions features Codehaven does not run.

use crate::actions::{self, Builtin};
use crate::diag::Diagnostic;
use crate::expr;
use crate::model::*;
use marked_yaml::types::{MarkedMappingNode, MarkedScalarNode};
use marked_yaml::{LoadError, LoaderOptions, Node};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// The most jobs one run may expand to, matrix cells included.
pub const MAX_MATRIX_JOBS: usize = 256;
/// A workflow file larger than this is refused before parsing.
pub const MAX_FILE_BYTES: usize = 512 * 1024;
/// How deep blocks and flow collections may nest. The YAML parser recurses with the nesting, and
/// a stack overflow takes the whole process down, so hostile files are turned away before it sees
/// them. Real workflows rarely go past 8.
pub const MAX_NESTING: usize = 50;

pub const SUPPORTED_EVENTS: &[&str] = &["push", "pull_request", "workflow_dispatch", "schedule", "release"];

/// Parses and validates. Every problem found is in the error, in file order.
pub fn parse(src: &str) -> Result<Workflow, Vec<Diagnostic>> {
    if src.len() > MAX_FILE_BYTES {
        return Err(vec![Diagnostic::new(0, 0, format!("the file is larger than {} KiB", MAX_FILE_BYTES / 1024))]);
    }
    if let Some(line) = nesting_too_deep(src) {
        return Err(vec![Diagnostic::new(line, 1, format!("the file is nested more than {MAX_NESTING} levels deep"))]);
    }
    let opts = LoaderOptions::default().error_on_duplicate_keys(true).prevent_coercion(true);
    let root = match marked_yaml::parse_yaml_with_options(0, src, opts) {
        Ok(n) => n,
        Err(e) => return Err(vec![load_error(&e)]),
    };
    let mut p = P { diags: Vec::new() };
    let wf = p.workflow(&root);
    p.diags.sort_by_key(|d| (d.line, d.column));
    p.diags.dedup();
    match wf {
        Some(w) if p.diags.is_empty() => Ok(w),
        _ => {
            if p.diags.is_empty() {
                p.diags.push(Diagnostic::new(0, 0, "the file is not a valid workflow"));
            }
            Err(p.diags)
        }
    }
}

/// The line where the nesting first passes `MAX_NESTING`, counting block indentation, `- `
/// sequence markers and `[`/`{` flow brackets. A rough scan, deliberately: it only has to
/// be safe, and it errs towards refusing.
pub fn nesting_too_deep(src: &str) -> Option<u32> {
    let mut indents: Vec<usize> = Vec::new();
    let mut flow: usize = 0;
    for (n, raw) in src.lines().enumerate() {
        let line_no = (n + 1) as u32;
        let trimmed = raw.trim_start_matches(' ');
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut col = raw.len() - trimmed.len();
        let mut rest = trimmed;
        // Block structure: each indent level, and each `- ` marker, is one level.
        if flow == 0 {
            while indents.last().is_some_and(|&t| t >= col) {
                indents.pop();
            }
            indents.push(col);
            while let Some(after) = rest.strip_prefix("- ") {
                let skipped = rest.len() - after.len();
                col += skipped;
                let after_trim = after.trim_start_matches(' ');
                col += after.len() - after_trim.len();
                rest = after_trim;
                indents.push(col);
                if indents.len() + flow > MAX_NESTING {
                    return Some(line_no);
                }
            }
        }
        if indents.len() + flow > MAX_NESTING {
            return Some(line_no);
        }
        // Flow structure: brackets outside quotes.
        let (mut in_s, mut in_d) = (false, false);
        let mut prev = ' ';
        for c in rest.chars() {
            match c {
                '\'' if !in_d => in_s = !in_s,
                '"' if !in_s && prev != '\\' => in_d = !in_d,
                '#' if !in_s && !in_d && prev == ' ' => break,
                '[' | '{' if !in_s && !in_d => {
                    flow += 1;
                    if indents.len() + flow > MAX_NESTING {
                        return Some(line_no);
                    }
                }
                ']' | '}' if !in_s && !in_d => flow = flow.saturating_sub(1),
                _ => {}
            }
            prev = c;
        }
    }
    None
}

fn load_error(e: &LoadError) -> Diagnostic {
    let at = |m: &marked_yaml::Marker, msg: String| Diagnostic::new(m.line() as u32, m.column() as u32, msg);
    match e {
        LoadError::TopLevelMustBeMapping(m) => at(m, "a workflow file must be a mapping with `on` and `jobs`".into()),
        LoadError::TopLevelMustBeSequence(m) => at(m, "expected a list".into()),
        LoadError::UnexpectedAnchor(m) => at(m, "YAML anchors and aliases are not supported. Write the value out in full".into()),
        LoadError::MappingKeyMustBeScalar(m) => at(m, "a key must be plain text".into()),
        LoadError::UnexpectedTag(m) => at(m, "YAML tags are not supported".into()),
        LoadError::ScanError(m, err) => at(m, format!("the YAML is not valid: {}", err.info())),
        LoadError::DuplicateKey(d) => {
            let s = d.key.span().start();
            Diagnostic::new(
                s.map(|m| m.line() as u32).unwrap_or(0),
                s.map(|m| m.column() as u32).unwrap_or(0),
                format!("`{}` appears twice in the same mapping", d.key.as_str()),
            )
        }
    }
}

fn pos(n: &Node) -> (u32, u32) {
    n.span().start().map(|m| (m.line() as u32, m.column() as u32)).unwrap_or((0, 0))
}
fn kpos(k: &MarkedScalarNode) -> (u32, u32) {
    k.span().start().map(|m| (m.line() as u32, m.column() as u32)).unwrap_or((0, 0))
}

fn is_null(n: &Node) -> bool {
    match n {
        Node::Scalar(s) => s.may_coerce() && matches!(s.as_str(), "" | "~" | "null" | "Null" | "NULL"),
        _ => false,
    }
}

struct P {
    diags: Vec<Diagnostic>,
}

type Entries<'a> = Vec<(&'a MarkedScalarNode, &'a Node)>;

impl P {
    fn err(&mut self, at: (u32, u32), msg: impl Into<String>) {
        self.diags.push(Diagnostic::new(at.0, at.1, msg));
    }

    fn map<'a>(&mut self, n: &'a Node, what: &str) -> Option<&'a MarkedMappingNode> {
        match n {
            Node::Mapping(m) => Some(m),
            _ if is_null(n) => None,
            _ => {
                self.err(pos(n), format!("`{what}` must be a mapping"));
                None
            }
        }
    }

    fn entries<'a>(&mut self, n: &'a Node, what: &str) -> Entries<'a> {
        self.map(n, what).map(|m| m.iter().collect()).unwrap_or_default()
    }

    /// Text of a scalar. A null (`key:` with nothing) is the empty string.
    fn text(&mut self, n: &Node, what: &str) -> Option<String> {
        match n {
            Node::Scalar(s) => Some(if is_null(n) { String::new() } else { s.as_str().to_string() }),
            _ => {
                self.err(pos(n), format!("`{what}` must be text"));
                None
            }
        }
    }

    /// Text that may hold `${{ }}`, checked for syntax.
    fn template(&mut self, n: &Node, what: &str) -> Option<String> {
        let t = self.text(n, what)?;
        if let Err(e) = expr::check_template(&t) {
            self.err(pos(n), format!("in `{what}`: {}", e.message));
        }
        Some(t)
    }

    fn condition(&mut self, n: &Node) -> Option<String> {
        let t = self.text(n, "if")?;
        let src = t.trim();
        let r = if expr::has_expression(src) {
            expr::check_template(src)
        } else if src.is_empty() {
            Ok(())
        } else {
            expr::check_expr_source(src)
        };
        if let Err(e) = r {
            self.err(pos(n), format!("in `if`: {}", e.message));
        }
        Some(t)
    }

    /// `true`, `false`, or an expression, kept as written.
    fn bool_or_expr(&mut self, n: &Node, what: &str) -> Option<String> {
        let Node::Scalar(s) = n else {
            self.err(pos(n), format!("`{what}` must be true, false or an expression"));
            return None;
        };
        let t = s.as_str();
        if s.may_coerce() && matches!(t, "true" | "false" | "True" | "False" | "TRUE" | "FALSE") {
            return Some(t.to_ascii_lowercase());
        }
        if expr::has_expression(t) {
            if let Err(e) = expr::check_template(t) {
                self.err(pos(n), format!("in `{what}`: {}", e.message));
            }
            return Some(t.to_string());
        }
        self.err(pos(n), format!("`{what}` must be true, false or an expression"));
        None
    }

    /// A whole number, or an expression.
    fn uint_or_expr(&mut self, n: &Node, what: &str) -> Option<String> {
        let Node::Scalar(s) = n else {
            self.err(pos(n), format!("`{what}` must be a number"));
            return None;
        };
        let t = s.as_str().trim();
        if expr::has_expression(t) {
            if let Err(e) = expr::check_template(t) {
                self.err(pos(n), format!("in `{what}`: {}", e.message));
            }
            return Some(t.to_string());
        }
        match t.parse::<u32>() {
            Ok(v) if v > 0 => Some(v.to_string()),
            _ => {
                self.err(pos(n), format!("`{what}` must be a whole number above zero"));
                None
            }
        }
    }

    /// A scalar or a list of scalars, as a list.
    fn string_list(&mut self, n: &Node, what: &str) -> Vec<String> {
        match n {
            Node::Scalar(_) if is_null(n) => Vec::new(),
            Node::Scalar(s) => vec![s.as_str().to_string()],
            Node::Sequence(seq) => {
                let mut out = Vec::new();
                for item in seq.iter() {
                    match item {
                        Node::Scalar(s) => out.push(s.as_str().to_string()),
                        _ => self.err(pos(item), format!("every entry of `{what}` must be text")),
                    }
                }
                out
            }
            Node::Mapping(_) => {
                self.err(pos(n), format!("`{what}` must be text or a list"));
                Vec::new()
            }
        }
    }

    fn string_map(&mut self, n: &Node, what: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (k, v) in self.entries(n, what) {
            match v {
                Node::Scalar(_) => {
                    if let Some(t) = self.template(v, &format!("{what}.{}", k.as_str())) {
                        out.insert(k.as_str().to_string(), t);
                    }
                }
                _ => self.err(pos(v), format!("`{what}.{}` must be a single value", k.as_str())),
            }
        }
        out
    }

    fn unknown_key(&mut self, k: &MarkedScalarNode, place: &str, known: &[&str]) {
        let near = known.iter().find(|c| near_miss(k.as_str(), c));
        let hint = near.map(|c| format!(". Did you mean `{c}`?")).unwrap_or_default();
        self.err(kpos(k), format!("unknown key `{}` in {place}{hint}", k.as_str()));
    }

    // ------------------------------------------------------------------ workflow

    fn workflow(&mut self, root: &Node) -> Option<Workflow> {
        let Node::Mapping(m) = root else {
            self.err(pos(root), "a workflow file must be a mapping with `on` and `jobs`");
            return None;
        };
        let mut wf = Workflow {
            name: None,
            run_name: None,
            triggers: Triggers::default(),
            env: BTreeMap::new(),
            defaults: Defaults::default(),
            concurrency: None,
            jobs: Vec::new(),
        };
        let mut have_on = false;
        let mut jobs_node: Option<&Node> = None;
        const KNOWN: &[&str] = &["name", "run-name", "on", "env", "defaults", "concurrency", "jobs", "permissions"];
        for (k, v) in m.iter() {
            match k.as_str() {
                "name" => wf.name = self.template(v, "name").filter(|s| !s.trim().is_empty()),
                "run-name" => wf.run_name = self.template(v, "run-name"),
                "on" => {
                    have_on = true;
                    let before = self.diags.len();
                    wf.triggers = self.triggers(v);
                    if wf.triggers.is_empty() && self.diags.len() == before {
                        self.err(kpos(k), "`on` does not name any event that Codehaven supports");
                    }
                }
                "env" => wf.env = self.string_map(v, "env"),
                "defaults" => wf.defaults = self.defaults(v),
                "concurrency" => wf.concurrency = self.concurrency(v),
                "jobs" => jobs_node = Some(v),
                // Accepted and ignored: job tokens are always read-only on contents.
                "permissions" => {}
                _ => self.unknown_key(k, "the workflow", KNOWN),
            }
        }
        if !have_on {
            self.err((0, 0), "`on` is required: say which events start this workflow");
        }
        match jobs_node {
            None => self.err((0, 0), "`jobs` is required"),
            Some(j) => self.jobs(j, &mut wf),
        }
        self.check_graph(&wf);
        Some(wf)
    }

    fn defaults(&mut self, n: &Node) -> Defaults {
        let mut d = Defaults::default();
        for (k, v) in self.entries(n, "defaults") {
            if k.as_str() == "run" {
                for (k2, v2) in self.entries(v, "defaults.run") {
                    match k2.as_str() {
                        "shell" => d.shell = self.template(v2, "defaults.run.shell"),
                        "working-directory" => d.working_directory = self.template(v2, "defaults.run.working-directory"),
                        _ => self.unknown_key(k2, "`defaults.run`", &["shell", "working-directory"]),
                    }
                }
            } else {
                self.unknown_key(k, "`defaults`", &["run"]);
            }
        }
        d
    }

    fn concurrency(&mut self, n: &Node) -> Option<Concurrency> {
        match n {
            Node::Scalar(_) => {
                let g = self.template(n, "concurrency")?;
                Some(Concurrency { group: g, cancel_in_progress: "false".into() })
            }
            _ => {
                let mut group = None;
                let mut cancel = "false".to_string();
                for (k, v) in self.entries(n, "concurrency") {
                    match k.as_str() {
                        "group" => group = self.template(v, "concurrency.group"),
                        "cancel-in-progress" => {
                            if let Some(c) = self.bool_or_expr(v, "concurrency.cancel-in-progress") {
                                cancel = c;
                            }
                        }
                        _ => self.unknown_key(k, "`concurrency`", &["group", "cancel-in-progress"]),
                    }
                }
                match group {
                    Some(g) => Some(Concurrency { group: g, cancel_in_progress: cancel }),
                    None => {
                        self.err(pos(n), "`concurrency.group` is required");
                        None
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ triggers

    fn triggers(&mut self, n: &Node) -> Triggers {
        let mut t = Triggers::default();
        let mut events: Vec<(&MarkedScalarNode, Option<&Node>)> = Vec::new();
        let mut name_nodes: Vec<(String, (u32, u32))> = Vec::new();
        match n {
            Node::Scalar(s) => name_nodes.push((s.as_str().to_string(), pos(n))),
            Node::Sequence(seq) => {
                for item in seq.iter() {
                    match item {
                        Node::Scalar(s) => name_nodes.push((s.as_str().to_string(), pos(item))),
                        _ => self.err(pos(item), "each event in `on` must be a name"),
                    }
                }
            }
            Node::Mapping(m) => {
                for (k, v) in m.iter() {
                    events.push((k, Some(v)));
                }
            }
        }
        // Names given without options behave like an event with an empty mapping.
        for (name, at) in &name_nodes {
            self.apply_event(&mut t, name, *at, None);
        }
        for (k, v) in events {
            self.apply_event(&mut t, k.as_str(), kpos(k), v);
        }
        t
    }

    fn apply_event(&mut self, t: &mut Triggers, name: &str, at: (u32, u32), v: Option<&Node>) {
        let body = v.filter(|n| !is_null(n));
        match name {
            "push" => t.push = Some(self.push_trigger(body)),
            "pull_request" => t.pull_request = Some(self.pr_trigger(body)),
            "workflow_dispatch" => t.workflow_dispatch = Some(self.dispatch(body)),
            "schedule" => match body {
                Some(Node::Sequence(seq)) => {
                    for item in seq.iter() {
                        for (k, cv) in self.entries(item, "schedule entry") {
                            if k.as_str() != "cron" {
                                self.unknown_key(k, "a `schedule` entry", &["cron"]);
                                continue;
                            }
                            if let Some(c) = self.text(cv, "cron") {
                                match crate::schedule::validate(&c) {
                                    Ok(()) => t.schedule.push(ScheduleTrigger { cron: c.trim().to_string(), line: pos(cv).0 }),
                                    Err(e) => self.err(pos(cv), format!("`cron: {c}` is not valid: {e}")),
                                }
                            }
                        }
                    }
                }
                _ => self.err(at, "`schedule` must be a list of `- cron: '...'` entries"),
            },
            "release" => {
                let mut r = ReleaseTrigger::default();
                if let Some(b) = body {
                    for (k, kv) in self.entries(b, "release") {
                        if k.as_str() == "types" {
                            r.types = self.string_list(kv, "release.types");
                            for ty in &r.types {
                                if !["published", "created", "released", "prereleased", "unpublished", "edited", "deleted"]
                                    .contains(&ty.as_str())
                                {
                                    self.err(pos(kv), format!("`{ty}` is not a release type"));
                                }
                            }
                        } else {
                            self.unknown_key(k, "`release`", &["types"]);
                        }
                    }
                }
                t.release = Some(r);
            }
            "workflow_call" => self.err(at, "reusable workflows (`workflow_call`) are not supported in this version"),
            "pull_request_target" => self.err(
                at,
                "`pull_request_target` is not supported: it would run code from a pull request with the base repository's secrets",
            ),
            other => self.err(at, format!("event `{other}` is not supported. Supported events: {}", SUPPORTED_EVENTS.join(", "))),
        }
    }

    fn patterns(&mut self, n: &Node, what: &str) -> Vec<String> {
        let list = self.string_list(n, what);
        for p in &list {
            if let Err(e) = crate::glob::check(p) {
                self.err(pos(n), format!("`{what}` pattern `{p}`: {e}"));
            }
        }
        list
    }

    fn push_trigger(&mut self, body: Option<&Node>) -> PushTrigger {
        let mut t = PushTrigger::default();
        let Some(b) = body else { return t };
        for (k, v) in self.entries(b, "push") {
            match k.as_str() {
                "branches" => t.branches = Some(self.patterns(v, "push.branches")),
                "branches-ignore" => t.branches_ignore = Some(self.patterns(v, "push.branches-ignore")),
                "tags" => t.tags = Some(self.patterns(v, "push.tags")),
                "tags-ignore" => t.tags_ignore = Some(self.patterns(v, "push.tags-ignore")),
                "paths" => t.paths = Some(self.patterns(v, "push.paths")),
                "paths-ignore" => t.paths_ignore = Some(self.patterns(v, "push.paths-ignore")),
                _ => self.unknown_key(k, "`push`", &["branches", "branches-ignore", "tags", "tags-ignore", "paths", "paths-ignore"]),
            }
        }
        let at = pos(b);
        for (a, c) in [("branches", "branches-ignore"), ("tags", "tags-ignore"), ("paths", "paths-ignore")] {
            let has = |n: &str| match n {
                "branches" => t.branches.is_some(),
                "branches-ignore" => t.branches_ignore.is_some(),
                "tags" => t.tags.is_some(),
                "tags-ignore" => t.tags_ignore.is_some(),
                "paths" => t.paths.is_some(),
                _ => t.paths_ignore.is_some(),
            };
            if has(a) && has(c) {
                self.err(at, format!("`{a}` and `{c}` cannot be used together in `push`. Use `!` patterns in `{a}` instead"));
            }
        }
        t
    }

    fn pr_trigger(&mut self, body: Option<&Node>) -> PullRequestTrigger {
        let mut t = PullRequestTrigger::default();
        let Some(b) = body else { return t };
        for (k, v) in self.entries(b, "pull_request") {
            match k.as_str() {
                "types" => {
                    t.types = self.string_list(v, "pull_request.types");
                    for ty in &t.types {
                        if !crate::trigger::PR_TYPES.contains(&ty.as_str()) {
                            self.err(pos(v), format!("`{ty}` is not a pull_request type. Types: {}", crate::trigger::PR_TYPES.join(", ")));
                        }
                    }
                }
                "branches" => t.branches = Some(self.patterns(v, "pull_request.branches")),
                "branches-ignore" => t.branches_ignore = Some(self.patterns(v, "pull_request.branches-ignore")),
                "paths" => t.paths = Some(self.patterns(v, "pull_request.paths")),
                "paths-ignore" => t.paths_ignore = Some(self.patterns(v, "pull_request.paths-ignore")),
                _ => self.unknown_key(k, "`pull_request`", &["types", "branches", "branches-ignore", "paths", "paths-ignore"]),
            }
        }
        let at = pos(b);
        if t.branches.is_some() && t.branches_ignore.is_some() {
            self.err(at, "`branches` and `branches-ignore` cannot be used together in `pull_request`");
        }
        if t.paths.is_some() && t.paths_ignore.is_some() {
            self.err(at, "`paths` and `paths-ignore` cannot be used together in `pull_request`");
        }
        t
    }

    fn dispatch(&mut self, body: Option<&Node>) -> Dispatch {
        let mut d = Dispatch::default();
        let Some(b) = body else { return d };
        for (k, v) in self.entries(b, "workflow_dispatch") {
            if k.as_str() != "inputs" {
                self.unknown_key(k, "`workflow_dispatch`", &["inputs"]);
                continue;
            }
            for (name, spec) in self.entries(v, "workflow_dispatch.inputs") {
                let at = kpos(name);
                let mut input = DispatchInput {
                    name: name.as_str().to_string(),
                    description: String::new(),
                    required: false,
                    kind: InputType::String,
                    default: None,
                    options: Vec::new(),
                };
                let mut type_seen = false;
                for (ik, iv) in self.entries(spec, "an input") {
                    match ik.as_str() {
                        "description" => input.description = self.text(iv, "description").unwrap_or_default(),
                        "required" => input.required = matches!(iv, Node::Scalar(s) if s.as_bool() == Some(true)),
                        "default" => input.default = Some(node_to_json(iv)),
                        "type" => {
                            type_seen = true;
                            input.kind = match self.text(iv, "type").unwrap_or_default().as_str() {
                                "string" => InputType::String,
                                "boolean" => InputType::Boolean,
                                "choice" => InputType::Choice,
                                "number" => InputType::Number,
                                other => {
                                    self.err(
                                        pos(iv),
                                        format!("input type `{other}` is not supported. Use string, boolean, choice or number"),
                                    );
                                    InputType::String
                                }
                            }
                        }
                        "options" => input.options = self.string_list(iv, "options"),
                        _ => self.unknown_key(ik, "an input", &["description", "required", "default", "type", "options"]),
                    }
                }
                let _ = type_seen;
                if input.kind == InputType::Choice && input.options.is_empty() {
                    self.err(at, format!("input `{}` is a choice and needs `options`", input.name));
                }
                if input.kind == InputType::Choice
                    && let Some(Json::String(def)) = &input.default
                    && !input.options.contains(def)
                {
                    self.err(at, format!("the default `{def}` of input `{}` is not one of its options", input.name));
                }
                d.inputs.push(input);
            }
        }
        d
    }

    // ------------------------------------------------------------------ jobs

    fn jobs(&mut self, n: &Node, wf: &mut Workflow) {
        let Node::Mapping(m) = n else {
            self.err(pos(n), "`jobs` must be a mapping of job names to jobs");
            return;
        };
        if m.is_empty() {
            self.err(pos(n), "`jobs` has no jobs");
        }
        for (k, v) in m.iter() {
            let key = k.as_str();
            if !valid_key(key) {
                self.err(
                    kpos(k),
                    format!("`{key}` is not a valid job name. Use letters, digits, `-` and `_`, starting with a letter or `_`"),
                );
            }
            if let Some(job) = self.job(key, kpos(k).0, v) {
                wf.jobs.push(job);
            }
        }
    }

    fn job(&mut self, key: &str, line: u32, n: &Node) -> Option<Job> {
        let Node::Mapping(m) = n else {
            self.err(pos(n), format!("job `{key}` must be a mapping"));
            return None;
        };
        let mut job = Job {
            key: key.to_string(),
            line,
            name: None,
            runs_on: Vec::new(),
            container: None,
            strategy: None,
            needs: Vec::new(),
            condition: None,
            timeout_minutes: None,
            continue_on_error: None,
            env: BTreeMap::new(),
            outputs: BTreeMap::new(),
            defaults: Defaults::default(),
            concurrency: None,
            steps: Vec::new(),
        };
        let mut have_runs_on = false;
        let mut have_steps = false;
        const KNOWN: &[&str] = &[
            "name",
            "runs-on",
            "needs",
            "if",
            "env",
            "defaults",
            "outputs",
            "steps",
            "strategy",
            "container",
            "timeout-minutes",
            "continue-on-error",
            "concurrency",
            "permissions",
        ];
        for (k, v) in m.iter() {
            match k.as_str() {
                "name" => job.name = self.template(v, "name"),
                "runs-on" => {
                    have_runs_on = true;
                    job.runs_on = self.runs_on(v);
                }
                "needs" => job.needs = self.string_list(v, "needs"),
                "if" => job.condition = self.condition(v),
                "env" => job.env = self.string_map(v, "env"),
                "defaults" => job.defaults = self.defaults(v),
                "outputs" => job.outputs = self.string_map(v, "outputs"),
                "steps" => {
                    have_steps = true;
                    job.steps = self.steps(v);
                }
                "strategy" => job.strategy = self.strategy(v, key),
                "container" => job.container = self.container(v),
                "timeout-minutes" => job.timeout_minutes = self.uint_or_expr(v, "timeout-minutes"),
                "continue-on-error" => job.continue_on_error = self.bool_or_expr(v, "continue-on-error"),
                "concurrency" => job.concurrency = self.concurrency(v),
                "permissions" => {}
                "services" => self.err(kpos(k), "`services:` is not supported in this version. Start the service in a `run:` step with `docker run -d`, or use a container job"),
                "environment" => self.err(kpos(k), "`environment:` (deployment environments and approvals) is not supported in this version"),
                "uses" | "with" | "secrets" => self.err(kpos(k), "reusable workflows (`uses:` on a job) are not supported in this version"),
                _ => self.unknown_key(k, &format!("job `{key}`"), KNOWN),
            }
        }
        if !have_runs_on {
            self.err((line, 1), format!("job `{key}`: `runs-on` is required"));
        } else if job.runs_on.is_empty() {
            self.err((line, 1), format!("job `{key}`: `runs-on` names no label"));
        }
        if !have_steps {
            self.err((line, 1), format!("job `{key}`: `steps` is required"));
        } else if job.steps.is_empty() {
            self.err((line, 1), format!("job `{key}`: `steps` has no steps"));
        }
        let mut seen = BTreeMap::new();
        for s in &job.steps {
            if let Some(id) = &s.id
                && let Some(first) = seen.insert(id.clone(), s.line)
            {
                self.err((s.line, 1), format!("step id `{id}` is used twice in job `{key}` (first on line {first})"));
            }
        }
        Some(job)
    }

    fn runs_on(&mut self, n: &Node) -> Vec<String> {
        match n {
            Node::Mapping(m) => {
                let mut out = Vec::new();
                for (k, v) in m.iter() {
                    match k.as_str() {
                        "labels" => out = self.string_list(v, "runs-on.labels"),
                        "group" => self.err(kpos(k), "runner groups are not supported. List labels instead"),
                        _ => self.unknown_key(k, "`runs-on`", &["labels"]),
                    }
                }
                self.check_labels(n, &out);
                out
            }
            _ => {
                let l = self.string_list(n, "runs-on");
                self.check_labels(n, &l);
                l
            }
        }
    }

    fn check_labels(&mut self, n: &Node, labels: &[String]) {
        for l in labels {
            if let Err(e) = expr::check_template(l) {
                self.err(pos(n), format!("in `runs-on`: {}", e.message));
            } else if l.trim().is_empty() {
                self.err(pos(n), "`runs-on` has an empty label");
            }
        }
    }

    fn container(&mut self, n: &Node) -> Option<Container> {
        let line = pos(n).0;
        match n {
            Node::Scalar(_) => {
                let image = self.template(n, "container")?;
                Some(Container { image, env: BTreeMap::new(), options: None, line })
            }
            _ => {
                let mut c = Container { image: String::new(), env: BTreeMap::new(), options: None, line };
                for (k, v) in self.entries(n, "container") {
                    match k.as_str() {
                        "image" => c.image = self.template(v, "container.image").unwrap_or_default(),
                        "env" => c.env = self.string_map(v, "container.env"),
                        "options" => {
                            c.options = self.template(v, "container.options");
                            if let Some(o) = &c.options
                                && !expr::has_expression(o)
                                && let Err(e) = crate::options::validate(o, &crate::options::Limits::default())
                            {
                                self.err(pos(v), format!("`container.options`: {e}"));
                            }
                        }
                        "ports" | "volumes" | "credentials" => self.err(
                            kpos(k),
                            format!(
                                "`container.{}` is not allowed: jobs cannot publish ports, mount volumes or bring registry credentials",
                                k.as_str()
                            ),
                        ),
                        _ => self.unknown_key(k, "`container`", &["image", "env", "options"]),
                    }
                }
                if c.image.trim().is_empty() {
                    self.err((line, 1), "`container.image` is required");
                }
                Some(c)
            }
        }
    }

    fn strategy(&mut self, n: &Node, job_key: &str) -> Option<Strategy> {
        let mut s = Strategy { matrix: None, fail_fast: true, max_parallel: None };
        for (k, v) in self.entries(n, "strategy") {
            match k.as_str() {
                "matrix" => s.matrix = self.matrix(v, job_key),
                "fail-fast" => {
                    if let Some(t) = self.bool_or_expr(v, "fail-fast") {
                        // An expression is treated as the default; fail-fast only ever stops work early.
                        s.fail_fast = t != "false";
                    }
                }
                "max-parallel" => {
                    if let Some(t) = self.uint_or_expr(v, "max-parallel") {
                        s.max_parallel = t.parse().ok();
                    }
                }
                _ => self.unknown_key(k, "`strategy`", &["matrix", "fail-fast", "max-parallel"]),
            }
        }
        Some(s)
    }

    fn matrix(&mut self, n: &Node, job_key: &str) -> Option<Matrix> {
        let mut m = Matrix::default();
        if let Node::Scalar(_) = n {
            let t = self.template(n, "matrix")?;
            if !expr::has_expression(&t) {
                self.err(pos(n), "`matrix` must be a mapping, or an expression such as ${{ fromJSON(...) }}");
                return None;
            }
            m.expression = Some(t);
            return Some(m);
        }
        for (k, v) in self.entries(n, "matrix") {
            match k.as_str() {
                "include" | "exclude" => {
                    let target_is_include = k.as_str() == "include";
                    let mut list = Vec::new();
                    match v {
                        Node::Sequence(seq) => {
                            for item in seq.iter() {
                                match item {
                                    Node::Mapping(map) => {
                                        let mut e = BTreeMap::new();
                                        for (ek, ev) in map.iter() {
                                            e.insert(ek.as_str().to_string(), node_to_json(ev));
                                        }
                                        list.push(e);
                                    }
                                    _ => self.err(pos(item), format!("every `{}` entry must be a mapping", k.as_str())),
                                }
                            }
                        }
                        Node::Scalar(_) if expr::has_expression(self.text(v, "matrix").unwrap_or_default().as_str()) => {
                            self.err(pos(v), format!("`{}` from an expression is not supported", k.as_str()));
                        }
                        _ => self.err(pos(v), format!("`{}` must be a list of mappings", k.as_str())),
                    }
                    if target_is_include {
                        m.include = list;
                    } else {
                        m.exclude = list;
                    }
                }
                axis => match v {
                    Node::Sequence(seq) => {
                        if seq.is_empty() {
                            self.err(pos(v), format!("matrix axis `{axis}` has no values"));
                        }
                        m.axes.push((axis.to_string(), seq.iter().map(node_to_json).collect()));
                    }
                    Node::Scalar(s) if expr::has_expression(s.as_str()) => {
                        // An axis computed from an expression is resolved when the run is planned.
                        m.axes.push((axis.to_string(), vec![Json::String(s.as_str().to_string())]));
                        self.err(pos(v), format!("matrix axis `{axis}` from an expression is not supported. Use `matrix: ${{{{ fromJSON(...) }}}}` for the whole matrix"));
                    }
                    _ => self.err(pos(v), format!("matrix axis `{axis}` must be a list of values")),
                },
            }
        }
        if m.axes.is_empty() && m.include.is_empty() {
            self.err(pos(n), format!("job `{job_key}`: the matrix has no axes"));
        }
        let cells: usize = m.axes.iter().map(|(_, v)| v.len().max(1)).try_fold(1usize, |a, b| a.checked_mul(b)).unwrap_or(usize::MAX);
        let cells = if m.axes.is_empty() { 0 } else { cells };
        if cells.saturating_add(m.include.len()) > MAX_MATRIX_JOBS {
            self.err(pos(n), format!("job `{job_key}`: the matrix makes more than {MAX_MATRIX_JOBS} jobs, which is the limit"));
        }
        Some(m)
    }

    fn steps(&mut self, n: &Node) -> Vec<Step> {
        let Node::Sequence(seq) = n else {
            self.err(pos(n), "`steps` must be a list");
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, item) in seq.iter().enumerate() {
            if let Some(s) = self.step(item, (i + 1) as u32) {
                out.push(s);
            }
        }
        out
    }

    fn step(&mut self, n: &Node, number: u32) -> Option<Step> {
        let Node::Mapping(m) = n else {
            self.err(pos(n), "a step must be a mapping");
            return None;
        };
        let line = pos(n).0;
        let (mut id, mut name, mut cond) = (None, None, None);
        let (mut run, mut uses, mut shell, mut wd) = (None::<String>, None::<(String, (u32, u32))>, None, None);
        let mut with: BTreeMap<String, String> = BTreeMap::new();
        let mut env = BTreeMap::new();
        let (mut coe, mut timeout) = (None, None);
        let mut run_at = (line, 1);
        const KNOWN: &[&str] =
            &["id", "name", "if", "uses", "with", "run", "shell", "working-directory", "env", "continue-on-error", "timeout-minutes"];
        for (k, v) in m.iter() {
            match k.as_str() {
                "id" => {
                    id = self.text(v, "id");
                    if let Some(i) = &id
                        && !valid_key(i)
                    {
                        self.err(pos(v), format!("`{i}` is not a valid step id. Use letters, digits, `-` and `_`"));
                    }
                }
                "name" => name = self.template(v, "name"),
                "if" => cond = self.condition(v),
                "uses" => uses = self.text(v, "uses").map(|u| (u, pos(v))),
                "with" => with = self.string_map(v, "with"),
                "run" => {
                    run_at = pos(v);
                    run = self.template(v, "run");
                }
                "shell" => shell = self.template(v, "shell"),
                "working-directory" => wd = self.template(v, "working-directory"),
                "env" => env = self.string_map(v, "env"),
                "continue-on-error" => coe = self.bool_or_expr(v, "continue-on-error"),
                "timeout-minutes" => timeout = self.uint_or_expr(v, "timeout-minutes"),
                _ => self.unknown_key(k, "a step", KNOWN),
            }
        }
        let kind = match (run, uses) {
            (Some(_), Some((_, at))) => {
                self.err(at, "a step cannot have both `run` and `uses`");
                return None;
            }
            (Some(run), None) => {
                if run.trim().is_empty() {
                    self.err(run_at, "`run` is empty");
                }
                if !with.is_empty() {
                    self.err((line, 1), "`with` only applies to `uses` steps");
                }
                StepKind::Run { run, shell, working_directory: wd }
            }
            (None, Some((u, at))) => {
                match actions::classify(&u) {
                    Ok(b) => self.check_inputs(&b, &with, at),
                    Err(e) => self.err(at, e),
                }
                if shell.is_some() || wd.is_some() {
                    self.err((line, 1), "`shell` and `working-directory` only apply to `run` steps");
                }
                StepKind::Uses { uses: u, with }
            }
            (None, None) => {
                self.err((line, 1), "a step needs either `run` or `uses`");
                return None;
            }
        };
        Some(Step { number, line, id, name, condition: cond, kind, env, continue_on_error: coe, timeout_minutes: timeout })
    }

    fn check_inputs(&mut self, b: &Builtin, with: &BTreeMap<String, String>, at: (u32, u32)) {
        for req in b.required_inputs() {
            if !with.contains_key(*req) {
                self.err(at, format!("`{}` needs `with: {req}`", b.name()));
            }
        }
        if matches!(b, Builtin::Docker { .. }) {
            // docker:// steps pass `args` and `entrypoint` only.
            for k in with.keys() {
                if !b.known_inputs().contains(&k.as_str()) {
                    self.err(at, format!("`docker://` steps accept `args` and `entrypoint`, not `{k}`"));
                }
            }
        }
    }

    // ------------------------------------------------------------------ graph checks

    fn check_graph(&mut self, wf: &Workflow) {
        let keys: Vec<&str> = wf.jobs.iter().map(|j| j.key.as_str()).collect();
        for j in &wf.jobs {
            for need in &j.needs {
                if !keys.contains(&need.as_str()) {
                    self.err((j.line, 1), format!("job `{}` needs `{need}`, which is not a job in this file", j.key));
                } else if need == &j.key {
                    self.err((j.line, 1), format!("job `{}` needs itself", j.key));
                }
            }
        }
        if let Some(cycle) = crate::plan::find_cycle(wf) {
            let line = wf.job(&cycle[0]).map(|j| j.line).unwrap_or(0);
            self.err((line, 1), format!("jobs depend on each other in a loop: {}", cycle.join(" -> ")));
        }
    }
}

/// Typed JSON from a node. Plain scalars read as numbers, booleans and null the way YAML says;
/// quoted ones stay strings.
pub fn node_to_json(n: &Node) -> Json {
    match n {
        Node::Scalar(s) => {
            let t = s.as_str();
            if !s.may_coerce() {
                return Json::String(t.to_string());
            }
            match t {
                "" | "~" | "null" | "Null" | "NULL" => Json::Null,
                "true" | "True" | "TRUE" => Json::Bool(true),
                "false" | "False" | "FALSE" => Json::Bool(false),
                _ => {
                    if let Ok(i) = t.parse::<i64>() {
                        // Leading zeros are text ("007"), not a number.
                        if t.len() > 1 && t.trim_start_matches(['-', '+']).starts_with('0') {
                            Json::String(t.to_string())
                        } else {
                            Json::from(i)
                        }
                    } else if let Ok(f) = t.parse::<f64>()
                        && f.is_finite()
                        && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
                        && let Some(num) = serde_json::Number::from_f64(f)
                    {
                        Json::Number(num)
                    } else {
                        Json::String(t.to_string())
                    }
                }
            }
        }
        Node::Sequence(seq) => Json::Array(seq.iter().map(node_to_json).collect()),
        Node::Mapping(m) => Json::Object(m.iter().map(|(k, v)| (k.as_str().to_string(), node_to_json(v))).collect()),
    }
}

fn valid_key(k: &str) -> bool {
    let mut it = k.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && it.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A cheap "did you mean": equal ignoring `-`/`_`/case, or one edit away.
fn near_miss(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.to_ascii_lowercase().replace(['-', '_'], "");
    let (x, y) = (norm(a), norm(b));
    if x == y {
        return true;
    }
    if x.len().abs_diff(y.len()) > 1 || x.len() < 3 {
        return false;
    }
    // One substitution, insertion or deletion.
    let (xs, ys): (Vec<char>, Vec<char>) = (x.chars().collect(), y.chars().collect());
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < xs.len() && j < ys.len() {
        if xs[i] == ys[j] {
            i += 1;
            j += 1;
        } else {
            edits += 1;
            if edits > 1 {
                return false;
            }
            match xs.len().cmp(&ys.len()) {
                std::cmp::Ordering::Greater => i += 1,
                std::cmp::Ordering::Less => j += 1,
                std::cmp::Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    edits + (xs.len() - i) + (ys.len() - j) <= 1
}
