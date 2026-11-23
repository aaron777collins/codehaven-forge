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
use std::path::{Component, Path, PathBuf};

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
        let not_regular = || invalid(format!("{} is not a regular file", file.display()));
        let fd = open_if_regular(&parent, last).map_err(|e| if e.kind() == io::ErrorKind::Unsupported { not_regular() } else { e })?;
        let st = fstat(&fd)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(not_regular());
        }
        // Reads of a regular file never block, so the non-blocking flag can stay.
        Ok(File::from(fd))
    }

    /// Opens `name` in `parent` for reading only when it is a regular file, so a device node or
    /// FIFO the job made is never opened at all (opening one can have effects of its own). On
    /// Linux the name is first opened as a bare handle (`O_PATH`, which opens nothing), checked,
    /// and only then reopened through that handle, so nothing can be swapped in between. A
    /// non-regular file comes back as `Unsupported`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn open_if_regular(parent: &OwnedFd, name: &OsString) -> io::Result<OwnedFd> {
        use std::os::fd::AsRawFd;
        let path = openat(parent, name.as_os_str(), OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        let st = fstat(&path)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        let fd = openat(
            rustix::fs::CWD,
            format!("/proc/self/fd/{}", path.as_raw_fd()),
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let again = fstat(&fd)?;
        if (again.st_dev, again.st_ino) != (st.st_dev, st.st_ino) {
            return Err(invalid("the file changed while it was opened".into()));
        }
        Ok(fd)
    }

    /// Elsewhere there is no `O_PATH`: the name is checked without following it, opened
    /// non-blocking and checked again by handle. Only Linux runs jobs; this keeps tests working.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn open_if_regular(parent: &OwnedFd, name: &OsString) -> io::Result<OwnedFd> {
        let st = statat(parent, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        let fd = openat(parent, name.as_os_str(), OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?;
        let again = fstat(&fd)?;
        if (again.st_dev, again.st_ino) != (st.st_dev, st.st_ino) {
            return Err(invalid("the file changed while it was opened".into()));
        }
        Ok(fd)
    }

    /// Every regular file below `root`, relative to it and sorted. Each directory is opened from
    /// its parent's handle with `O_NOFOLLOW`, so a directory the job swaps for a link mid-walk is
    /// skipped, never followed out of the tree (ch-ctk.1). Unreadable or vanished entries are
    /// skipped; more than `max` files is an error.
    pub fn walk_files(root: &Path, max: usize) -> io::Result<Vec<PathBuf>> {
        let fd = open_root(root)?;
        let mut out = Vec::new();
        walk(&fd, &PathBuf::new(), 0, max, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk(fd: &OwnedFd, rel: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) -> io::Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        let names = match entries(fd) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound) => return Ok(()),
            Err(e) => return Err(e),
        };
        for n in names {
            let st = match statat(fd, n.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::ACCESS) => continue,
                Err(e) => return Err(e.into()),
            };
            match FileType::from_raw_mode(st.st_mode) {
                FileType::RegularFile => {
                    out.push(rel.join(&n));
                    if out.len() > max {
                        return Err(invalid(format!("more than {max} files")));
                    }
                }
                FileType::Directory => match openat(fd, n.as_os_str(), DIR, Mode::empty()) {
                    Ok(sub) => walk(&sub, &rel.join(&n), depth + 1, max, out)?,
                    // Gone, unreadable, or swapped for a link or a file since the stat.
                    Err(rustix::io::Errno::NOENT | rustix::io::Errno::ACCESS | rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {}
                    Err(e) => return Err(e.into()),
                },
                _ => {}
            }
        }
        Ok(())
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

/// There is no way to open a path one component at a time without following links here, and a
/// check of the path before using it can be raced. So every operation is refused: the runner
/// runs jobs on Linux only, and fails closed anywhere else (ch-ctk.1).
#[cfg(not(unix))]
mod imp {
    use super::*;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "safe file access inside a job directory needs a Unix system")
    }
    pub fn create_dir_all(_root: &Path, _dir: &Path, _mode: u32) -> io::Result<()> {
        Err(unsupported())
    }
    pub fn fresh_dir(_root: &Path, _dir: &Path, _mode: u32) -> io::Result<()> {
        Err(unsupported())
    }
    pub fn create_new(_root: &Path, _file: &Path, _mode: u32) -> io::Result<File> {
        Err(unsupported())
    }
    pub fn open_regular(_root: &Path, _file: &Path) -> io::Result<File> {
        Err(unsupported())
    }
    pub fn walk_files(_root: &Path, _max: usize) -> io::Result<Vec<PathBuf>> {
        Err(unsupported())
    }
    pub fn move_into(_root: &Path, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(unsupported())
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

/// Reads `file` as `open_regular` opens it. A file over `max` bytes is refused with
/// `FileTooLarge` before anything is read, so a sparse file of any size cannot fill the runner's
/// memory.
pub fn read_to_string(root: &Path, file: &Path, max: u64) -> io::Result<String> {
    let f = open_regular(root, file)?;
    let too_large = || io::Error::new(io::ErrorKind::FileTooLarge, format!("{} is larger than {max} bytes", file.display()));
    if f.metadata()?.len() > max {
        return Err(too_large());
    }
    let mut s = String::new();
    // The job may still be appending to it.
    f.take(max + 1).read_to_string(&mut s)?;
    if s.len() as u64 > max {
        return Err(too_large());
    }
    Ok(s)
}

/// Every regular file below `root`, as paths relative to it, sorted. `root` must not be a link,
/// links below it are skipped, and no directory is entered through a link, even one swapped in
/// during the walk. More than `max` files is an error.
pub fn walk_files(root: &Path, max: usize) -> io::Result<Vec<std::path::PathBuf>> {
    imp::walk_files(root, max)
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
        assert!(read_to_string(root.path(), &root.path().join("d/f"), 1 << 20).is_err());
        assert!(read_to_string(root.path(), &root.path().join("l"), 1 << 20).is_err());
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
        assert!(read_to_string(root.path(), &p, 1 << 20).is_err());
    }

    #[test]
    fn paths_outside_the_root_are_refused() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_to_string(root.path(), Path::new("/etc/hostname"), 1 << 20).is_err());
        assert!(read_to_string(root.path(), &root.path().join("a/../../b"), 1 << 20).is_err());
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

    /// A FIFO or device node is refused before it is opened at all (inventory G2). A writer
    /// blocked on opening a FIFO wakes the moment a reader opens it, so it shows whether
    /// `open_regular` opened it.
    #[test]
    fn a_fifo_is_never_opened() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("fifo");
        rustix::fs::mknodat(rustix::fs::CWD, &p, rustix::fs::FileType::Fifo, rustix::fs::Mode::from_raw_mode(0o644), 0).unwrap();
        let opened = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (p, opened) = (p.clone(), opened.clone());
            std::thread::spawn(move || {
                let f = std::fs::OpenOptions::new().write(true).open(&p);
                opened.store(true, std::sync::atomic::Ordering::SeqCst);
                drop(f);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(open_regular(root.path(), &p).is_err());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let was_opened = opened.load(std::sync::atomic::Ordering::SeqCst);
        // Let the writer finish whatever happened.
        let _reader = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32).open(&p)
        };
        writer.join().unwrap();
        assert!(!was_opened, "open_regular opened the FIFO before refusing it");
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("big");
        std::fs::File::create(&p).unwrap().set_len(1 << 30).unwrap();
        let e = read_to_string(root.path(), &p, 1 << 20).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge);
        std::fs::write(&p, "small").unwrap();
        assert_eq!(read_to_string(root.path(), &p, 1 << 20).unwrap(), "small");
    }

    #[test]
    fn walking_lists_regular_files_and_skips_links() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("secret"), "x").unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/f"), "x").unwrap();
        std::fs::write(root.path().join("top"), "x").unwrap();
        symlink(out.path(), root.path().join("a/l")).unwrap();
        symlink(out.path().join("secret"), root.path().join("fl")).unwrap();
        let got = walk_files(root.path(), 100).unwrap();
        assert_eq!(got, vec![PathBuf::from("a/b/f"), PathBuf::from("top")]);
        assert!(walk_files(root.path(), 1).is_err(), "more files than allowed");
        let linked = tempfile::tempdir().unwrap();
        symlink(root.path(), linked.path().join("ws")).unwrap();
        assert!(walk_files(&linked.path().join("ws"), 100).is_err(), "a root that is a link is refused");
    }
}
