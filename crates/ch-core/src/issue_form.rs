//! YAML issue forms (ch-dxk): `.github/ISSUE_TEMPLATE/*.yml` or `.codehaven/ISSUE_TEMPLATE/*.yml`.
//!
//! A form is a list of fields. The web layer draws them as an ordinary HTML form on the server, so
//! no script runs in the page and the content security policy stays as strict as it is. What
//! comes back is never trusted as a definition: the page sends field values only, the server reads
//! the form again from the repository, checks each value against its field and builds the issue
//! body from them in the order the form gives.
//!
//! Supported: `name`, `description`, `title`, `labels` (a list or comma separated text),
//! `assignees` (read and dropped, as a form cannot assign people it cannot see), and a `body` of
//! `markdown`, `input`, `textarea`, `dropdown` and `checkboxes` fields. A form that cannot be
//! read is left out of the list and says why in [`parse`].

use crate::yaml_guard;
use marked_yaml::{LoaderOptions, Node};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const MAX_FORM_BYTES: usize = 64 * 1024;
pub const MAX_FIELDS: usize = 40;
pub const MAX_OPTIONS: usize = 50;
/// The most one field's value may hold, in bytes.
pub const MAX_VALUE: usize = 64 * 1024;
const MAX_TEXT: usize = 4000;
/// How deep blocks and flow collections may nest. The YAML parser recurses with the nesting, and
/// a stack overflow takes the whole process down, so a hostile file is turned away before the
/// parser sees it. A form needs about 6 levels.
pub const MAX_NESTING: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Markdown,
    Input,
    Textarea,
    Dropdown,
    Checkboxes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormOption {
    pub label: String,
    /// A checkbox that must be ticked to submit.
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    pub kind: FieldKind,
    /// The form field name is `f_<id>`. Markdown fields have none; they get `m<position>`.
    pub id: String,
    pub label: String,
    pub description: String,
    pub placeholder: String,
    /// The prefilled value of an input or textarea, or the text of a markdown block.
    pub value: String,
    pub options: Vec<FormOption>,
    pub multiple: bool,
    pub required: bool,
    /// A language: the answer goes into a fenced code block of it.
    pub render: String,
}

impl FormField {
    /// The name of the HTML control.
    pub fn control(&self) -> String {
        format!("f_{}", self.id)
    }
    pub fn is_input(&self) -> bool {
        self.kind == FieldKind::Input
    }
    pub fn is_textarea(&self) -> bool {
        self.kind == FieldKind::Textarea
    }
    pub fn is_dropdown(&self) -> bool {
        self.kind == FieldKind::Dropdown
    }
    pub fn is_checkboxes(&self) -> bool {
        self.kind == FieldKind::Checkboxes
    }
    pub fn is_markdown(&self) -> bool {
        self.kind == FieldKind::Markdown
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueForm {
    pub fields: Vec<FormField>,
}

/// What `parse` reads from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedForm {
    pub name: String,
    pub about: String,
    pub title: String,
    pub labels: Vec<String>,
    pub form: IssueForm,
}

fn scalar(n: &Node) -> Option<String> {
    match n {
        Node::Scalar(s) => Some(s.as_str().to_string()),
        _ => None,
    }
}

fn clip(s: String) -> String {
    s.chars().take(MAX_TEXT).collect()
}

fn truthy(n: &Node) -> bool {
    scalar(n).is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 50 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The same scan as the workflow parser's (`ch-core` may not depend on `ch-workflow`, so it is kept
/// here too; the tests below pin the same cases).
/// Why a file was turned away before the YAML parser saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Blocks, `- ` markers and brackets nest past `MAX_NESTING`.
    TooDeep,
    /// A node starts with the explicit-key marker `? ` or `: `.
    ExplicitKey,
}

impl Refusal {
    /// The sentence a person reads.
    fn message(self) -> String {
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
#[cfg(test)]
fn nesting_too_deep(src: &str) -> Option<u32> {
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
fn scan(src: &str) -> Option<(u32, Refusal)> {
    // The same line breaks the parser is given: `str::lines` splits on LF and CRLF only, and a
    // lone CR hid a whole dash chain from this scan (ch-qop).
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

/// Reads a YAML issue form. The error says what is wrong in a sentence.
pub fn parse(file_name: &str, text: &str) -> Result<ParsedForm, String> {
    if text.len() > MAX_FORM_BYTES {
        return Err("the file is larger than 64 KB".into());
    }
    // One normalisation of every line break, then the scan and the parser read the same text, and
    // the parser runs on a stack sized for the deepest file the cap allows (ch-qop).
    let text = yaml_guard::normalise(text);
    if let Some((_, why)) = scan(&text) {
        return Err(why.message());
    }
    yaml_guard::on_parser_stack(MAX_FORM_BYTES, || parse_text(file_name, &text))?
}

fn parse_text(file_name: &str, text: &str) -> Result<ParsedForm, String> {
    let opts = LoaderOptions::default().error_on_duplicate_keys(true).prevent_coercion(true);
    let root = marked_yaml::parse_yaml_with_options(0, text, opts).map_err(|e| format!("not valid YAML: {e}"))?;
    let Node::Mapping(top) = &root else { return Err("the file must be a mapping with a `body`".into()) };
    let mut name = file_name.trim_end_matches(".yml").trim_end_matches(".yaml").replace(['-', '_'], " ");
    let (mut about, mut title, mut labels) = (String::new(), String::new(), Vec::new());
    let mut body = None;
    for (k, v) in top.iter() {
        match k.as_str() {
            "name" => {
                if let Some(s) = scalar(v).filter(|s| !s.trim().is_empty()) {
                    name = clip(s.trim().to_string());
                }
            }
            "description" | "about" => about = scalar(v).map(clip).unwrap_or_default(),
            "title" => title = scalar(v).map(|s| s.chars().take(256).collect()).unwrap_or_default(),
            "labels" => match v {
                Node::Sequence(seq) => {
                    labels = seq.iter().filter_map(scalar).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
                }
                other => {
                    labels = scalar(other).unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
                }
            },
            "body" => body = Some(v),
            _ => {}
        }
    }
    labels.truncate(20);
    let Some(Node::Sequence(items)) = body else { return Err("`body` must be a list of fields".into()) };
    if items.len() > MAX_FIELDS {
        return Err(format!("a form has at most {MAX_FIELDS} fields"));
    }
    let mut fields: Vec<FormField> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        let Node::Mapping(m) = item else { return Err(format!("field {n} must be a mapping with a `type`")) };
        let mut kind = None;
        let mut id = String::new();
        let mut attrs = None;
        let mut required = false;
        for (k, v) in m.iter() {
            match k.as_str() {
                "type" => {
                    kind = Some(match scalar(v).unwrap_or_default().as_str() {
                        "markdown" => FieldKind::Markdown,
                        "input" => FieldKind::Input,
                        "textarea" => FieldKind::Textarea,
                        "dropdown" => FieldKind::Dropdown,
                        "checkboxes" => FieldKind::Checkboxes,
                        other => {
                            return Err(format!(
                                "field {n} has the type `{other}`, which is not supported (use markdown, input, textarea, dropdown or checkboxes)"
                            ));
                        }
                    })
                }
                "id" => id = scalar(v).unwrap_or_default(),
                "attributes" => attrs = Some(v),
                "validations" => {
                    if let Node::Mapping(vm) = v {
                        for (vk, vv) in vm.iter() {
                            if vk.as_str() == "required" {
                                required = truthy(vv);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(kind) = kind else { return Err(format!("field {n} needs a `type`")) };
        let mut f = FormField {
            kind,
            id: if id.is_empty() { format!("{}{n}", if kind == FieldKind::Markdown { "m" } else { "field" }) } else { id },
            label: String::new(),
            description: String::new(),
            placeholder: String::new(),
            value: String::new(),
            options: Vec::new(),
            multiple: false,
            required: required && kind != FieldKind::Markdown && kind != FieldKind::Checkboxes,
            render: String::new(),
        };
        if !valid_id(&f.id) {
            return Err(format!("field {n} has an id with characters other than letters, digits, dashes and underscores"));
        }
        if fields.iter().any(|o| o.id == f.id) {
            return Err(format!("the id `{}` is used twice", f.id));
        }
        if let Some(Node::Mapping(am)) = attrs {
            for (k, v) in am.iter() {
                match k.as_str() {
                    "label" => f.label = scalar(v).map(clip).unwrap_or_default(),
                    "description" => f.description = scalar(v).map(clip).unwrap_or_default(),
                    "placeholder" => f.placeholder = scalar(v).map(clip).unwrap_or_default(),
                    "value" => f.value = scalar(v).map(clip).unwrap_or_default(),
                    "multiple" => f.multiple = truthy(v),
                    "render" => {
                        f.render = scalar(v)
                            .unwrap_or_default()
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '#' | '.'))
                            .take(20)
                            .collect()
                    }
                    "options" => {
                        let Node::Sequence(opts) = v else { return Err(format!("`options` of field {n} must be a list")) };
                        if opts.len() > MAX_OPTIONS {
                            return Err(format!("field {n} has more than {MAX_OPTIONS} options"));
                        }
                        for o in opts.iter() {
                            match o {
                                Node::Scalar(s) => f.options.push(FormOption { label: clip(s.as_str().to_string()), required: false }),
                                Node::Mapping(om) => {
                                    let mut label = String::new();
                                    let mut req = false;
                                    for (ok, ov) in om.iter() {
                                        match ok.as_str() {
                                            "label" => label = scalar(ov).map(clip).unwrap_or_default(),
                                            "required" => req = truthy(ov),
                                            _ => {}
                                        }
                                    }
                                    f.options.push(FormOption { label, required: req });
                                }
                                _ => return Err(format!("an option of field {n} must be text")),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if kind == FieldKind::Markdown {
            if f.value.trim().is_empty() {
                return Err(format!("the markdown field {n} needs `attributes.value`"));
            }
        } else if f.label.trim().is_empty() {
            return Err(format!("field {n} needs `attributes.label`"));
        }
        if matches!(kind, FieldKind::Dropdown | FieldKind::Checkboxes)
            && (f.options.is_empty() || f.options.iter().any(|o| o.label.trim().is_empty()))
        {
            return Err(format!("field {n} needs `attributes.options`, none of them empty"));
        }
        fields.push(f);
    }
    if fields.iter().all(|f| f.kind == FieldKind::Markdown) {
        return Err("a form needs at least one field that is not markdown".into());
    }
    Ok(ParsedForm { name, about, title, labels, form: IssueForm { fields } })
}

/// What a person typed, by control name. A control sent twice (checkboxes, a multiple dropdown)
/// has several values.
pub type Submitted = HashMap<String, Vec<String>>;

/// Why a value was refused: `(control name, message)`.
pub type FormErrors = Vec<(String, String)>;

fn one<'a>(sub: &'a Submitted, control: &str) -> &'a str {
    sub.get(control).and_then(|v| v.first()).map(String::as_str).unwrap_or("")
}

/// Checks every submitted value against its field and builds the issue body. A section per field,
/// headed by its label, in the form's order; an empty optional answer reads `_No response_`.
pub fn compose(form: &IssueForm, sub: &Submitted) -> Result<String, FormErrors> {
    let mut errors: FormErrors = Vec::new();
    let mut out = String::new();
    for f in &form.fields {
        if f.is_markdown() {
            continue;
        }
        let c = f.control();
        let answer = match f.kind {
            FieldKind::Input | FieldKind::Textarea => {
                let v = one(sub, &c).replace("\r\n", "\n");
                if v.len() > MAX_VALUE {
                    errors.push((c.clone(), "That answer is too long.".into()));
                }
                if f.is_input() && v.contains('\n') {
                    errors.push((c.clone(), "Keep this answer on one line.".into()));
                }
                if f.required && v.trim().is_empty() {
                    errors.push((c.clone(), "This answer is required.".into()));
                }
                let v = v.trim_end().to_string();
                if v.trim().is_empty() {
                    "_No response_".to_string()
                } else if f.render.is_empty() {
                    v
                } else {
                    // A longer fence than any run of backticks inside, so the answer cannot end it.
                    let longest = v.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
                    let fence = "`".repeat(longest.max(2) + 1);
                    format!("{fence}{}\n{v}\n{fence}", f.render)
                }
            }
            FieldKind::Dropdown => {
                let picked: Vec<&String> = sub.get(&c).map(|v| v.iter().filter(|s| !s.is_empty()).collect()).unwrap_or_default();
                for p in &picked {
                    if !f.options.iter().any(|o| &o.label == *p) {
                        errors.push((c.clone(), "Choose one of the listed options.".into()));
                        break;
                    }
                }
                if picked.len() > 1 && !f.multiple {
                    errors.push((c.clone(), "Choose one option.".into()));
                }
                if f.required && picked.is_empty() {
                    errors.push((c.clone(), "Choose an option.".into()));
                }
                if picked.is_empty() {
                    "_No response_".to_string()
                } else {
                    picked.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                }
            }
            FieldKind::Checkboxes => {
                let ticked: Vec<&String> = sub.get(&c).map(|v| v.iter().collect()).unwrap_or_default();
                let mut lines = Vec::new();
                for (i, o) in f.options.iter().enumerate() {
                    // Each box is its own value, its position, so two options with one label stay apart.
                    let on = ticked.iter().any(|t| t.as_str() == i.to_string());
                    if o.required && !on {
                        errors.push((c.clone(), format!("Tick “{}” to go on.", o.label)));
                    }
                    lines.push(format!("- [{}] {}", if on { "x" } else { " " }, o.label));
                }
                lines.join("\n")
            }
            FieldKind::Markdown => continue,
        };
        out.push_str(&format!("### {}\n\n{answer}\n\n", f.label.trim()));
    }
    if errors.is_empty() { Ok(out.trim_end().to_string() + "\n") } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUG: &str = "name: Bug report
description: Something broke
title: \"[Bug]: \"
labels: [bug, triage]
body:
  - type: markdown
    attributes:
      value: Thanks for the report.
  - type: input
    id: version
    attributes:
      label: Version
      placeholder: 1.2.3
    validations:
      required: true
  - type: textarea
    id: log
    attributes:
      label: Log
      render: shell
  - type: dropdown
    id: where
    attributes:
      label: Where
      options: [Linux, Mac]
  - type: checkboxes
    id: terms
    attributes:
      label: Terms
      options:
        - label: I searched first
          required: true
        - label: I can help
";

    /// A deeply nested file must be turned away before the YAML parser recurses into it: a stack
    /// overflow takes the whole server down, and this runs on every new-issue page load.
    #[test]
    fn a_deeply_nested_form_is_refused_not_parsed() {
        let seq = format!("name: x\nbody:\n{}", "- ".repeat(8000));
        assert!(seq.len() < MAX_FORM_BYTES);
        let e = parse("x.yml", &seq).unwrap_err();
        assert!(e.contains("nested more than"), "{e}");
        let flow = format!("name: x\nbody: {}{}\n", "[".repeat(30_000), "]".repeat(30_000));
        assert!(parse("x.yml", &flow).is_err());
        let braces = format!("name: x\nbody: {}{}\n", "{a: ".repeat(5000), "}".repeat(5000));
        assert!(parse("x.yml", &braces).unwrap_err().contains("nested more than"));
        let mut block = String::from("name: x\nbody:\n");
        for i in 0..100 {
            block.push_str(&format!("{}k{i}:\n", "  ".repeat(i + 1)));
        }
        assert!(parse("x.yml", &block).unwrap_err().contains("nested more than"));
        // An ordinary form is untouched.
        assert!(parse("bug.yml", BUG).is_ok());
        assert!(nesting_too_deep("a:\n  b:\n    - c: [1, {d: 2}]\n").is_none());
    }

    /// YAML complex-key markers (`? ` and `: `) recurse in the parser the way brackets do, and the
    /// first guard did not count them: a 2 KB file of `? ` aborted the whole server. No issue form
    /// needs an explicit key, so one is refused outright, wherever it starts a node.
    #[test]
    fn an_explicit_yaml_key_is_refused_not_parsed() {
        let head = "name: x\nbody:\n";
        let cases = [
            ("2 KB question marks", format!("{head}{}x\n", "? ".repeat(1000))),
            ("16 KB question marks", format!("{head}{}x\n", "? ".repeat(8000))),
            ("short question marks", format!("{head}{}x\n", "? ".repeat(600))),
            ("colons", format!("{head}{}x\n", ": ".repeat(8000))),
            ("question mark and dash", format!("{head}{}x\n", "? - ".repeat(8000))),
            ("dash and question mark", format!("{head}{}x\n", "- ? ".repeat(8000))),
            ("indented key", format!("{head}  {}x\n", "? ".repeat(8000))),
            ("indented colons", format!("{head}  {}x\n", ": ".repeat(8000))),
            ("one key", format!("{head}  ? a\n  : b\n")),
            ("tab after the marker", format!("{head}{}x\n", "?\t".repeat(8000))),
            ("tab after a dash", format!("{head}{}x\n", "-\t".repeat(8000))),
            ("tab dash then key", format!("{head}{}x\n", "-\t? ".repeat(8000))),
            ("key after a value colon", format!("{head}a: {}x\n", "? ".repeat(8000))),
            ("colons after a key", format!("{head}a: {}x\n", ": ".repeat(8000))),
            ("key in a flow list", format!("name: x\nbody: [{}x]\n", "? ".repeat(8000))),
            ("key in a flow map", format!("name: x\nbody: {{{}x}}\n", "? ".repeat(8000))),
            ("key after a flow comma", "name: x\nbody: [a, ? b]\n".to_string()),
            ("lone marker", format!("{head}  ?\n")),
        ];
        for (what, text) in &cases {
            assert!(text.len() < MAX_FORM_BYTES, "{what} must fit under the size limit to test the guard");
            let e = parse("x.yml", text).unwrap_err();
            assert!(e.contains("explicit") || e.contains("nested more than"), "{what}: {e}");
        }
        let e = parse("x.yml", &format!("{head}  ? a\n  : b\n")).unwrap_err();
        assert!(e.contains("explicit"), "a plain error says what is wrong: {e}");
    }

    /// The refusal must not catch ordinary text that merely contains these characters.
    #[test]
    fn question_marks_and_colons_in_ordinary_text_are_fine() {
        let ok = "name: Bug? Tell us\ndescription: \"Why? : because\"\nbody:\n  - type: textarea\n    id: a\n    attributes:\n      label: What happened?\n      description: 'a ? b : c'\n      placeholder: |\n        ? this line is inside a block scalar\n        : and so is this one\n";
        assert!(nesting_too_deep(ok).is_none());
        let p = parse("x.yml", ok).unwrap();
        assert_eq!(p.form.fields.len(), 1);
        assert!(nesting_too_deep("a: b ? c\nd: http://x/y?z\ne: ?x\nf: :y\n").is_none());
    }

    /// Every repeated YAML marker, at the start of a line, indented, after a key and after a dash,
    /// must come back as a normal result and never overflow the stack.
    #[test]
    fn repeated_markers_never_crash_the_parser() {
        let tokens = [
            "? ", ": ", "- ", "? - ", "- ? ", "- : ", "&a ", "!a ", "*a ", "? &a ", "[", "{", "[? ", "{? ", "[- ", "{a: ", "? [", "? {",
            "|", "> ", "'", "\"", "-\t", "?\t", ":\t", "a: ", "a:\n  ", "- a:\n  ", "? a\n: ", "--- ", "%TAG ", "<<: ", "!!map ", "\\\" ",
            "' ", "key: ? ",
        ];
        for t in tokens {
            for reps in [50, 700, 4000] {
                let body = t.repeat(reps);
                for shape in [
                    format!("{body}x\n"),
                    format!("  {body}x\n"),
                    format!("a: {body}x\n"),
                    format!("- {body}x\n"),
                    format!("a:\n  b: {body}x\n"),
                    format!("{}x\n", format!("{t}\n").repeat(reps)),
                ] {
                    let _ = parse("x.yml", &format!("name: x\nbody:\n{shape}")).is_ok() | parse("x.yml", &shape).is_ok();
                }
            }
        }
    }

    /// Every way YAML ends a line must end a line for the scan too. A lone CR hid a 6000 dash
    /// chain from it, the parser overflowed its stack and the server aborted on every view of the
    /// new-issue page (ch-qop). CR, NEL, LS and PS are normalised to LF once, and the scan and the
    /// parser read the same text.
    #[test]
    fn a_dash_chain_behind_any_line_break_is_refused() {
        let chain = "- ".repeat(6000);
        for (name, sep) in [("lone CR", "\r"), ("CRLF", "\r\n"), ("NEL", "\u{85}"), ("LS", "\u{2028}"), ("PS", "\u{2029}"), ("LF", "\n")] {
            let text = format!("name: x{sep}body:{sep}{chain}x\n");
            assert!(text.len() < MAX_FORM_BYTES, "{name}");
            let e = parse("x.yml", &text).expect_err(name);
            assert!(e.contains("nested more than"), "{name}: {e}");
            assert!(nesting_too_deep(&format!("body:{sep}{chain}x\n")).is_some(), "{name}");
            let mut block = String::from("name: x\nbody:\n");
            for i in 0..100 {
                block.push_str(&format!("{}k{i}:\n", "  ".repeat(i + 1)));
            }
            let e = parse("x.yml", &block.replace('\n', sep)).expect_err(name);
            assert!(e.contains("nested more than"), "{name} block: {e}");
            let flow = format!("name: x{sep}body: {}{}\n", "[".repeat(5000), "]".repeat(5000));
            assert!(parse("x.yml", &flow).is_err(), "{name} flow");
            let keys = format!("name: x{sep}body:{sep}{}x\n", "? ".repeat(6000));
            assert!(parse("x.yml", &keys).is_err(), "{name} keys");
        }
    }

    /// A form that is fine with LF stays fine with any other line break.
    #[test]
    fn other_line_breaks_read_like_lf() {
        let want = parse("bug.yml", BUG).unwrap();
        for sep in ["\r", "\r\n"] {
            let got = parse("bug.yml", &BUG.replace('\n', sep)).unwrap_or_else(|e| panic!("{sep:?}: {e}"));
            assert_eq!(got.form.fields.len(), want.form.fields.len(), "{sep:?}");
        }
    }

    /// Random bytes and random mixes of YAML markers, CR, tabs, quotes and Unicode separators, at
    /// the cap size. Nothing may abort the process: an abort ends this test binary, so reaching
    /// the end is the proof.
    #[test]
    fn fuzzed_files_at_the_cap_never_abort() {
        let cap = MAX_FORM_BYTES;
        let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let atoms = [
            "- ",
            "? ",
            ": ",
            "[",
            "{",
            "]",
            "}",
            ",",
            "\r",
            "\n",
            "\r\n",
            "\t",
            "\u{85}",
            "\u{2028}",
            "\u{2029}",
            "\u{feff}",
            "'",
            "\"",
            "\\",
            "|",
            ">",
            "#",
            "&a ",
            "*a ",
            "!t ",
            "--- ",
            "...",
            "%",
            "  ",
            "a: ",
            "-\t",
            "?\t",
            "x",
            "k:\n  ",
            "\0",
            "é",
            "\u{1F600}",
        ];
        for round in 0..200 {
            let bytes: Vec<u8> = (0..cap / 3).map(|_| next() as u8).collect();
            let _ = parse("x.yml", &String::from_utf8_lossy(&bytes));
            let pool: Vec<&str> = (0..1 + (next() % 4) as usize).map(|_| atoms[(next() % atoms.len() as u64) as usize]).collect();
            let mut s = String::from(if round % 2 == 0 { "name: x\nbody:\n" } else { "" });
            while s.len() < cap - 16 {
                s.push_str(pool[(next() % pool.len() as u64) as usize]);
                if next() % 64 == 0 {
                    s.push_str(atoms[(next() % atoms.len() as u64) as usize]);
                }
            }
            let _ = parse("x.yml", &s);
            let mut t = s.clone();
            t.truncate(t.char_indices().map(|(i, _)| i).take_while(|&i| i <= cap / 2).last().unwrap_or(0));
            let _ = parse("x.yml", &t.replace('\n', "\r"));
            let _ = parse("x.yml", &t.replace('\n', "\u{85}"));
        }
    }

    fn sub(pairs: &[(&str, &[&str])]) -> Submitted {
        pairs.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect()
    }

    #[test]
    fn a_form_is_read_in_order_with_its_rules() {
        let p = parse("bug.yml", BUG).unwrap();
        assert_eq!((p.name.as_str(), p.about.as_str(), p.title.as_str()), ("Bug report", "Something broke", "[Bug]: "));
        assert_eq!(p.labels, vec!["bug", "triage"]);
        let kinds: Vec<FieldKind> = p.form.fields.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [FieldKind::Markdown, FieldKind::Input, FieldKind::Textarea, FieldKind::Dropdown, FieldKind::Checkboxes]);
        assert!(p.form.fields[1].required && !p.form.fields[2].required);
        assert!(p.form.fields[4].options[0].required);
        assert_eq!(p.form.fields[1].control(), "f_version");
    }

    #[test]
    fn bad_forms_say_what_is_wrong() {
        let bad = |t: &str, want: &str| {
            let e = parse("x.yml", t).unwrap_err();
            assert!(e.contains(want), "{e}");
        };
        bad("name: x\n", "`body` must be a list");
        bad("body:\n  - type: markdown\n    attributes:\n      value: hi\n", "at least one field");
        bad("body:\n  - type: color\n", "not supported");
        bad("body:\n  - type: input\n    attributes: {}\n", "needs `attributes.label`");
        bad("body:\n  - type: dropdown\n    attributes:\n      label: A\n", "needs `attributes.options`");
        bad(
            "body:\n  - type: input\n    id: a\n    attributes: {label: A}\n  - type: input\n    id: a\n    attributes: {label: B}\n",
            "used twice",
        );
        bad("body:\n  - type: input\n    id: \"a b\"\n    attributes: {label: A}\n", "id with characters");
        bad("a: [\n", "not valid YAML");
        bad(&format!("body:\n{}", "  - type: input\n    attributes: {label: A}\n".repeat(MAX_FIELDS + 1)), "at most");
    }

    #[test]
    fn answers_become_a_body_and_required_ones_are_enforced() {
        let form = parse("bug.yml", BUG).unwrap().form;
        let ok = sub(&[("f_version", &["1.2.3"]), ("f_log", &["boom\nbang"]), ("f_where", &["Mac"]), ("f_terms", &["0"])]);
        let body = compose(&form, &ok).unwrap();
        assert_eq!(
            body,
            "### Version\n\n1.2.3\n\n### Log\n\n```shell\nboom\nbang\n```\n\n### Where\n\nMac\n\n### Terms\n\n- [x] I searched first\n- [ ] I can help\n"
        );
        let none = compose(&form, &sub(&[])).unwrap_err();
        let fields: Vec<&str> = none.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(fields, ["f_version", "f_terms"], "a required input and a required box");
        // Optional answers are marked, not left blank.
        let b = compose(&form, &sub(&[("f_version", &["1"]), ("f_terms", &["0"])])).unwrap();
        assert!(b.contains("### Log\n\n_No response_") && b.contains("### Where\n\n_No response_"));
    }

    #[test]
    fn a_value_cannot_make_up_an_option_or_break_out_of_its_code_block() {
        let form = parse("bug.yml", BUG).unwrap().form;
        let forged = sub(&[("f_version", &["1"]), ("f_where", &["Windows"]), ("f_terms", &["0"])]);
        assert_eq!(compose(&form, &forged).unwrap_err()[0].0, "f_where");
        let two = sub(&[("f_version", &["1"]), ("f_where", &["Mac", "Linux"]), ("f_terms", &["0"])]);
        assert!(compose(&form, &two).is_err(), "one option unless `multiple`");
        let sneaky = sub(&[("f_version", &["1"]), ("f_log", &["x\n```\n# heading\n```"]), ("f_terms", &["0"])]);
        let b = compose(&form, &sneaky).unwrap();
        assert!(b.contains("````shell\nx\n```\n# heading\n```\n````"), "{b}");
        let multi_line = sub(&[("f_version", &["1\n2"]), ("f_terms", &["0"])]);
        assert!(compose(&form, &multi_line).is_err(), "an input is one line");
    }
}
