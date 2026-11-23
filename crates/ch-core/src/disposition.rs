//! `Content-Disposition` for downloads (ch-yci). A name with characters outside printable ASCII is
//! sent twice, as RFC 6266 says: an ASCII stand-in in `filename`, and the real name, UTF-8 and
//! percent-encoded, in `filename*`. Browsers that know `filename*` (all current ones) use it, so a
//! non-ASCII name no longer arrives as mojibake. Quotes, backslashes and control characters never
//! reach the header, so a name cannot add a parameter or a header line.

use std::fmt::Write;

/// The header value that saves a download as `name`.
pub fn attachment(name: &str) -> String {
    if name.trim().is_empty() {
        return "attachment; filename=\"download\"".into();
    }
    let fallback: String = name.chars().map(|c| if (' '..='~').contains(&c) && c != '"' && c != '\\' { c } else { '_' }).collect();
    if fallback == name {
        return format!("attachment; filename=\"{fallback}\"");
    }
    let mut encoded = String::with_capacity(name.len() * 3);
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            let _ = write!(encoded, "%{b:02X}");
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::attachment;

    #[test]
    fn plain_names_stay_plain() {
        assert_eq!(attachment("app-1.2.tar.gz"), "attachment; filename=\"app-1.2.tar.gz\"");
        assert_eq!(attachment("notes v2.txt"), "attachment; filename=\"notes v2.txt\"");
    }

    #[test]
    fn non_ascii_names_get_an_encoded_copy() {
        assert_eq!(attachment("résumé.pdf"), "attachment; filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf");
        assert_eq!(attachment("日本.txt"), "attachment; filename=\"__.txt\"; filename*=UTF-8''%E6%97%A5%E6%9C%AC.txt");
    }

    #[test]
    fn nothing_can_break_out_of_the_header() {
        for bad in ["a\"b", "a\\b", "a\r\nSet-Cookie: x=1", "a;b=c"] {
            let v = attachment(bad);
            assert!(!v.contains('\r') && !v.contains('\n'), "{v}");
            assert!(axum::http::HeaderValue::from_str(&v).is_ok(), "{v}");
            let quoted = v.split('"').nth(1).unwrap();
            assert!(!quoted.contains('\\'), "{v}");
        }
        assert_eq!(attachment(""), "attachment; filename=\"download\"");
    }
}
