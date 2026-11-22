//! The `${{ }}` expression language, as the runner evaluates it.
//!
//! ARCHITECTURE.md section 9.2 gives `ch-workflow` the job of owning this engine so the server
//! and the runner agree on what an `if:` means. `ch-workflow` is built by the ci stream in
//! parallel with this one, so the runner carries its own copy of the evaluator for now. The
//! surface the rest of the runner uses is small and all of it is in this file's public
//! functions (`eval_condition`, `interpolate`, `eval_template`), which is what to swap when the
//! shared engine lands. Both are meant to follow GitHub's documented semantics, including:
//!
//! - names are case-insensitive, strings compare case-insensitively,
//! - `&&` and `||` return the operand that decided the result, not a bool,
//! - values of different types compare as numbers, `null` is 0 and an empty string is 0,
//! - `a.*.b` filters an object or array,
//! - an `if:` with no status function in it means `success() && (...)`.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Longest expression the engine accepts, in bytes. GitHub's own limit is 21000.
pub const MAX_EXPR_LEN: usize = 21_000;
/// Deepest nesting the parser follows before giving up, so hostile input cannot overflow the stack.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ExprError {
    pub message: String,
}

fn err<T>(m: impl Into<String>) -> Result<T, ExprError> {
    Err(ExprError { message: m.into() })
}

/// Where the job stands, for `success()`, `failure()`, `cancelled()` and `always()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Success,
    Failure,
    Cancelled,
}

type HashFilesFn<'a> = dyn Fn(&[String]) -> Result<String, String> + 'a;

/// Everything an expression can see.
pub struct EvalContext<'a> {
    /// Top-level contexts by lower-case name: `github`, `env`, `vars`, `secrets`, `matrix`, ...
    pub contexts: BTreeMap<String, Value>,
    pub status: JobStatus,
    /// `hashFiles(patterns...)`, run by the runner against the workspace.
    pub hash_files: Option<&'a HashFilesFn<'a>>,
}

impl<'a> EvalContext<'a> {
    pub fn new(status: JobStatus) -> EvalContext<'a> {
        EvalContext { contexts: BTreeMap::new(), status, hash_files: None }
    }

    pub fn set(&mut self, name: &str, v: Value) {
        self.contexts.insert(name.to_ascii_lowercase(), v);
    }
}

// ---------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Star,
    Not,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

fn lex(src: &str) -> Result<Vec<Tok>, ExprError> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '(' => {
                out.push(Tok::LParen);
                i += 1
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1
            }
            '.' => {
                // `.5` is a number only when it follows an operator; after a name it is a dot.
                let prev_is_value =
                    matches!(out.last(), Some(Tok::Ident(_) | Tok::RParen | Tok::RBracket | Tok::Star | Tok::Str(_) | Tok::Num(_)));
                if !prev_is_value && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    let (n, next) = lex_number(&chars, i)?;
                    out.push(Tok::Num(n));
                    i = next;
                } else {
                    out.push(Tok::Dot);
                    i += 1;
                }
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1
            }
            '*' => {
                out.push(Tok::Star);
                i += 1
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Tok::Ne);
                    i += 2;
                } else {
                    out.push(Tok::Not);
                    i += 1;
                }
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Tok::Eq);
                    i += 2;
                } else {
                    return err("unexpected '=', did you mean '=='?");
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Tok::Le);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    out.push(Tok::And);
                    i += 2;
                } else {
                    return err("unexpected '&', did you mean '&&'?");
                }
            }
            '|' => {
                if chars.get(i + 1) == Some(&'|') {
                    out.push(Tok::Or);
                    i += 2;
                } else {
                    return err("unexpected '|', did you mean '||'?");
                }
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return err("unterminated string"),
                        Some('\'') => {
                            if chars.get(i + 1) == Some(&'\'') {
                                s.push('\'');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(ch) => {
                            s.push(*ch);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Str(s));
            }
            '-' | '0'..='9' => {
                // A leading '-' belongs to a number; a '-' inside a name is handled in the ident arm.
                let (n, next) = lex_number(&chars, i)?;
                out.push(Tok::Num(n));
                i = next;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '-') {
                    i += 1;
                }
                out.push(Tok::Ident(chars[start..i].iter().collect()));
            }
            other => return err(format!("unexpected character {other:?}")),
        }
        if out.len() > 4096 {
            return err("expression is too long");
        }
    }
    Ok(out)
}

fn lex_number(chars: &[char], start: usize) -> Result<(f64, usize), ExprError> {
    let mut i = start;
    if chars[i] == '-' {
        i += 1;
    }
    // Hex: 0x1F
    if chars.get(i) == Some(&'0') && matches!(chars.get(i + 1), Some('x' | 'X')) {
        let mut j = i + 2;
        while chars.get(j).is_some_and(|c| c.is_ascii_hexdigit()) {
            j += 1;
        }
        if j == i + 2 {
            return err("invalid hex number");
        }
        let s: String = chars[i + 2..j].iter().collect();
        let n = u64::from_str_radix(&s, 16).map(|v| v as f64).map_err(|_| ExprError { message: "hex number is too large".into() })?;
        return Ok((if chars[start] == '-' { -n } else { n }, j));
    }
    let digits_start = i;
    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
    }
    if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
        while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    }
    if i == digits_start {
        return err("unexpected '-'");
    }
    if matches!(chars.get(i), Some('e' | 'E')) {
        let mut j = i + 1;
        if matches!(chars.get(j), Some('+' | '-')) {
            j += 1;
        }
        if chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
            while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
                j += 1;
            }
            i = j;
        }
    }
    let s: String = chars[start..i].iter().collect();
    match s.parse::<f64>() {
        Ok(n) => Ok((n, i)),
        Err(_) => err(format!("invalid number {s:?}")),
    }
}

// ---------------------------------------------------------------------------------------
// AST and parser
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    Ident(String),
    Prop(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    /// `.*`
    Filter(Box<Expr>),
    Call(String, Vec<Expr>),
    Not(Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn or(&mut self, depth: usize) -> Result<Expr, ExprError> {
        if depth > MAX_DEPTH {
            return err("expression is nested too deeply");
        }
        let mut l = self.and(depth)?;
        while self.eat(&Tok::Or) {
            let r = self.and(depth)?;
            l = Expr::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let mut l = self.equality(depth)?;
        while self.eat(&Tok::And) {
            let r = self.equality(depth)?;
            l = Expr::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn equality(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let mut l = self.relational(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => CmpOp::Eq,
                Some(Tok::Ne) => CmpOp::Ne,
                _ => break,
            };
            self.pos += 1;
            let r = self.relational(depth)?;
            l = Expr::Cmp(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn relational(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let mut l = self.unary(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => CmpOp::Lt,
                Some(Tok::Le) => CmpOp::Le,
                Some(Tok::Gt) => CmpOp::Gt,
                Some(Tok::Ge) => CmpOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let r = self.unary(depth)?;
            l = Expr::Cmp(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self, depth: usize) -> Result<Expr, ExprError> {
        if depth > MAX_DEPTH {
            return err("expression is nested too deeply");
        }
        if self.eat(&Tok::Not) {
            return Ok(Expr::Not(Box::new(self.unary(depth + 1)?)));
        }
        self.postfix(depth)
    }

    fn postfix(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let mut e = self.primary(depth)?;
        loop {
            if self.eat(&Tok::Dot) {
                match self.next() {
                    Some(Tok::Ident(name)) => e = Expr::Prop(Box::new(e), name),
                    // `a.0` and `a.1b` read as property names in GitHub; allow digits-led names.
                    Some(Tok::Num(n)) if n >= 0.0 && n.fract() == 0.0 => e = Expr::Prop(Box::new(e), format!("{}", n as i64)),
                    Some(Tok::Star) => e = Expr::Filter(Box::new(e)),
                    _ => return err("expected a property name after '.'"),
                }
            } else if self.eat(&Tok::LBracket) {
                let idx = self.or(depth + 1)?;
                if !self.eat(&Tok::RBracket) {
                    return err("expected ']'");
                }
                e = Expr::Index(Box::new(e), Box::new(idx));
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn primary(&mut self, depth: usize) -> Result<Expr, ExprError> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Lit(num_value(n))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::String(s))),
            Some(Tok::LParen) => {
                let e = self.or(depth + 1)?;
                if !self.eat(&Tok::RParen) {
                    return err("expected ')'");
                }
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !self.eat(&Tok::RParen) {
                        loop {
                            args.push(self.or(depth + 1)?);
                            if self.eat(&Tok::Comma) {
                                continue;
                            }
                            if self.eat(&Tok::RParen) {
                                break;
                            }
                            return err("expected ',' or ')' in the argument list");
                        }
                    }
                    return Ok(Expr::Call(name, args));
                }
                match name.as_str() {
                    "true" => Ok(Expr::Lit(Value::Bool(true))),
                    "false" => Ok(Expr::Lit(Value::Bool(false))),
                    "null" => Ok(Expr::Lit(Value::Null)),
                    "NaN" => Ok(Expr::Lit(Value::String("NaN".into()))).map(|_| Expr::Call("__nan".into(), vec![])),
                    "Infinity" => Ok(Expr::Call("__inf".into(), vec![])),
                    _ => Ok(Expr::Ident(name)),
                }
            }
            Some(t) => err(format!("unexpected {}", describe(&t))),
            None => err("the expression ends too soon"),
        }
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Num(_) => "number".into(),
        Tok::Str(_) => "string".into(),
        Tok::Ident(n) => format!("name {n:?}"),
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::LBracket => "'['".into(),
        Tok::RBracket => "']'".into(),
        Tok::Dot => "'.'".into(),
        Tok::Comma => "','".into(),
        Tok::Star => "'*'".into(),
        Tok::Not => "'!'".into(),
        Tok::Eq => "'=='".into(),
        Tok::Ne => "'!='".into(),
        Tok::Lt => "'<'".into(),
        Tok::Le => "'<='".into(),
        Tok::Gt => "'>'".into(),
        Tok::Ge => "'>='".into(),
        Tok::And => "'&&'".into(),
        Tok::Or => "'||'".into(),
    }
}

/// Parses one expression (no `${{ }}` around it).
pub fn parse(src: &str) -> Result<Expr, ExprError> {
    if src.len() > MAX_EXPR_LEN {
        return err(format!("expression is longer than {MAX_EXPR_LEN} characters"));
    }
    let toks = lex(src)?;
    if toks.is_empty() {
        return err("the expression is empty");
    }
    let mut p = Parser { toks, pos: 0 };
    let e = p.or(0)?;
    if let Some(t) = p.peek() {
        return err(format!("unexpected {} after the expression", describe(t)));
    }
    Ok(e)
}

/// True when the expression calls `success`, `failure`, `always` or `cancelled`.
pub fn has_status_call(e: &Expr) -> bool {
    match e {
        Expr::Call(n, args) => {
            matches!(n.to_ascii_lowercase().as_str(), "success" | "failure" | "always" | "cancelled") || args.iter().any(has_status_call)
        }
        Expr::Prop(a, _) | Expr::Filter(a) | Expr::Not(a) => has_status_call(a),
        Expr::Index(a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => has_status_call(a) || has_status_call(b),
        Expr::Lit(_) | Expr::Ident(_) => false,
    }
}

// ---------------------------------------------------------------------------------------
// Values and coercion
// ---------------------------------------------------------------------------------------

fn num_value(n: f64) -> Value {
    if n.is_finite() {
        if n.fract() == 0.0 && n.abs() < 9.0e15 {
            return Value::from(n as i64);
        }
        if let Some(x) = serde_json::Number::from_f64(n) {
            return Value::Number(x);
        }
    }
    // NaN and infinities have no JSON form. Strings carry them; `to_number` reads them back.
    Value::String(if n.is_nan() {
        "NaN".into()
    } else if n > 0.0 {
        "Infinity".into()
    } else {
        "-Infinity".into()
    })
}

pub fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(*b),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => str_to_number(s),
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return match u64::from_str_radix(h, 16) {
            Ok(n) => {
                if neg {
                    -(n as f64)
                } else {
                    n as f64
                }
            }
            Err(_) => f64::NAN,
        };
    }
    // Rust accepts "inf", "nan" and "infinity"; GitHub does not.
    if body.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return f64::NAN;
    }
    match body.parse::<f64>() {
        Ok(n) => {
            if neg {
                -n
            } else {
                n
            }
        }
        Err(_) => f64::NAN,
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// How a value reads inside a string: `${{ }}` interpolation and `format()`.
pub fn to_string_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(_) => "Array".into(),
        Value::Object(_) => "Object".into(),
    }
}

pub fn format_number(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if f == 0.0 {
        "0".into()
    } else if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{}", f as i128)
    } else {
        format!("{f}")
    }
}

fn kind(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

/// Is this a number or one of the strings that stand in for NaN and the infinities?
fn is_special_num(v: &Value) -> bool {
    matches!(v, Value::String(s) if matches!(s.as_str(), "NaN" | "Infinity" | "-Infinity"))
}

pub fn loose_eq(a: &Value, b: &Value) -> bool {
    if kind(a) == kind(b) {
        return match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Number(_), Value::Number(_)) => to_number(a) == to_number(b),
            (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y) || x.to_lowercase() == y.to_lowercase(),
            (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => a == b,
            _ => false,
        };
    }
    // Arrays and objects equal nothing of another type.
    if matches!(a, Value::Array(_) | Value::Object(_)) || matches!(b, Value::Array(_) | Value::Object(_)) {
        return false;
    }
    let (x, y) = (to_number(a), to_number(b));
    x == y
}

fn compare(op: CmpOp, a: &Value, b: &Value) -> bool {
    match op {
        CmpOp::Eq => return loose_eq(a, b),
        CmpOp::Ne => return !loose_eq(a, b),
        _ => {}
    }
    if let (Value::String(x), Value::String(y)) = (a, b)
        && (!is_special_num(a) || !is_special_num(b))
    {
        let (x, y) = (x.to_lowercase(), y.to_lowercase());
        return match op {
            CmpOp::Lt => x < y,
            CmpOp::Le => x <= y,
            CmpOp::Gt => x > y,
            CmpOp::Ge => x >= y,
            _ => false,
        };
    }
    if matches!(a, Value::Array(_) | Value::Object(_)) || matches!(b, Value::Array(_) | Value::Object(_)) {
        return false;
    }
    let (x, y) = (to_number(a), to_number(b));
    match op {
        CmpOp::Lt => x < y,
        CmpOp::Le => x <= y,
        CmpOp::Gt => x > y,
        CmpOp::Ge => x >= y,
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------

/// A value, or the array an `a.*` filter produced (properties map over it).
enum Tmp {
    Val(Value),
    Filtered(Vec<Value>),
}

impl Tmp {
    fn into_value(self) -> Value {
        match self {
            Tmp::Val(v) => v,
            Tmp::Filtered(a) => Value::Array(a),
        }
    }
}

fn lookup(v: &Value, key: &str) -> Value {
    match v {
        Value::Object(m) => get_ci(m, key),
        Value::Array(a) => key.parse::<usize>().ok().and_then(|i| a.get(i)).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn get_ci(m: &Map<String, Value>, key: &str) -> Value {
    if let Some(v) = m.get(key) {
        return v.clone();
    }
    let lk = key.to_lowercase();
    m.iter().find(|(k, _)| k.to_lowercase() == lk).map(|(_, v)| v.clone()).unwrap_or(Value::Null)
}

fn index_value(base: &Value, idx: &Value) -> Value {
    match (base, idx) {
        (Value::Array(a), Value::Number(n)) => {
            n.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0).and_then(|f| a.get(f as usize)).cloned().unwrap_or(Value::Null)
        }
        (Value::Object(m), Value::String(s)) => get_ci(m, s),
        (Value::Object(m), other) => get_ci(m, &to_string_value(other)),
        (Value::Array(a), Value::String(s)) => s.parse::<usize>().ok().and_then(|i| a.get(i)).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

impl EvalContext<'_> {
    fn eval(&self, e: &Expr) -> Result<Tmp, ExprError> {
        Ok(Tmp::Val(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Ident(name) => self.contexts.get(&name.to_ascii_lowercase()).cloned().unwrap_or(Value::Null),
            Expr::Prop(base, name) => match self.eval(base)? {
                Tmp::Val(v) => lookup(&v, name),
                Tmp::Filtered(items) => return Ok(Tmp::Filtered(items.iter().map(|i| lookup(i, name)).collect())),
            },
            Expr::Index(base, idx) => {
                let i = self.eval(idx)?.into_value();
                match self.eval(base)? {
                    Tmp::Val(v) => index_value(&v, &i),
                    Tmp::Filtered(items) => return Ok(Tmp::Filtered(items.iter().map(|x| index_value(x, &i)).collect())),
                }
            }
            Expr::Filter(base) => {
                let v = self.eval(base)?.into_value();
                return Ok(Tmp::Filtered(match v {
                    Value::Array(a) => a,
                    Value::Object(m) => m.into_iter().map(|(_, v)| v).collect(),
                    _ => Vec::new(),
                }));
            }
            Expr::Not(a) => Value::Bool(!truthy(&self.eval(a)?.into_value())),
            Expr::Cmp(op, a, b) => {
                let (x, y) = (self.eval(a)?.into_value(), self.eval(b)?.into_value());
                Value::Bool(compare(*op, &x, &y))
            }
            Expr::And(a, b) => {
                let x = self.eval(a)?.into_value();
                if !truthy(&x) { x } else { self.eval(b)?.into_value() }
            }
            Expr::Or(a, b) => {
                let x = self.eval(a)?.into_value();
                if truthy(&x) { x } else { self.eval(b)?.into_value() }
            }
            Expr::Call(name, args) => self.call(name, args)?,
        }))
    }

    fn arg(&self, args: &[Expr], i: usize) -> Result<Value, ExprError> {
        Ok(self.eval(&args[i])?.into_value())
    }

    fn call(&self, name: &str, args: &[Expr]) -> Result<Value, ExprError> {
        let lname = name.to_ascii_lowercase();
        let want = |lo: usize, hi: usize| -> Result<(), ExprError> {
            if args.len() < lo || args.len() > hi {
                if lo == hi {
                    return err(format!("{name}() takes {lo} argument{}", if lo == 1 { "" } else { "s" }));
                }
                return err(format!("{name}() takes {lo} to {hi} arguments"));
            }
            Ok(())
        };
        Ok(match lname.as_str() {
            "__nan" => Value::String("NaN".into()),
            "__inf" => Value::String("Infinity".into()),
            "success" => {
                want(0, 0)?;
                Value::Bool(self.status == JobStatus::Success)
            }
            "failure" => {
                want(0, 0)?;
                Value::Bool(self.status == JobStatus::Failure)
            }
            "always" => {
                want(0, 0)?;
                Value::Bool(true)
            }
            "cancelled" => {
                want(0, 0)?;
                Value::Bool(self.status == JobStatus::Cancelled)
            }
            "contains" => {
                want(2, 2)?;
                let (hay, needle) = (self.arg(args, 0)?, self.arg(args, 1)?);
                match &hay {
                    Value::Array(items) => Value::Bool(items.iter().any(|i| loose_eq(i, &needle))),
                    other => Value::Bool(to_string_value(other).to_lowercase().contains(&to_string_value(&needle).to_lowercase())),
                }
            }
            "startswith" => {
                want(2, 2)?;
                let (a, b) = (self.arg(args, 0)?, self.arg(args, 1)?);
                match (&a, &b) {
                    (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => Value::Bool(false),
                    _ => Value::Bool(to_string_value(&a).to_lowercase().starts_with(&to_string_value(&b).to_lowercase())),
                }
            }
            "endswith" => {
                want(2, 2)?;
                let (a, b) = (self.arg(args, 0)?, self.arg(args, 1)?);
                match (&a, &b) {
                    (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => Value::Bool(false),
                    _ => Value::Bool(to_string_value(&a).to_lowercase().ends_with(&to_string_value(&b).to_lowercase())),
                }
            }
            "format" => {
                if args.is_empty() {
                    return err("format() needs a format string");
                }
                let f = to_string_value(&self.arg(args, 0)?);
                let mut vals = Vec::new();
                for i in 1..args.len() {
                    vals.push(to_string_value(&self.arg(args, i)?));
                }
                Value::String(format_string(&f, &vals)?)
            }
            "join" => {
                want(1, 2)?;
                let sep = if args.len() == 2 { to_string_value(&self.arg(args, 1)?) } else { ",".to_string() };
                match self.arg(args, 0)? {
                    Value::Array(items) => Value::String(items.iter().map(to_string_value).collect::<Vec<_>>().join(&sep)),
                    other => Value::String(to_string_value(&other)),
                }
            }
            "tojson" => {
                want(1, 1)?;
                Value::String(serde_json::to_string_pretty(&self.arg(args, 0)?).unwrap_or_default())
            }
            "fromjson" => {
                want(1, 1)?;
                let s = to_string_value(&self.arg(args, 0)?);
                match serde_json::from_str::<Value>(&s) {
                    Ok(v) => v,
                    Err(e) => return err(format!("fromJSON could not read its argument: {e}")),
                }
            }
            "hashfiles" => {
                if args.is_empty() {
                    return err("hashFiles() needs at least one pattern");
                }
                let Some(f) = self.hash_files else {
                    return err("hashFiles() is not available here");
                };
                let mut patterns = Vec::new();
                for i in 0..args.len() {
                    patterns.push(to_string_value(&self.arg(args, i)?));
                }
                Value::String(f(&patterns).map_err(|m| ExprError { message: format!("hashFiles failed: {m}") })?)
            }
            _ => return err(format!("unknown function {name}()")),
        })
    }
}

/// `format('{0} and {1}', a, b)`. `{{` and `}}` are literal braces.
fn format_string(f: &str, args: &[String]) -> Result<String, ExprError> {
    let chars: Vec<char> = f.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                if chars.get(i + 1) == Some(&'{') {
                    out.push('{');
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                let mut num = String::new();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    num.push(chars[j]);
                    j += 1;
                }
                if num.is_empty() || chars.get(j) != Some(&'}') {
                    return err("format(): a '{' must start {n} or be doubled as '{{'");
                }
                let n: usize = num.parse().map_err(|_| ExprError { message: "format(): the argument index is too large".into() })?;
                match args.get(n) {
                    Some(a) => out.push_str(a),
                    None => return err(format!("format(): {{{n}}} refers to an argument that was not given")),
                }
                i = j + 1;
            }
            '}' => {
                if chars.get(i + 1) == Some(&'}') {
                    out.push('}');
                    i += 2;
                } else {
                    return err("format(): a '}' must be doubled as '}}'");
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------------------

/// Evaluates one bare expression.
pub fn eval_expr(src: &str, ctx: &EvalContext<'_>) -> Result<Value, ExprError> {
    let e = parse(src)?;
    Ok(ctx.eval(&e)?.into_value())
}

/// A step's `if:`. Accepts the bare form and the `${{ }}` form. With no status function in it,
/// the condition means `success() && (condition)`.
pub fn eval_condition(src: &str, ctx: &EvalContext<'_>) -> Result<bool, ExprError> {
    let t = src.trim();
    if t.is_empty() {
        return Ok(ctx.status == JobStatus::Success);
    }
    let body = match unwrap_single(t) {
        Some(inner) => inner.to_string(),
        None => t.to_string(),
    };
    let e = parse(&body)?;
    let v = ctx.eval(&e)?.into_value();
    if has_status_call(&e) { Ok(truthy(&v)) } else { Ok(ctx.status == JobStatus::Success && truthy(&v)) }
}

/// `${{ expr }}` covering the whole string: returns the inner text.
fn unwrap_single(t: &str) -> Option<&str> {
    let rest = t.strip_prefix("${{")?;
    let (inner, end) = find_close(rest)?;
    if rest[end..].trim().is_empty() { Some(inner) } else { None }
}

/// Finds the `}}` that closes an expression, skipping over quoted strings. Returns the inner
/// text and the index just past the closing braces.
fn find_close(s: &str) -> Option<(&str, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                if in_str && b.get(i + 1) == Some(&b'\'') {
                    i += 1;
                } else {
                    in_str = !in_str;
                }
            }
            b'}' if !in_str && b.get(i + 1) == Some(&b'}') => return Some((&s[..i], i + 2)),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Replaces every `${{ expr }}` in a template with the expression's string value. A template
/// with no expression comes back unchanged.
pub fn interpolate(template: &str, ctx: &EvalContext<'_>) -> Result<String, ExprError> {
    if !template.contains("${{") {
        return Ok(template.to_string());
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let Some((inner, end)) = find_close(after) else {
            return err("an expression opened with '${{' is never closed with '}}'");
        };
        let v = eval_expr(inner, ctx)?;
        out.push_str(&to_string_value(&v));
        rest = &after[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Like `interpolate`, but when the whole template is one expression the typed value comes back.
pub fn eval_template(template: &str, ctx: &EvalContext<'_>) -> Result<Value, ExprError> {
    if let Some(inner) = unwrap_single(template.trim()) {
        return eval_expr(inner, ctx);
    }
    Ok(Value::String(interpolate(template, ctx)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> EvalContext<'static> {
        let mut c = EvalContext::new(JobStatus::Success);
        c.set(
            "github",
            json!({"event_name": "push", "ref": "refs/heads/main", "actor": "Alice", "event": {"commits": [{"message": "one", "id": "a"}, {"message": "two", "id": "b"}], "n": 3}}),
        );
        c.set("env", json!({"CI": "true", "EMPTY": ""}));
        c.set("matrix", json!({"os": "linux", "n": 2}));
        c.set("steps", json!({"v": {"outputs": {"ver": "1.2"}, "outcome": "success"}}));
        c.set("needs", json!({"lint": {"result": "success", "outputs": {"x": "1"}}}));
        c
    }

    fn ev(s: &str) -> Value {
        eval_expr(s, &ctx()).unwrap()
    }

    #[test]
    fn literals() {
        assert_eq!(ev("true"), json!(true));
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("42"), json!(42));
        assert_eq!(ev("-1.5"), json!(-1.5));
        assert_eq!(ev("0xff"), json!(255));
        assert_eq!(ev("1e3"), json!(1000));
        assert_eq!(ev("'it''s'"), json!("it's"));
    }

    #[test]
    fn property_access_is_case_insensitive() {
        assert_eq!(ev("github.event_name"), json!("push"));
        assert_eq!(ev("GITHUB.EVENT_NAME"), json!("push"));
        assert_eq!(ev("github['actor']"), json!("Alice"));
        assert_eq!(ev("github.nothing.deeper"), Value::Null);
        assert_eq!(ev("steps.v.outputs.ver"), json!("1.2"));
        assert_eq!(ev("needs.lint.outputs.x"), json!("1"));
        assert_eq!(ev("github.event.commits[1].id"), json!("b"));
        assert_eq!(ev("env.CI"), json!("true"));
    }

    #[test]
    fn filters() {
        assert_eq!(ev("github.event.commits.*.message"), json!(["one", "two"]));
        assert_eq!(ev("join(github.event.commits.*.id, '+')"), json!("a+b"));
        assert_eq!(ev("contains(github.event.commits.*.message, 'TWO')"), json!(true));
    }

    #[test]
    fn comparison_rules() {
        assert_eq!(ev("'Linux' == 'linux'"), json!(true));
        assert_eq!(ev("1 == '1'"), json!(true));
        assert_eq!(ev("null == 0"), json!(true));
        assert_eq!(ev("'' == 0"), json!(true));
        assert_eq!(ev("true == 1"), json!(true));
        assert_eq!(ev("'abc' == 0"), json!(false), "NaN equals nothing");
        assert_eq!(ev("'b' > 'A'"), json!(true));
        assert_eq!(ev("2 >= 2"), json!(true));
        assert_eq!(ev("matrix.n < 3"), json!(true));
        assert_eq!(ev("github.event.commits == github.event.commits"), json!(true));
        assert_eq!(ev("github.event.commits == 1"), json!(false));
        assert_eq!(ev("'a' != 'A'"), json!(false));
    }

    #[test]
    fn logical_operators_return_operands() {
        assert_eq!(ev("'' || 'fallback'"), json!("fallback"));
        assert_eq!(ev("'x' && 'y'"), json!("y"));
        assert_eq!(ev("0 && 'y'"), json!(0));
        assert_eq!(ev("null || false"), json!(false));
        assert_eq!(ev("!'a'"), json!(false));
        assert_eq!(ev("!''"), json!(true));
        assert_eq!(ev("true || nope()"), json!(true), "right side is not evaluated");
        assert_eq!(ev("env.EMPTY || 'dflt'"), json!("dflt"));
        assert_eq!(ev("matrix.os == 'linux' && 'yes' || 'no'"), json!("yes"));
    }

    #[test]
    fn precedence_and_parentheses() {
        assert_eq!(ev("true || false && false"), json!(true));
        assert_eq!(ev("(true || false) && false"), json!(false));
        assert_eq!(ev("!true == false"), json!(true));
        assert_eq!(ev("1 < 2 == true"), json!(true));
    }

    #[test]
    fn functions() {
        assert_eq!(ev("contains('Hello World', 'WORLD')"), json!(true));
        assert_eq!(ev("contains(fromJSON('[1,2,3]'), 2)"), json!(true));
        assert_eq!(ev("startsWith('refs/heads/main', 'refs/heads/')"), json!(true));
        assert_eq!(ev("endsWith('a.TXT', '.txt')"), json!(true));
        assert_eq!(ev("format('{0}-{1}-{0}', 'a', 2)"), json!("a-2-a"));
        assert_eq!(ev("format('{{literal}} {0}', 1)"), json!("{literal} 1"));
        assert_eq!(ev("join(fromJSON('[\"a\",\"b\"]'))"), json!("a,b"));
        assert_eq!(ev("fromJSON('{\"a\": {\"b\": 5}}').a.b"), json!(5));
        assert_eq!(ev("toJSON(fromJSON('[1]'))"), json!("[\n  1\n]"));
        assert_eq!(ev("CONTAINS('abc', 'b')"), json!(true), "function names are case-insensitive");
    }

    #[test]
    fn function_errors_are_clear() {
        let c = ctx();
        assert!(eval_expr("nope()", &c).unwrap_err().message.contains("unknown function"));
        assert!(eval_expr("contains('a')", &c).unwrap_err().message.contains("2 arguments"));
        assert!(eval_expr("format('{1}', 'a')", &c).is_err());
        assert!(eval_expr("format('}', 'a')", &c).is_err());
        assert!(eval_expr("fromJSON('{')", &c).is_err());
        assert!(eval_expr("hashFiles('x')", &c).unwrap_err().message.contains("not available"));
    }

    #[test]
    fn syntax_errors() {
        let c = ctx();
        for bad in ["", "a ==", "(1", "1)", "a b", "'open", "a = b", "a & b", "a | b", "foo(", "foo(1,", "a.", "a[", "@", "1 2"] {
            assert!(eval_expr(bad, &c).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn hash_files_callback() {
        let mut c = ctx();
        let f = |p: &[String]| -> Result<String, String> { Ok(format!("hash:{}", p.join("|"))) };
        c.hash_files = Some(&f);
        assert_eq!(eval_expr("hashFiles('**/Cargo.lock', 'x')", &c).unwrap(), json!("hash:**/Cargo.lock|x"));
        let bad = |_: &[String]| -> Result<String, String> { Err("boom".into()) };
        c.hash_files = Some(&bad);
        assert!(eval_expr("hashFiles('x')", &c).unwrap_err().message.contains("boom"));
    }

    #[test]
    fn status_functions() {
        let mut c = ctx();
        assert!(eval_condition("success()", &c).unwrap());
        assert!(!eval_condition("failure()", &c).unwrap());
        assert!(eval_condition("always()", &c).unwrap());
        c.status = JobStatus::Failure;
        assert!(!eval_condition("success()", &c).unwrap());
        assert!(eval_condition("failure()", &c).unwrap());
        assert!(eval_condition("always()", &c).unwrap());
        assert!(!eval_condition("cancelled()", &c).unwrap());
        c.status = JobStatus::Cancelled;
        assert!(eval_condition("cancelled()", &c).unwrap());
        assert!(!eval_condition("failure()", &c).unwrap());
    }

    #[test]
    fn if_without_a_status_function_means_success_and() {
        let mut c = ctx();
        assert!(eval_condition("github.event_name == 'push'", &c).unwrap());
        assert!(!eval_condition("github.event_name == 'pull_request'", &c).unwrap());
        assert!(eval_condition("${{ github.event_name == 'push' }}", &c).unwrap());
        assert!(eval_condition("", &c).unwrap(), "no condition means success()");
        c.status = JobStatus::Failure;
        assert!(!eval_condition("github.event_name == 'push'", &c).unwrap(), "a failed job skips plain conditions");
        assert!(eval_condition("always() && github.event_name == 'push'", &c).unwrap());
        assert!(eval_condition("failure() && true", &c).unwrap());
        assert!(eval_condition("${{ always() }}", &c).unwrap());
        assert!(!eval_condition("", &c).unwrap());
    }

    #[test]
    fn interpolation() {
        let c = ctx();
        assert_eq!(interpolate("plain", &c).unwrap(), "plain");
        assert_eq!(interpolate("v${{ steps.v.outputs.ver }}-${{matrix.os}}", &c).unwrap(), "v1.2-linux");
        assert_eq!(interpolate("${{ 'a}}b' }}!", &c).unwrap(), "a}}b!", "braces inside a string do not close the expression");
        assert_eq!(interpolate("[${{ github.nothing }}]", &c).unwrap(), "[]");
        assert_eq!(interpolate("${{ true }} ${{ 1.5 }} ${{ 3 }}", &c).unwrap(), "true 1.5 3");
        assert_eq!(interpolate("${{ github.event.commits }}", &c).unwrap(), "Array");
        assert!(interpolate("${{ oops", &c).is_err());
        assert!(interpolate("${{ nope() }}", &c).is_err());
    }

    #[test]
    fn typed_templates() {
        let c = ctx();
        assert_eq!(eval_template("${{ github.event.n }}", &c).unwrap(), json!(3));
        assert_eq!(eval_template("n=${{ github.event.n }}", &c).unwrap(), json!("n=3"));
        assert_eq!(eval_template("literal", &c).unwrap(), json!("literal"));
    }

    #[test]
    fn numbers_format_like_github() {
        assert_eq!(format_number(1.0), "1");
        assert_eq!(format_number(-0.0), "0");
        assert_eq!(format_number(0.1), "0.1");
        assert_eq!(format_number(1e21), "1000000000000000000000");
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(str_to_number(" 12 "), 12.0);
        assert!(str_to_number("12abc").is_nan());
        assert!(str_to_number("inf").is_nan());
        assert_eq!(str_to_number("0x10"), 16.0);
    }

    #[test]
    fn hostile_input_is_rejected_not_crashed() {
        let c = ctx();
        let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert!(eval_expr(&deep, &c).is_err());
        let nots = format!("{}true", "!".repeat(5000));
        assert!(eval_expr(&nots, &c).is_err());
        let long = format!("'{}'", "a".repeat(MAX_EXPR_LEN + 1));
        assert!(eval_expr(&long, &c).is_err());
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn never_panics_on_arbitrary_text(s in "\\PC{0,80}") {
                let c = ctx();
                let _ = eval_expr(&s, &c);
                let _ = interpolate(&s, &c);
                let _ = eval_condition(&s, &c);
            }

            #[test]
            fn never_panics_on_expression_shaped_text(s in "[a-z0-9_.'\\[\\]()!=<>&| ,*{}$-]{0,60}") {
                let c = ctx();
                let _ = eval_expr(&s, &c);
                let _ = interpolate(&format!("${{{{ {s} }}}}"), &c);
            }

            #[test]
            fn string_literals_round_trip(s in "[ -~]{0,40}") {
                let c = ctx();
                let lit = format!("'{}'", s.replace('\'', "''"));
                prop_assert_eq!(eval_expr(&lit, &c).unwrap(), Value::String(s));
            }

            #[test]
            fn text_without_expressions_is_unchanged(s in "[ -~&&[^$]]{0,60}") {
                let c = ctx();
                prop_assert_eq!(interpolate(&s, &c).unwrap(), s);
            }

            #[test]
            fn equality_is_symmetric(a in "[ -~]{0,8}", b in "[ -~]{0,8}") {
                let c = ctx();
                let (la, lb) = (format!("'{}'", a.replace('\'', "''")), format!("'{}'", b.replace('\'', "''")));
                let ab = eval_expr(&format!("{la} == {lb}"), &c).unwrap();
                let ba = eval_expr(&format!("{lb} == {la}"), &c).unwrap();
                prop_assert_eq!(ab, ba);
            }

            #[test]
            fn integers_survive_a_round_trip(n in -1_000_000i64..1_000_000) {
                let c = ctx();
                prop_assert_eq!(eval_expr(&format!("fromJSON(format('{{0}}', {n}))"), &c).unwrap(), json!(n));
            }

            #[test]
            fn double_negation_is_truthiness(s in "[ -~]{0,8}") {
                let c = ctx();
                let lit = format!("'{}'", s.replace('\'', "''"));
                prop_assert_eq!(eval_expr(&format!("!!{lit}"), &c).unwrap(), Value::Bool(!s.is_empty()));
            }
        }
    }
}
