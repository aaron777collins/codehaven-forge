//! Markdown (GFM, sanitized), syntax highlighting with our own class palettes, ANSI to HTML
//! and generated SVG avatars. Pure functions.
//!
//! Skeleton from the foundation stream. The web stream builds it. See ARCHITECTURE.md
//! sections 1 and 12.

pub mod ansi;
pub mod avatar;
pub mod badge;
pub mod highlight;
pub mod markdown;

/// Escapes text for an HTML body or a quoted attribute. The one function every template
/// helper may rely on until the real renderer lands.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A heading anchor: lowercase, letters and digits kept, separators become single dashes.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if (c.is_whitespace() || c == '-' || c == '_') && !out.is_empty() {
            dash = true;
        }
        if dash && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// "0 B", "1.5 KB", "12 MB": base 1024, one decimal only below 10 of the unit.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[i]) } else { format!("{} {}", v.round() as u64, UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Getting Started"), "getting-started");
        assert_eq!(slug("  A -- b__c  "), "a-b-c");
        assert_eq!(slug("What's new?"), "whats-new");
        assert_eq!(slug("---"), "");
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(12 * 1024 * 1024), "12 MB");
        assert_eq!(human_bytes(1023), "1023 B");
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(escape_html("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
