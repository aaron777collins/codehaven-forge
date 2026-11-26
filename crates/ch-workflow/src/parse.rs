//! Reads a workflow file into the typed model, collecting every problem with its line.
//!
//! The parser keeps going after an error so a person sees all of them at once. It is strict
//! about keys it does not know (a typo should not silently turn a job off) and specific about
//! the Actions features Codehaven does not run.

use crate::actions::{self, Builtin};
use crate::diag::Diagnostic;
use crate::expr;
use crate::model::*;
use crate::yaml_guard;
use marked_yaml::types::{MarkedMappingNode, MarkedScalarNode};
use marked_yaml::{LoadError, LoaderOptions, Node};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// The most jobs one run may expand to, matrix cells included.
pub const MAX_MATRIX_JOBS: usize = 256;
/// A workflow or action.yml larger than this is refused before parsing.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// How deep blocks and flow collections may nest. The YAML parser recurses with the nesting, and
/// a stack overflow takes the whole process down, so hostile files are turned away before it sees
/// them. Real workflows rarely go past 8.
pub const MAX_NESTING: usize = 50;

pub const SUPPORTED_EVENTS: &[&str] = &["push", "pull_request", "workflow_dispatch", "schedule", "release", "workflow_call"];

/// Parses and validates. Every problem found is in the error, in file order.
pub fn parse(src: &str) -> Result<Workflow, Vec<Diagnostic>> {
    let text = guard_input(src)?;
    on_parser_stack(|| {
        let root = load(&text)?;
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
    })
}

/// Reads an action's `action.yml` (ch-ctk.4). Keys the runner does not use (`description`,
/// `author`, `branding`) are ignored, as GitHub ignores them.
pub fn parse_action(src: &str) -> Result<crate::action_def::ActionDef, Vec<Diagnostic>> {
    let text = guard_input(src)?;
    on_parser_stack(|| {
        let root = load(&text)?;
        let mut p = P { diags: Vec::new() };
        let def = p.action(&root);
        p.diags.sort_by_key(|d| (d.line, d.column));
        p.diags.dedup();
        match def {
            Some(d) if p.diags.is_empty() => Ok(d),
            _ => {
                if p.diags.is_empty() {
                    p.diags.push(Diagnostic::new(0, 0, "the file is not a valid action.yml"));
                }
                Err(p.diags)
            }
        }
    })
}

/// Everything a file must pass before the YAML parser sees it, in one place for the workflow and
/// the action.yml road. The size cap, then one normalisation of every line break (see
/// `yaml_guard`), then the scan. The text it returns is the text the scan read and is the only
/// text the parser is given, so the two always see the same lines.
fn guard_input(src: &str) -> Result<String, Vec<Diagnostic>> {
    if src.len() > MAX_FILE_BYTES {
        return Err(vec![Diagnostic::new(0, 0, format!("the file is larger than {} KiB", MAX_FILE_BYTES / 1024))]);
    }
    let text = yaml_guard::normalise(src).into_owned();
    if let Some((line, why)) = scan(&text) {
        return Err(vec![Diagnostic::new(line, 1, why.message())]);
    }
    Ok(text)
}

/// Runs a parse on the thread `yaml_guard` sets up, so that a file the scan let through that
/// recurses deeper than it should still cannot overflow the stack the server runs on.
fn on_parser_stack<T: Send>(f: impl FnOnce() -> Result<T, Vec<Diagnostic>> + Send) -> Result<T, Vec<Diagnostic>> {
    yaml_guard::on_parser_stack(MAX_FILE_BYTES, f).unwrap_or_else(|e| Err(vec![Diagnostic::new(0, 0, e)]))
}

/// The YAML parser, with the options every road here uses.
fn load(text: &str) -> Result<Node, Vec<Diagnostic>> {
    let opts = || LoaderOptions::default().error_on_duplicate_keys(true).prevent_coercion(true);
    match marked_yaml::parse_yaml_with_options(0, text, opts()) {
        Ok(n) => Ok(n),
        // The reader has no anchors, GitHub's syntax has: write the file out with each alias
        // replaced by its value (size-limited, see `anchors`), and read that.
        Err(LoadError::UnexpectedAnchor(m)) => {
            let expanded = crate::anchors::expand(text).map_err(|msg| vec![Diagnostic::new(m.line() as u32, m.column() as u32, msg)])?;
            marked_yaml::parse_yaml_with_options(0, &expanded, opts()).map_err(|e| vec![load_error(&e)])
        }
        Err(e) => Err(vec![load_error(&e)]),
    }
}

/// Why a file was turned away before the YAML parser saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Blocks, `- ` markers and brackets nest past `MAX_NESTING`.
    TooDeep,
    /// A node starts with the explicit-key marker `? ` or `: `.
    ExplicitKey,
}

impl Refusal {
    /// The sentence a person reads.
    pub fn message(self) -> String {
        match self {
            Refusal::TooDeep => format!("the file is nested more than {MAX_NESTING} levels deep"),
            Refusal::ExplicitKey => {
                "explicit YAML keys (`? ` and `: ` at the start of a node) are not supported. Write the key on one line as `key: value`"
                    .into()
            }
        }
    }
}

/// The line where the nesting first passes `MAX_NESTING`, or where a node starts with an
/// explicit-key marker. Same as [`scan`] without the reason.
pub fn nesting_too_deep(src: &str) -> Option<u32> {
    scan(src).map(|(line, _)| line)
}

fn blank_or_end(c: Option<char>) -> bool {
    c.is_none_or(char::is_whitespace)
}

/// The text after a `-` sequence marker (a dash then a space or tab), or `None`.
fn after_dash(rest: &str) -> Option<&str> {
    let tail = rest.strip_prefix('-')?;
    tail.starts_with([' ', '\t']).then(|| tail.trim_start_matches([' ', '\t']))
}

/// Does this line open a block scalar (`key: |`, `key: >-`, `- |`)? Only the plain, unquoted
/// shapes count, so a quote the scan could misread never hides lines from it. Returns whether
/// the header is a bare indicator (after a dash), which decides the indent its text must beat.
fn block_scalar_header(rest: &str, after_dash: bool) -> Option<bool> {
    let t = match rest.find(" #") {
        Some(i) => &rest[..i],
        None => rest,
    }
    .trim_end();
    let tok = t.rsplit([' ', '\t']).next()?;
    let mut chars = tok.chars();
    if !matches!(chars.next(), Some('|' | '>')) || tok.len() > 3 || !chars.all(|c| matches!(c, '-' | '+' | '0'..='9')) {
        return None;
    }
    let before = t[..t.len() - tok.len()].trim_end();
    if before.is_empty() {
        return after_dash.then_some(true);
    }
    let plain = !before.contains(['\'', '"', '[', ']', '{', '}', '#', '|', '>']);
    (before.ends_with(':') && plain).then_some(false)
}

/// A rough scan of a YAML file, run before the parser: the parser recurses with the nesting, a
/// stack overflow takes the whole process down, and it ignores nothing it is given. It counts
/// block indentation, `- ` markers and `[`/`{` flow brackets, and it refuses any node that starts
/// with the explicit-key marker `? ` or `: ` (which recurse too, and which no file here needs).
/// Block scalars (`run: |`) are text, so their lines are skipped. Deliberately rough: it only has
/// to be safe, and it errs towards refusing.
pub fn scan(src: &str) -> Option<(u32, Refusal)> {
    // The same line breaks the parser is given (see `yaml_guard::normalise`): `str::lines` splits
    // on LF and CRLF only, and a lone CR hid a whole dash chain from this scan (ch-qop).
    let src = yaml_guard::normalise(src);
    let mut indents: Vec<usize> = Vec::new();
    let mut flow: usize = 0;
    // Inside a block scalar: the indent its text must beat, and the indent its first line set.
    let mut scalar: Option<(usize, Option<usize>)> = None;
    for (n, raw) in src.lines().enumerate() {
        let line_no = (n + 1) as u32;
        let trimmed = raw.trim_start_matches(' ');
        if trimmed.is_empty() {
            continue;
        }
        let mut col = raw.len() - trimmed.len();
        if let Some((parent, text)) = scalar {
            match text {
                None if col > parent => {
                    scalar = Some((parent, Some(col)));
                    continue;
                }
                Some(at) if col >= at => continue,
                _ => scalar = None,
            }
        }
        if trimmed.starts_with('#') {
            continue;
        }
        let mut rest = trimmed;
        let mut marker_col = col;
        let mut dashed = false;
        // Block structure: each indent level, and each `- ` marker, is one level.
        let block = flow == 0;
        if block {
            while indents.last().is_some_and(|&t| t >= col) {
                indents.pop();
            }
            indents.push(col);
            while let Some(after) = after_dash(rest) {
                marker_col = col;
                col += rest.len() - after.len();
                rest = after;
                dashed = true;
                indents.push(col);
                if indents.len() + flow > MAX_NESTING {
                    return Some((line_no, Refusal::TooDeep));
                }
            }
        }
        if indents.len() + flow > MAX_NESTING {
            return Some((line_no, Refusal::TooDeep));
        }
        // Flow structure and explicit keys: brackets and markers outside quotes.
        let (mut in_s, mut in_d) = (false, false);
        let mut prev = ' ';
        let mut start = true; // the next character begins a node
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            let quoted = in_s || in_d;
            if !quoted {
                if start && matches!(c, '?' | ':') && blank_or_end(chars.peek().copied()) {
                    return Some((line_no, Refusal::ExplicitKey));
                }
                if c == '#' && prev.is_whitespace() {
                    break;
                }
            }
            match c {
                // A quote opens a quoted scalar only where a node starts; inside plain text it is
                // just a character, and reading it as a quote would hide what follows.
                '\'' if !in_d && (in_s || start) => in_s = !in_s,
                '"' if !in_s && prev != '\\' && (in_d || start) => in_d = !in_d,
                '[' | '{' if !quoted => {
                    flow += 1;
                    if indents.len() + flow > MAX_NESTING {
                        return Some((line_no, Refusal::TooDeep));
                    }
                }
                ']' | '}' if !quoted => flow = flow.saturating_sub(1),
                _ => {}
            }
            start = if in_s || in_d || c == '\'' || c == '"' {
                false
            } else {
                match c {
                    '[' | '{' => true,
                    ',' => flow > 0 || start,
                    ':' if flow > 0 || blank_or_end(chars.peek().copied()) => true,
                    '-' if start && blank_or_end(chars.peek().copied()) => true,
                    c if c.is_whitespace() => start,
                    _ => false,
                }
            };
            prev = c;
        }
        if block
            && flow == 0
            && let Some(bare) = block_scalar_header(rest, dashed)
        {
            scalar = Some((if bare { marker_col } else { col }, None));
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

    /// `environment: name` or `environment: {name, url}` (ch-bj5). The name is plain text: it
    /// is looked up in the repository's environments, so it cannot be worked out at run time.
    fn environment(&mut self, n: &Node) -> Option<String> {
        let name = match n {
            Node::Scalar(_) => self.text(n, "environment")?,
            Node::Mapping(m) => {
                let mut name = None;
                for (k, v) in m.iter() {
                    match k.as_str() {
                        "name" => name = self.text(v, "environment.name"),
                        "url" => {
                            let _ = self.template(v, "environment.url");
                        }
                        _ => self.unknown_key(k, "`environment`", &["name", "url"]),
                    }
                }
                match name {
                    Some(name) => name,
                    None => {
                        self.err(pos(n), "`environment` needs a `name`");
                        return None;
                    }
                }
            }
            _ => {
                self.err(pos(n), "`environment` must be a name, or a mapping with `name` and `url`");
                return None;
            }
        };
        let name = name.trim().to_string();
        if name.contains("${{") {
            self.err(pos(n), "the environment name must be plain text in this version, not an expression");
            return None;
        }
        if !valid_environment_name(&name) {
            self.err(
                pos(n),
                "an environment name starts with a letter or digit and holds at most 100 letters, digits, spaces, dots, dashes and underscores",
            );
            return None;
        }
        Some(name)
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

    /// `permissions: read-all | write-all | {scope: read|write|none, ...}`.
    fn permissions(&mut self, n: &Node) -> Option<crate::permissions::Declared> {
        use crate::permissions::{Declared, check_entry};
        match n {
            Node::Scalar(s) if !is_null(n) => {
                let d = Declared::from_word(s.as_str().trim());
                if d.is_none() {
                    self.err(pos(n), "`permissions` must be read-all, write-all or a mapping of scopes");
                }
                d
            }
            Node::Sequence(_) => {
                self.err(pos(n), "`permissions` must be read-all, write-all or a mapping of scopes");
                None
            }
            _ => {
                let mut out = BTreeMap::new();
                for (k, v) in self.entries(n, "permissions") {
                    let Node::Scalar(val) = v else {
                        self.err(pos(v), format!("permission `{}` must be read, write or none", k.as_str()));
                        continue;
                    };
                    match check_entry(k.as_str(), val.as_str().trim()) {
                        Ok(level) => {
                            out.insert(k.as_str().to_string(), level);
                        }
                        Err(msg) => self.err(kpos(k), msg),
                    }
                }
                Some(Declared::Scopes(out))
            }
        }
    }

    // ------------------------------------------------------------------ action.yml

    fn action(&mut self, root: &Node) -> Option<crate::action_def::ActionDef> {
        use crate::action_def::{ActionDef, ActionInput, ActionRuns};
        let Node::Mapping(m) = root else {
            self.err(pos(root), "an action.yml must be a mapping with `runs`");
            return None;
        };
        let mut def = ActionDef {
            name: String::new(),
            inputs: Vec::new(),
            outputs: BTreeMap::new(),
            runs: ActionRuns::Docker { image: String::new() },
        };
        let mut runs = None;
        for (k, v) in m.iter() {
            match k.as_str() {
                "name" => def.name = self.text(v, "name").unwrap_or_default(),
                "inputs" => {
                    for (id, body) in self.entries(v, "inputs") {
                        let mut input = ActionInput { id: id.as_str().to_string(), default: None, required: false };
                        for (ik, iv) in self.entries(body, &format!("inputs.{}", id.as_str())) {
                            match ik.as_str() {
                                "default" => input.default = self.text(iv, "default"),
                                "required" => {
                                    input.required =
                                        matches!(self.text(iv, "required").as_deref().map(str::trim), Some("true" | "True" | "TRUE"))
                                }
                                _ => {}
                            }
                        }
                        def.inputs.push(input);
                    }
                }
                "outputs" => {
                    for (id, body) in self.entries(v, "outputs") {
                        let mut value = None;
                        for (ok, ov) in self.entries(body, &format!("outputs.{}", id.as_str())) {
                            if ok.as_str() == "value" {
                                value = self.template(ov, "value");
                            }
                        }
                        def.outputs.insert(id.as_str().to_string(), value);
                    }
                }
                "runs" => runs = Some(v),
                _ => {}
            }
        }
        let Some(runs) = runs else {
            self.err(pos(root), "an action.yml needs `runs`");
            return None;
        };
        let mut using = String::new();
        let (mut main, mut pre, mut pre_if, mut post, mut post_if, mut image) = (None, None, None, None, None, None);
        let mut steps = None;
        for (k, v) in self.entries(runs, "runs") {
            match k.as_str() {
                "using" => using = self.text(v, "using").unwrap_or_default().trim().to_ascii_lowercase(),
                "main" => main = self.text(v, "main"),
                "pre" => pre = self.text(v, "pre"),
                "pre-if" => pre_if = self.text(v, "pre-if"),
                "post" => post = self.text(v, "post"),
                "post-if" => post_if = self.text(v, "post-if"),
                "image" => image = self.text(v, "image"),
                "steps" => steps = Some(self.steps(v)),
                _ => {}
            }
        }
        def.runs = match using.as_str() {
            "node12" | "node16" | "node20" | "node24" => match main {
                Some(main) if !main.trim().is_empty() => ActionRuns::Node { using, main, pre, pre_if, post, post_if },
                _ => {
                    self.err(pos(runs), "a JavaScript action needs `runs.main`");
                    return None;
                }
            },
            "composite" => ActionRuns::Composite { steps: steps.unwrap_or_default() },
            "docker" => ActionRuns::Docker { image: image.unwrap_or_default() },
            other => {
                self.err(pos(runs), format!("`runs.using: {other}` is not something the runner can run. Use node20, node24 or composite"));
                return None;
            }
        };
        Some(def)
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
            permissions: None,
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
                "permissions" => wf.permissions = self.permissions(v),
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
            "workflow_call" => t.workflow_call = Some(self.workflow_call(body)),
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

    /// `on: workflow_call` with its `inputs`, `secrets` and `outputs`.
    fn workflow_call(&mut self, body: Option<&Node>) -> WorkflowCall {
        let mut wc = WorkflowCall::default();
        let Some(b) = body else { return wc };
        for (k, v) in self.entries(b, "workflow_call") {
            match k.as_str() {
                "inputs" => {
                    for (name, spec) in self.entries(v, "workflow_call.inputs") {
                        let line = kpos(name).0;
                        let mut input = CallInput {
                            name: name.as_str().to_string(),
                            description: String::new(),
                            required: false,
                            kind: CallInputType::String,
                            default: None,
                            line,
                        };
                        let mut typed = false;
                        for (ik, iv) in self.entries(spec, "an input") {
                            match ik.as_str() {
                                "description" => input.description = self.text(iv, "description").unwrap_or_default(),
                                "required" => input.required = matches!(iv, Node::Scalar(s) if s.as_bool() == Some(true)),
                                "default" => input.default = Some(node_to_json(iv)),
                                "type" => {
                                    typed = true;
                                    input.kind = match self.text(iv, "type").unwrap_or_default().as_str() {
                                        "string" => CallInputType::String,
                                        "boolean" => CallInputType::Boolean,
                                        "number" => CallInputType::Number,
                                        other => {
                                            self.err(
                                                pos(iv),
                                                format!("input type `{other}` is not supported. Use string, boolean or number"),
                                            );
                                            CallInputType::String
                                        }
                                    }
                                }
                                _ => self.unknown_key(ik, "an input", &["description", "required", "default", "type"]),
                            }
                        }
                        if !typed {
                            self.err((line, 1), format!("input `{}` needs a `type` (string, boolean or number)", input.name));
                        }
                        wc.inputs.push(input);
                    }
                }
                "secrets" => {
                    for (name, spec) in self.entries(v, "workflow_call.secrets") {
                        let mut secret = CallSecret { name: name.as_str().to_string(), required: false, line: kpos(name).0 };
                        if !is_null(spec) {
                            for (sk, sv) in self.entries(spec, "a secret") {
                                match sk.as_str() {
                                    "required" => secret.required = matches!(sv, Node::Scalar(s) if s.as_bool() == Some(true)),
                                    "description" => {
                                        let _ = self.text(sv, "description");
                                    }
                                    _ => self.unknown_key(sk, "a secret", &["description", "required"]),
                                }
                            }
                        }
                        wc.secrets.push(secret);
                    }
                }
                "outputs" => {
                    for (name, spec) in self.entries(v, "workflow_call.outputs") {
                        let mut out = CallOutput {
                            name: name.as_str().to_string(),
                            description: String::new(),
                            value: String::new(),
                            line: kpos(name).0,
                        };
                        for (ok, ov) in self.entries(spec, "an output") {
                            match ok.as_str() {
                                "description" => out.description = self.text(ov, "description").unwrap_or_default(),
                                "value" => {
                                    out.value = self.text(ov, "value").unwrap_or_default();
                                    if let Err(e) = expr::check_template_with(&out.value, &["jobs"]) {
                                        self.err(pos(ov), format!("in `value`: {}", e.message));
                                    }
                                }
                                _ => self.unknown_key(ok, "an output", &["description", "value"]),
                            }
                        }
                        if out.value.trim().is_empty() {
                            self.err((out.line, 1), format!("output `{}` needs a `value`", out.name));
                        }
                        wc.outputs.push(out);
                    }
                }
                _ => self.unknown_key(k, "`workflow_call`", &["inputs", "secrets", "outputs"]),
            }
        }
        for (kind, names) in [
            ("input", wc.inputs.iter().map(|i| i.name.clone()).collect::<Vec<_>>()),
            ("secret", wc.secrets.iter().map(|i| i.name.clone()).collect()),
            ("output", wc.outputs.iter().map(|i| i.name.clone()).collect()),
        ] {
            for n in &names {
                if n.is_empty() || n.len() > 64 || !n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
                    self.err((0, 0), format!("{kind} name `{n}` may use letters, digits, `_` and `-`"));
                }
            }
        }
        wc
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
            permissions: None,
            services: BTreeMap::new(),
            environment: None,
            call: None,
            call_role: None,
            called: false,
            steps: Vec::new(),
        };
        let mut call_uses: Option<(String, (u32, u32))> = None;
        let mut call_with = BTreeMap::new();
        let mut call_secrets = CallSecrets::Map(BTreeMap::new());
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
            "uses",
            "with",
            "secrets",
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
                "permissions" => job.permissions = self.permissions(v),
                "services" => job.services = self.services(v),
                "environment" => job.environment = self.environment(v),
                "uses" => call_uses = self.text(v, "uses").map(|u| (u, pos(v))),
                "with" => call_with = self.string_map(v, "with"),
                "secrets" => match v {
                    Node::Scalar(s) if s.as_str().trim() == "inherit" => call_secrets = CallSecrets::Inherit,
                    Node::Mapping(_) => call_secrets = CallSecrets::Map(self.string_map(v, "secrets")),
                    _ => self.err(pos(v), "`secrets` must be `inherit` or a mapping of names to values"),
                },
                _ => self.unknown_key(k, &format!("job `{key}`"), KNOWN),
            }
        }
        if let Some((uses, at)) = call_uses {
            // A job that calls a reusable workflow has the called workflow's jobs in its place, so
            // nothing that belongs to a job of its own may sit beside `uses`.
            for (k, _) in m.iter() {
                if !matches!(k.as_str(), "name" | "needs" | "if" | "uses" | "with" | "secrets" | "permissions") {
                    self.err(
                        kpos(k),
                        format!("`{}` cannot be used on job `{key}`, which calls a reusable workflow. Only name, needs, if, uses, with, secrets and permissions can", k.as_str()),
                    );
                }
            }
            if let Err(e) = crate::reusable::parse_call_uses(&uses) {
                self.err(at, format!("job `{key}`: {e}"));
            }
            job.call = Some(Call { uses, with: call_with, secrets: call_secrets, line });
            return Some(job);
        }
        if !call_with.is_empty() || matches!(call_secrets, CallSecrets::Inherit) {
            self.err((line, 1), format!("job `{key}`: `with` and `secrets` belong to a job that calls a reusable workflow with `uses`"));
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

    fn services(&mut self, n: &Node) -> BTreeMap<String, crate::model::Service> {
        let mut out = BTreeMap::new();
        for (k, v) in self.entries(n, "services") {
            let id = k.as_str().to_string();
            if !valid_key(&id) || id.len() > 63 {
                self.err(kpos(k), format!("`{id}` is not a valid service name. Use letters, digits, `-` and `_`"));
                continue;
            }
            let line = pos(v).0;
            let mut svc = crate::model::Service { image: String::new(), env: BTreeMap::new(), ports: Vec::new(), options: None, line };
            for (sk, sv) in self.entries(v, &format!("services.{id}")) {
                match sk.as_str() {
                    "image" => svc.image = self.template(sv, "image").unwrap_or_default(),
                    "env" => svc.env = self.string_map(sv, "env"),
                    "ports" => {
                        for p in self.string_list(sv, "ports") {
                            // `5432`, `5432/tcp`, `5432:5432` or `8080:80`: the container's side.
                            let container = p.rsplit(':').next().unwrap_or("").split('/').next().unwrap_or("").trim().to_string();
                            match container.parse::<u16>() {
                                Ok(port) if port > 0 => svc.ports.push(port),
                                _ => self.err(pos(sv), format!("`services.{id}.ports`: `{p}` is not a port")),
                            }
                        }
                    }
                    "options" => {
                        svc.options = self.template(sv, "options");
                        if let Some(o) = &svc.options
                            && !expr::has_expression(o)
                            && let Err(e) = crate::options::validate_service(o)
                        {
                            self.err(pos(sv), format!("`services.{id}.options`: {e}"));
                        }
                    }
                    "volumes" | "credentials" => self.err(
                        kpos(sk),
                        format!(
                            "`services.{id}.{}` is not allowed: services cannot mount volumes or bring registry credentials",
                            sk.as_str()
                        ),
                    ),
                    _ => self.unknown_key(sk, &format!("`services.{id}`"), &["image", "env", "ports", "options"]),
                }
            }
            if svc.image.trim().is_empty() {
                self.err((line, 1), format!("`services.{id}.image` is required"));
            }
            out.insert(id, svc);
        }
        if out.len() > 10 {
            self.err(pos(n), "a job may have at most 10 services");
        }
        out
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
                    // Not built in: an action from this repository (`./path`), from GitHub
                    // (`owner/repo@ref`) or from a repository here (`haven_owner/repo@ref`), which
                    // the site's policy decides when the job runs (ch-ctk.5).
                    Err(e) => match crate::uses::parse_uses(&u) {
                        Ok(_) => {}
                        Err(pe) if u.contains('@') || u.trim_start().starts_with("./") => self.err(at, pe.0),
                        Err(_) => self.err(at, e),
                    },
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

/// The names an environment may have: what the `environments.name` column accepts.
pub fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().count() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ' ' | '-'))
}
