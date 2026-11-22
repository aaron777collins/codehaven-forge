//! Inputs for a manual run (`workflow_dispatch`).

use crate::model::{Dispatch, InputType};
use serde_json::{Map, Value as Json};

fn coerce(kind: InputType, name: &str, v: &Json, options: &[String]) -> Result<Json, String> {
    match kind {
        InputType::String => match v {
            Json::String(s) => Ok(Json::String(s.clone())),
            Json::Number(n) => Ok(Json::String(n.to_string())),
            Json::Bool(b) => Ok(Json::String(b.to_string())),
            Json::Null => Ok(Json::String(String::new())),
            _ => Err(format!("input `{name}` must be text")),
        },
        InputType::Boolean => match v {
            Json::Bool(b) => Ok(Json::Bool(*b)),
            Json::String(s) if s.eq_ignore_ascii_case("true") => Ok(Json::Bool(true)),
            Json::String(s) if s.eq_ignore_ascii_case("false") => Ok(Json::Bool(false)),
            _ => Err(format!("input `{name}` must be true or false")),
        },
        InputType::Number => match v {
            Json::Number(n) => Ok(Json::Number(n.clone())),
            Json::String(s) => s
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .and_then(|f| {
                    if f.fract() == 0.0 && f.abs() < 9e15 {
                        Some(Json::from(f as i64))
                    } else {
                        serde_json::Number::from_f64(f).map(Json::Number)
                    }
                })
                .ok_or_else(|| format!("input `{name}` must be a number")),
            _ => Err(format!("input `{name}` must be a number")),
        },
        InputType::Choice => {
            let s = match v {
                Json::String(s) => s.clone(),
                Json::Number(n) => n.to_string(),
                Json::Bool(b) => b.to_string(),
                _ => return Err(format!("input `{name}` must be one of: {}", options.join(", "))),
            };
            if options.contains(&s) { Ok(Json::String(s)) } else { Err(format!("input `{name}` must be one of: {}", options.join(", "))) }
        }
    }
}

/// Checks the inputs a person gave against what the workflow declares and fills in defaults.
/// All problems come back together.
pub fn resolve(d: &Dispatch, given: &Map<String, Json>) -> Result<Map<String, Json>, Vec<String>> {
    let mut errors = Vec::new();
    let mut out = Map::new();
    for k in given.keys() {
        if !d.inputs.iter().any(|i| &i.name == k) {
            errors.push(format!("input `{k}` is not declared by this workflow"));
        }
    }
    for input in &d.inputs {
        let value = match given.get(&input.name) {
            Some(Json::Null) | None => match &input.default {
                Some(def) => def.clone(),
                None if input.required => {
                    errors.push(format!("input `{}` is required", input.name));
                    continue;
                }
                None => match input.kind {
                    InputType::Boolean => Json::Bool(false),
                    InputType::Number => Json::Null,
                    InputType::String => Json::String(String::new()),
                    InputType::Choice => input.options.first().cloned().map(Json::String).unwrap_or(Json::Null),
                },
            },
            Some(v) => v.clone(),
        };
        if value.is_null() && input.kind == InputType::Number {
            out.insert(input.name.clone(), Json::Null);
            continue;
        }
        match coerce(input.kind, &input.name, &value, &input.options) {
            Ok(v) => {
                out.insert(input.name.clone(), v);
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() { Ok(out) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use serde_json::json;

    fn dispatch() -> Dispatch {
        parse(
            "on:
  workflow_dispatch:
    inputs:
      level:
        type: choice
        options: [quick, full]
        default: quick
      verbose:
        type: boolean
      count:
        type: number
        default: 3
      who:
        required: true
      note:
        description: free text
jobs:
  j:
    runs-on: x
    steps:
      - run: echo
",
        )
        .unwrap_or_else(|e| panic!("{e:?}"))
        .triggers
        .workflow_dispatch
        .unwrap()
    }
    fn given(v: serde_json::Value) -> Map<String, Json> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn defaults_and_coercion() {
        let r = resolve(&dispatch(), &given(json!({"who": "me", "count": "7", "verbose": "TRUE"}))).unwrap();
        assert_eq!(r["level"], "quick");
        assert_eq!(r["count"], 7);
        assert_eq!(r["verbose"], true);
        assert_eq!(r["who"], "me");
        assert_eq!(r["note"], "");
    }

    #[test]
    fn everything_wrong_is_reported_at_once() {
        let e = resolve(&dispatch(), &given(json!({"level": "huge", "count": "lots", "verbose": "maybe", "extra": 1}))).unwrap_err();
        assert_eq!(e.len(), 5, "{e:?}");
        assert!(e.iter().any(|m| m.contains("`who` is required")));
        assert!(e.iter().any(|m| m.contains("one of: quick, full")));
        assert!(e.iter().any(|m| m.contains("`extra`")));
    }

    #[test]
    fn no_declared_inputs_accepts_none() {
        let d = Dispatch::default();
        assert!(resolve(&d, &Map::new()).unwrap().is_empty());
        assert!(resolve(&d, &given(json!({"a": 1}))).is_err());
    }
}
