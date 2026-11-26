//! Parsing a single-range `Range: bytes=...` header for blob and release downloads (RFC 9110 section 14),
//! and cutting a byte stream down to the range.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};

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

/// What a download of a stored file answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serve {
    /// `HEAD`: the headers a GET would send, no body.
    Head,
    /// A well formed range that starts past the end: 416.
    Unsatisfiable,
    /// The whole file, 200.
    Full,
    /// Bytes `start..=end`, 206.
    Partial { start: u64, end: u64 },
}

impl Serve {
    /// Decides the answer from the method and the `Range` header. A header that is not one well
    /// formed range is ignored (the whole file is sent), as RFC 9110 allows.
    pub fn plan(range: Option<&str>, size: u64, head: bool) -> Serve {
        if head {
            return Serve::Head;
        }
        match resolve(range, size) {
            RangeOutcome::Full => Serve::Full,
            RangeOutcome::Partial { start, end } => Serve::Partial { start, end },
            RangeOutcome::Unsatisfiable => Serve::Unsatisfiable,
        }
    }

    /// Whether the file has to be opened to answer.
    pub fn needs_body(self) -> bool {
        matches!(self, Serve::Full | Serve::Partial { .. })
    }
}

/// The answer to a download of `name` (`size` bytes, SHA-256 `sha256`). `stream` is the whole
/// file and is read only for `Full` and `Partial`. The name goes into `Content-Disposition` with
/// quotes and control characters removed, and `nosniff` stops a browser from guessing a type.
pub fn file_response(
    name: &str,
    size: u64,
    sha256: &str,
    serve: Serve,
    stream: Option<BoxStream<'static, std::io::Result<Bytes>>>,
) -> Response {
    let (status, body, len, content_range) = match (serve, stream) {
        (Serve::Unsatisfiable, _) => (StatusCode::RANGE_NOT_SATISFIABLE, Body::empty(), 0, Some(format!("bytes */{size}"))),
        (Serve::Full, Some(s)) => (StatusCode::OK, Body::from_stream(s), size, None),
        (Serve::Partial { start, end }, Some(s)) => {
            let len = end - start + 1;
            (StatusCode::PARTIAL_CONTENT, Body::from_stream(slice_stream(s, start, len)), len, Some(format!("bytes {start}-{end}/{size}")))
        }
        // Head, or a body that was not opened.
        _ => (StatusCode::OK, Body::empty(), size, None),
    };
    let mut r = Response::new(body);
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if status != StatusCode::RANGE_NOT_SATISFIABLE
        && let Ok(v) = HeaderValue::from_str(&len.to_string())
    {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Some(cr) = content_range
        && let Ok(v) = HeaderValue::from_str(&cr)
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&crate::disposition::attachment(name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("\"{sha256}\"")) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    r
}

/// Keeps `len` bytes of a stream after skipping the first `start`, and stops reading once it has
/// them, so a range at the front of a large file does not read the rest of it.
pub fn slice_stream(
    stream: BoxStream<'static, std::io::Result<Bytes>>,
    start: u64,
    len: u64,
) -> BoxStream<'static, std::io::Result<Bytes>> {
    stream
        .scan((start, len), |(skip, left), item| {
            if *left == 0 {
                return std::future::ready(None);
            }
            let out = match item {
                Err(e) => {
                    *left = 0;
                    Some(Err(e))
                }
                Ok(mut chunk) => {
                    let n = chunk.len() as u64;
                    if *skip >= n {
                        *skip -= n;
                        None
                    } else {
                        let mut piece = chunk.split_off(usize::try_from(*skip).unwrap_or(0));
                        *skip = 0;
                        piece.truncate(usize::try_from(*left).unwrap_or(usize::MAX));
                        *left -= piece.len() as u64;
                        Some(Ok(piece))
                    }
                }
            };
            std::future::ready(Some(out))
        })
        .filter_map(std::future::ready)
        .boxed()
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
