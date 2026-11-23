//! File access inside a job directory that never follows a symbolic link (ch-ctk.1).
//!
//! The job's container can write anywhere in its directory, the runner's part included, and it
//! can do so while the runner works there. So every path the runner opens below the job root is
//! walked one component at a time from a directory handle, each opened with `O_NOFOLLOW`. A link
//! anywhere on the way is an error, never a detour, and a file is read only when it is a regular
//! file (a FIFO cannot make the runner hang). Files the runner writes are made new with
//! `O_EXCL`: whatever the job left at that name is removed first, never written through.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path};

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// The plain names from `root` down to `path`. `path` must be `root` or lie under it, spelled
/// without `.` or `..`.
fn names_below(root: &Path, path: &Path) -> io::Result<Vec<OsString>> {
    let rel = path.strip_prefix(root).map_err(|_| invalid(format!("{} is not inside {}", path.display(), root.display())))?;
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(n.to_os_string()),
            _ => return Err(invalid(format!("{} is not a plain path inside {}", path.display(), root.display()))),
        }
    }
    Ok(out)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fchmod, fstat, mkdirat, openat, renameat, statat, unlinkat};
    use std::os::fd::OwnedFd;

    const DIR: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

    fn open_root(root: &Path) -> io::Result<OwnedFd> {
        Ok(openat(rustix::fs::CWD, root, DIR, Mode::empty())?)
    }

    /// Opens (and with `create`, makes) each directory in `names` below `fd`. A directory made
    /// here gets `mode` whatever the umask; one that existed keeps its own.
    fn descend(mut fd: OwnedFd, names: &[OsString], create: bool, mode: u32) -> io::Result<OwnedFd> {
        for n in names {
            let mut made = false;
            if create {
                match mkdirat(&fd, n.as_os_str(), Mode::from_raw_mode(mode as _)) {
                    Ok(()) => made = true,
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            fd = openat(&fd, n.as_os_str(), DIR, Mode::empty())?;
            if made {
                fchmod(&fd, Mode::from_raw_mode(mode as _))?;
            }
        }
        Ok(fd)
    }

    pub fn create_dir_all(root: &Path, dir: &Path, mode: u32) -> io::Result<()> {
        let names = names_below(root, dir)?;
        descend(open_root(root)?, &names, true, mode)?;
        Ok(())
    }

    /// Removes whatever is at `dir` unless it is a real directory, then makes it if missing.
    pub fn fresh_dir(root: &Path, dir: &Path, mode: u32) -> io::Result<()> {
        let names = names_below(root, dir)?;
        let Some((last, parents)) = names.split_last() else { return Err(invalid("no directory name".into())) };
        let parent = descend(open_root(root)?, parents, true, 0o755)?;
        match openat(&parent, last.as_os_str(), DIR, Mode::empty()) {
            Ok(_) => {}
            Err(rustix::io::Errno::NOENT) => {}
            // A link, a file or anything else that is not a plain directory: take it away.
            Err(_) => unlinkat(&parent, last.as_os_str(), AtFlags::empty())?,
        }
        let fd = descend(parent, std::slice::from_ref(last), true, mode)?;
        fchmod(&fd, Mode::from_raw_mode(mode as _))?;
        Ok(())
    }

    pub fn create_new(root: &Path, file: &Path, mode: u32) -> io::Result<File> {
        let names = names_below(root, file)?;
        let Some((last, parents)) = names.split_last() else { return Err(invalid("no file name".into())) };
        let parent = descend(open_root(root)?, parents, false, 0)?;
        match unlinkat(&parent, last.as_os_str(), AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(e) => return Err(e.into()),
        }
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = openat(&parent, last.as_os_str(), flags, Mode::from_raw_mode(mode as _))?;
        // The umask may have taken bits away.
        fchmod(&fd, Mode::from_raw_mode(mode as _))?;
        Ok(File::from(fd))
    }

    pub fn open_regular(root: &Path, file: &Path) -> io::Result<File> {
        let names = names_below(root, file)?;
        let Some((last, parents)) = names.split_last() else { return Err(invalid("no file name".into())) };
        let parent = descend(open_root(root)?, parents, false, 0)?;
        // Non-blocking so that opening a FIFO returns at once; the check below then refuses it.
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = openat(&parent, last.as_os_str(), flags, Mode::empty())?;
        let st = fstat(&fd)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(invalid(format!("{} is not a regular file", file.display())));
        }
        // Reads of a regular file never block, so the flag can stay.
        Ok(File::from(fd))
    }

    /// The names in the directory `fd`, without `.` and `..`, read before any is changed.
    fn entries(fd: &OwnedFd) -> io::Result<Vec<OsString>> {
        use std::os::unix::ffi::OsStringExt;
        let mut d = rustix::fs::Dir::read_from(fd)?;
        let mut out = Vec::new();
        while let Some(e) = d.read() {
            let n = e?.file_name().to_bytes().to_vec();
            if n != b"." && n != b".." {
                out.push(OsString::from_vec(n));
            }
        }
        Ok(out)
    }

    /// `Some(true)` for a real directory, `Some(false)` for anything else (a link included),
    /// `None` when nothing is there.
    fn kind_at(fd: &OwnedFd, name: &OsString) -> io::Result<Option<bool>> {
        match statat(fd, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(FileType::from_raw_mode(st.st_mode) == FileType::Directory)),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Removes `name` in `parent` and, when it is a real directory, everything in it. A link is
    /// removed itself; nothing is ever followed.
    fn remove_entry(parent: &OwnedFd, name: &OsString, depth: usize) -> io::Result<()> {
        if depth > MAX_DEPTH {
            return Err(invalid("the tree is too deep to remove".into()));
        }
        match openat(parent, name.as_os_str(), DIR, Mode::empty()) {
            Ok(fd) => {
                for n in entries(&fd)? {
                    remove_entry(&fd, &n, depth + 1)?;
                }
                drop(fd);
                Ok(unlinkat(parent, name.as_os_str(), AtFlags::REMOVEDIR)?)
            }
            Err(rustix::io::Errno::NOENT) => Ok(()),
            // A link (O_NOFOLLOW) or something that is not a directory.
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => match unlinkat(parent, name.as_os_str(), AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        }
    }

    const MAX_DEPTH: usize = 512;

    /// Puts `name` from `src` at `name` in `dst`: into the directory already there when both are
    /// real directories (and it is not `.git`), in place of whatever else is there otherwise.
    fn place(src: &OwnedFd, dst: &OwnedFd, name: &OsString, depth: usize) -> io::Result<()> {
        let src_dir = kind_at(src, name)?.unwrap_or(false);
        let git = name.as_os_str() == ".git";
        match kind_at(dst, name)? {
            None => {}
            Some(true) if src_dir && !git => {
                let from = openat(src, name.as_os_str(), DIR, Mode::empty())?;
                // Opening with O_NOFOLLOW: if the job swapped in a link since the check, this
                // fails and the next try replaces it.
                let to = openat(dst, name.as_os_str(), DIR, Mode::empty())?;
                return merge(&from, &to, depth + 1);
            }
            // Two non-directories: rename replaces the old one (a link itself, not its target).
            Some(false) if !src_dir => {}
            Some(_) => remove_entry(dst, name, depth)?,
        }
        Ok(renameat(src, name.as_os_str(), dst, name.as_os_str())?)
    }

    fn merge(src: &OwnedFd, dst: &OwnedFd, depth: usize) -> io::Result<()> {
        if depth > MAX_DEPTH {
            return Err(invalid("the tree is too deep to move".into()));
        }
        for name in entries(src)? {
            let mut tries = 0;
            loop {
                match place(src, dst, &name, depth) {
                    Ok(()) => break,
                    // Something in the job changed the name while it was being replaced.
                    Err(e)
                        if tries < 3
                            && matches!(
                                rustix::io::Errno::from_io_error(&e),
                                Some(
                                    rustix::io::Errno::EXIST
                                        | rustix::io::Errno::NOTEMPTY
                                        | rustix::io::Errno::ISDIR
                                        | rustix::io::Errno::NOTDIR
                                        | rustix::io::Errno::LOOP
                                        | rustix::io::Errno::NOENT
                                )
                            ) =>
                    {
                        tries += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    pub fn move_into(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
        let names = names_below(root, to)?;
        let dst = descend(open_root(root)?, &names, false, 0)?;
        let src = openat(rustix::fs::CWD, from, DIR, Mode::empty())?;
        merge(&src, &dst, 0)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    fn check(root: &Path, path: &Path) -> io::Result<()> {
        let mut p = root.to_path_buf();
        for n in names_below(root, path)? {
            p.push(n);
            if std::fs::symlink_metadata(&p).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                return Err(invalid(format!("{} is a link", p.display())));
            }
        }
        Ok(())
    }
    pub fn create_dir_all(root: &Path, dir: &Path, _mode: u32) -> io::Result<()> {
        check(root, dir)?;
        std::fs::create_dir_all(dir)
    }
    pub fn fresh_dir(root: &Path, dir: &Path, _mode: u32) -> io::Result<()> {
        if std::fs::symlink_metadata(dir).is_ok_and(|m| !m.is_dir()) {
            std::fs::remove_file(dir)?;
        }
        create_dir_all(root, dir, 0)
    }
    pub fn create_new(root: &Path, file: &Path, _mode: u32) -> io::Result<File> {
        check(root, file)?;
        let _ = std::fs::remove_file(file);
        std::fs::OpenOptions::new().write(true).create_new(true).open(file)
    }
    pub fn open_regular(root: &Path, file: &Path) -> io::Result<File> {
        check(root, file)?;
        let f = File::open(file)?;
        if !f.metadata()?.is_file() {
            return Err(invalid(format!("{} is not a regular file", file.display())));
        }
        Ok(f)
    }

    pub fn move_into(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
        check(root, to)?;
        merge(from, to)
    }
    fn merge(from: &Path, to: &Path) -> io::Result<()> {
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let (src, dst) = (e.path(), to.join(e.file_name()));
            let src_dir = e.file_type()?.is_dir();
            match std::fs::symlink_metadata(&dst) {
                Ok(m) if m.is_dir() && src_dir && e.file_name() != ".git" => {
                    merge(&src, &dst)?;
                    continue;
                }
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&dst)?,
                Ok(_) => std::fs::remove_file(&dst)?,
                Err(_) => {}
            }
            std::fs::rename(&src, &dst)?;
        }
        Ok(())
    }
}

/// Makes `dir` and every directory between `root` and it, refusing any link on the way. Those
/// it makes get `mode`; those that exist are left as they are.
pub fn create_dir_all(root: &Path, dir: &Path, mode: u32) -> io::Result<()> {
    imp::create_dir_all(root, dir, mode)
}

/// Makes `dir` a real directory with `mode`: a link or file the job put there is removed first.
/// The directories above it must not be links.
pub fn fresh_dir(root: &Path, dir: &Path, mode: u32) -> io::Result<()> {
    imp::fresh_dir(root, dir, mode)
}

/// A new, empty file at `file` with `mode`, replacing whatever was at that name. Its directory
/// must exist and no directory on the way may be a link.
pub fn create_new(root: &Path, file: &Path, mode: u32) -> io::Result<File> {
    imp::create_new(root, file, mode)
}

/// Writes `data` to a new file at `file`, as `create_new` does.
pub fn write_new(root: &Path, file: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let mut f = create_new(root, file, mode)?;
    f.write_all(data)?;
    f.flush()
}

/// Opens `file` for reading when it is a regular file reached without any link.
pub fn open_regular(root: &Path, file: &Path) -> io::Result<File> {
    imp::open_regular(root, file)
}

/// Reads `file` as `open_regular` opens it.
pub fn read_to_string(root: &Path, file: &Path) -> io::Result<String> {
    let mut s = String::new();
    open_regular(root, file)?.read_to_string(&mut s)?;
    Ok(s)
}

/// Moves everything in `from`, a directory the job cannot reach, into `to`, a directory under
/// `root`. A directory already in `to` is merged into; anything else at the same name (a file, a
/// link, a `.git`) is replaced whole. Every step goes through directory handles opened with
/// `O_NOFOLLOW`, so a link the job left or swaps in is replaced, never followed (ch-ctk.1).
/// `from` and `to` must be on the same filesystem.
pub fn move_into(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
    imp::move_into(root, from, to)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn a_link_on_the_way_or_at_the_end_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("f"), "outside").unwrap();
        symlink(out.path(), root.path().join("d")).unwrap();
        symlink(out.path().join("f"), root.path().join("l")).unwrap();
        assert!(read_to_string(root.path(), &root.path().join("d/f")).is_err());
        assert!(read_to_string(root.path(), &root.path().join("l")).is_err());
        assert!(create_new(root.path(), &root.path().join("d/f"), 0o644).is_err());
        assert!(create_dir_all(root.path(), &root.path().join("d/x"), 0o755).is_err());
        assert_eq!(std::fs::read_to_string(out.path().join("f")).unwrap(), "outside");
        assert!(!out.path().join("x").exists());
    }

    #[test]
    fn writing_replaces_a_planted_link_instead_of_following_it() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("f"), "outside").unwrap();
        symlink(out.path().join("f"), root.path().join("l")).unwrap();
        write_new(root.path(), &root.path().join("l"), b"mine", 0o640).unwrap();
        assert_eq!(std::fs::read_to_string(out.path().join("f")).unwrap(), "outside");
        assert_eq!(std::fs::read_to_string(root.path().join("l")).unwrap(), "mine");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(root.path().join("l")).unwrap().permissions().mode() & 0o777, 0o640);
        symlink(out.path(), root.path().join("dir")).unwrap();
        fresh_dir(root.path(), &root.path().join("dir"), 0o777).unwrap();
        assert!(std::fs::symlink_metadata(root.path().join("dir")).unwrap().is_dir());
    }

    #[test]
    fn a_fifo_is_not_read() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("fifo");
        rustix::fs::mknodat(rustix::fs::CWD, &p, rustix::fs::FileType::Fifo, rustix::fs::Mode::from_raw_mode(0o644), 0).unwrap();
        assert!(read_to_string(root.path(), &p).is_err());
    }

    #[test]
    fn paths_outside_the_root_are_refused() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_to_string(root.path(), Path::new("/etc/hostname")).is_err());
        assert!(read_to_string(root.path(), &root.path().join("a/../../b")).is_err());
    }

    #[test]
    fn moving_in_merges_directories_and_replaces_links_without_following_them() {
        let root = tempfile::tempdir().unwrap();
        let from = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (r, f) = (root.path(), from.path());
        std::fs::write(out.path().join("a"), "outside").unwrap();
        // What the checkout made.
        std::fs::create_dir_all(f.join("src/deep")).unwrap();
        std::fs::create_dir_all(f.join(".git/refs")).unwrap();
        std::fs::create_dir_all(f.join("docs")).unwrap();
        std::fs::write(f.join("src/deep/a"), "new").unwrap();
        std::fs::write(f.join("docs/a"), "new").unwrap();
        std::fs::write(f.join("a"), "new").unwrap();
        std::fs::write(f.join(".git/HEAD"), "ref").unwrap();
        // What the job left: a link at a file name, a link where a directory goes, an old .git
        // and a file of its own.
        std::fs::create_dir_all(r.join("ws/src")).unwrap();
        std::fs::create_dir_all(r.join("ws/.git/hooks")).unwrap();
        std::fs::write(r.join("ws/.git/hooks/post-checkout"), "evil").unwrap();
        std::fs::write(r.join("ws/src/mine"), "kept").unwrap();
        symlink(out.path().join("a"), r.join("ws/a")).unwrap();
        symlink(out.path(), r.join("ws/docs")).unwrap();
        move_into(r, f, &r.join("ws")).unwrap();
        assert_eq!(std::fs::read_to_string(out.path().join("a")).unwrap(), "outside");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1, "nothing written through a link");
        for (p, want) in [("ws/a", "new"), ("ws/docs/a", "new"), ("ws/src/deep/a", "new"), ("ws/src/mine", "kept"), ("ws/.git/HEAD", "ref")]
        {
            assert!(!std::fs::symlink_metadata(r.join(p)).unwrap().file_type().is_symlink(), "{p}");
            assert_eq!(std::fs::read_to_string(r.join(p)).unwrap(), want, "{p}");
        }
        assert!(!r.join("ws/.git/hooks").exists(), "an old .git is replaced whole, not merged");
        assert!(move_into(r, f, &r.join("nope/x")).is_err());
    }
}
