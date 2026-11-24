//! Request paths on the pages host (`docs/design/pages.md`, section 3). Nothing here touches a
//! file system: the result names a file in a published copy's list, looked up exactly.

/// The longest request path accepted, in bytes as sent.
pub const MAX_PATH: usize = 2048;

/// What a request on the pages host names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub owner: String,
    pub project: String,
    /// The per-site token from `/-/t/<token>/...`, unchecked.
    pub token: Option<String>,
    /// The path inside the site, decoded: `""` for the root, otherwise never starting with `/`.
    /// A trailing `/` is kept: it means `index.html` there.
    pub rest: String,
    /// The request stopped at the project with no slash after it.
    pub needs_slash: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Site(Target),
    Robots,
    NotFound,
}

/// `/<owner>/<project>/<rest>`, or the same after `/-/t/<token>`.
pub fn parse_path_form(raw: &str) -> Parsed {
    let Some(segs) = segments(raw) else { return Parsed::NotFound };
    if segs.len() == 1 && segs[0] == "robots.txt" {
        return Parsed::Robots;
    }
    let (token, segs) = take_token(&segs);
    let Some(token) = token else { return Parsed::NotFound };
    let Some((owner, rest)) = segs.split_first() else { return Parsed::NotFound };
    if !ch_core::pages::valid_owner_label(owner) {
        return Parsed::NotFound;
    }
    site(owner, rest, token)
}

/// Full mode, where the host names the owner: `/<project>/<rest>`, or after `/-/t/<token>`.
pub fn parse_sub_form(owner: &str, raw: &str) -> Parsed {
    if !ch_core::pages::valid_owner_label(owner) {
        return Parsed::NotFound;
    }
    let Some(segs) = segments(raw) else { return Parsed::NotFound };
    if segs.len() == 1 && segs[0] == "robots.txt" {
        return Parsed::Robots;
    }
    let (token, segs) = take_token(&segs);
    let Some(token) = token else { return Parsed::NotFound };
    site(owner, segs, token)
}

/// The candidates for one request, in order: the file itself, then for a name with no
/// extension `<rest>.html`. `index.html` for a folder. The second value is the file whose
/// presence means "add a slash and redirect".
pub fn candidates(rest: &str) -> (Vec<String>, Option<String>) {
    if rest.is_empty() || rest.ends_with('/') {
        return (vec![format!("{rest}index.html")], None);
    }
    let last = rest.rsplit('/').next().unwrap_or(rest);
    if last.contains('.') {
        return (vec![rest.to_string()], None);
    }
    (vec![rest.to_string(), format!("{rest}.html")], Some(format!("{rest}/index.html")))
}

/// A token segment: what `token::mint` writes, and nothing that needs escaping anywhere.
pub fn token_shape_ok(t: &str) -> bool {
    !t.is_empty() && t.len() <= 256 && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `Some(None)` with no token, `Some(Some(t))` with one, `None` for a malformed `/-/` prefix.
type TokenSplit<'a> = (Option<Option<String>>, &'a [String]);

fn take_token(segs: &[String]) -> TokenSplit<'_> {
    if segs.first().map(String::as_str) != Some("-") {
        return (Some(None), segs);
    }
    match segs {
        [_, t, token, rest @ ..] if t == "t" && token_shape_ok(token) => (Some(Some(token.clone())), rest),
        _ => (None, segs),
    }
}

fn site(owner: &str, segs: &[String], token: Option<String>) -> Parsed {
    let Some((project, rest)) = segs.split_first() else { return Parsed::NotFound };
    if ch_core::access::valid_repo_name(project) != Some(project.as_str()) {
        return Parsed::NotFound;
    }
    // `segments` keeps one empty last segment for a trailing slash; an empty one anywhere else
    // was already refused.
    let needs_slash = rest.is_empty();
    let rest = rest.join("/");
    Parsed::Site(Target { owner: owner.to_string(), project: project.clone(), token, rest, needs_slash })
}

/// Splits and decodes the path. `None` for anything refused: too long, not starting with `/`,
/// an empty segment before the end, a bad escape, bytes that are not UTF-8, or a decoded
/// segment that is `.`, `..` or holds a slash, a backslash or a control character. A trailing
/// slash leaves one empty last segment.
fn segments(raw: &str) -> Option<Vec<String>> {
    if raw.len() > MAX_PATH {
        return None;
    }
    let body = raw.strip_prefix('/')?;
    if body.is_empty() {
        return None;
    }
    let parts: Vec<&str> = body.split('/').collect();
    let mut out = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if p.is_empty() {
            if last {
                out.push(String::new());
                continue;
            }
            return None;
        }
        let d = decode(p)?;
        if d == "." || d == ".." || d.chars().any(|c| c == '/' || c == '\\' || c.is_control()) {
            return None;
        }
        out.push(d);
    }
    Some(out)
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex(*b.get(i + 1)?)?;
            let lo = hex(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(owner: &str, project: &str, token: Option<&str>, rest: &str, needs_slash: bool) -> Parsed {
        Parsed::Site(Target { owner: owner.into(), project: project.into(), token: token.map(Into::into), rest: rest.into(), needs_slash })
    }

    #[test]
    fn the_path_form() {
        assert_eq!(parse_path_form("/alice/docs/"), t("alice", "docs", None, "", false));
        assert_eq!(parse_path_form("/alice/docs"), t("alice", "docs", None, "", true));
        assert_eq!(parse_path_form("/alice/docs/guide/intro.html"), t("alice", "docs", None, "guide/intro.html", false));
        assert_eq!(parse_path_form("/alice/docs/guide/"), t("alice", "docs", None, "guide/", false));
        assert_eq!(parse_path_form("/alice/docs/a%20b.html"), t("alice", "docs", None, "a b.html", false));
        assert_eq!(parse_path_form("/-/t/v1.2.3.4.abc_-/alice/docs/x"), t("alice", "docs", Some("v1.2.3.4.abc_-"), "x", false));
        assert_eq!(parse_path_form("/robots.txt"), Parsed::Robots);
    }

    #[test]
    fn traversal_and_odd_bytes_are_refused() {
        for bad in [
            "/",
            "",
            "alice/docs/",
            "/alice/docs/../x",
            "/alice/docs/%2e%2e/x",
            "/alice/docs/.",
            "/alice/docs/a%2fb",
            "/alice/docs/a%5cb",
            "/alice/docs/a\\b",
            "/alice/docs/%00",
            "/alice/docs/%0a",
            "/alice/docs/%zz",
            "/alice/docs/%e",
            "/alice/docs/%ff",
            "/alice//docs/",
            "/alice/docs//x",
            "/alice/.git/",
            "/-alice/docs/",
            "/a_b/docs/",
            "/alice/do cs/",
            "/alice/docs.git/",
            "/-/t//alice/docs/",
            "/-/t/a%2Fb/alice/docs/",
            "/-/x/tok/alice/docs/",
            "/-/alice/docs/",
        ] {
            assert_eq!(parse_path_form(bad), Parsed::NotFound, "{bad:?}");
        }
        assert_eq!(parse_path_form(&format!("/alice/docs/{}", "a".repeat(MAX_PATH))), Parsed::NotFound);
    }

    #[test]
    fn the_subdomain_form() {
        assert_eq!(parse_sub_form("alice", "/docs/x.css"), t("alice", "docs", None, "x.css", false));
        assert_eq!(parse_sub_form("alice", "/docs"), t("alice", "docs", None, "", true));
        assert_eq!(parse_sub_form("alice", "/-/t/tok/docs/"), t("alice", "docs", Some("tok"), "", false));
        assert_eq!(parse_sub_form("a_b", "/docs/"), Parsed::NotFound);
        assert_eq!(parse_sub_form("alice", "/../docs/"), Parsed::NotFound);
    }

    #[test]
    fn candidates_follow_github() {
        assert_eq!(candidates(""), (vec!["index.html".to_string()], None));
        assert_eq!(candidates("guide/"), (vec!["guide/index.html".to_string()], None));
        assert_eq!(candidates("a.css"), (vec!["a.css".to_string()], None));
        assert_eq!(candidates("guide"), (vec!["guide".to_string(), "guide.html".to_string()], Some("guide/index.html".to_string())));
    }
}
