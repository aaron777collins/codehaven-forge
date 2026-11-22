//! Disk checks. The runner refuses new jobs when a guarded file system is nearly full, because
//! a full disk takes down every service that shares it (ARCHITECTURE.md section 2.5).

use std::path::Path;

/// Percent of the file system under `path` that is in use, the way `df` counts it (blocks
/// available to ordinary users against the total they could ever have).
pub fn usage_percent(path: &Path) -> std::io::Result<u8> {
    let st = rustix::fs::statvfs(path).map_err(std::io::Error::from)?;
    Ok(percent(st.f_blocks, st.f_bfree, st.f_bavail))
}

pub fn percent(blocks: u64, free: u64, avail: u64) -> u8 {
    let used = blocks.saturating_sub(free);
    let usable = used + avail;
    if usable == 0 {
        return 0;
    }
    (((used as f64 / usable as f64) * 100.0).ceil() as u64).min(100) as u8
}

/// The highest usage among the guarded paths, and which path it is. Paths that do not exist
/// are skipped: a guard on a mount that is not there must not stop the runner.
pub fn worst(paths: &[std::path::PathBuf]) -> Option<(std::path::PathBuf, u8)> {
    paths.iter().filter_map(|p| usage_percent(p).ok().map(|u| (p.clone(), u))).max_by_key(|(_, u)| *u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_math() {
        assert_eq!(percent(1000, 500, 500), 50);
        assert_eq!(percent(1000, 100, 50), 95, "root-reserved blocks count as used, like df");
        assert_eq!(percent(0, 0, 0), 0);
        assert_eq!(percent(1000, 0, 0), 100);
        assert_eq!(percent(1000, 1000, 1000), 0);
    }

    #[test]
    fn a_real_directory_reports_something_sane() {
        let dir = tempfile::tempdir().unwrap();
        let u = usage_percent(dir.path()).unwrap();
        assert!(u <= 100);
    }

    #[test]
    fn missing_paths_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let w = worst(&[dir.path().join("nope"), dir.path().to_path_buf()]);
        assert_eq!(w.unwrap().0, dir.path());
        assert!(worst(&[dir.path().join("nope")]).is_none());
    }
}
