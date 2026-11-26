//! The same guard as `ch-workflow`'s `yaml_guard` (`ch-core` may not depend on it, so it is kept
//! here too, and both carry the same tests). What every YAML file read from a repository goes through before and while the parser has it
//! (ch-qop). The parser recurses with the nesting of the file, and a stack overflow aborts the
//! whole server, so the guard is layered:
//!
//! 1. a size cap ([`crate::issue_form::MAX_FORM_BYTES`]);
//! 2. [`normalise`]: every line break YAML knows (CRLF, lone CR, NEL, LS, PS) becomes LF, once,
//!    and the scan and the parser are both handed that same text, so they cannot disagree about
//!    where a line ends;
//! 3. the nesting scan in `issue_form.rs`;
//! 4. [`on_parser_stack`]: the parse, and the walk over what it built, run on a thread whose stack
//!    is large enough for the deepest file the size cap allows, even if the scan let one through.

use std::borrow::Cow;

/// Stack the parser thread gets per byte of the size cap. Measured on `marked-yaml` 0.8 in a test
/// build: the worst shape (a chain of `? ` or `: ` markers, 2 bytes per level) needs about 1.1 KiB
/// of stack per byte of input, 288 MiB for a 256 KiB file (72 MiB for 64 KiB). 5 KiB per byte is 4 times that and
/// more. It is address space, not memory: pages are used only as the parser goes deeper.
pub(crate) const STACK_PER_CAP_BYTE: usize = 5 * 1024;

/// The file with every line break YAML recognises turned into one LF, and a leading byte order
/// mark dropped. CRLF is one break, not two.
pub(crate) fn normalise(src: &str) -> Cow<'_, str> {
    let body = src.trim_start_matches('\u{feff}');
    if !body.contains(['\r', '\u{85}', '\u{2028}', '\u{2029}']) {
        return Cow::Borrowed(body);
    }
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{85}' | '\u{2028}' | '\u{2029}' => out.push('\n'),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Runs `f` on a thread whose stack fits the deepest file of `cap` bytes, and returns what it
/// returned. The error is a plain sentence: the thread could not start, or `f` panicked. Neither
/// takes the process down.
pub(crate) fn on_parser_stack<T: Send>(cap: usize, f: impl FnOnce() -> T + Send) -> Result<T, String> {
    on_stack(cap.saturating_mul(STACK_PER_CAP_BYTE), f)
}

fn on_stack<T: Send>(stack: usize, f: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .name("yaml-parse".into())
            .stack_size(stack)
            .spawn_scoped(s, f)
            .map_err(|e| format!("the file could not be read right now: {e}"))?;
        handle.join().map_err(|_| "the file could not be read".to_string())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::issue_form::MAX_FORM_BYTES as MAX_FILE_BYTES;
    use marked_yaml::LoaderOptions;

    #[test]
    fn every_line_break_becomes_one_lf() {
        assert_eq!(normalise("a\rb\r\nc\nd\u{85}e\u{2028}f\u{2029}g\r\r\nh"), "a\nb\nc\nd\ne\nf\ng\n\nh");
        assert_eq!(normalise("\u{feff}\u{feff}a: 1\n"), "a: 1\n");
        assert!(matches!(normalise("a: 1\nb: 2\n"), Cow::Borrowed(_)), "nothing to change, nothing copied");
    }

    #[test]
    fn a_thread_that_panics_or_cannot_start_is_an_error_not_an_abort() {
        assert_eq!(on_stack(1 << 20, || 7).unwrap(), 7);
        assert!(on_stack(1 << 20, || -> u8 { panic!("boom") }).is_err());
        assert!(on_stack(usize::MAX / 2, || 7).is_err(), "an impossible stack is refused, quietly");
    }

    /// The files that make the parser recurse deepest for their size: one level per two bytes.
    /// These are what the scan is there to refuse. Here they go straight to the parser.
    fn deepest_files(cap: usize) -> Vec<(&'static str, String)> {
        let cap = cap - 16; // room for the head and tail
        vec![
            ("dash", "- ".repeat(cap / 2) + "x\n"),
            ("question", "? ".repeat(cap / 2) + "x\n"),
            ("colon", ": ".repeat(cap / 2) + "x\n"),
            ("dash question", "- ? ".repeat(cap / 4) + "x\n"),
            ("question colon", "? : ".repeat(cap / 4) + "x\n"),
            ("dash colon", "- : ".repeat(cap / 4) + "x\n"),
            ("tab dash", "-\t".repeat(cap / 2) + "x\n"),
            ("flow", "[".repeat(cap / 2) + &"]".repeat(cap / 2) + "\n"),
            ("braces", "{a: ".repeat(cap / 8) + &"}".repeat(cap / 8) + "\n"),
            ("mixed", "- ? [ {a: ".repeat(cap / 20) + "x\n"),
            ("CR chain", "k:\r".to_string() + &"- ".repeat(cap / 2) + "x\n"),
        ]
    }

    /// Parses every deepest file straight on a stack of `stack` bytes, as the guard would if the
    /// scan had let it through. An overflow aborts the process, so returning is the proof.
    fn parse_all_on(stack: usize) {
        for (name, text) in deepest_files(MAX_FILE_BYTES) {
            assert!(text.len() <= MAX_FILE_BYTES, "{name} must fit the cap");
            on_stack(stack, || {
                let opts = LoaderOptions::default().error_on_duplicate_keys(true).prevent_coercion(true);
                let _ = marked_yaml::parse_yaml_with_options(0, &text, opts);
            })
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn the_parser_thread_survives_the_deepest_file_the_cap_allows() {
        parse_all_on(MAX_FILE_BYTES * STACK_PER_CAP_BYTE);
    }

    /// Runs this test binary again with the parser stack cut by `div`, in a child process so that
    /// an overflow ends the child and not the whole run. True if it came through.
    fn survives_with_stack_divided_by(div: usize) -> bool {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "yaml_guard::tests::stack_child", "--include-ignored", "--test-threads=1"])
            .env("CH_YAML_GUARD_STACK_DIV", div.to_string())
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    #[ignore = "run by the two tests below, in a child process"]
    fn stack_child() {
        let div: usize = std::env::var("CH_YAML_GUARD_STACK_DIV").unwrap().parse().unwrap();
        parse_all_on(MAX_FILE_BYTES * STACK_PER_CAP_BYTE / div);
    }

    /// The same files on a quarter of the stack. The guard has at least 4 times the stack it
    /// needs, and a `marked-yaml` upgrade that makes its frames bigger shows up here first.
    #[test]
    fn the_parser_stack_has_four_times_the_headroom_it_needs() {
        assert!(survives_with_stack_divided_by(4));
    }

    /// The control: on a sixty-fourth of the stack the same files do overflow, so the two tests
    /// above are measuring something.
    #[test]
    fn the_deepest_files_do_overflow_a_stack_that_is_too_small() {
        assert!(!survives_with_stack_divided_by(64));
    }
}
