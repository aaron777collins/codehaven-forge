//! ANSI colour sequences and runner log commands to HTML, for CI logs.
//!
//! Colours are classes (`ansi-fg-1`, `ansi-bg-4`, `ansi-bold`) because the page may not carry
//! inline styles. Every line comes back self contained: spans are closed at the end of the
//! line and the style is reopened at the start of the next.

use crate::escape_html;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Style {
    fg: Option<u8>,
    bg: Option<u8>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl Style {
    fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    fn classes(&self) -> String {
        let mut v = Vec::new();
        if let Some(f) = self.fg {
            v.push(format!("ansi-fg-{f}"));
        }
        if let Some(b) = self.bg {
            v.push(format!("ansi-bg-{b}"));
        }
        if self.bold {
            v.push("ansi-bold".to_string());
        }
        if self.dim {
            v.push("ansi-dim".to_string());
        }
        if self.italic {
            v.push("ansi-italic".to_string());
        }
        if self.underline {
            v.push("ansi-underline".to_string());
        }
        v.join(" ")
    }

    fn apply(&mut self, params: &[u32]) {
        let mut i = 0;
        if params.is_empty() {
            *self = Style::default();
            return;
        }
        while i < params.len() {
            match params[i] {
                0 => *self = Style::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                n @ 30..=37 => self.fg = Some((n - 30) as u8),
                n @ 90..=97 => self.fg = Some((n - 90 + 8) as u8),
                39 => self.fg = None,
                n @ 40..=47 => self.bg = Some((n - 40) as u8),
                n @ 100..=107 => self.bg = Some((n - 100 + 8) as u8),
                49 => self.bg = None,
                n @ (38 | 48) => {
                    let colour = match params.get(i + 1) {
                        Some(5) => {
                            let c = params.get(i + 2).map(|c| palette_256(*c as u8));
                            i += 2;
                            c
                        }
                        Some(2) => {
                            let c = match (params.get(i + 2), params.get(i + 3), params.get(i + 4)) {
                                (Some(r), Some(g), Some(b)) => Some(nearest16(*r as i32, *g as i32, *b as i32)),
                                _ => None,
                            };
                            i += 4;
                            c
                        }
                        _ => None,
                    };
                    if n == 38 {
                        self.fg = colour.or(self.fg);
                    } else {
                        self.bg = colour.or(self.bg);
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

const BASE16: [(i32, i32, i32); 16] = [
    (0, 0, 0),
    (205, 49, 49),
    (13, 188, 121),
    (229, 229, 16),
    (36, 114, 200),
    (188, 63, 188),
    (17, 168, 205),
    (229, 229, 229),
    (102, 102, 102),
    (241, 76, 76),
    (35, 209, 139),
    (245, 245, 67),
    (59, 142, 234),
    (214, 112, 214),
    (41, 184, 219),
    (255, 255, 255),
];

fn nearest16(r: i32, g: i32, b: i32) -> u8 {
    let mut best = (0u8, i32::MAX);
    for (i, (pr, pg, pb)) in BASE16.iter().enumerate() {
        let d = (r - pr).pow(2) + (g - pg).pow(2) + (b - pb).pow(2);
        if d < best.1 {
            best = (i as u8, d);
        }
    }
    best.0
}

fn palette_256(n: u8) -> u8 {
    match n {
        0..=15 => n,
        16..=231 => {
            let n = (n - 16) as i32;
            let level = |v: i32| if v == 0 { 0 } else { 55 + v * 40 };
            nearest16(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (n as i32 - 232) * 10;
            nearest16(v, v, v)
        }
    }
}

/// Pulls escape sequences out of a line. Returns plain text segments paired with the style in
/// force for each, after handling carriage-return overwrites.
fn segments(line: &str, style: &mut Style) -> Vec<(Style, String)> {
    // A bare carriage return means the rest of the text overwrites the line (progress bars).
    let line = line.rsplit('\r').next().unwrap_or(line);
    let mut out: Vec<(Style, String)> = Vec::new();
    let mut text = String::new();
    let mut chars = line.chars().peekable();
    let flush = |text: &mut String, style: &Style, out: &mut Vec<(Style, String)>| {
        if !text.is_empty() {
            out.push((style.clone(), std::mem::take(text)));
        }
    };
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            text.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut fin = None;
                for p in chars.by_ref() {
                    if ('@'..='~').contains(&p) {
                        fin = Some(p);
                        break;
                    }
                    params.push(p);
                }
                if fin == Some('m') {
                    flush(&mut text, style, &mut out);
                    let nums: Vec<u32> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
                    style.apply(&if params.is_empty() { Vec::new() } else { nums });
                }
            }
            Some(']') => {
                chars.next();
                // Operating system command: ends at BEL or ESC backslash.
                while let Some(p) = chars.next() {
                    if p == '\u{7}' {
                        break;
                    }
                    if p == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut text, style, &mut out);
    out
}

fn line_html(line: &str, style: &mut Style) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    // Runner commands.
    for (prefix, class) in [
        ("##[error]", "log-error"),
        ("##[warning]", "log-warning"),
        ("##[notice]", "log-notice"),
        ("##[debug]", "log-debug"),
        ("##[group]", "log-group"),
        ("::error::", "log-error"),
        ("::warning::", "log-warning"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return format!("<span class=\"{class}\">{}</span>", escape_html(rest));
        }
    }
    if line.starts_with("##[endgroup]") {
        return String::new();
    }
    // The style at the start of the line is what the previous line left.
    let mut out = String::new();
    for (st, text) in segments(line, style) {
        let esc = escape_html(&text);
        if st.is_plain() {
            out.push_str(&esc);
        } else {
            out.push_str(&format!("<span class=\"{}\">{esc}</span>", st.classes()));
        }
    }
    out
}

/// One html string per source line. The carried style makes a colour that spans lines work.
pub fn to_lines(text: &str) -> Vec<String> {
    let mut style = Style::default();
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.into_iter().map(|l| line_html(l, &mut style)).collect()
}

pub fn to_html(text: &str) -> String {
    to_lines(text).join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balanced(s: &str) -> bool {
        s.matches("<span").count() == s.matches("</span>").count()
    }

    #[test]
    fn colours_and_reset() {
        assert_eq!(to_html("\u{1b}[31mred\u{1b}[0m plain"), "<span class=\"ansi-fg-1\">red</span> plain");
        assert_eq!(to_html("\u{1b}[92mgreen\u{1b}[39m"), "<span class=\"ansi-fg-10\">green</span>");
    }

    #[test]
    fn bold_and_colour_combine_on_one_span() {
        assert_eq!(to_html("\u{1b}[1;34mhi\u{1b}[0m"), "<span class=\"ansi-fg-4 ansi-bold\">hi</span>");
        assert_eq!(to_html("\u{1b}[44mbg\u{1b}[0m"), "<span class=\"ansi-bg-4\">bg</span>");
    }

    #[test]
    fn extended_colours_map_to_the_sixteen() {
        assert_eq!(to_html("\u{1b}[38;5;1mx"), "<span class=\"ansi-fg-1\">x</span>");
        let hi = to_html("\u{1b}[38;5;196mx");
        assert!(hi.contains("ansi-fg-"), "{hi}");
        let tc = to_html("\u{1b}[38;2;255;0;0mx");
        assert!(tc.contains("ansi-fg-9") || tc.contains("ansi-fg-1"), "{tc}");
    }

    #[test]
    fn carriage_return_keeps_the_last_overwrite() {
        assert_eq!(to_html("10%\r50%\r100% done"), "100% done");
        assert_eq!(to_lines("a\r\nb"), vec!["a", "b"]);
    }

    #[test]
    fn other_escapes_are_dropped() {
        assert_eq!(to_html("a\u{1b}[2Kb\u{1b}]0;title\u{7}c"), "abc");
        assert_eq!(to_html("x\u{1b}"), "x");
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(to_html("<script>alert(1)</script> & \"q\""), "&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;q&quot;");
    }

    #[test]
    fn style_carries_across_lines_but_every_line_is_balanced() {
        let lines = to_lines("\u{1b}[32mone\ntwo\u{1b}[0m\nthree");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| balanced(l)), "{lines:?}");
        assert!(lines[1].contains("ansi-fg-2"));
        assert!(!lines[2].contains("<span"));
    }

    #[test]
    fn runner_commands() {
        assert_eq!(
            to_lines("##[group]Set up\nbody\n##[endgroup]\n##[error]boom <b>\n##[warning]careful\n::error::bad"),
            vec![
                "<span class=\"log-group\">Set up</span>",
                "body",
                "",
                "<span class=\"log-error\">boom &lt;b&gt;</span>",
                "<span class=\"log-warning\">careful</span>",
                "<span class=\"log-error\">bad</span>",
            ]
        );
    }

    #[test]
    fn line_count_matches_the_source() {
        assert_eq!(to_lines("a\nb\nc\n").len(), 3);
        assert_eq!(to_lines("").len(), 0);
    }
}
