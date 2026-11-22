//! The `.runner` file: what `register` learned and `run` needs. Mode 0600, written atomically.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_FILE: &str = ".runner";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// Where the server is, as given to `register` (no trailing slash).
    pub server_url: String,
    pub runner_id: i64,
    pub name: String,
    pub labels: Vec<String>,
    /// The bearer credential. Shown once by the server, kept only here.
    pub credential: String,
}

// A derived Debug would put the credential in a log line.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("server_url", &self.server_url)
            .field("runner_id", &self.runner_id)
            .field("name", &self.name)
            .field("labels", &self.labels)
            .field("credential", &"***")
            .finish()
    }
}

impl Credential {
    pub fn load(path: &Path) -> Result<Credential> {
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}. Run `haven-runner register` first", path.display()))?;
        let c: Credential = serde_json::from_str(&s).with_context(|| format!("{} is not a runner credential file", path.display()))?;
        if c.credential.is_empty() || c.server_url.is_empty() {
            bail!("{} is incomplete. Register again", path.display());
        }
        Ok(c)
    }

    /// Writes the file with mode 0600 from the first byte, then renames it into place so a crash
    /// never leaves half a credential.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

/// Where the credential lives when `--credential-file` is not given: `.runner` in the current
/// directory, or `$HAVEN_RUNNER_HOME/.runner`.
pub fn default_path() -> PathBuf {
    match std::env::var_os("HAVEN_RUNNER_HOME").filter(|v| !v.is_empty()) {
        Some(h) => PathBuf::from(h).join(DEFAULT_FILE),
        None => PathBuf::from(DEFAULT_FILE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credential {
        Credential {
            server_url: "https://h.example".into(),
            runner_id: 7,
            name: "runner-1".into(),
            labels: vec!["linux".into()],
            credential: "chr_secret".into(),
        }
    }

    #[test]
    fn round_trip_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join(".runner");
        sample().save(&p).unwrap();
        assert_eq!(Credential::load(&p).unwrap(), sample());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!p.with_extension("tmp").exists());
    }

    #[test]
    fn overwriting_keeps_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".runner");
        sample().save(&p).unwrap();
        let mut c = sample();
        c.runner_id = 8;
        c.save(&p).unwrap();
        assert_eq!(Credential::load(&p).unwrap().runner_id, 8);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn debug_hides_the_secret() {
        let s = format!("{:?}", sample());
        assert!(!s.contains("chr_secret"));
        assert!(s.contains("runner-1"));
    }

    #[test]
    fn load_errors_are_helpful() {
        let dir = tempfile::tempdir().unwrap();
        let e = format!("{:#}", Credential::load(&dir.path().join("missing")).unwrap_err());
        assert!(e.contains("register"));
        let p = dir.path().join("bad");
        std::fs::write(&p, "{}").unwrap();
        assert!(Credential::load(&p).is_err());
        std::fs::write(&p, r#"{"server_url":"","runner_id":1,"name":"x","labels":[],"credential":""}"#).unwrap();
        assert!(format!("{:#}", Credential::load(&p).unwrap_err()).contains("incomplete"));
    }
}
