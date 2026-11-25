//! The `${{ }}` expression engine.
//!
//! Follows the documented semantics of GitHub Actions expressions: literals, property access,
//! indexing, `.*` filters, `!`, comparisons, `&&`, `||`, parentheses and the built-in
//! functions. Comparison coerces like Actions does (strings compare ignoring case, mixed types
//! compare as numbers), `&&` and `||` return an operand rather than a boolean, and unknown
//! properties are `null` rather than errors. An unknown top-level name is an error, because
//! that is nearly always a typo.
//!
//! Pure: no IO, no clock. `hashFiles` is supplied by the caller (only the runner has the files).
//! The parser has a depth limit and a length limit so hostile input cannot overflow the stack.

use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Longest expression accepted, in bytes. The same limit Actions uses.
pub const MAX_EXPR_LEN: usize = 21_000;
/// Deepest nesting of parentheses, calls and indexes.
pub const MAX_DEPTH: usize = 50;

/// The names that may start an expression, besides function calls.
pub const KNOWN_CONTEXTS: &[&str] =
    &["github", "codehaven", "env", "vars", "secrets", "inputs", "matrix", "strategy", "needs", "steps", "job", "runner"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ExprError {
    pub message: String,
    /// Byte offset in the expression source, when known.
    pub offset: Option<usize>,
}

impl ExprError {
    fn at(message: impl Into<String>, offset: usize) -> Self {
        Self { message: message.into(), offset: Some(offset) }
    }
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), offset: None }
    }
}

/// A value in an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// Falsy: `false`, `0`, `-0`, `NaN`, `""` and `null`. Everything else is truthy,
    /// including empty arrays and objects.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => !(*n == 0.0 || n.is_nan()),
            Value::String(s) => !s.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        }
    }

    /// How the value reads when it is placed inside a string.
    pub fn to_display(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_to_string(*n),
            Value::String(s) => s.clone(),
            Value::Array(_) => "Array".into(),
            Value::Object(_) => "Object".into(),
        }
    }

    fn to_number(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Number(n) => *n,
            Value::String(s) => parse_number(s),
            Value::Array(_) | Value::Object(_) => f64::NAN,
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => {
                if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
                    Json::from(*n as i64)
                } else {
                    serde_json::Number::from_f64(*n).map(Json::Number).unwrap_or(Json::Null)
                }
            }
            Value::String(s) => Json::String(s.clone()),
            Value::Array(a) => Json::Array(a.iter().map(Value::to_json).collect()),
            Value::Object(o) => Json::Object(o.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }

    pub fn from_json(j: &Json) -> Value {
        match j {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(a) => Value::Array(a.iter().map(Value::from_json).collect()),
            Json::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), Value::from_json(v))).collect()),
        }
    }

    /// Property lookup: exact name first, then ignoring case, as Actions does.
    fn property(&self, name: &str) -> Value {
        match self {
            Value::Object(o) => {
                if let Some(v) = o.get(name) {
                    return v.clone();
                }
                o.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).unwrap_or(Value::Null)
            }
            _ => Value::Null,
        }
    }
}

impl From<Json> for Value {
    fn from(j: Json) -> Self {
        Value::from_json(&j)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.into())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn parse_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust accepts "inf" and "nan" spellings that Actions does not.
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

// ---------------------------------------------------------------------------- AST

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A top-level name such as `github` or `matrix`.
    Name(String),
    Prop(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    /// `.*`
    Star(Box<Expr>),
    Not(Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

// ---------------------------------------------------------------------------- lexer

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

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ExprError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'(' => {
                out.push((Tok::LParen, start));
                i += 1;
            }
            b')' => {
                out.push((Tok::RParen, start));
                i += 1;
            }
            b'[' => {
                out.push((Tok::LBracket, start));
                i += 1;
            }
            b']' => {
                out.push((Tok::RBracket, start));
                i += 1;
            }
            b',' => {
                out.push((Tok::Comma, start));
                i += 1;
            }
            b'*' => {
                out.push((Tok::Star, start));
                i += 1;
            }
            b'.' => {
                // A leading-dot number such as `.5` is not valid in Actions, so `.` is always a dot.
                out.push((Tok::Dot, start));
                i += 1;
            }
            b'!' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push((Tok::Ne, start));
                    i += 2;
                } else {
                    out.push((Tok::Not, start));
                    i += 1;
                }
            }
            b'=' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push((Tok::Eq, start));
                    i += 2;
                } else {
                    return Err(ExprError::at("Unexpected symbol '='. Did you mean '=='?", start));
                }
            }
            b'<' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push((Tok::Le, start));
                    i += 2;
                } else {
                    out.push((Tok::Lt, start));
                    i += 1;
                }
            }
            b'>' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push((Tok::Ge, start));
                    i += 2;
                } else {
                    out.push((Tok::Gt, start));
                    i += 1;
                }
            }
            b'&' => {
                if b.get(i + 1) == Some(&b'&') {
                    out.push((Tok::And, start));
                    i += 2;
                } else {
                    return Err(ExprError::at("Unexpected symbol '&'. Did you mean '&&'?", start));
                }
            }
            b'|' => {
                if b.get(i + 1) == Some(&b'|') {
                    out.push((Tok::Or, start));
                    i += 2;
                } else {
                    return Err(ExprError::at("Unexpected symbol '|'. Did you mean '||'?", start));
                }
            }
            b'\'' => {
                let mut s = Vec::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => return Err(ExprError::at("Unterminated string", start)),
                        Some(b'\'') => {
                            if b.get(i + 1) == Some(&b'\'') {
                                s.push(b'\'');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                let text = String::from_utf8(s).map_err(|_| ExprError::at("Invalid text in string", start))?;
                out.push((Tok::Str(text), start));
            }
            b'0'..=b'9' | b'-' | b'+' => {
                // A sign belongs to a number only when a digit follows; Actions has no unary minus.
                let mut j = i;
                if b[j] == b'-' || b[j] == b'+' {
                    j += 1;
                }
                if !b.get(j).is_some_and(u8::is_ascii_digit) {
                    return Err(ExprError::at(format!("Unexpected symbol '{}'", c as char), start));
                }
                if b[j] == b'0' && matches!(b.get(j + 1), Some(b'x' | b'X')) {
                    let mut k = j + 2;
                    while b.get(k).is_some_and(u8::is_ascii_hexdigit) {
                        k += 1;
                    }
                    let digits = &src[j + 2..k];
                    let v = i64::from_str_radix(digits, 16).map_err(|_| ExprError::at("Invalid hex number", start))? as f64;
                    out.push((Tok::Num(if b[i] == b'-' { -v } else { v }), start));
                    i = k;
                    continue;
                }
                while b.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                    while b.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                }
                if matches!(b.get(j), Some(b'e' | b'E')) {
                    let mut k = j + 1;
                    if matches!(b.get(k), Some(b'+' | b'-')) {
                        k += 1;
                    }
                    if b.get(k).is_some_and(u8::is_ascii_digit) {
                        while b.get(k).is_some_and(u8::is_ascii_digit) {
                            k += 1;
                        }
                        j = k;
                    }
                }
                let v: f64 = src[i..j].parse().map_err(|_| ExprError::at("Invalid number", start))?;
                out.push((Tok::Num(v), start));
                i = j;
            }
            c if c == b'_' || c.is_ascii_alphabetic() => {
                let mut j = i;
                while b.get(j).is_some_and(|&ch| ch == b'_' || ch == b'-' || ch.is_ascii_alphanumeric()) {
                    j += 1;
                }
                out.push((Tok::Ident(src[i..j].to_string()), start));
                i = j;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap_or('?');
                return Err(ExprError::at(format!("Unexpected symbol '{ch}'"), start));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------- parser

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }
    fn offset(&self) -> usize {
        self.toks.get(self.pos).map(|t| t.1).unwrap_or(self.end)
    }
    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.0.clone());
        self.pos += 1;
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
            return Err(ExprError::at("Expression is nested too deeply", self.offset()));
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
                _ => return Ok(l),
            };
            self.pos += 1;
            let r = self.relational(depth)?;
            l = Expr::Cmp(op, Box::new(l), Box::new(r));
        }
    }

    fn relational(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let mut l = self.unary(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => CmpOp::Lt,
                Some(Tok::Le) => CmpOp::Le,
                Some(Tok::Gt) => CmpOp::Gt,
                Some(Tok::Ge) => CmpOp::Ge,
                _ => return Ok(l),
            };
            self.pos += 1;
            let r = self.unary(depth)?;
            l = Expr::Cmp(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self, depth: usize) -> Result<Expr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::at("Expression is nested too deeply", self.offset()));
        }
        if self.eat(&Tok::Not) {
            let e = self.unary(depth + 1)?;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.postfix(depth)
    }

    fn postfix(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let mut e = self.primary(depth)?;
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    match self.bump() {
                        Some(Tok::Ident(name)) => e = Expr::Prop(Box::new(e), name),
                        Some(Tok::Star) => e = Expr::Star(Box::new(e)),
                        // `github.event.0` style numeric segments are not supported.
                        _ => return Err(ExprError::at("Expected a property name after '.'", self.offset())),
                    }
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    let idx = self.or(depth + 1)?;
                    if !self.eat(&Tok::RBracket) {
                        return Err(ExprError::at("Expected ']'", self.offset()));
                    }
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self, depth: usize) -> Result<Expr, ExprError> {
        let off = self.offset();
        match self.bump() {
            None => Err(ExprError::at("Unexpected end of expression", self.end)),
            Some(Tok::Num(n)) => Ok(Expr::Literal(Value::Number(n))),
            Some(Tok::Str(s)) => Ok(Expr::Literal(Value::String(s))),
            Some(Tok::LParen) => {
                let e = self.or(depth + 1)?;
                if !self.eat(&Tok::RParen) {
                    return Err(ExprError::at("Expected ')'", self.offset()));
                }
                Ok(e)
            }
            Some(Tok::Ident(name)) => match name.as_str() {
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                "null" => Ok(Expr::Literal(Value::Null)),
                "NaN" => Ok(Expr::Literal(Value::Number(f64::NAN))),
                "Infinity" => Ok(Expr::Literal(Value::Number(f64::INFINITY))),
                _ => {
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
                                return Err(ExprError::at("Expected ',' or ')' in the argument list", self.offset()));
                            }
                        }
                        check_call(&name, args.len(), off)?;
                        Ok(Expr::Call(name, args))
                    } else {
                        Ok(Expr::Name(name))
                    }
                }
            },
            Some(t) => Err(ExprError::at(format!("Unexpected symbol: {}", tok_name(&t)), off)),
        }
    }
}

fn tok_name(t: &Tok) -> &'static str {
    match t {
        Tok::RParen => "')'",
        Tok::RBracket => "']'",
        Tok::Comma => "','",
        Tok::Dot => "'.'",
        Tok::Star => "'*'",
        Tok::Eq => "'=='",
        Tok::Ne => "'!='",
        Tok::Lt => "'<'",
        Tok::Le => "'<='",
        Tok::Gt => "'>'",
        Tok::Ge => "'>='",
        Tok::And => "'&&'",
        Tok::Or => "'||'",
        _ => "token",
    }
}

/// Names and argument counts of the built-in functions.
fn check_call(name: &str, n: usize, off: usize) -> Result<(), ExprError> {
    let (min, max) = match name.to_ascii_lowercase().as_str() {
        "contains" | "startswith" | "endswith" => (2, 2),
        "format" => (1, usize::MAX),
        "join" => (1, 2),
        "tojson" | "fromjson" => (1, 1),
        "hashfiles" => (1, usize::MAX),
        "success" | "failure" | "always" | "cancelled" => (0, 0),
        _ => return Err(ExprError::at(format!("Unrecognized function: '{name}'"), off)),
    };
    if n < min || n > max {
        return Err(ExprError::at(format!("Wrong number of arguments for '{name}'"), off));
    }
    Ok(())
}

/// Parses one expression, the text between `${{` and `}}`.
pub fn parse_expr(src: &str) -> Result<Expr, ExprError> {
    if src.len() > MAX_EXPR_LEN {
        return Err(ExprError::new("Expression is too long"));
    }
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(ExprError::at("Empty expression", 0));
    }
    let mut p = Parser { toks, pos: 0, end: src.len() };
    let e = p.or(0)?;
    if p.pos < p.toks.len() {
        return Err(ExprError::at("Unexpected symbol after the end of the expression", p.offset()));
    }
    Ok(e)
}

// ---------------------------------------------------------------------------- evaluation

/// Where the job or step stands, for `success()`, `failure()` and `cancelled()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Success,
    Failure,
    Cancelled,
}

type HashFiles<'a> = &'a (dyn Fn(&[String]) -> Result<String, String> + Sync);

/// What an expression can see.
#[derive(Default, Clone)]
pub struct Context<'a> {
    names: BTreeMap<String, Value>,
    pub status: Status,
    hash_files: Option<HashFiles<'a>>,
}

impl<'a> Context<'a> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.names.insert(name.to_string(), value.into());
        self
    }
    pub fn set(&mut self, name: &str, value: impl Into<Value>) {
        self.names.insert(name.to_string(), value.into());
    }
    pub fn with_status(mut self, s: Status) -> Self {
        self.status = s;
        self
    }
    pub fn with_hash_files(mut self, f: HashFiles<'a>) -> Self {
        self.hash_files = Some(f);
        self
    }
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.names.get(name).or_else(|| self.names.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
    }
}

/// True when the expression is, or reads through, a `.*` filter.
fn starred(e: &Expr) -> bool {
    match e {
        Expr::Star(_) => true,
        Expr::Prop(b, _) | Expr::Index(b, _) => starred(b),
        _ => false,
    }
}

impl Expr {
    pub fn eval(&self, ctx: &Context<'_>) -> Result<Value, ExprError> {
        match self {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Name(n) => {
                // `codehaven` is the same data as `github`.
                match ctx.get(n) {
                    Some(v) => Ok(v.clone()),
                    None if n.eq_ignore_ascii_case("codehaven") => Ok(ctx.get("github").cloned().unwrap_or(Value::Null)),
                    None => Err(ExprError::new(format!("Unrecognized named-value: '{n}'"))),
                }
            }
            Expr::Prop(base, name) => {
                let b = base.eval(ctx)?;
                if starred(base)
                    && let Value::Array(items) = &b
                {
                    // After a `.*` filter a property is read from every element.
                    return Ok(Value::Array(items.iter().map(|v| v.property(name)).filter(|v| *v != Value::Null).collect()));
                }
                Ok(b.property(name))
            }
            Expr::Index(base, idx) => {
                let b = base.eval(ctx)?;
                let i = idx.eval(ctx)?;
                if starred(base)
                    && let (Value::Array(items), Value::String(key)) = (&b, &i)
                {
                    return Ok(Value::Array(items.iter().map(|v| v.property(key)).filter(|v| *v != Value::Null).collect()));
                }
                Ok(match (&b, &i) {
                    (Value::Array(a), Value::Number(n)) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => {
                        a.get(*n as usize).cloned().unwrap_or(Value::Null)
                    }
                    (Value::Array(a), Value::String(s)) => match s.parse::<usize>() {
                        Ok(n) => a.get(n).cloned().unwrap_or(Value::Null),
                        Err(_) => Value::Null,
                    },
                    (Value::Object(_), Value::String(s)) => b.property(s),
                    (Value::Object(_), Value::Number(n)) => b.property(&number_to_string(*n)),
                    _ => Value::Null,
                })
            }
            Expr::Star(base) => {
                // `a.*.b` collects `b` from every element: the filter yields the elements, and a
                // following property is mapped over them.
                let b = base.eval(ctx)?;
                Ok(match b {
                    Value::Array(a) => Value::Array(a),
                    Value::Object(o) => Value::Array(o.into_values().collect()),
                    _ => Value::Array(Vec::new()),
                })
            }
            Expr::Not(e) => Ok(Value::Bool(!e.eval(ctx)?.truthy())),
            Expr::Cmp(op, l, r) => {
                let a = l.eval(ctx)?;
                let b = r.eval(ctx)?;
                Ok(Value::Bool(compare(*op, &a, &b)))
            }
            Expr::And(l, r) => {
                let a = l.eval(ctx)?;
                if !a.truthy() { Ok(a) } else { r.eval(ctx) }
            }
            Expr::Or(l, r) => {
                let a = l.eval(ctx)?;
                if a.truthy() { Ok(a) } else { r.eval(ctx) }
            }
            Expr::Call(name, args) => call(name, args, ctx),
        }
    }

    /// Every context name and first property the expression reads, for finding which secrets
    /// and variables a job needs. `toJSON(secrets)` and `secrets[x]` count as "all of them".
    pub fn collect_refs(&self, refs: &mut Refs) {
        match self {
            Expr::Literal(_) => {}
            Expr::Name(n) => {
                refs.names.insert(n.to_ascii_lowercase());
                match n.to_ascii_lowercase().as_str() {
                    "secrets" => refs.all_secrets = true,
                    "vars" => refs.all_vars = true,
                    _ => {}
                }
            }
            Expr::Prop(base, name) => {
                if let Expr::Name(n) = &**base {
                    let n = n.to_ascii_lowercase();
                    refs.names.insert(n.clone());
                    match n.as_str() {
                        "secrets" => {
                            refs.secrets.insert(name.clone());
                        }
                        "vars" => {
                            refs.vars.insert(name.clone());
                        }
                        "needs" => {
                            refs.needs.insert(name.clone());
                        }
                        _ => {}
                    }
                } else {
                    base.collect_refs(refs);
                }
            }
            Expr::Index(base, idx) => {
                if let (Expr::Name(n), Expr::Literal(Value::String(s))) = (&**base, &**idx) {
                    let n = n.to_ascii_lowercase();
                    refs.names.insert(n.clone());
                    match n.as_str() {
                        "secrets" => {
                            refs.secrets.insert(s.clone());
                        }
                        "vars" => {
                            refs.vars.insert(s.clone());
                        }
                        "needs" => {
                            refs.needs.insert(s.clone());
                        }
                        _ => {}
                    }
                } else {
                    base.collect_refs(refs);
                    idx.collect_refs(refs);
                }
            }
            Expr::Star(e) | Expr::Not(e) => e.collect_refs(refs),
            Expr::Cmp(_, l, r) | Expr::And(l, r) | Expr::Or(l, r) => {
                l.collect_refs(refs);
                r.collect_refs(refs);
            }
            Expr::Call(name, args) => {
                if name.eq_ignore_ascii_case("hashfiles") {
                    refs.uses_hash_files = true;
                }
                for a in args {
                    a.collect_refs(refs);
                }
            }
        }
    }
}

/// What a set of expressions reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub names: BTreeSet<String>,
    pub secrets: BTreeSet<String>,
    pub vars: BTreeSet<String>,
    pub needs: BTreeSet<String>,
    /// The whole `secrets` or `vars` object is read, so every one is needed.
    pub all_secrets: bool,
    pub all_vars: bool,
    pub uses_hash_files: bool,
}

impl Refs {
    pub fn merge(&mut self, other: &Refs) {
        self.names.extend(other.names.iter().cloned());
        self.secrets.extend(other.secrets.iter().cloned());
        self.vars.extend(other.vars.iter().cloned());
        self.needs.extend(other.needs.iter().cloned());
        self.all_secrets |= other.all_secrets;
        self.all_vars |= other.all_vars;
        self.uses_hash_files |= other.uses_hash_files;
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::String(x), Value::String(y)) => x.to_lowercase() == y.to_lowercase(),
        // Arrays and objects are never equal, even to themselves, as in Actions.
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => false,
        _ => {
            let (x, y) = (a.to_number(), b.to_number());
            x == y
        }
    }
}

fn compare(op: CmpOp, a: &Value, b: &Value) -> bool {
    match op {
        CmpOp::Eq => loose_eq(a, b),
        CmpOp::Ne => !loose_eq(a, b),
        _ => {
            let ord = match (a, b) {
                (Value::String(x), Value::String(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
                (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => None,
                _ => a.to_number().partial_cmp(&b.to_number()),
            };
            match (op, ord) {
                (_, None) => false,
                (CmpOp::Lt, Some(o)) => o.is_lt(),
                (CmpOp::Le, Some(o)) => o.is_le(),
                (CmpOp::Gt, Some(o)) => o.is_gt(),
                (CmpOp::Ge, Some(o)) => o.is_ge(),
                _ => false,
            }
        }
    }
}

fn call(name: &str, args: &[Expr], ctx: &Context<'_>) -> Result<Value, ExprError> {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        "success" => return Ok(Value::Bool(ctx.status == Status::Success)),
        "failure" => return Ok(Value::Bool(ctx.status == Status::Failure)),
        "cancelled" => return Ok(Value::Bool(ctx.status == Status::Cancelled)),
        "always" => return Ok(Value::Bool(true)),
        _ => {}
    }
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(a.eval(ctx)?);
    }
    match lname.as_str() {
        "contains" => {
            let (search, item) = (&vals[0], &vals[1]);
            Ok(Value::Bool(match search {
                Value::Array(a) => a.iter().any(|v| loose_eq(v, item)),
                Value::Object(_) => false,
                other => {
                    let hay = other.to_display().to_lowercase();
                    hay.contains(&item.to_display().to_lowercase())
                }
            }))
        }
        "startswith" => {
            let (a, b) = (&vals[0], &vals[1]);
            Ok(Value::Bool(match (a, b) {
                (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => false,
                _ => a.to_display().to_lowercase().starts_with(&b.to_display().to_lowercase()),
            }))
        }
        "endswith" => {
            let (a, b) = (&vals[0], &vals[1]);
            Ok(Value::Bool(match (a, b) {
                (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => false,
                _ => a.to_display().to_lowercase().ends_with(&b.to_display().to_lowercase()),
            }))
        }
        "format" => {
            let Value::String(fmt) = &vals[0] else {
                return Ok(Value::String(vals[0].to_display()));
            };
            format_string(fmt, &vals[1..]).map(Value::String)
        }
        "join" => {
            let sep = vals.get(1).map(Value::to_display).unwrap_or_else(|| ",".into());
            Ok(Value::String(match &vals[0] {
                Value::Array(a) => a.iter().map(Value::to_display).collect::<Vec<_>>().join(&sep),
                Value::Object(_) => String::new(),
                other => other.to_display(),
            }))
        }
        "tojson" => Ok(Value::String(serde_json::to_string_pretty(&vals[0].to_json()).map_err(|e| ExprError::new(e.to_string()))?)),
        "fromjson" => {
            let text = vals[0].to_display();
            serde_json::from_str::<Json>(&text)
                .map(|j| Value::from_json(&j))
                .map_err(|e| ExprError::new(format!("Invalid JSON given to fromJSON: {e}")))
        }
        "hashfiles" => {
            let Some(f) = ctx.hash_files else {
                return Err(ExprError::new("hashFiles() is only available while a job is running"));
            };
            let patterns: Vec<String> = vals.iter().map(Value::to_display).collect();
            f(&patterns).map(Value::String).map_err(ExprError::new)
        }
        _ => Err(ExprError::new(format!("Unrecognized function: '{name}'"))),
    }
}

fn format_string(fmt: &str, args: &[Value]) -> Result<String, ExprError> {
    let mut out = String::new();
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' => {
                if it.peek() == Some(&'{') {
                    it.next();
                    out.push('{');
                    continue;
                }
                let mut digits = String::new();
                loop {
                    match it.next() {
                        Some('}') => break,
                        Some(d) if d.is_ascii_digit() => digits.push(d),
                        _ => return Err(ExprError::new(format!("The format string '{fmt}' is invalid"))),
                    }
                }
                let n: usize = digits.parse().map_err(|_| ExprError::new(format!("The format string '{fmt}' is invalid")))?;
                let Some(v) = args.get(n) else {
                    return Err(ExprError::new(format!("The format string '{fmt}' references argument {n}, which was not given")));
                };
                out.push_str(&v.to_display());
            }
            '}' => {
                if it.peek() == Some(&'}') {
                    it.next();
                    out.push('}');
                } else {
                    return Err(ExprError::new(format!("The format string '{fmt}' is invalid")));
                }
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------- templates

/// A piece of a string that may hold `${{ }}` segments.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Text(String),
    /// The expression source with the delimiters removed.
    Expr(String),
}

/// Splits a string into text and `${{ }}` segments. A `}}` inside a quoted string does not end
/// the segment. An unterminated `${{` is an error.
pub fn segments(s: &str) -> Result<Vec<Segment>, ExprError> {
    let mut out = Vec::new();
    let mut rest = s;
    let mut consumed = 0;
    while let Some(i) = rest.find("${{") {
        if i > 0 {
            out.push(Segment::Text(rest[..i].to_string()));
        }
        let body_start = i + 3;
        let body = &rest[body_start..];
        let mut j = 0;
        let bytes = body.as_bytes();
        let mut in_str = false;
        let mut end = None;
        while j < bytes.len() {
            match bytes[j] {
                b'\'' => {
                    if in_str && bytes.get(j + 1) == Some(&b'\'') {
                        j += 1;
                    } else {
                        in_str = !in_str;
                    }
                }
                b'}' if !in_str && bytes.get(j + 1) == Some(&b'}') => {
                    end = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let Some(end) = end else {
            return Err(ExprError::at("Unterminated expression: missing '}}'", consumed + i));
        };
        out.push(Segment::Expr(body[..end].trim().to_string()));
        let adv = body_start + end + 2;
        consumed += adv;
        rest = &rest[adv..];
    }
    if !rest.is_empty() {
        out.push(Segment::Text(rest.to_string()));
    }
    Ok(out)
}

/// True when the string has any `${{ }}` in it.
pub fn has_expression(s: &str) -> bool {
    s.contains("${{")
}

/// Checks every expression in a string parses, without evaluating anything.
pub fn check_template(s: &str) -> Result<(), ExprError> {
    check_template_with(s, &[])
}

/// `check_template`, also accepting the context names in `extra` (the `jobs` context of a
/// reusable workflow's outputs).
pub fn check_template_with(s: &str, extra: &[&str]) -> Result<(), ExprError> {
    for seg in segments(s)? {
        if let Segment::Expr(src) = seg {
            let e = parse_expr(&src)?;
            check_names_with(&e, extra)?;
        }
    }
    Ok(())
}

/// Parses a bare expression (no `${{ }}`), as written in an `if:`, and checks its names.
pub fn check_expr_source(src: &str) -> Result<(), ExprError> {
    check_names(&parse_expr(src)?)
}

fn check_names(e: &Expr) -> Result<(), ExprError> {
    check_names_with(e, &[])
}

fn check_names_with(e: &Expr, extra: &[&str]) -> Result<(), ExprError> {
    match e {
        Expr::Name(n) => {
            if KNOWN_CONTEXTS.iter().chain(extra.iter()).any(|k| k.eq_ignore_ascii_case(n)) {
                Ok(())
            } else {
                Err(ExprError::new(format!("Unrecognized named-value: '{n}'")))
            }
        }
        Expr::Literal(_) => Ok(()),
        Expr::Prop(b, _) | Expr::Star(b) | Expr::Not(b) => check_names_with(b, extra),
        Expr::Index(a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            check_names_with(a, extra)?;
            check_names_with(b, extra)
        }
        Expr::Call(_, args) => args.iter().try_for_each(|a| check_names_with(a, extra)),
    }
}

/// Every reference inside a string's `${{ }}` segments. Unparseable segments are skipped.
pub fn template_refs(s: &str) -> Refs {
    let mut refs = Refs::default();
    if let Ok(segs) = segments(s) {
        for seg in segs {
            if let Segment::Expr(src) = seg
                && let Ok(e) = parse_expr(&src)
            {
                e.collect_refs(&mut refs);
            }
        }
    }
    refs
}

/// Evaluates a string that may hold `${{ }}` segments, giving text.
pub fn interpolate(s: &str, ctx: &Context<'_>) -> Result<String, ExprError> {
    if !has_expression(s) {
        return Ok(s.to_string());
    }
    let mut out = String::new();
    for seg in segments(s)? {
        match seg {
            Segment::Text(t) => out.push_str(&t),
            Segment::Expr(src) => out.push_str(&parse_expr(&src)?.eval(ctx)?.to_display()),
        }
    }
    Ok(out)
}

/// Like `interpolate`, but a string that is exactly one `${{ }}` keeps the value's type, so
/// `${{ fromJSON(...) }}` stays an array and `${{ true }}` stays a boolean.
pub fn eval_typed(s: &str, ctx: &Context<'_>) -> Result<Value, ExprError> {
    let segs = segments(s)?;
    if let [Segment::Expr(src)] = segs.as_slice() {
        return parse_expr(src)?.eval(ctx);
    }
    interpolate(s, ctx).map(Value::String)
}

/// Evaluates an `if:` condition. The `${{ }}` wrapper is optional. An empty condition is
/// `success()`, the default for jobs and steps.
pub fn eval_condition(cond: Option<&str>, ctx: &Context<'_>) -> Result<bool, ExprError> {
    let Some(c) = cond.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(ctx.status == Status::Success);
    };
    let v = if has_expression(c) {
        let segs = segments(c)?;
        if let [Segment::Expr(src)] = segs.as_slice() { parse_expr(src)?.eval(ctx)? } else { Value::String(interpolate(c, ctx)?) }
    } else {
        parse_expr(c)?.eval(ctx)?
    };
    // Actions adds an implicit `success() &&` unless the expression calls a status function.
    // Callers that want that behavior ask `has_status_function`.
    Ok(v.truthy())
}

/// True when an `if:` condition calls `success()`, `failure()`, `always()` or `cancelled()`.
/// Without one, Actions treats the condition as `success() && (condition)`.
pub fn has_status_function(cond: &str) -> bool {
    let src = match segments(cond) {
        Ok(segs) => segs.into_iter().filter_map(|s| if let Segment::Expr(e) = s { Some(e) } else { None }).collect::<Vec<_>>(),
        Err(_) => return false,
    };
    let exprs: Vec<String> = if src.is_empty() { vec![cond.to_string()] } else { src };
    exprs.iter().any(|s| parse_expr(s).map(|e| expr_has_status(&e)).unwrap_or(false))
}

fn expr_has_status(e: &Expr) -> bool {
    match e {
        Expr::Call(n, args) => {
            matches!(n.to_ascii_lowercase().as_str(), "success" | "failure" | "always" | "cancelled") || args.iter().any(expr_has_status)
        }
        Expr::Prop(b, _) | Expr::Star(b) | Expr::Not(b) => expr_has_status(b),
        Expr::Index(a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => expr_has_status(a) || expr_has_status(b),
        Expr::Literal(_) | Expr::Name(_) => false,
    }
}

/// The full rule for a job or step `if:`: no condition means `success()`, a condition with no
/// status function means `success() && condition`, otherwise the condition decides.
pub fn should_run(cond: Option<&str>, ctx: &Context<'_>) -> Result<bool, ExprError> {
    match cond.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok(ctx.status == Status::Success),
        Some(c) => {
            let v = eval_condition(Some(c), ctx)?;
            if has_status_function(c) { Ok(v) } else { Ok(v && ctx.status == Status::Success) }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn ctx() -> Context<'static> {
        Context::new()
            .with("github", Value::from(json!({"event_name": "push", "ref": "refs/heads/main", "actor": "Alice", "event": {"number": 7, "labels": [{"name": "bug"}, {"name": "ui"}]}})))
            .with("matrix", Value::from(json!({"os": "linux", "n": 3})))
            .with("env", Value::from(json!({"A": "1", "EMPTY": ""})))
            .with("vars", Value::from(json!({"X": "y"})))
            .with("needs", Value::from(json!({"build": {"result": "success", "outputs": {"ver": "1.2"}}})))
    }

    fn ev(s: &str) -> Value {
        parse_expr(s).unwrap().eval(&ctx()).unwrap()
    }

    #[test]
    fn literals() {
        assert_eq!(ev("true"), Value::Bool(true));
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("12"), Value::Number(12.0));
        assert_eq!(ev("-3.5"), Value::Number(-3.5));
        assert_eq!(ev("0x10"), Value::Number(16.0));
        assert_eq!(ev("1e3"), Value::Number(1000.0));
        assert_eq!(ev("'it''s'"), Value::String("it's".into()));
    }

    #[test]
    fn property_access_is_case_insensitive_and_null_when_missing() {
        assert_eq!(ev("github.event_name"), Value::String("push".into()));
        assert_eq!(ev("GITHUB.EVENT_NAME"), Value::String("push".into()));
        assert_eq!(ev("github.nope.deeper"), Value::Null);
        assert_eq!(ev("github['ref']"), Value::String("refs/heads/main".into()));
        assert_eq!(ev("github.event.labels[1].name"), Value::String("ui".into()));
        assert_eq!(ev("github.event.labels[9].name"), Value::Null);
    }

    #[test]
    fn codehaven_is_an_alias_for_github() {
        let c = Context::new().with("github", Value::from(json!({"actor": "a"})));
        assert_eq!(parse_expr("codehaven.actor").unwrap().eval(&c).unwrap(), Value::String("a".into()));
    }

    #[test]
    fn unknown_top_level_name_is_an_error() {
        assert!(parse_expr("nope.x").unwrap().eval(&ctx()).is_err());
    }

    #[test]
    fn comparison_follows_actions_rules() {
        assert_eq!(ev("'ABC' == 'abc'"), Value::Bool(true));
        assert_eq!(ev("1 == '1'"), Value::Bool(true));
        assert_eq!(ev("null == 0"), Value::Bool(true));
        assert_eq!(ev("null == ''"), Value::Bool(true));
        assert_eq!(ev("true == 1"), Value::Bool(true));
        assert_eq!(ev("'a' < 'B'"), Value::Bool(true));
        assert_eq!(ev("2 > 10"), Value::Bool(false));
        assert_eq!(ev("'2' > '10'"), Value::Bool(true), "two strings compare as strings");
        assert_eq!(ev("matrix.n >= 3"), Value::Bool(true));
        assert_eq!(ev("github.event == github.event"), Value::Bool(false));
        assert_eq!(ev("'x' != 'y'"), Value::Bool(true));
        assert_eq!(ev("'abc' < 1"), Value::Bool(false), "NaN compares false");
    }

    #[test]
    fn logic_returns_operands() {
        assert_eq!(ev("'' || 'fallback'"), Value::String("fallback".into()));
        assert_eq!(ev("'a' && 'b'"), Value::String("b".into()));
        assert_eq!(ev("0 && 'b'"), Value::Number(0.0));
        assert_eq!(ev("!''"), Value::Bool(true));
        assert_eq!(ev("!github.event"), Value::Bool(false));
        assert_eq!(ev("true || nope.x"), Value::Bool(true), "right side not evaluated");
        assert_eq!(ev("false && nope.x"), Value::Bool(false));
    }

    #[test]
    fn precedence() {
        assert_eq!(ev("true || false && false"), Value::Bool(true));
        assert_eq!(ev("(true || false) && false"), Value::Bool(false));
        assert_eq!(ev("!true == false"), Value::Bool(true));
        assert_eq!(ev("1 < 2 == true"), Value::Bool(true));
    }

    #[test]
    fn truthiness() {
        assert!(!Value::Number(f64::NAN).truthy());
        assert!(!Value::Number(-0.0).truthy());
        assert!(Value::Array(vec![]).truthy());
        assert!(Value::Object(BTreeMap::new()).truthy());
        assert!(Value::String("0".into()).truthy());
    }

    #[test]
    fn object_filter_collects_properties() {
        let v = ev("github.event.labels.*");
        assert!(matches!(v, Value::Array(ref a) if a.len() == 2));
    }

    #[test]
    fn string_functions() {
        assert_eq!(ev("contains('Hello World', 'world')"), Value::Bool(true));
        assert_eq!(ev("contains(github.event.labels.*.name, 'bug')"), Value::Bool(true));
        assert_eq!(ev("contains(github.event.labels.*.name, 'nope')"), Value::Bool(false));
        assert_eq!(ev("join(github.event.labels.*.name, '/')"), Value::String("bug/ui".into()));
        assert_eq!(ev("contains(fromJSON('[\"a\",\"b\"]'), 'B')"), Value::Bool(true));
        assert_eq!(ev("startsWith(github.ref, 'refs/heads/')"), Value::Bool(true));
        assert_eq!(ev("endsWith('file.RS', '.rs')"), Value::Bool(true));
        assert_eq!(ev("startsWith(github.event, 'x')"), Value::Bool(false));
    }

    #[test]
    fn format_and_join() {
        assert_eq!(ev("format('{0}-{1}-{0}', 'a', 2)"), Value::String("a-2-a".into()));
        assert_eq!(ev("format('{{literal}} {0}', 'x')"), Value::String("{literal} x".into()));
        assert!(parse_expr("format('{1}', 'a')").unwrap().eval(&ctx()).is_err());
        assert!(parse_expr("format('{', 'a')").unwrap().eval(&ctx()).is_err());
        assert_eq!(ev("join(fromJSON('[1,2,3]'), '-')"), Value::String("1-2-3".into()));
        assert_eq!(ev("join(fromJSON('[1,2,3]'))"), Value::String("1,2,3".into()));
        assert_eq!(ev("join('solo', ',')"), Value::String("solo".into()));
    }

    #[test]
    fn json_round_trip() {
        assert_eq!(ev("fromJSON('{\"a\": [1, true, null]}').a[0]"), Value::Number(1.0));
        assert_eq!(ev("fromJSON(toJSON(matrix)).os"), Value::String("linux".into()));
        assert!(parse_expr("fromJSON('{nope')").unwrap().eval(&ctx()).is_err());
        assert_eq!(ev("toJSON('a')"), Value::String("\"a\"".into()));
        assert_eq!(ev("toJSON(null)"), Value::String("null".into()));
    }

    #[test]
    fn status_functions_follow_the_context() {
        let e = |s: &str, st: Status| parse_expr(s).unwrap().eval(&Context::new().with_status(st)).unwrap();
        assert_eq!(e("success()", Status::Success), Value::Bool(true));
        assert_eq!(e("success()", Status::Failure), Value::Bool(false));
        assert_eq!(e("failure()", Status::Failure), Value::Bool(true));
        assert_eq!(e("cancelled()", Status::Cancelled), Value::Bool(true));
        assert_eq!(e("always()", Status::Cancelled), Value::Bool(true));
    }

    #[test]
    fn hash_files_goes_through_the_callback() {
        let f = |p: &[String]| Ok(format!("hash-of-{}", p.join("+")));
        let c = Context::new().with_hash_files(&f);
        assert_eq!(parse_expr("hashFiles('a', 'b')").unwrap().eval(&c).unwrap(), Value::String("hash-of-a+b".into()));
        assert!(parse_expr("hashFiles('a')").unwrap().eval(&Context::new()).is_err(), "server side has no files");
    }

    #[test]
    fn syntax_errors_name_the_problem() {
        for bad in [
            "",
            "1 +",
            "a ==",
            "(1",
            "f(",
            "x[",
            "a = b",
            "a & b",
            "a | b",
            "'open",
            "nofn(1)",
            "contains(1)",
            "success(1)",
            "1 2",
            "a.",
            "@",
        ] {
            assert!(parse_expr(bad).is_err(), "{bad:?} should not parse");
        }
        let e = parse_expr("a = b").unwrap_err();
        assert!(e.message.contains("=="), "{e}");
    }

    #[test]
    fn deep_nesting_is_refused_not_crashed() {
        let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert!(parse_expr(&deep).is_err());
        let nots = format!("{}true", "!".repeat(5000));
        assert!(parse_expr(&nots).is_err());
        let idx = format!("a{}", "[0]".repeat(100));
        assert!(parse_expr(&idx).is_ok());
    }

    #[test]
    fn overlong_expressions_are_refused() {
        assert!(parse_expr(&"1 || ".repeat(MAX_EXPR_LEN)).is_err());
    }

    #[test]
    fn segments_split_text_and_expressions() {
        let s = segments("a ${{ x }} b ${{ y }}").unwrap();
        assert_eq!(s, vec![Segment::Text("a ".into()), Segment::Expr("x".into()), Segment::Text(" b ".into()), Segment::Expr("y".into())]);
        assert_eq!(segments("plain").unwrap(), vec![Segment::Text("plain".into())]);
        assert_eq!(segments("").unwrap(), vec![]);
        assert!(segments("${{ x").is_err());
        let s = segments("${{ '}}' == x }}").unwrap();
        assert_eq!(s, vec![Segment::Expr("'}}' == x".into())]);
    }

    #[test]
    fn interpolation() {
        let c = ctx();
        assert_eq!(interpolate("os=${{ matrix.os }} n=${{ matrix.n }}", &c).unwrap(), "os=linux n=3");
        assert_eq!(interpolate("no expressions", &c).unwrap(), "no expressions");
        assert_eq!(interpolate("[${{ env.EMPTY }}]", &c).unwrap(), "[]");
        assert_eq!(interpolate("${{ matrix.missing }}", &c).unwrap(), "");
        assert_eq!(interpolate("${{ github.event }}", &c).unwrap(), "Object");
        assert_eq!(interpolate("${{ 1.5 }} ${{ 2.0 }} ${{ true }}", &c).unwrap(), "1.5 2 true");
        assert!(interpolate("${{ nope }}", &c).is_err());
    }

    #[test]
    fn typed_evaluation_keeps_the_type() {
        let c = ctx();
        assert_eq!(eval_typed("${{ matrix.n }}", &c).unwrap(), Value::Number(3.0));
        assert_eq!(eval_typed("${{ fromJSON('[1,2]') }}", &c).unwrap(), Value::Array(vec![Value::Number(1.0), Value::Number(2.0)]));
        assert_eq!(eval_typed("n=${{ matrix.n }}", &c).unwrap(), Value::String("n=3".into()));
    }

    #[test]
    fn conditions() {
        let c = ctx();
        assert!(eval_condition(Some("github.event_name == 'push'"), &c).unwrap());
        assert!(eval_condition(Some("${{ github.event_name == 'push' }}"), &c).unwrap());
        assert!(!eval_condition(Some("github.event_name != 'push'"), &c).unwrap());
        assert!(eval_condition(None, &c).unwrap(), "default is success()");
        assert!(eval_condition(Some("   "), &c).unwrap());
        assert!(eval_condition(Some("needs.build.outputs.ver == '1.2'"), &c).unwrap());
        assert!(!eval_condition(Some("needs.missing.result == 'success'"), &c).unwrap());
    }

    #[test]
    fn implicit_success_unless_a_status_function_is_called() {
        let failed = ctx().with_status(Status::Failure);
        assert!(!should_run(None, &failed).unwrap());
        assert!(!should_run(Some("github.event_name == 'push'"), &failed).unwrap(), "implicit success() &&");
        assert!(should_run(Some("failure()"), &failed).unwrap());
        assert!(should_run(Some("always() && github.event_name == 'push'"), &failed).unwrap());
        assert!(should_run(Some("${{ always() }}"), &failed).unwrap());
        assert!(!should_run(Some("success() || false"), &failed).unwrap());
        let cancelled = ctx().with_status(Status::Cancelled);
        assert!(!should_run(Some("failure()"), &cancelled).unwrap());
        assert!(should_run(Some("cancelled()"), &cancelled).unwrap());
        assert!(should_run(Some("github.event_name == 'push'"), &ctx()).unwrap());
    }

    #[test]
    fn references_are_collected() {
        let r = template_refs("${{ secrets.TOKEN }} ${{ vars.X && secrets['OTHER'] }} ${{ needs.build.outputs.v }} ${{ hashFiles('a') }}");
        assert_eq!(r.secrets, BTreeSet::from(["TOKEN".to_string(), "OTHER".to_string()]));
        assert_eq!(r.vars, BTreeSet::from(["X".to_string()]));
        assert_eq!(r.needs, BTreeSet::from(["build".to_string()]));
        assert!(r.uses_hash_files);
        assert!(!r.all_secrets);
        let all = template_refs("${{ toJSON(secrets) }}");
        assert!(all.all_secrets);
        let dynamic = template_refs("${{ secrets[matrix.name] }}");
        assert!(dynamic.all_secrets, "a computed secret name needs every secret");
    }

    #[test]
    fn check_template_catches_typos() {
        assert!(check_template("${{ github.sha }}").is_ok());
        assert!(check_template("${{ githb.sha }}").is_err());
        assert!(check_template("${{ matrix.os == }}").is_err());
        assert!(check_template("plain text").is_ok());
        assert!(check_template("${{ contains(a }}").is_err());
    }

    #[test]
    fn numbers_print_like_actions() {
        assert_eq!(number_to_string(3.0), "3");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(0.5), "0.5");
        assert_eq!(number_to_string(f64::NAN), "NaN");
        assert_eq!(number_to_string(f64::INFINITY), "Infinity");
        assert_eq!(parse_number(""), 0.0);
        assert_eq!(parse_number(" 12 "), 12.0);
        assert!(parse_number("inf").is_nan(), "Rust's spelling is not Actions'");
        assert!(parse_number("12abc").is_nan());
        assert_eq!(parse_number("0x1F"), 31.0);
    }

    // ---- property tests

    fn arb_json() -> impl Strategy<Value = Json> {
        let leaf = prop_oneof![
            Just(Json::Null),
            any::<bool>().prop_map(Json::Bool),
            (-1_000_000i64..1_000_000).prop_map(Json::from),
            "[a-zA-Z0-9 _.-]{0,12}".prop_map(Json::String),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Json::Array),
                prop::collection::btree_map("[a-z]{1,5}", inner, 0..4).prop_map(|m| Json::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Whatever the text, parsing returns, never panics or overflows the stack.
        #[test]
        fn parser_never_panics(s in "\\PC{0,200}") {
            let _ = parse_expr(&s);
            let _ = segments(&s);
        }

        #[test]
        fn parser_never_panics_on_operator_soup(s in "[a-z0-9_.()\\[\\]'!=<>&| ,*-]{0,120}") {
            if let Ok(e) = parse_expr(&s) {
                let _ = e.eval(&Context::new().with("github", Value::Null));
            }
        }

        #[test]
        fn to_json_from_json_round_trips(j in arb_json()) {
            let c = Context::new().with("v", Value::from_json(&j));
            let out = parse_expr("fromJSON(toJSON(v))").unwrap().eval(&c).unwrap();
            prop_assert_eq!(out.to_json(), j);
        }

        #[test]
        fn string_literals_round_trip(s in "[a-zA-Z0-9 '_.{}$-]{0,30}") {
            let quoted = format!("'{}'", s.replace('\'', "''"));
            prop_assert_eq!(parse_expr(&quoted).unwrap().eval(&Context::new()).unwrap(), Value::String(s));
        }

        #[test]
        fn integer_literals_round_trip(n in -1_000_000_000i64..1_000_000_000) {
            let v = parse_expr(&n.to_string()).unwrap().eval(&Context::new()).unwrap();
            prop_assert_eq!(v, Value::Number(n as f64));
        }

        // Double negation is the truthiness of the operand.
        #[test]
        fn double_negation_is_truthiness(j in arb_json()) {
            let c = Context::new().with("v", Value::from_json(&j));
            let v = parse_expr("!!v").unwrap().eval(&c).unwrap();
            prop_assert_eq!(v, Value::Bool(Value::from_json(&j).truthy()));
        }

        #[test]
        fn de_morgan(a in any::<bool>(), b in any::<bool>()) {
            let c = Context::new().with("a", Value::Bool(a)).with("b", Value::Bool(b));
            let lhs = parse_expr("!(a && b)").unwrap().eval(&c).unwrap();
            let rhs = parse_expr("!a || !b").unwrap().eval(&c).unwrap();
            prop_assert_eq!(lhs, rhs);
        }

        // Comparing strings is case-blind and symmetric.
        #[test]
        fn string_equality_ignores_case(s in "[a-zA-Z]{0,10}") {
            let c = Context::new().with("a", Value::String(s.to_lowercase())).with("b", Value::String(s.to_uppercase()));
            prop_assert_eq!(parse_expr("a == b").unwrap().eval(&c).unwrap(), Value::Bool(true));
            prop_assert_eq!(parse_expr("a == b").unwrap().eval(&c).unwrap(), parse_expr("b == a").unwrap().eval(&c).unwrap());
        }

        // interpolate of text with no `${{` is the identity, and a segment split never loses text.
        #[test]
        fn plain_text_is_unchanged(s in "[a-zA-Z0-9 .,:;/_-]{0,60}") {
            prop_assert_eq!(interpolate(&s, &Context::new()).unwrap(), s);
        }

        #[test]
        fn format_without_placeholders_is_identity(s in "[a-zA-Z0-9 .,:;/_-]{0,40}") {
            let out = format_string(&s, &[]).unwrap();
            prop_assert_eq!(out, s);
        }

        #[test]
        fn contains_matches_str_contains_ignoring_case(hay in "[a-zA-Z]{0,12}", needle in "[a-zA-Z]{0,4}") {
            let c = Context::new().with("h", Value::String(hay.clone())).with("n", Value::String(needle.clone()));
            let v = parse_expr("contains(h, n)").unwrap().eval(&c).unwrap();
            prop_assert_eq!(v, Value::Bool(hay.to_lowercase().contains(&needle.to_lowercase())));
        }
    }
}
