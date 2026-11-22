//! Secret masking. Every secret value, and every line of a multi-line one, is replaced with
//! `***` in each log line before the line leaves the runner. `::add-mask::` adds values while
//! a job runs.

use std::sync::RwLock;

pub const MASK: &str = "***";

/// Shared by every log writer of one job.
#[derive(Default)]
pub struct Masker {
    /// Longest first, so a secret that contains another one is masked whole.
    values: RwLock<Vec<String>>,
}

impl Masker {
    pub fn new() -> Masker {
        Masker::default()
    }

    /// Adds a secret. A multi-line value adds each non-blank line as well, because logs are
    /// masked line by line and the whole value never appears in one line.
    pub fn add(&self, secret: &str) {
        let mut candidates: Vec<&str> = vec![secret];
        if secret.contains('\n') || secret.contains('\r') {
            candidates.extend(secret.split(['\n', '\r']));
        }
        let Ok(mut values) = self.values.write() else { return };
        for c in candidates {
            // Trimming keeps a trailing newline in a PEM key from defeating the mask, and a
            // blank value masks nothing.
            let c = c.trim();
            if c.is_empty() || values.iter().any(|v| v == c) {
                continue;
            }
            values.push(c.to_string());
        }
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    }

    pub fn mask(&self, line: &str) -> String {
        let Ok(values) = self.values.read() else { return line.to_string() };
        if values.is_empty() {
            return line.to_string();
        }
        let mut out = line.to_string();
        for v in values.iter() {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), MASK);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.values.read().map(|v| v.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_every_occurrence() {
        let m = Masker::new();
        m.add("hunter2");
        assert_eq!(m.mask("pw=hunter2 and again hunter2!"), "pw=*** and again ***!");
        assert_eq!(m.mask("nothing here"), "nothing here");
    }

    #[test]
    fn masks_each_line_of_a_multiline_secret() {
        let m = Masker::new();
        m.add("-----BEGIN KEY-----\nabcDEF123\nxyz789\n-----END KEY-----\n");
        assert_eq!(m.mask("line: abcDEF123"), "line: ***");
        assert_eq!(m.mask("xyz789"), "***");
        assert_eq!(m.mask("-----BEGIN KEY-----"), "***");
        assert_eq!(m.mask("-----BEGIN KEY-----\r"), "***\r");
    }

    #[test]
    fn handles_windows_line_endings() {
        let m = Masker::new();
        m.add("one\r\ntwo");
        assert_eq!(m.mask("two"), "***");
        assert_eq!(m.mask("one"), "***");
    }

    #[test]
    fn blank_secrets_mask_nothing() {
        let m = Masker::new();
        m.add("");
        m.add("   \n  ");
        assert!(m.is_empty());
        assert_eq!(m.mask("anything at all"), "anything at all");
    }

    #[test]
    fn longer_secrets_win_over_their_substrings() {
        let m = Masker::new();
        m.add("pass");
        m.add("password123");
        assert_eq!(m.mask("x password123 y"), "x *** y");
        assert_eq!(m.mask("pass"), "***");
    }

    #[test]
    fn duplicates_are_kept_once() {
        let m = Masker::new();
        m.add("abc");
        m.add("abc");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn masking_is_idempotent() {
        let m = Masker::new();
        m.add("s3cr3t");
        let once = m.mask("tok=s3cr3t");
        assert_eq!(m.mask(&once), once);
    }
}
