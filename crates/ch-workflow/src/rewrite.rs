//! Rewrites the expressions of a reusable workflow before it is inlined into its caller
//! (ch-ctk.7, `crate::reusable`).
//!
//! The called workflow's jobs run inside the caller's run, so the contexts that mean something
//! only inside the called workflow are replaced by what they stand for: `inputs.x` by what the
//! caller passed, `secrets.X` by what the caller passed under that name (an empty string when it
//! passed nothing, so a called job can never read a secret it was not given), `needs.id` and
//! `jobs.id` by the new job key. This is a small scanner, not a full parser. It understands
//! single-quoted strings (with `''` for a quote), identifiers and the two ways to name a
//! property, and leaves every other byte alone.

use std::collections::BTreeMap;

/// What to substitute. Keys of every map are lower case.
#[derive(Debug, Clone, Default)]
pub struct Rewrite {
    /// `inputs.<name>` and `inputs['<name>']` are replaced by this expression fragment (already
    /// safe to paste: `'abc'`, `true`, `3` or `(github.ref)`). A name that is not here is an error.
    pub inputs: BTreeMap<String, String>,
    /// `secrets.<NAME>` and `secrets['<NAME>']` are replaced by the fragment. A name not in the
    /// map becomes `''` unless `inherit_secrets`, in which case it is left as written.
    /// `GITHUB_TOKEN` is always left as written.
    pub secrets: BTreeMap<String, String>,
    pub inherit_secrets: bool,
    /// `needs.<id>` and `jobs.<id>` both become `needs.<new key>`: job id (lower case) to new
    /// key. An id that is not here is left as written.
    pub jobs: BTreeMap<String, String>,
}

/// The longest text a rewrite may make from one string. A value that repeats `inputs.x` many
/// times, passed down through several calls, would otherwise grow at every level.
pub const MAX_REWRITTEN: usize = 64 * 1024;

fn too_long() -> String {
    format!("it grows past {} KB when the inputs are pasted in, which is too long", MAX_REWRITTEN / 1024)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// The end of a single-quoted literal that starts at `i` (which holds the opening quote), as the
/// index after the closing quote. An unterminated literal runs to the end.
fn literal_end(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\'' {
            if b.get(j + 1) == Some(&b'\'') {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

fn next_non_space(b: &[u8], mut i: usize) -> Option<u8> {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    b.get(i).copied()
}

/// One step of the scanner over a bare expression.
enum Tok<'a> {
    /// A string literal, a number, an operator, a space: copied as it is.
    Plain,
    /// An identifier, with whether it starts an access chain (not after a `.`, not a function).
    Word { text: &'a str, root: bool },
}

/// Walks `expr`, calling `f(token, end)` for each token, where `end` is the index after it.
/// `f` may return a new position to jump to (after consuming more text itself).
fn scan<'a>(expr: &'a str, mut f: impl FnMut(Tok<'a>, usize) -> Result<Option<usize>, String>) -> Result<(), String> {
    let b = expr.as_bytes();
    let mut i = 0;
    let mut prev: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c == b'\'' {
            i = literal_end(b, i);
            prev = Some(b'\'');
            if let Some(j) = f(Tok::Plain, i)? {
                i = j;
            }
        } else if c.is_ascii_digit() {
            // A number, hex or exponent form included, never read as a name.
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            prev = Some(b'0');
            if let Some(j) = f(Tok::Plain, i)? {
                i = j;
            }
        } else if is_ident_start(c) {
            while i < b.len() && is_ident_char(b[i]) {
                i += 1;
            }
            let root = prev != Some(b'.') && next_non_space(b, i) != Some(b'(');
            prev = Some(b'a');
            if let Some(j) = f(Tok::Word { text: &expr[start..i], root }, i)? {
                i = j;
                prev = Some(b')');
            }
        } else {
            let ch = expr[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            i += ch;
            if !c.is_ascii_whitespace() {
                prev = Some(c);
            }
            if let Some(j) = f(Tok::Plain, i)? {
                i = j;
            }
        }
    }
    Ok(())
}

/// How a root context is accessed: by a name, or not in a way we can read.
enum Access {
    /// `.name` or `['name']`, with the index after it.
    Named(String, usize),
    /// A `[...]` that is not a quoted literal, `.*`, or no access at all.
    Other,
}

fn access_at(expr: &str, i: usize) -> Access {
    let b = expr.as_bytes();
    match b.get(i) {
        Some(b'.') => {
            let s = i + 1;
            if s < b.len() && is_ident_start(b[s]) {
                let mut e = s;
                while e < b.len() && is_ident_char(b[e]) {
                    e += 1;
                }
                Access::Named(expr[s..e].to_string(), e)
            } else {
                Access::Other
            }
        }
        Some(b'[') => {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if b.get(j) != Some(&b'\'') {
                return Access::Other;
            }
            let end = literal_end(b, j);
            if end < j + 2 || b[end - 1] != b'\'' {
                return Access::Other;
            }
            let name = expr[j + 1..end - 1].replace("''", "'");
            let mut k = end;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if b.get(k) == Some(&b']') { Access::Named(name, k + 1) } else { Access::Other }
        }
        _ => Access::Other,
    }
}

/// Rewrites one bare expression (no `${{ }}` around it).
pub fn rewrite_expr(expr: &str, r: &Rewrite) -> Result<String, String> {
    let mut out = String::with_capacity(expr.len());
    let mut copied_to = 0usize;
    scan(expr, |tok, end| {
        let Tok::Word { text, root: true } = tok else { return Ok(None) };
        let lower = text.to_ascii_lowercase();
        if !matches!(lower.as_str(), "inputs" | "secrets" | "needs" | "jobs") {
            return Ok(None);
        }
        let start = end - text.len();
        let access = access_at(expr, end);
        let (replacement, next) = match (lower.as_str(), access) {
            ("inputs", Access::Named(name, next)) => match r.inputs.get(&name.to_ascii_lowercase()) {
                Some(f) => (f.clone(), next),
                None => return Err(format!("the workflow has no input `{name}`")),
            },
            ("inputs", Access::Other) => {
                return Err("the whole `inputs` context cannot be used here, name the input you need".into());
            }
            ("secrets", Access::Named(name, next)) => {
                let key = name.to_ascii_lowercase();
                if key == "github_token" {
                    return Ok(None);
                }
                match r.secrets.get(&key) {
                    Some(f) => (f.clone(), next),
                    None if r.inherit_secrets => return Ok(None),
                    None => ("''".to_string(), next),
                }
            }
            ("secrets", Access::Other) => {
                if r.inherit_secrets {
                    return Ok(None);
                }
                return Err("the whole `secrets` context cannot be used here, name the secret you need".into());
            }
            (_, Access::Named(name, next)) => match r.jobs.get(&name.to_ascii_lowercase()) {
                Some(new) => (format!("needs.{new}"), next),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        out.push_str(&expr[copied_to..start]);
        out.push_str(&replacement);
        if out.len() > MAX_REWRITTEN {
            return Err(too_long());
        }
        copied_to = next;
        Ok(Some(next))
    })?;
    out.push_str(&expr[copied_to.min(expr.len())..]);
    if out.len() > MAX_REWRITTEN {
        return Err(too_long());
    }
    Ok(out)
}

/// The index of the `}}` that ends an expression whose text starts at `from`, outside any
/// single-quoted string.
fn expr_close(text: &str, from: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\'' => i = literal_end(b, i),
            b'}' if b.get(i + 1) == Some(&b'}') => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Rewrites every `${{ ... }}` in `text` and leaves all other bytes untouched.
pub fn rewrite_text(text: &str, r: &Rewrite) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = 0usize;
    while let Some(open) = text[rest..].find("${{") {
        let open = rest + open;
        let inner = open + 3;
        let Some(close) = expr_close(text, inner) else { return Err("unclosed ${{ }}".into()) };
        out.push_str(&text[rest..inner]);
        out.push_str(&rewrite_expr(&text[inner..close], r)?);
        out.push_str("}}");
        if out.len() > MAX_REWRITTEN {
            return Err(too_long());
        }
        rest = close + 2;
    }
    out.push_str(&text[rest..]);
    Ok(out)
}

/// For an `if:` value, which may be written with or without `${{ }}`.
pub fn rewrite_condition(text: &str, r: &Rewrite) -> Result<String, String> {
    if text.contains("${{") { rewrite_text(text, r) } else { rewrite_expr(text, r) }
}

/// The kind of a reusable workflow input, for [`fragment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragType {
    String,
    Boolean,
    Number,
}

/// A value split into plain text and `${{ }}` expressions.
enum Part<'a> {
    Text(&'a str),
    Expr(&'a str),
}

fn parts(value: &str) -> Result<Vec<Part<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = 0usize;
    while let Some(open) = value[rest..].find("${{") {
        let open = rest + open;
        let Some(close) = expr_close(value, open + 3) else { return Err("unclosed ${{ }}".into()) };
        if open > rest {
            out.push(Part::Text(&value[rest..open]));
        }
        out.push(Part::Expr(value[open + 3..close].trim()));
        rest = close + 2;
    }
    if rest < value.len() {
        out.push(Part::Text(&value[rest..]));
    }
    Ok(out)
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Turns the text a caller wrote for an input into an expression fragment to paste. A value that
/// is exactly one `${{ expr }}` becomes `(expr)`. A string that mixes text and expressions
/// becomes a `format(...)` call. Anything else is a literal of the input's type.
pub fn fragment(value: &str, ty: FragType) -> Result<String, String> {
    if value.contains("${{") {
        let ps = parts(value)?;
        let only_expr = match ps.as_slice() {
            [Part::Expr(e)] => Some(*e),
            [Part::Text(a), Part::Expr(e)] if a.trim().is_empty() => Some(*e),
            [Part::Expr(e), Part::Text(z)] if z.trim().is_empty() => Some(*e),
            [Part::Text(a), Part::Expr(e), Part::Text(z)] if a.trim().is_empty() && z.trim().is_empty() => Some(*e),
            _ => None,
        };
        if let Some(e) = only_expr {
            return Ok(format!("({e})"));
        }
        if ty != FragType::String {
            return Err(match ty {
                FragType::Boolean => "expected true or false, or a single expression".into(),
                _ => "expected a number, or a single expression".into(),
            });
        }
        let mut fmt = String::new();
        let mut args: Vec<String> = Vec::new();
        for p in &ps {
            match p {
                Part::Text(t) => fmt.push_str(&t.replace('\'', "''").replace('{', "{{").replace('}', "}}")),
                Part::Expr(e) => {
                    fmt.push_str(&format!("{{{}}}", args.len()));
                    let trivial = !e.is_empty() && e.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
                    args.push(if trivial { (*e).to_string() } else { format!("({e})") });
                }
            }
        }
        return Ok(format!("format('{fmt}', {})", args.join(", ")));
    }
    match ty {
        FragType::String => Ok(quote(value)),
        FragType::Boolean => match value.trim().to_ascii_lowercase().as_str() {
            "true" => Ok("true".into()),
            "false" => Ok("false".into()),
            _ => Err("expected true or false".into()),
        },
        FragType::Number => {
            let t = value.trim();
            let body = t.strip_prefix('-').unwrap_or(t);
            let (int, frac) = body.split_once('.').map_or((body, None), |(a, b)| (a, Some(b)));
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
            if digits(int) && frac.is_none_or(digits) { Ok(t.to_string()) } else { Err("expected a number".into()) }
        }
    }
}

/// The names of the contexts an expression (or fragment) reads, lower case, each once, sorted:
/// every identifier outside string literals that starts an access chain, other than the words
/// `true`, `false`, `null`, `NaN` and `Infinity`.
pub fn contexts_read(expr: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    let _ = scan(expr, |tok, _| {
        if let Tok::Word { text, root: true } = tok {
            let l = text.to_ascii_lowercase();
            if !matches!(l.as_str(), "true" | "false" | "null" | "nan" | "infinity") {
                found.insert(l);
            }
        }
        Ok(None)
    });
    found.into_iter().collect()
}

/// The jobs an expression reads through `needs`, lower case, each once: `needs.build.outputs.v` and
/// `needs['build'].result` both give `build`. The whole context (`toJSON(needs)`, `needs.*`) is
/// an error, because what it reads cannot be named.
pub fn needs_read(expr: &str) -> Result<Vec<String>, String> {
    let mut found = std::collections::BTreeSet::new();
    scan(expr, |tok, end| {
        let Tok::Word { text, root: true } = tok else { return Ok(None) };
        if !text.eq_ignore_ascii_case("needs") {
            return Ok(None);
        }
        match access_at(expr, end) {
            Access::Named(name, next) => {
                found.insert(name.to_ascii_lowercase());
                Ok(Some(next))
            }
            Access::Other => Err("the whole `needs` context cannot be used here, name the job you need".into()),
        }
    })?;
    Ok(found.into_iter().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn needs_read_names_the_jobs() {
        assert_eq!(needs_read("needs.build.outputs.v == needs['Test'].result").unwrap(), ["build", "test"]);
        assert_eq!(needs_read("format('{0}', needs.a.outputs.x, needs.a.result)").unwrap(), ["a"]);
        assert_eq!(needs_read("'needs.x' == github.needs.y").unwrap(), Vec::<String>::new());
        for bad in ["toJSON(needs)", "needs.*", "needs[github.ref]"] {
            assert!(needs_read(bad).unwrap_err().contains("whole `needs` context"), "{bad}");
        }
    }

    fn rw() -> Rewrite {
        let mut r = Rewrite::default();
        r.inputs.insert("name".into(), "'abc'".into());
        r.inputs.insert("flag".into(), "true".into());
        r
    }

    #[test]
    fn inputs_are_replaced_and_strings_and_other_paths_are_not() {
        let r = rw();
        assert_eq!(rewrite_expr("format('x-{0}', inputs.name)", &r).unwrap(), "format('x-{0}', 'abc')");
        assert_eq!(rewrite_expr("INPUTS['Name'] == 'abc'", &r).unwrap(), "'abc' == 'abc'");
        assert_eq!(rewrite_expr("'inputs.name' == github.inputs.name", &r).unwrap(), "'inputs.name' == github.inputs.name");
        assert_eq!(rewrite_expr("inputs.name.length", &r).unwrap(), "'abc'.length");
        assert_eq!(rewrite_expr("inputs.flag && !inputs.flag", &r).unwrap(), "true && !true");
        assert_eq!(rewrite_expr("fromJSON(x).inputs", &r).unwrap(), "fromJSON(x).inputs");
    }

    #[test]
    fn an_unknown_input_and_the_whole_context_are_refused() {
        let r = rw();
        assert!(rewrite_expr("inputs.nope", &r).unwrap_err().contains("`nope`"));
        for bad in ["toJSON(inputs)", "inputs.*", "inputs[github.ref]", "inputs"] {
            assert!(rewrite_expr(bad, &r).unwrap_err().contains("whole `inputs` context"), "{bad}");
        }
    }

    #[test]
    fn secrets_are_what_was_passed_and_nothing_else() {
        let mut r = Rewrite::default();
        r.secrets.insert("token".into(), "(secrets.DEPLOY)".into());
        assert_eq!(rewrite_expr("secrets.TOKEN", &r).unwrap(), "(secrets.DEPLOY)");
        assert_eq!(rewrite_expr("secrets['token']", &r).unwrap(), "(secrets.DEPLOY)");
        assert_eq!(rewrite_expr("secrets.OTHER", &r).unwrap(), "''");
        assert_eq!(rewrite_expr("secrets.GITHUB_TOKEN", &r).unwrap(), "secrets.GITHUB_TOKEN");
        assert!(rewrite_expr("toJSON(secrets)", &r).unwrap_err().contains("whole `secrets` context"));
        r.inherit_secrets = true;
        assert_eq!(rewrite_expr("secrets.OTHER", &r).unwrap(), "secrets.OTHER");
        assert_eq!(rewrite_expr("secrets.GITHUB_TOKEN", &r).unwrap(), "secrets.GITHUB_TOKEN");
        assert_eq!(rewrite_expr("toJSON(secrets)", &r).unwrap(), "toJSON(secrets)");
    }

    #[test]
    fn needs_and_jobs_move_to_the_new_keys() {
        let mut r = Rewrite::default();
        r.jobs.insert("build".into(), "caller__build".into());
        assert_eq!(rewrite_expr("needs.build.outputs.v", &r).unwrap(), "needs.caller__build.outputs.v");
        assert_eq!(rewrite_expr("jobs.build.outputs.v", &r).unwrap(), "needs.caller__build.outputs.v");
        assert_eq!(rewrite_expr("needs['build'].result == 'success'", &r).unwrap(), "needs.caller__build.result == 'success'");
        assert_eq!(rewrite_expr("needs.other.result", &r).unwrap(), "needs.other.result");
        assert_eq!(rewrite_expr("needs[format('{0}', x)].result", &r).unwrap(), "needs[format('{0}', x)].result");
    }

    #[test]
    fn text_is_rewritten_only_inside_its_expressions() {
        let r = rw();
        assert_eq!(
            rewrite_text("echo ${{ inputs.name }} and ${{ 'x}}y' }} $VAR ${HOME} {{ inputs.name }}", &r).unwrap(),
            "echo ${{ 'abc' }} and ${{ 'x}}y' }} $VAR ${HOME} {{ inputs.name }}"
        );
        assert!(rewrite_text("echo ${{ inputs.name", &r).unwrap_err().contains("unclosed"));
        assert_eq!(rewrite_text("plain", &r).unwrap(), "plain");
    }

    #[test]
    fn a_condition_may_be_bare_or_wrapped() {
        let r = rw();
        assert_eq!(rewrite_condition("inputs.flag == true", &r).unwrap(), "true == true");
        assert_eq!(rewrite_condition("${{ inputs.flag == true }}", &r).unwrap(), "${{ true == true }}");
    }

    #[test]
    fn fragments_follow_the_type_of_the_input() {
        use FragType::*;
        assert_eq!(fragment("${{ github.ref }}", String).unwrap(), "(github.ref)");
        assert_eq!(fragment("  ${{ github.ref }} ", Boolean).unwrap(), "(github.ref)");
        assert_eq!(fragment("v${{ inputs.x }}-${{ github.run_id }}", String).unwrap(), "format('v{0}-{1}', inputs.x, github.run_id)");
        assert_eq!(fragment("a ${{ x == 'y' }} b", String).unwrap(), "format('a {0} b', (x == 'y'))");
        assert_eq!(fragment("it's", String).unwrap(), "'it''s'");
        assert_eq!(fragment("{x}", String).unwrap(), "'{x}'");
        assert_eq!(fragment("{x} ${{ y }}", String).unwrap(), "format('{{x}} {0}', y)");
        assert_eq!(fragment("True", Boolean).unwrap(), "true");
        assert_eq!(fragment("false", Boolean).unwrap(), "false");
        assert!(fragment("yes", Boolean).is_err());
        assert_eq!(fragment("-3.5", Number).unwrap(), "-3.5");
        assert_eq!(fragment("42", Number).unwrap(), "42");
        for bad in ["1e3", "", "1.", ".5", "0x10", "a1"] {
            assert!(fragment(bad, Number).is_err(), "{bad}");
        }
        assert!(fragment("n${{ x }}", Boolean).is_err());
        assert!(fragment("n${{ x }}", Number).is_err());
        assert!(fragment("${{ x", String).is_err());
    }

    #[test]
    fn the_contexts_an_expression_reads_are_listed() {
        assert_eq!(
            contexts_read("github.ref == 'needs' && needs.a.result == format('{0}', env.X)"),
            ["env".to_string(), "github".into(), "needs".into()]
        );
        assert_eq!(contexts_read("true && 0x1F == null && x.github"), ["x".to_string()]);
        assert_eq!(contexts_read("(secrets.A || inputs.b)"), ["inputs".to_string(), "secrets".into()]);
    }
}
