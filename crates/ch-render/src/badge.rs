//! Workflow status badges: a two-part flat SVG, the workflow's name on the left and its status on
//! the right (ch-c1d). Pure functions, no IO, attributes only so a strict CSP allows it.

use crate::escape_html;

/// What a run's conclusion reads as on a badge, and its colour.
pub fn status(conclusion: Option<&str>) -> (&'static str, &'static str) {
    match conclusion {
        Some("success") => ("passing", "#1a7f37"),
        Some("failure" | "timed_out" | "startup_failure") => ("failing", "#cf222e"),
        Some("cancelled") => ("cancelled", "#57606a"),
        Some("skipped" | "neutral") => ("skipped", "#57606a"),
        Some(_) => ("unknown", "#57606a"),
        None => ("no status", "#57606a"),
    }
}

/// Width in pixels of `s` in the badge font: 7 a character, plus padding on both sides.
fn width(s: &str) -> usize {
    s.chars().count() * 7 + 12
}

/// The badge. `label` is cut at 60 characters so a long workflow name cannot make a huge image.
pub fn svg(label: &str, conclusion: Option<&str>) -> String {
    let label: String = label.chars().take(60).collect();
    let (message, colour) = status(conclusion);
    let (lw, mw) = (width(&label), width(message));
    let total = lw + mw;
    let (l, m) = (escape_html(&label), escape_html(message));
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total}\" height=\"20\" role=\"img\" aria-label=\"{l}: {m}\">\
<title>{l}: {m}</title>\
<rect width=\"{lw}\" height=\"20\" fill=\"#24292f\"/>\
<rect x=\"{lw}\" width=\"{mw}\" height=\"20\" fill=\"{colour}\"/>\
<g fill=\"#ffffff\" font-family=\"Verdana,DejaVu Sans,sans-serif\" font-size=\"11\" text-anchor=\"middle\">\
<text x=\"{lx}\" y=\"14\">{l}</text><text x=\"{mx}\" y=\"14\">{m}</text></g></svg>",
        lx = lw / 2,
        mx = lw + mw / 2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_badge_names_the_workflow_and_its_status() {
        let s = svg("ci", Some("success"));
        assert!(s.starts_with("<svg") && s.ends_with("</svg>"));
        assert!(s.contains(">ci</text>") && s.contains(">passing</text>") && s.contains("#1a7f37"), "{s}");
        assert!(svg("ci", Some("timed_out")).contains(">failing</text>"));
        assert!(svg("ci", None).contains(">no status</text>"));
    }

    #[test]
    fn names_are_escaped_and_cut() {
        let s = svg("<script>\"x\"</script>", Some("failure"));
        assert!(!s.contains("<script>") && s.contains("&lt;script&gt;"), "{s}");
        assert!(!s.contains("style="));
        let long = svg(&"a".repeat(500), None);
        assert!(!long.contains(&"a".repeat(61)));
    }
}
