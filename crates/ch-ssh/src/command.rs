//! What a client may ask for over SSH: `git-upload-pack`, `git-receive-pack` and
//! `git-upload-archive` on one repository, and nothing else.
//!
//! The command is parsed here and never handed to a shell, so quoting tricks have nothing to
//! work on. Both spellings git clients use are accepted (`git-upload-pack 'a/b.git'` and
//! `git upload-pack 'a/b.git'`).

use ch_core::services::GitWire;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommandLine {
    pub wire: GitWire,
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not one of the three git commands, or not valid UTF-8.
    NotGit,
    /// A git command with a path that is not `owner/repo`.
    BadPath,
}

/// Parses an exec request.
pub fn parse(data: &[u8]) -> Result<GitCommandLine, Refusal> {
    let text = std::str::from_utf8(data).map_err(|_| Refusal::NotGit)?;
    if text.len() > 512 || text.chars().any(|c| c.is_control()) {
        return Err(Refusal::NotGit);
    }
    let (program, rest) = text.trim().split_once(' ').ok_or(Refusal::NotGit)?;
    let (name, rest) = match program {
        "git" => rest.trim_start().split_once(' ').ok_or(Refusal::NotGit)?,
        other => (other.strip_prefix("git-").ok_or(Refusal::NotGit)?, rest),
    };
    let wire = match name {
        "upload-pack" => GitWire::UploadPack,
        "receive-pack" => GitWire::ReceivePack,
        "upload-archive" => GitWire::UploadArchive,
        _ => return Err(Refusal::NotGit),
    };
    let path = unquote(rest.trim()).ok_or(Refusal::BadPath)?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else { return Err(Refusal::BadPath) };
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && !s.starts_with('-')
            && s != "."
            && s != ".."
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    };
    if !ok(owner) || !ok(repo) {
        return Err(Refusal::BadPath);
    }
    Ok(GitCommandLine { wire, owner: owner.to_string(), repo: repo.to_string() })
}

/// One argument, bare or wrapped in matching single or double quotes. Anything with a quote
/// inside, or more than one argument, is refused.
fn unquote(s: &str) -> Option<&str> {
    let inner = if let Some(i) = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        i
    } else if let Some(i) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        i
    } else {
        s
    };
    if inner.is_empty() || inner.contains(['\'', '"', ' ', '\\', ';', '|', '&', '$', '`', '<', '>', '(', ')']) {
        return None;
    }
    Some(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ok(s: &str) -> GitCommandLine {
        parse(s.as_bytes()).unwrap_or_else(|e| panic!("{s:?}: {e:?}"))
    }

    #[test]
    fn accepted_spellings() {
        let a = ok("git-upload-pack '/alice/demo.git'");
        assert_eq!((a.wire, a.owner.as_str(), a.repo.as_str()), (GitWire::UploadPack, "alice", "demo"));
        assert_eq!(ok("git-receive-pack 'alice/demo'").wire, GitWire::ReceivePack);
        assert_eq!(ok("git upload-pack \"alice/demo.git\"").wire, GitWire::UploadPack);
        assert_eq!(ok("git-upload-archive /alice/demo.git").wire, GitWire::UploadArchive);
        assert_eq!(ok("git-upload-pack 'alice/my.repo-2_x.git'").repo, "my.repo-2_x");
    }

    #[test]
    fn everything_else_is_refused() {
        for bad in [
            "",
            "ls",
            "bash -c id",
            "git-upload-pack",
            "git-upload-pack ''",
            "git-upload-pack 'alice'",
            "git-upload-pack 'a/b/c'",
            "git-upload-pack '../etc'",
            "git-upload-pack 'alice/..'",
            "git-upload-pack 'alice/demo.git; id'",
            "git-upload-pack 'alice/demo' extra",
            "git-upload-pack $(id)",
            "git-upload-pack `id`",
            "git-upload-pack 'a lice/demo'",
            "git-lfs-authenticate 'alice/demo' download",
            "git daemon alice/demo",
            "git-upload-pack 'alice/demo'\nid",
            "scp -t /tmp",
            "git-upload-pack '/alice/demo.git' --upload-pack=evil",
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad:?} must be refused");
        }
        assert!(parse(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn a_leading_dash_is_never_a_name() {
        assert!(parse(b"git-upload-pack '--help/demo'").is_err());
        assert!(parse(b"git-upload-pack 'alice/-demo'").is_err());
    }

    proptest! {
        #[test]
        fn never_panics(data in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = parse(&data);
        }

        #[test]
        fn accepted_names_are_plain(owner in "[A-Za-z0-9._-]{1,20}", repo in "[A-Za-z0-9._-]{1,20}") {
            let cmd = format!("git-upload-pack '{owner}/{repo}.git'");
            if let Ok(p) = parse(cmd.as_bytes()) {
                prop_assert!(p.owner.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)));
                prop_assert!(p.owner != ".." && p.repo != "..");
            }
        }
    }
}
