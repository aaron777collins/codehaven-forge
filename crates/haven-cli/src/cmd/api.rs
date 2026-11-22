//! `haven api`: a raw REST call, like `curl` that knows the host and your token.

use crate::Ctx;
use crate::cli::ApiArgs;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde_json::{Map, Value};

pub async fn run(ctx: &Ctx, a: ApiArgs) -> Result<i32> {
    let client = ctx.api()?;
    let method: Method = a.method.to_uppercase().parse().map_err(|_| anyhow!("{:?} is not an HTTP method", a.method))?;
    let fields = build_fields(ctx, &a.field, &a.raw_field)?;
    let mut headers = Vec::new();
    for h in &a.header {
        let (k, v) = h.split_once(':').ok_or_else(|| anyhow!("header {h:?} must be `Name: value`"))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    let mut path = a.path.clone();
    let mut body: Option<Vec<u8>> = None;
    if let Some(input) = &a.input {
        body = Some(if input == "-" {
            ctx.read_stdin()?.into_bytes()
        } else {
            std::fs::read(input).with_context(|| format!("reading {input}"))?
        });
    } else if !fields.is_empty() {
        if method == Method::GET || method == Method::HEAD {
            let q: Vec<String> = fields.iter().map(|(k, v)| format!("{}={}", enc(k), enc(&value_to_query(v)))).collect();
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str(&q.join("&"));
        } else {
            body = Some(serde_json::to_vec(&Value::Object(fields.into_iter().collect()))?);
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
        }
    }

    if a.paginate {
        return paginate(ctx, &client, method, &path, &headers, a.include).await;
    }

    let resp = client.raw(method, &path, body, &headers).await?;
    if a.include {
        ctx.println(format!("HTTP {}", resp.status));
        for (k, v) in resp.headers.iter() {
            ctx.println(format!("{}: {}", k, v.to_str().unwrap_or("<binary>")));
        }
        ctx.println("");
    }
    print_body(ctx, &resp.body);
    Ok(if resp.status.is_success() { 0 } else { 1 })
}

async fn paginate(
    ctx: &Ctx,
    client: &crate::ApiClient,
    method: Method,
    path: &str,
    headers: &[(String, String)],
    include: bool,
) -> Result<i32> {
    let mut all: Vec<Value> = Vec::new();
    let mut url = path.to_string();
    for _ in 0..1000 {
        let resp = client.raw(method.clone(), &url, None, headers).await?;
        if !resp.status.is_success() {
            if include {
                ctx.println(format!("HTTP {}", resp.status));
            }
            print_body(ctx, &resp.body);
            return Ok(1);
        }
        let v: Value = serde_json::from_slice(&resp.body).context("a paginated call must answer with JSON")?;
        match v {
            Value::Array(mut a) => all.append(&mut a),
            other => all.push(other),
        }
        match crate::api::next_link(&resp.headers) {
            Some(n) => url = n,
            None => break,
        }
    }
    ctx.println(serde_json::to_string_pretty(&all)?);
    Ok(0)
}

fn print_body(ctx: &Ctx, body: &[u8]) {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => ctx.println(serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(_) => ctx.println(String::from_utf8_lossy(body)),
    }
}

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn value_to_query(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `-f` fields are always strings. `-F` fields are typed: `true`, `false`, `null`, numbers, and
/// `@path` for the contents of a file.
pub fn build_fields(ctx: &Ctx, strings: &[String], typed: &[String]) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    for f in strings {
        let (k, v) = f.split_once('=').ok_or_else(|| anyhow!("field {f:?} must be key=value"))?;
        m.insert(k.to_string(), Value::String(v.to_string()));
    }
    for f in typed {
        let (k, v) = f.split_once('=').ok_or_else(|| anyhow!("field {f:?} must be key=value"))?;
        let val = if let Some(path) = v.strip_prefix('@') {
            if path == "-" {
                Value::String(ctx.read_stdin()?)
            } else {
                Value::String(std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?)
            }
        } else if v == "true" {
            Value::Bool(true)
        } else if v == "false" {
            Value::Bool(false)
        } else if v == "null" {
            Value::Null
        } else if let Ok(n) = v.parse::<i64>() {
            Value::from(n)
        } else if let Ok(n) = v.parse::<f64>() {
            serde_json::Number::from_f64(n).map(Value::Number).unwrap_or_else(|| Value::String(v.to_string()))
        } else {
            Value::String(v.to_string())
        };
        m.insert(k.to_string(), val);
    }
    if m.keys().any(|k| k.is_empty()) {
        bail!("a field name is empty");
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding() {
        assert_eq!(enc("a b&c"), "a%20b%26c");
        assert_eq!(enc("safe-1_2.3~"), "safe-1_2.3~");
    }
}
