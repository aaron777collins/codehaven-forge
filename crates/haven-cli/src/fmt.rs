//! Plain text output helpers: tables, relative times, one-line summaries.

use chrono::{DateTime, Utc};

/// Renders rows as aligned columns separated by two spaces. No header rule, no colours, so
/// the output pipes cleanly into `cut` and `grep`.
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            line.push_str(c);
            if i + 1 < r.len() {
                let pad = widths[i].saturating_sub(c.chars().count()) + 2;
                line.push_str(&" ".repeat(pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// "3 minutes ago", "2 days ago", "just now".
pub fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    if secs < 0 {
        return "just now".into();
    }
    let (n, unit) = match secs {
        0..=44 => return "just now".into(),
        45..=3_599 => ((secs + 30) / 60, "minute"),
        3_600..=86_399 => ((secs + 1_800) / 3_600, "hour"),
        86_400..=2_591_999 => ((secs + 43_200) / 86_400, "day"),
        2_592_000..=31_535_999 => ((secs + 1_296_000) / 2_592_000, "month"),
        _ => ((secs + 15_768_000) / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Cuts a string to `max` characters, ending in `...` when it was cut. Takes the first line only.
pub fn truncate(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() <= max {
        first.to_string()
    } else {
        let cut: String = first.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

/// `1.4 MB`, `812 B`.
pub fn bytes(n: i64) -> String {
    let n = n.max(0) as f64;
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{} B", n as i64) } else { format!("{v:.1} {}", UNITS[i]) }
}

/// A status word for a run or job: the conclusion once it has one, otherwise the status.
pub fn state_word(status: Option<&str>, conclusion: Option<&str>) -> String {
    match (status, conclusion) {
        (_, Some(c)) if !c.is_empty() => c.to_string(),
        (Some(s), _) if !s.is_empty() => s.to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn tables_align() {
        let rows = vec![vec!["a".to_string(), "bbb".into(), "c".into()], vec!["dddd".into(), "e".into(), "f".into()]];
        assert_eq!(table(&rows), "a     bbb  c\ndddd  e    f\n");
    }

    #[test]
    fn relative_times() {
        let now = t("2026-01-10T12:00:00Z");
        assert_eq!(ago(t("2026-01-10T11:59:50Z"), now), "just now");
        assert_eq!(ago(t("2026-01-10T11:58:00Z"), now), "2 minutes ago");
        assert_eq!(ago(t("2026-01-10T11:00:00Z"), now), "1 hour ago");
        assert_eq!(ago(t("2026-01-08T12:00:00Z"), now), "2 days ago");
        assert_eq!(ago(t("2025-10-10T12:00:00Z"), now), "3 months ago");
        assert_eq!(ago(t("2026-01-11T12:00:00Z"), now), "just now");
    }

    #[test]
    fn truncation_and_sizes() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a longer title here", 10), "a longe...");
        assert_eq!(truncate("line one\nline two", 40), "line one");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(1_468_006), "1.4 MB");
    }

    #[test]
    fn state_words() {
        assert_eq!(state_word(Some("completed"), Some("success")), "success");
        assert_eq!(state_word(Some("in_progress"), None), "in_progress");
        assert_eq!(state_word(None, None), "unknown");
    }
}
