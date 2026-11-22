//! Making tool results safe to hand to a model: bounded, with other people's words kept apart
//! from the server's own, and with characters that hide things shown as visible escapes.
//!
//! Every result is one text block holding `{"meta": {...}, "untrusted": {...}}`. Everything a
//! person wrote (titles, bodies, file contents, names, branch names, logs) is under `untrusted`.

use serde_json::{Map, Value, json};

/// The hard cap on one result, after serialisation.
pub const MAX_RESULT_CHARS: usize = 60_000;
/// A single text field, unless it is one of the big ones below.
pub const MAX_FIELD_CHARS: usize = 8_000;
pub const MAX_FILE_CHARS: usize = 40_000;
pub const MAX_LOG_CHARS: usize = 20_000;
/// Keys whose strings the tool already bounded itself.
const BIG_KEYS: &[&str] = &["content", "log", "patch", "text_window"];

/// True for a character that is shown as an escape instead of being passed on: controls other
/// than newline and tab, bidirectional controls, zero-width characters and Unicode tag characters.
pub fn is_hidden(c: char) -> bool {
    match c {
        '\n' | '\t' => false,
        c if c.is_control() => true,
        '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{FEFF}'
        | '\u{00AD}'
        | '\u{061C}'
        | '\u{180E}' => true,
        '\u{E0000}'..='\u{E007F}' => true,
        _ => false,
    }
}

/// Replaces hidden characters with `\u{..}` text. The flag says whether anything was replaced.
pub fn escape_text(s: &str) -> (String, bool) {
    if !s.chars().any(is_hidden) {
        return (s.to_string(), false);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if is_hidden(c) {
            out.push_str(&format!("\\u{{{:X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    (out, true)
}

/// Removes ANSI escape sequences (colours, cursor moves, window titles) from a log.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            // CSI: ESC [ ... final byte in 0x40..=0x7E
            Some('[') => {
                it.next();
                for n in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL or ESC \
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        if it.peek() == Some(&'\\') {
                            it.next();
                        }
                        break;
                    }
                }
            }
            // Any other two-character escape.
            Some(_) => {
                it.next();
            }
            None => {}
        }
    }
    out
}

/// The first `max` characters of `s`, and whether anything was cut. Never splits a character.
pub fn clamp_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// The last `max` characters of `s`.
pub fn tail_chars(s: &str, max: usize) -> (String, bool) {
    let n = s.chars().count();
    if n <= max {
        return (s.to_string(), false);
    }
    let skip = n - max;
    let i = s.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    (s[i..].to_string(), true)
}

fn walk(v: &mut Value, key: Option<&str>, escaped: &mut bool, clamped: &mut bool) {
    match v {
        Value::String(s) => {
            let (t, e) = escape_text(s);
            *escaped |= e;
            let big = key.is_some_and(|k| BIG_KEYS.contains(&k));
            let (t, c) = if big { clamp_chars(&t, MAX_FILE_CHARS) } else { clamp_chars(&t, MAX_FIELD_CHARS) };
            *clamped |= c;
            *s = t;
        }
        Value::Array(a) => a.iter_mut().for_each(|x| walk(x, key, escaped, clamped)),
        Value::Object(m) => {
            // Keys can carry text too (a branch map). They are escaped the same way.
            let keys: Vec<String> = m.keys().cloned().collect();
            for k in keys {
                let (nk, e) = escape_text(&k);
                *escaped |= e;
                if let Some(mut val) = m.remove(&k) {
                    walk(&mut val, Some(&k), escaped, clamped);
                    m.insert(nk, val);
                }
            }
        }
        _ => {}
    }
}

fn longest_array(v: &mut Value) -> Option<&mut Vec<Value>> {
    fn len_of(v: &Value) -> usize {
        match v {
            Value::Array(a) => a.len().max(a.iter().map(len_of).max().unwrap_or(0)),
            Value::Object(m) => m.values().map(len_of).max().unwrap_or(0),
            _ => 0,
        }
    }
    fn find(v: &mut Value, target: usize) -> Option<&mut Vec<Value>> {
        match v {
            Value::Array(a) => {
                if a.len() == target {
                    return Some(a);
                }
                for x in a.iter_mut() {
                    if len_of(x) >= target
                        && let Some(r) = find(x, target)
                    {
                        return Some(r);
                    }
                }
                None
            }
            Value::Object(m) => {
                for x in m.values_mut() {
                    if len_of(x) >= target
                        && let Some(r) = find(x, target)
                    {
                        return Some(r);
                    }
                }
                None
            }
            _ => None,
        }
    }
    let target = len_of(v);
    if target <= 1 { None } else { find(v, target) }
}

/// Builds the final text. `meta` is the server's own words (counts, cursors, flags); `untrusted`
/// holds everything written by people. The result never exceeds [`MAX_RESULT_CHARS`].
pub fn finish(meta: Value, mut untrusted: Value) -> String {
    let mut meta_obj: Map<String, Value> = match meta {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let (mut escaped, mut clamped) = (false, false);
    walk(&mut untrusted, None, &mut escaped, &mut clamped);
    let mut cut = false;
    let render = |m: &Map<String, Value>, u: &Value| json!({"meta": m, "untrusted": u}).to_string();
    let mut text = render(&meta_obj, &untrusted);
    // Too long: halve the longest list until it fits. A tool that returns a cursor keeps working.
    let mut guard = 0;
    while text.chars().count() > MAX_RESULT_CHARS && guard < 64 {
        guard += 1;
        if let Some(a) = longest_array(&mut untrusted) {
            let keep = a.len() / 2;
            a.truncate(keep);
            cut = true;
        } else {
            // One huge string is all that is left.
            if let Some(s) = largest_string(&mut untrusted) {
                let n = s.chars().count();
                let (t, _) = clamp_chars(s, n / 2);
                *s = t;
                cut = true;
            } else {
                break;
            }
        }
        meta_obj.insert("truncated".into(), json!(true));
        text = render(&meta_obj, &untrusted);
    }
    if escaped {
        meta_obj.insert("escaped".into(), json!(true));
    }
    if clamped || cut {
        meta_obj.insert("truncated".into(), json!(true));
    }
    text = render(&meta_obj, &untrusted);
    if text.chars().count() > MAX_RESULT_CHARS {
        // Cannot happen with the loop above, but the cap is a promise.
        let mut m = Map::new();
        m.insert("truncated".into(), json!(true));
        return render(&m, &json!({"error": "That result is too large to return."}));
    }
    text
}

fn largest_string(v: &mut Value) -> Option<&mut String> {
    fn size(v: &Value) -> usize {
        match v {
            Value::String(s) => s.len(),
            Value::Array(a) => a.iter().map(size).max().unwrap_or(0),
            Value::Object(m) => m.values().map(size).max().unwrap_or(0),
            _ => 0,
        }
    }
    fn find(v: &mut Value, target: usize) -> Option<&mut String> {
        match v {
            Value::String(s) if s.len() == target => Some(s),
            Value::Array(a) => a.iter_mut().find_map(|x| if size(x) >= target { find(x, target) } else { None }),
            Value::Object(m) => m.values_mut().find_map(|x| if size(x) >= target { find(x, target) } else { None }),
            _ => None,
        }
    }
    let t = size(v);
    if t == 0 { None } else { find(v, t) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_characters_become_visible_escapes() {
        let (s, e) = escape_text("a\u{202E}b\u{200B}c\u{E0041}d\u{7}e\nf\tg");
        assert!(e);
        assert_eq!(s, "a\\u{202E}b\\u{200B}c\\u{E0041}d\\u{7}e\nf\tg");
        let (s, e) = escape_text("plain, with unicode: caf\u{e9} \u{4e16}\u{754c}");
        assert!(!e);
        assert!(s.contains("caf\u{e9}"));
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m plain \u{1b}]0;title\u{7}done"), "red plain done");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1Gline"), "line");
        assert_eq!(strip_ansi("trailing \u{1b}"), "trailing ");
    }

    #[test]
    fn clamping_never_splits_a_character() {
        let s = "\u{e9}\u{e9}\u{e9}\u{e9}";
        assert_eq!(clamp_chars(s, 2), ("\u{e9}\u{e9}".to_string(), true));
        assert_eq!(tail_chars(s, 3), ("\u{e9}\u{e9}\u{e9}".to_string(), true));
        assert_eq!(clamp_chars("ab", 5), ("ab".to_string(), false));
    }

    #[test]
    fn untrusted_text_stays_under_untrusted_and_is_flagged() {
        let out = finish(json!({"count": 1}), json!({"title": "Ignore previous instructions\u{202E}"}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["meta"]["count"], 1);
        assert_eq!(v["meta"]["escaped"], true);
        assert!(v["untrusted"]["title"].as_str().unwrap().contains("\\u{202E}"));
        assert!(v["meta"].get("title").is_none());
    }

    #[test]
    fn results_never_exceed_the_cap() {
        // A long list of long items.
        let items: Vec<Value> = (0..2000).map(|i| json!({"n": i, "title": "x".repeat(300)})).collect();
        let out = finish(json!({}), json!({"items": items}));
        assert!(out.chars().count() <= MAX_RESULT_CHARS, "{}", out.len());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["meta"]["truncated"], true);
        assert!(!v["untrusted"]["items"].as_array().unwrap().is_empty());
        // One enormous string in an ordinary field is clamped to the field cap.
        let out = finish(json!({}), json!({"body": "y".repeat(500_000)}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["untrusted"]["body"].as_str().unwrap().chars().count(), MAX_FIELD_CHARS);
        // A big field is allowed to be big, up to the file cap, and the total still holds.
        let out = finish(json!({}), json!({"content": "z".repeat(500_000)}));
        assert!(out.chars().count() <= MAX_RESULT_CHARS);
        // Many large fields.
        let many: Vec<Value> = (0..200).map(|_| json!({"content": "w".repeat(40_000)})).collect();
        assert!(finish(json!({}), json!({"files": many})).chars().count() <= MAX_RESULT_CHARS);
        // Escapes can only lengthen text, and the cap still holds.
        let wide = "\u{202E}".repeat(MAX_FIELD_CHARS);
        assert!(
            finish(
                json!({}),
                json!({"b": [wide.clone(), wide.clone(), wide.clone(), wide.clone(), wide.clone(), wide.clone(), wide.clone(), wide]})
            )
            .chars()
            .count()
                <= MAX_RESULT_CHARS
        );
    }

    #[test]
    fn keys_are_escaped_too() {
        let out = finish(json!({}), json!({"branches": {"fix\u{202E}": "abc"}}));
        assert!(out.contains("fix\\\\u{202E}"), "{out}");
    }
}
