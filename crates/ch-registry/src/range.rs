//! Parsing a single-range `Range: bytes=...` header for blob downloads (RFC 9110 section 14).

/// What a Range header asks for, once checked against the blob's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOutcome {
    /// No usable Range header: serve the whole blob with 200.
    Full,
    /// Serve bytes `start..=end` (both inclusive, `end` < size) with 206.
    Partial { start: u64, end: u64 },
    /// The range is well formed but starts at or past the end: answer 416.
    Unsatisfiable,
}

/// A run of ASCII digits as a number. Empty is `Ok(None)`; anything else, or overflow, is `Err`.
fn number(s: &str) -> Result<Option<u64>, ()> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    s.parse().map(Some).map_err(|_| ())
}

/// `header` is the raw Range header value, or `None` when absent. `size` is the blob size in
/// bytes. Anything that is not one well formed byte range is ignored (`Full`), as the RFC says a
/// server may; only a well formed range that starts past the end is an error.
pub fn resolve(header: Option<&str>, size: u64) -> RangeOutcome {
    let Some(spec) = header.map(str::trim).and_then(|h| h.strip_prefix("bytes=")) else { return RangeOutcome::Full };
    if spec.contains(',') {
        return RangeOutcome::Full;
    }
    let mut parts = spec.split('-');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else { return RangeOutcome::Full };
    let (Ok(first), Ok(last)) = (number(a), number(b)) else { return RangeOutcome::Full };
    match (first, last) {
        (None, None) => RangeOutcome::Full,
        (None, Some(n)) => {
            if n == 0 || size == 0 {
                RangeOutcome::Unsatisfiable
            } else {
                RangeOutcome::Partial { start: size.saturating_sub(n), end: size - 1 }
            }
        }
        (Some(start), last) => {
            if start >= size {
                return RangeOutcome::Unsatisfiable;
            }
            match last {
                None => RangeOutcome::Partial { start, end: size - 1 },
                Some(end) if end < start => RangeOutcome::Full,
                Some(end) => RangeOutcome::Partial { start, end: end.min(size - 1) },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RangeOutcome::{Full, Partial, Unsatisfiable};
    use super::*;

    fn r(h: &str, size: u64) -> RangeOutcome {
        resolve(Some(h), size)
    }

    #[test]
    fn absent_or_empty_is_the_whole_blob() {
        assert_eq!(resolve(None, 100), Full);
        assert_eq!(r("", 100), Full);
        assert_eq!(r("bytes=", 100), Full);
    }

    #[test]
    fn single_ranges() {
        assert_eq!(r("bytes=0-9", 100), Partial { start: 0, end: 9 });
        assert_eq!(r("bytes=10-", 100), Partial { start: 10, end: 99 });
        assert_eq!(r("bytes=0-0", 100), Partial { start: 0, end: 0 });
        assert_eq!(r("bytes=90-500", 100), Partial { start: 90, end: 99 });
        assert_eq!(r("bytes=99-", 100), Partial { start: 99, end: 99 });
        assert_eq!(r("  bytes=0-9  ", 100), Partial { start: 0, end: 9 });
    }

    #[test]
    fn suffix_ranges() {
        assert_eq!(r("bytes=-10", 100), Partial { start: 90, end: 99 });
        assert_eq!(r("bytes=-500", 100), Partial { start: 0, end: 99 });
        assert_eq!(r("bytes=-0", 100), Unsatisfiable);
    }

    #[test]
    fn past_the_end_is_unsatisfiable() {
        assert_eq!(r("bytes=100-", 100), Unsatisfiable);
        assert_eq!(r("bytes=100-110", 100), Unsatisfiable);
        assert_eq!(r("bytes=0-", 0), Unsatisfiable);
        assert_eq!(r("bytes=-5", 0), Unsatisfiable);
    }

    #[test]
    fn bad_syntax_is_ignored() {
        for h in [
            "bytes=5-2",
            "bytes=0-9,20-29",
            "items=0-9",
            "Bytes=0-9",
            "bytes=a-b",
            "bytes=1-2-3",
            "bytes=+1-5",
            "bytes=99999999999999999999-",
            "bytes=-",
            "bytes 0-9",
        ] {
            assert_eq!(r(h, 100), Full, "{h}");
        }
    }

    #[test]
    fn a_range_is_always_inside_the_blob() {
        for size in [1u64, 2, 100, 4096] {
            for start in 0..size.min(50) {
                for end in start..start + 10 {
                    assert_eq!(
                        r(&format!("bytes={start}-{end}"), size),
                        Partial { start, end: end.min(size - 1) },
                        "{start}-{end} of {size}"
                    );
                }
            }
        }
    }
}
