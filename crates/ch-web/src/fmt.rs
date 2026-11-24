//! Small formatting helpers the views share: relative times, label colours, status kinds,
//! plurals. Everything here is pure and tested, so templates stay dumb.

use chrono::{DateTime, Datelike, Utc};

/// A time ready for a `<time>` element.
#[derive(Debug, Clone, Default)]
pub struct TimeView {
    pub iso: String,
    pub ago: String,
    pub title: String,
}

pub fn time(t: DateTime<Utc>) -> TimeView {
    time_at(t, Utc::now())
}

pub fn time_opt(t: Option<DateTime<Utc>>) -> Option<TimeView> {
    t.map(time)
}

pub fn time_at(t: DateTime<Utc>, now: DateTime<Utc>) -> TimeView {
    TimeView { iso: t.to_rfc3339(), ago: ago(t, now), title: t.format("%a %b %-d, %Y at %H:%M UTC").to_string() }
}

/// "3 hours ago", "yesterday", "on Mar 4", and for a time still to come "in 5 days". A time a
/// few seconds ahead reads as "just now" because clocks on different machines drift.
pub fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    if secs < -45 {
        return until(-secs, then, now);
    }
    if secs < 45 {
        return "just now".into();
    }
    let mins = (secs + 30) / 60;
    if secs < 90 {
        return "a minute ago".into();
    }
    if mins < 45 {
        return format!("{mins} minutes ago");
    }
    let hours = (secs + 1800) / 3600;
    if secs < 5400 {
        return "an hour ago".into();
    }
    if hours < 22 {
        return format!("{hours} hours ago");
    }
    if hours < 36 {
        return "yesterday".into();
    }
    let days = (secs + 43_200) / 86_400;
    if days < 26 {
        return format!("{days} days ago");
    }
    if then.year() == now.year() { then.format("on %b %-d").to_string() } else { then.format("on %b %-d, %Y").to_string() }
}

fn until(secs: i64, then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let (mins, hours, days) = ((secs + 30) / 60, (secs + 1800) / 3600, (secs + 43_200) / 86_400);
    if secs < 5400 {
        return if mins <= 1 { "in a minute".into() } else { format!("in {mins} minutes") };
    }
    if hours < 22 {
        return format!("in {hours} hours");
    }
    if hours < 36 {
        return "tomorrow".into();
    }
    if days < 26 {
        return format!("in {days} days");
    }
    if then.year() == now.year() { then.format("on %b %-d").to_string() } else { then.format("on %b %-d, %Y").to_string() }
}

/// "1m 23s", "2h 5m", "45s".
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn duration_between(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> Option<String> {
    let (s, e) = (start?, end.unwrap_or_else(Utc::now));
    Some(duration((e - s).num_seconds()))
}

pub fn plural(n: i64, one: &str, many: &str) -> String {
    if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") }
}

/// A title as HTML: escaped, with `backtick` spans drawn as code, the way issue and pull
/// request titles read on GitHub. An unpaired backtick stays as it is.
pub fn title_html(title: &str) -> String {
    fn esc(s: &str, out: &mut String) {
        for ch in s.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                c => out.push(c),
            }
        }
    }
    let mut out = String::with_capacity(title.len() + 16);
    let mut rest = title;
    while let Some(open) = rest.find('`') {
        esc(&rest[..open], &mut out);
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) if close > 0 => {
                out.push_str("<code>");
                esc(&after[..close], &mut out);
                out.push_str("</code>");
                rest = &after[close + 1..];
            }
            _ => {
                out.push('`');
                rest = after;
            }
        }
    }
    esc(rest, &mut out);
    out
}

pub fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The first line of a commit message.
pub fn title_of(message: &str) -> String {
    message.lines().next().unwrap_or("").trim().to_string()
}

/// Everything after the first line, trimmed.
pub fn body_of(message: &str) -> String {
    message.split_once('\n').map(|(_, b)| b.trim().to_string()).unwrap_or_default()
}

pub fn initials(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into())
}

/// Percent-encodes a path segment or query value.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encodes a path but keeps its slashes.
pub fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

/// The colour classes a label can have. CSP forbids inline styles, so a label's hex colour
/// is mapped to the nearest hue family and the stylesheet defines each family as `lbl-0` to
/// `lbl-11`: red, orange, yellow, lime, green, teal, cyan, blue, indigo, purple, pink, slate.
pub fn label_class(hex: &str) -> String {
    let h = hex.trim_start_matches('#');
    let parse = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok());
    let (Some(r), Some(g), Some(b)) = (parse(0), parse(2), parse(4)) else { return "lbl-11".into() };
    let (rf, gf, bf) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let (mx, mn) = (rf.max(gf).max(bf), rf.min(gf).min(bf));
    let chroma = mx - mn;
    // Greys and near greys are slate, whatever their exact value.
    if chroma < 0.1 {
        return "lbl-11".into();
    }
    let hue = if mx == rf {
        60.0 * (((gf - bf) / chroma).rem_euclid(6.0))
    } else if mx == gf {
        60.0 * ((bf - rf) / chroma + 2.0)
    } else {
        60.0 * ((rf - gf) / chroma + 4.0)
    };
    let class = match hue {
        h if !(15.0..345.0).contains(&h) => 0,
        h if h < 40.0 => 1,
        h if h < 65.0 => 2,
        h if h < 100.0 => 3,
        h if h < 160.0 => 4,
        h if h < 185.0 => 5,
        h if h < 205.0 => 6,
        h if h < 250.0 => 7,
        h if h < 275.0 => 8,
        h if h < 310.0 => 9,
        _ => 10,
    };
    format!("lbl-{class}")
}

/// How a run, job, step or check is drawn.
#[derive(Debug, Clone, Default)]
pub struct StatusView {
    /// `ok`, `fail`, `run`, `queued`, `skip` or `cancel`: a CSS class.
    pub class: String,
    pub icon: String,
    pub label: String,
}

pub fn status(status: &str, conclusion: Option<&str>) -> StatusView {
    let (class, icon, label) = match (status, conclusion) {
        (_, Some("success")) | ("success", _) => ("ok", "circle-check", "Passed"),
        (_, Some("failure")) | ("failure", _) | ("error", _) | (_, Some("timed_out")) => ("fail", "circle-x", "Failed"),
        (_, Some("cancelled")) | ("cancelled", _) => ("cancel", "ban", "Cancelled"),
        (_, Some("skipped")) | ("skipped", _) | (_, Some("neutral")) => ("skip", "circle-slash", "Skipped"),
        ("in_progress", _) | ("running", _) => ("run", "loader", "Running"),
        ("queued", _) | ("pending", _) | ("waiting", _) => ("queued", "circle-dashed", "Queued"),
        (_, Some(_)) => ("skip", "circle-slash", "Finished"),
        _ => ("queued", "circle-dashed", "Queued"),
    };
    StatusView { class: class.into(), icon: icon.into(), label: label.into() }
}

pub fn is_active(status: &str) -> bool {
    matches!(status, "queued" | "in_progress" | "running" | "pending" | "waiting")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn ago_reads_naturally() {
        let now = at(2026, 6, 15, 12);
        assert_eq!(ago(now - Duration::seconds(10), now), "just now");
        assert_eq!(ago(now + Duration::seconds(30), now), "just now");
        assert_eq!(ago(now - Duration::seconds(60), now), "a minute ago");
        assert_eq!(ago(now - Duration::minutes(10), now), "10 minutes ago");
        assert_eq!(ago(now - Duration::minutes(60), now), "an hour ago");
        assert_eq!(ago(now - Duration::hours(5), now), "5 hours ago");
        assert_eq!(ago(now - Duration::hours(28), now), "yesterday");
        assert_eq!(ago(now - Duration::days(4), now), "4 days ago");
        assert_eq!(ago(at(2026, 3, 4, 8), now), "on Mar 4");
        assert_eq!(ago(at(2024, 3, 4, 8), now), "on Mar 4, 2024");
    }

    #[test]
    fn times_still_to_come_read_as_in() {
        let now = at(2026, 6, 15, 12);
        assert_eq!(ago(now + Duration::minutes(10), now), "in 10 minutes");
        assert_eq!(ago(now + Duration::hours(5), now), "in 5 hours");
        assert_eq!(ago(now + Duration::hours(28), now), "tomorrow");
        assert_eq!(ago(now + Duration::days(9), now), "in 9 days");
        assert_eq!(ago(now + Duration::days(120), now), "on Oct 13");
        assert_eq!(ago(now + Duration::days(400), now), "on Jul 20, 2027");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(83), "1m 23s");
        assert_eq!(duration(7500), "2h 5m");
        assert_eq!(duration(-3), "0s");
    }

    #[test]
    fn plurals_and_shas() {
        assert_eq!(plural(1, "commit", "commits"), "1 commit");
        assert_eq!(plural(0, "commit", "commits"), "0 commits");
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(title_of("Fix it\n\nlong body"), "Fix it");
        assert_eq!(body_of("Fix it\n\nlong body\n"), "long body");
        assert_eq!(body_of("only a title"), "");
    }

    #[test]
    fn encoding() {
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
        assert_eq!(enc_path("dir one/file #1.rs"), "dir%20one/file%20%231.rs");
    }

    #[test]
    fn label_colours_map_to_the_palette() {
        assert_eq!(label_class("d73a4a"), "lbl-0");
        assert_eq!(label_class("#0e8a16"), "lbl-4");
        assert_eq!(label_class("0075ca"), "lbl-7");
        assert_eq!(label_class("cccccc"), "lbl-11");
        assert_eq!(label_class("nonsense"), "lbl-11");
    }

    #[test]
    fn statuses() {
        assert_eq!(status("completed", Some("success")).class, "ok");
        assert_eq!(status("completed", Some("failure")).class, "fail");
        assert_eq!(status("in_progress", None).class, "run");
        assert_eq!(status("queued", None).class, "queued");
        assert_eq!(status("completed", Some("cancelled")).class, "cancel");
        assert!(is_active("queued") && !is_active("completed"));
    }

    #[test]
    fn titles_draw_backtick_spans_as_code_and_escape_the_rest() {
        assert_eq!(title_html("Fix `ch-web` <b>"), "Fix <code>ch-web</code> &lt;b&gt;");
        assert_eq!(title_html("a `b` and `c`"), "a <code>b</code> and <code>c</code>");
        assert_eq!(title_html("one ` alone"), "one ` alone");
        assert_eq!(title_html("empty `` pair"), "empty `` pair");
        assert_eq!(title_html("`<script>`"), "<code>&lt;script&gt;</code>");
        assert_eq!(title_html("plain"), "plain");
    }
}
