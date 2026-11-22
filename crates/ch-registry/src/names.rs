//! Parsing and validating what a registry client puts in the URL: the repository name, a tag
//! or digest, and which operation the path means. Pure functions, with property tests.

use regex::Regex;
use std::sync::OnceLock;

/// What a path under `/v2/` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Blob { digest: String },
    UploadStart,
    Upload { id: String },
    Manifest { reference: String },
    TagsList,
    Referrers { digest: String },
}

/// `owner/image/blobs/sha256:...` and friends, split into the repository name and the
/// operation. The name is everything before the marker segment, so the marker words can never
/// be part of a name. Returns `None` for anything that is not a registry operation.
pub fn parse(rest: &str) -> Option<(String, Op)> {
    let segs: Vec<&str> = rest.split('/').collect();
    let n = segs.len();
    let name = |upto: usize| -> Option<String> {
        if upto == 0 || segs[..upto].iter().any(|s| s.is_empty()) { None } else { Some(segs[..upto].join("/")) }
    };
    if n >= 3 && segs[n - 3] == "blobs" && segs[n - 2] == "uploads" && !segs[n - 1].is_empty() {
        return Some((name(n - 3)?, Op::Upload { id: segs[n - 1].to_string() }));
    }
    if n >= 3 && segs[n - 3] == "blobs" && segs[n - 2] == "uploads" && segs[n - 1].is_empty() {
        return Some((name(n - 3)?, Op::UploadStart));
    }
    // `blobs/uploads` without the trailing slash is accepted too.
    if n >= 3 && segs[n - 2] == "blobs" && segs[n - 1] == "uploads" {
        return Some((name(n - 2)?, Op::UploadStart));
    }
    if n >= 3 && segs[n - 2] == "blobs" {
        return Some((name(n - 2)?, Op::Blob { digest: segs[n - 1].to_string() }));
    }
    if n >= 3 && segs[n - 2] == "manifests" {
        return Some((name(n - 2)?, Op::Manifest { reference: segs[n - 1].to_string() }));
    }
    if n >= 3 && segs[n - 2] == "tags" && segs[n - 1] == "list" {
        return Some((name(n - 2)?, Op::TagsList));
    }
    if n >= 3 && segs[n - 2] == "referrers" {
        return Some((name(n - 2)?, Op::Referrers { digest: segs[n - 1].to_string() }));
    }
    None
}

fn image_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9]+(?:[._-][a-z0-9]+)*$").expect("static regex"))
}

fn owner_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$").expect("static regex"))
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$").expect("static regex"))
}

/// A repository name is exactly `owner/image`: the account that owns it and a lowercase image
/// name, so every image has a home and no path can wander elsewhere.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let (owner, image) = name.split_once('/')?;
    if image.contains('/') || image.len() > 100 || !owner_re().is_match(owner) || !image_re().is_match(image) {
        return None;
    }
    Some((owner, image))
}

pub fn is_tag(s: &str) -> bool {
    tag_re().is_match(s)
}

/// `sha256:` and 64 lowercase hex characters. Other algorithms are not accepted.
pub fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// The hex part of a digest, once it has been checked.
pub fn hex_of(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(String),
}

pub fn parse_reference(s: &str) -> Option<Reference> {
    if is_digest(s) {
        Some(Reference::Digest(s.to_string()))
    } else if is_tag(s) {
        Some(Reference::Tag(s.to_string()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const D: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn routes() {
        assert_eq!(parse(&format!("alice/demo/blobs/{D}")), Some(("alice/demo".into(), Op::Blob { digest: D.into() })));
        assert_eq!(parse("alice/demo/blobs/uploads/"), Some(("alice/demo".into(), Op::UploadStart)));
        assert_eq!(parse("alice/demo/blobs/uploads"), Some(("alice/demo".into(), Op::UploadStart)));
        assert_eq!(parse("alice/demo/blobs/uploads/9f1c"), Some(("alice/demo".into(), Op::Upload { id: "9f1c".into() })));
        assert_eq!(parse("alice/demo/manifests/latest"), Some(("alice/demo".into(), Op::Manifest { reference: "latest".into() })));
        assert_eq!(parse("alice/demo/tags/list"), Some(("alice/demo".into(), Op::TagsList)));
        assert_eq!(parse(&format!("alice/demo/referrers/{D}")), Some(("alice/demo".into(), Op::Referrers { digest: D.into() })));
        // A deeper name still parses; `split_name` is what refuses it.
        assert_eq!(parse("a/b/c/manifests/x").unwrap().0, "a/b/c");
    }

    #[test]
    fn not_routes() {
        for p in [
            "",
            "alice",
            "alice/demo",
            "alice/demo/blobs",
            "alice/demo/manifests",
            "alice/demo/tags/other",
            "/manifests/x",
            "alice//manifests/x",
            "blobs/x",
            "alice/demo/unknown/x",
        ] {
            assert_eq!(parse(p), None, "{p:?}");
        }
    }

    #[test]
    fn names() {
        assert_eq!(split_name("alice/demo"), Some(("alice", "demo")));
        assert_eq!(split_name("Alice-1/my.app_x-y"), Some(("Alice-1", "my.app_x-y")));
        assert_eq!(split_name("alice/my-app"), Some(("alice", "my-app")));
        assert_eq!(split_name("alice/my.app"), Some(("alice", "my.app")));
        for bad in [
            "alice",
            "alice/",
            "/demo",
            "a/b/c",
            "alice/Demo",
            "alice/-demo",
            "alice/demo-",
            "alice/de..mo",
            "alice/de mo",
            "-alice/demo",
            "alice/../demo",
            "alice/%2e%2e",
        ] {
            assert_eq!(split_name(bad), None, "{bad:?}");
        }
        assert_eq!(split_name(&format!("alice/{}", "a".repeat(101))), None);
    }

    #[test]
    fn tags_and_digests() {
        for good in ["latest", "v1.2.3", "_x", "A-b_c.d", &"a".repeat(128)] {
            assert!(is_tag(good), "{good}");
        }
        for bad in ["", ".x", "-x", "a b", "a/b", &"a".repeat(129), "sha256:abc"] {
            assert!(!is_tag(bad), "{bad}");
        }
        assert!(is_digest(D));
        for bad in ["sha256:ABC", "sha512:00", "sha256:", &D.to_uppercase(), &format!("{D}0"), "0123456789abcdef"] {
            assert!(!is_digest(bad), "{bad}");
        }
        assert_eq!(parse_reference(D), Some(Reference::Digest(D.into())));
        assert_eq!(parse_reference("latest"), Some(Reference::Tag("latest".into())));
        assert_eq!(parse_reference("not a ref"), None);
        assert_eq!(hex_of(D).len(), 64);
    }

    proptest! {
        #[test]
        fn parsing_never_panics(s in ".{0,200}") {
            let _ = parse(&s);
            let _ = split_name(&s);
            let _ = parse_reference(&s);
        }

        #[test]
        fn a_parsed_name_never_contains_a_marker_segment_boundary(a in "[a-z]{1,8}", b in "[a-z]{1,8}", tag in "[a-z0-9]{1,10}") {
            let (name, op) = parse(&format!("{a}/{b}/manifests/{tag}")).unwrap();
            prop_assert_eq!(name, format!("{a}/{b}"));
            prop_assert_eq!(op, Op::Manifest { reference: tag });
        }
    }
}
