//! Text references: `@user`, `@org/team`, `#12`, `owner/repo#12`, commit shas and closing
//! keywords (`fixes #12`). Pure and hand written, so `ch-collab`, `ch-notify` and `ch-render`
//! agree on what a mention is (ARCHITECTURE.md section 4.5).
//!
//! Text inside fenced code blocks and inline code spans is ignored. The scanner works on
//! `char`s and never indexes into the middle of one, so any Unicode input is safe.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IssueRef {
    /// `Some((owner, repo))` for `owner/repo#12`, `None` for a bare `#12`.
    pub repo: Option<(String, String)>,
    pub number: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Refs {
    /// `@user`, as written, de-duplicated case-insensitively, first-seen order.
    pub mentions: Vec<String>,
    /// `@org/team` as `(org, team)`.
    pub team_mentions: Vec<(String, String)>,
    /// `#12` and `owner/repo#12`, including the ones that are also in `closes`.
    pub issues: Vec<IssueRef>,
    /// Lowercase hex shas, 7 to 40 characters.
    pub commits: Vec<String>,
    /// References that follow a closing keyword.
    pub closes: Vec<IssueRef>,
}

const KEYWORDS: &[&str] = &["close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved"];

pub fn refs(text: &str) -> Refs {
    let cs = mask_code(text);
    let mut out = Refs::default();
    let n = cs.len();
    let mut i = 0;
    while i < n {
        let c = cs[i];
        let prev = if i > 0 { Some(cs[i - 1]) } else { None };

        if c == '@'
            && prev.is_none_or(|p| !is_mention_blocker(p))
            && let Some((m, end)) = parse_mention(&cs, i + 1)
        {
            match m {
                Mention::User(u) => push_ci(&mut out.mentions, u),
                Mention::Team(o, t) => {
                    if !out.team_mentions.iter().any(|(a, b)| a.eq_ignore_ascii_case(&o) && b.eq_ignore_ascii_case(&t)) {
                        out.team_mentions.push((o, t));
                    }
                }
            }
            i = end;
            continue;
        }

        if c == '#'
            && prev.is_none_or(|p| !is_word(p))
            && let Some((number, end)) = parse_number(&cs, i + 1)
        {
            push_issue(&mut out.issues, IssueRef { repo: None, number });
            i = end;
            continue;
        }

        if is_word(c) {
            // `owner/repo#12` needs a clean start: not in the middle of a word, a path or a name.
            if prev.is_none_or(|p| !is_word(p) && !matches!(p, '-' | '.' | '/'))
                && let Some((r, end)) = parse_cross_ref(&cs, i)
            {
                push_issue(&mut out.issues, r);
                i = end;
                continue;
            }
            let end = word_end(&cs, i);
            let word: String = cs[i..end].iter().collect();
            if prev.is_none_or(|p| !is_word(p)) {
                if KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k))
                    && let Some((r, rend)) = parse_after_keyword(&cs, end)
                {
                    push_issue(&mut out.issues, r.clone());
                    push_issue(&mut out.closes, r);
                    i = rend;
                    continue;
                }
                if is_sha(&word) {
                    let lower = word.to_ascii_lowercase();
                    if !out.commits.contains(&lower) {
                        out.commits.push(lower);
                    }
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A character that stops a following `@` from being a mention (emails, urls, handles in words).
fn is_mention_blocker(c: char) -> bool {
    is_word(c) || matches!(c, '.' | '-' | '/')
}

fn word_end(cs: &[char], start: usize) -> usize {
    let mut j = start;
    while j < cs.len() && is_word(cs[j]) {
        j += 1;
    }
    j
}

fn push_ci(list: &mut Vec<String>, s: String) {
    if !list.iter().any(|x| x.eq_ignore_ascii_case(&s)) {
        list.push(s);
    }
}

fn push_issue(list: &mut Vec<IssueRef>, r: IssueRef) {
    let key = |x: &IssueRef| (x.repo.as_ref().map(|(o, n)| (o.to_ascii_lowercase(), n.to_ascii_lowercase())), x.number);
    if !list.iter().any(|x| key(x) == key(&r)) {
        list.push(r);
    }
}

enum Mention {
    User(String),
    Team(String, String),
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// A user or org name: 1 to 39 characters, ASCII alphanumeric or `-`, not starting or ending
/// with `-`. Returns the end index, after trimming trailing hyphens.
fn parse_name(cs: &[char], start: usize) -> Option<usize> {
    let mut j = start;
    while j < cs.len() && is_name_char(cs[j]) {
        j += 1;
    }
    while j > start && cs[j - 1] == '-' {
        j -= 1;
    }
    let len = j - start;
    if len == 0 || len > 39 || cs[start] == '-' {
        return None;
    }
    Some(j)
}

fn parse_mention(cs: &[char], start: usize) -> Option<(Mention, usize)> {
    let end = parse_name(cs, start)?;
    // After the name: not a word character. (A trimmed trailing hyphen ends the name.)
    if end < cs.len() && is_word(cs[end]) {
        return None;
    }
    let name: String = cs[start..end].iter().collect();
    if end < cs.len() && cs[end] == '/' {
        let mut j = end + 1;
        while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '-' || cs[j] == '_') {
            j += 1;
        }
        let slug_len = j - (end + 1);
        if (1..=100).contains(&slug_len) {
            let team: String = cs[end + 1..j].iter().collect();
            return Some((Mention::Team(name, team), j));
        }
    }
    Some((Mention::User(name), end))
}

/// Digits 1 to 9 long, no leading zero, not followed by a word character.
fn parse_number(cs: &[char], start: usize) -> Option<(i64, usize)> {
    let mut j = start;
    while j < cs.len() && cs[j].is_ascii_digit() {
        j += 1;
    }
    let len = j - start;
    if len == 0 || len > 9 || cs[start] == '0' {
        return None;
    }
    if j < cs.len() && is_word(cs[j]) {
        return None;
    }
    let n: i64 = cs[start..j].iter().collect::<String>().parse().ok()?;
    Some((n, j))
}

/// `owner/repo#12` starting at `start`.
fn parse_cross_ref(cs: &[char], start: usize) -> Option<(IssueRef, usize)> {
    let oend = parse_name(cs, start)?;
    if cs.get(oend) != Some(&'/') {
        return None;
    }
    let rstart = oend + 1;
    let mut rend = rstart;
    while rend < cs.len() && (cs[rend].is_ascii_alphanumeric() || matches!(cs[rend], '-' | '_' | '.')) {
        rend += 1;
    }
    let rlen = rend - rstart;
    if rlen == 0 || rlen > 100 || cs.get(rend) != Some(&'#') {
        return None;
    }
    let (number, end) = parse_number(cs, rend + 1)?;
    let owner: String = cs[start..oend].iter().collect();
    let repo: String = cs[rstart..rend].iter().collect();
    Some((IssueRef { repo: Some((owner, repo)), number }, end))
}

/// After a closing keyword: an optional `:`, one or more spaces or tabs, then a reference.
fn parse_after_keyword(cs: &[char], mut j: usize) -> Option<(IssueRef, usize)> {
    if cs.get(j) == Some(&':') {
        j += 1;
    }
    let ws_start = j;
    while j < cs.len() && matches!(cs[j], ' ' | '\t') {
        j += 1;
    }
    if j == ws_start {
        return None;
    }
    if cs.get(j) == Some(&'#') {
        let (number, end) = parse_number(cs, j + 1)?;
        return Some((IssueRef { repo: None, number }, end));
    }
    parse_cross_ref(cs, j)
}

/// 7 to 40 hex digits with at least one digit and at least one letter a to f.
fn is_sha(word: &str) -> bool {
    let len = word.len();
    (7..=40).contains(&len)
        && word.bytes().all(|b| b.is_ascii_hexdigit())
        && word.bytes().any(|b| b.is_ascii_digit())
        && word.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Replaces fenced code blocks and inline code spans with spaces, keeping newlines.
fn mask_code(text: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(text.len());
    let mut fence: Option<char> = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let trimmed = body.trim_start_matches([' ', '\t']);
        let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let run = fence_char.map(|f| trimmed.chars().take_while(|c| *c == f).count()).unwrap_or(0);
        let is_fence_line = fence_char.is_some() && run >= 3;
        match (fence, is_fence_line) {
            (None, true) => {
                fence = fence_char;
                blank(&mut out, line);
            }
            (Some(f), true) if Some(f) == fence_char => {
                fence = None;
                blank(&mut out, line);
            }
            (Some(_), _) => blank(&mut out, line),
            (None, false) => mask_inline(&mut out, line),
        }
    }
    out
}

fn blank(out: &mut Vec<char>, line: &str) {
    out.extend(line.chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
}

fn mask_inline(out: &mut Vec<char>, line: &str) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut open = 0;
        while i + open < chars.len() && chars[i + open] == '`' {
            open += 1;
        }
        // Find a closing run of exactly `open` backticks.
        let mut j = i + open;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let mut run = 0;
                while j + run < chars.len() && chars[j + run] == '`' {
                    run += 1;
                }
                if run == open {
                    close = Some(j + run);
                    break;
                }
                j += run;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out.extend(std::iter::repeat_n(' ', end - i));
                i = end;
            }
            None => {
                out.extend(std::iter::repeat_n('`', open));
                i += open;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bare(n: i64) -> IssueRef {
        IssueRef { repo: None, number: n }
    }
    fn cross(o: &str, r: &str, n: i64) -> IssueRef {
        IssueRef { repo: Some((o.into(), r.into())), number: n }
    }

    #[test]
    fn user_mentions() {
        let r = refs("hi @alice and @Bob-2, thanks @alice!");
        assert_eq!(r.mentions, vec!["alice", "Bob-2"]);
    }

    #[test]
    fn mentions_dedupe_case_insensitively_keeping_first_spelling() {
        assert_eq!(refs("@Alice @alice @ALICE").mentions, vec!["Alice"]);
    }

    #[test]
    fn emails_and_urls_are_not_mentions() {
        let r = refs("mail a@b.com or see https://x.io/@alice and foo.@bar and a-@b and x_@y");
        assert!(r.mentions.is_empty(), "{:?}", r.mentions);
    }

    #[test]
    fn mention_name_rules() {
        assert!(refs("@-bad").mentions.is_empty());
        assert!(refs("@user_name").mentions.is_empty());
        assert_eq!(refs("@a").mentions, vec!["a"]);
        assert_eq!(refs("@user-").mentions, vec!["user"]);
        assert_eq!(refs(&format!("@{}", "a".repeat(39))).mentions.len(), 1);
        assert!(refs(&format!("@{}", "a".repeat(40))).mentions.is_empty());
        assert!(refs("@").mentions.is_empty());
    }

    #[test]
    fn team_mentions() {
        let r = refs("ping @acme/core-team please, and @acme/dev_ops");
        assert_eq!(r.team_mentions, vec![("acme".to_string(), "core-team".to_string()), ("acme".to_string(), "dev_ops".to_string())]);
        assert!(r.mentions.is_empty());
        assert_eq!(refs("@acme/ alone").mentions, vec!["acme"]);
    }

    #[test]
    fn bare_issue_refs() {
        let r = refs("see #12, #3 and #12 again");
        assert_eq!(r.issues, vec![bare(12), bare(3)]);
    }

    #[test]
    fn issue_ref_number_rules() {
        assert!(refs("#0").issues.is_empty());
        assert!(refs("#012").issues.is_empty());
        assert!(refs("#1234567890").issues.is_empty());
        assert_eq!(refs("#123456789").issues, vec![bare(123_456_789)]);
        assert!(refs("a#12").issues.is_empty());
        assert!(refs("#12abc").issues.is_empty());
        assert!(refs("#").issues.is_empty());
        assert_eq!(refs("(#7)").issues, vec![bare(7)]);
    }

    #[test]
    fn cross_repo_refs_are_not_also_reported_bare() {
        let r = refs("see alice/demo#5 and #6");
        assert_eq!(r.issues, vec![cross("alice", "demo", 5), bare(6)]);
    }

    #[test]
    fn cross_repo_needs_a_clean_start() {
        assert!(refs("x/alice/demo#5").issues.is_empty());
        assert!(refs("a.alice/demo#5").issues.is_empty());
        assert_eq!(refs("alice/my.repo_x-1#9").issues, vec![cross("alice", "my.repo_x-1", 9)]);
    }

    #[test]
    fn commit_shas() {
        let r = refs("fixed in 1a2B3c4 and also da39a3ee5e6b4b0d3255bfef95601890afd80709.");
        assert_eq!(r.commits, vec!["1a2b3c4", "da39a3ee5e6b4b0d3255bfef95601890afd80709"]);
    }

    #[test]
    fn sha_needs_digit_and_letter_and_the_right_length() {
        assert!(refs("1234567").commits.is_empty(), "digits only is a number");
        assert!(refs("abcdefa").commits.is_empty(), "letters only is a word");
        assert!(refs("abc123").commits.is_empty(), "6 characters");
        assert!(refs(&format!("a1{}", "b".repeat(39))).commits.is_empty(), "41 characters");
        assert_eq!(refs(&format!("a1{}", "b".repeat(38))).commits.len(), 1, "40 characters");
        assert!(refs("xa1b2c3d").commits.is_empty(), "part of a longer word");
        assert!(refs("a1b2c3d_").commits.is_empty(), "underscore continues the word");
    }

    #[test]
    fn sha_in_a_commit_url() {
        let r = refs("https://code.example.com/alice/demo/commit/1a2b3c4d5e");
        assert_eq!(r.commits, vec!["1a2b3c4d5e"]);
    }

    #[test]
    fn closing_keywords() {
        let r = refs("Fixes #1, fixes #2 and closes: #3. Resolved alice/demo#4");
        assert_eq!(r.closes, vec![bare(1), bare(2), bare(3), cross("alice", "demo", 4)]);
        assert_eq!(r.issues, r.closes);
    }

    #[test]
    fn closing_keyword_forms_and_case() {
        for kw in ["close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved", "FIXES", "Closes"] {
            assert_eq!(refs(&format!("{kw} #9")).closes, vec![bare(9)], "{kw}");
            assert_eq!(refs(&format!("{kw}: #9")).closes, vec![bare(9)], "{kw}:");
        }
    }

    #[test]
    fn closing_keyword_needs_a_whole_word_and_a_space() {
        assert!(refs("prefix #9").closes.is_empty());
        assert!(refs("unfixes #9").closes.is_empty());
        assert!(refs("fixes#9").closes.is_empty());
        assert!(refs("fixes\n#9").closes.is_empty(), "same line only");
        assert_eq!(refs("fixes #9").issues, vec![bare(9)]);
        assert_eq!(refs("see #9 fixes").closes, vec![]);
    }

    #[test]
    fn fenced_code_is_ignored() {
        let t = "before @a #1\n```\n@b #2 fixes #3 1a2b3c4\n```\nafter @c #4\n~~~rust\n@d #5\n~~~\nend";
        let r = refs(t);
        assert_eq!(r.mentions, vec!["a", "c"]);
        assert_eq!(r.issues, vec![bare(1), bare(4)]);
        assert!(r.commits.is_empty());
        assert!(r.closes.is_empty());
    }

    #[test]
    fn a_fence_closes_only_on_its_own_kind() {
        let r = refs("```\n@in\n~~~\n@still-in\n```\n@out");
        assert_eq!(r.mentions, vec!["out"]);
    }

    #[test]
    fn unclosed_fence_hides_the_rest() {
        assert!(refs("```\n@hidden").mentions.is_empty());
    }

    #[test]
    fn inline_code_is_ignored() {
        let r = refs("use `@alice #1` but @bob #2, and ``a ` @carol`` ok @dave");
        assert_eq!(r.mentions, vec!["bob", "dave"]);
        assert_eq!(r.issues, vec![bare(2)]);
    }

    #[test]
    fn an_unmatched_backtick_is_just_text() {
        assert_eq!(refs("a ` @alice").mentions, vec!["alice"]);
        assert_eq!(refs("``@alice`").mentions, vec!["alice"]);
    }

    #[test]
    fn unicode_never_panics_and_does_not_confuse_boundaries() {
        let r = refs("é@alice 日本語 @bob\u{200b}#1 😀#2 ñ#3 @ünï");
        assert_eq!(r.mentions, vec!["bob"]);
        assert_eq!(r.issues, vec![bare(1), bare(2)]);
    }

    #[test]
    fn empty_and_plain_text() {
        assert_eq!(refs(""), Refs::default());
        assert_eq!(refs("nothing to see here"), Refs::default());
    }

    proptest! {
        #[test]
        fn never_panics(s in "\\PC*") {
            let _ = refs(&s);
        }

        #[test]
        fn never_panics_on_reference_heavy_text(s in "[@#/`~a-f0-9 \\n:.\\-_xfiesclosr]{0,120}") {
            let _ = refs(&s);
        }

        #[test]
        fn results_are_always_well_formed(s in "[@#/`~a-f0-9 \\n:.\\-_xfiesclosr]{0,120}") {
            let r = refs(&s);
            for i in r.issues.iter().chain(r.closes.iter()) {
                prop_assert!(i.number >= 1);
            }
            for c in &r.commits {
                prop_assert!((7..=40).contains(&c.len()));
                prop_assert!(c.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            }
            for c in &r.closes {
                prop_assert!(r.issues.contains(c));
            }
        }
    }
}
