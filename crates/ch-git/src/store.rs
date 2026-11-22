//! Where repositories live on disk and how a new one is made (ARCHITECTURE.md section 5.1).
//!
//! ```text
//! data/git/<shard>/<repo_id>.git     bare repositories, shard = repo_id % 256 in hex
//! data/git/_hooks/                   the shared hook scripts every repository points at
//! data/git/_run/                     the hook socket
//! data/git/_home/                    an empty HOME for git
//! data/git/_trash/                   deleted repositories, removed in the background
//! ```
//!
//! Names live only in Postgres, so rename and transfer never move anything.

use crate::command::{GitCommand, GitEnv};
use ch_core::{Error, RepoId, Result};
use std::path::{Path, PathBuf};

/// The probe script [`Store::exec_probe`] runs: it only has to execute, proving the hooks
/// directory is not on a noexec mount.
const EXEC_PROBE: &str = ".exec-probe";
const EXEC_PROBE_BODY: &str = "#!/bin/sh\nexit 0\n";

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
    pub hooks_dir: PathBuf,
    pub run_dir: PathBuf,
    pub trash_dir: PathBuf,
    pub env: GitEnv,
}

/// Settings every repository gets. The transport also passes them with `-c`, so a repository
/// whose config file was touched by hand still behaves.
pub fn repo_config(hooks_dir: &Path) -> Vec<(String, String)> {
    vec![
        ("core.hooksPath".into(), hooks_dir.to_string_lossy().into_owned()),
        // Objects a client sends are checked before they are accepted.
        ("receive.fsckObjects".into(), "true".into()),
        // Housekeeping is the server's job (a worker), not a side effect of someone's push.
        ("receive.autogc".into(), "false".into()),
        ("gc.auto".into(), "0".into()),
        ("uploadpack.allowFilter".into(), "true".into()),
        ("core.logAllRefUpdates".into(), "false".into()),
        ("pack.threads".into(), "2".into()),
        ("core.bigFileThreshold".into(), "50m".into()),
    ]
}

/// Writes an executable script atomically: to a temporary file beside the target, mode 755,
/// then rename, so a push that is running never reads half a script. Skipped only when the
/// content matches and no exec bit was lost, so an intact script is left alone while one that
/// was damaged is repaired.
fn write_exec_script(path: &Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 == 0o111)
        && std::fs::read_to_string(path).is_ok_and(|c| c == body)
    {
        return Ok(());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, path)
}

/// The one error `exec_probe` reports: which directory cannot run programs and the likely
/// cause, so the operator knows which mount to fix.
fn noexec_error(hooks_dir: &Path, e: std::io::Error) -> String {
    format!("the hooks directory {} cannot run programs ({e}). It may be on a noexec mount", hooks_dir.display())
}

impl Store {
    pub fn new(git_root: PathBuf) -> Self {
        let env = GitEnv::new(git_root.join("_home"));
        Self { hooks_dir: git_root.join("_hooks"), run_dir: git_root.join("_run"), trash_dir: git_root.join("_trash"), root: git_root, env }
    }

    /// Creates the support directories.
    pub fn init_dirs(&self) -> Result<()> {
        for d in [&self.root, &self.hooks_dir, &self.run_dir, &self.trash_dir, &self.env.home] {
            std::fs::create_dir_all(d).map_err(|e| Error::internal(anyhow::anyhow!("creating {}: {e}", d.display())))?;
        }
        Ok(())
    }

    pub fn repo_path(&self, id: RepoId) -> PathBuf {
        self.root.join(id.shard()).join(format!("{}.git", id.0))
    }

    /// A git command that runs in this repository.
    pub fn cmd(&self, id: RepoId) -> GitCommand {
        GitCommand::new(&self.env).git_dir(self.repo_path(id))
    }

    /// A git command with no repository, for `init` and friends.
    pub fn bare_cmd(&self) -> GitCommand {
        GitCommand::new(&self.env)
    }

    pub fn exists(&self, id: RepoId) -> bool {
        self.repo_path(id).join("HEAD").is_file()
    }

    /// Writes the two hook scripts, and the probe script `exec_probe` runs. Each hook execs
    /// this program with `hook <stage>`, which talks to the server over a socket. Rewritten at
    /// every start so a moved binary keeps working, and a script that lost its exec bit is
    /// repaired rather than left for git to skip.
    pub fn install_hooks(&self, program: &Path) -> std::io::Result<()> {
        let quoted = program.to_string_lossy().replace('\'', "'\\''");
        for stage in ["pre-receive", "post-receive"] {
            let body = format!("#!/bin/sh\nexec '{quoted}' hook {stage}\n");
            write_exec_script(&self.hooks_dir.join(stage), &body)?;
        }
        write_exec_script(&self.hooks_dir.join(EXEC_PROBE), EXEC_PROBE_BODY)
    }

    /// Spawns a tiny probe script that lives in `hooks_dir` (`.exec-probe`) and fails when it
    /// cannot run. Git silently skips a hook it cannot execute, so a noexec mount or a lost
    /// exec bit would turn branch protection off; the error will name the hooks directory.
    /// The probe is written when missing, so `codehaven doctor` works before the first start.
    pub fn exec_probe(&self) -> Result<(), String> {
        let probe = self.hooks_dir.join(EXEC_PROBE);
        if !probe.exists()
            && let Err(e) = write_exec_script(&probe, EXEC_PROBE_BODY)
        {
            return Err(noexec_error(&self.hooks_dir, e));
        }
        let ran = std::process::Command::new(&probe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env_clear()
            .status();
        // Only that the kernel agreed to run it matters, not what it printed or returned.
        ran.map(|_| ()).map_err(|e| noexec_error(&self.hooks_dir, e))
    }

    /// Lists every problem with the hook scripts: each of `pre-receive` and `post-receive`
    /// that is missing from `hooks_dir` or has lost its executable bit.
    pub fn hook_script_problems(&self) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let mut out = Vec::new();
        for stage in ["pre-receive", "post-receive"] {
            let path = self.hooks_dir.join(stage);
            match std::fs::metadata(&path) {
                Ok(m) if m.permissions().mode() & 0o111 == 0 => out.push(format!("hook script {} is not executable", path.display())),
                Ok(_) => {}
                Err(_) => out.push(format!("hook script {} is missing", path.display())),
            }
        }
        out
    }

    /// `hook_script_problems` and then `exec_probe`, first problem wins: the server can then
    /// refuse pushes instead of failing open when hook enforcement is broken.
    pub fn verify_hooks(&self) -> Result<(), String> {
        if let Some(problem) = self.hook_script_problems().first() {
            return Err(problem.clone());
        }
        self.exec_probe()
    }

    /// `git init --bare` with the repository config applied. Fails if the directory exists.
    pub async fn init_bare(&self, id: RepoId, default_branch: &str) -> Result<()> {
        let path = self.repo_path(id);
        if path.exists() {
            return Err(Error::internal(anyhow::anyhow!("{} already exists", path.display())));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::internal(anyhow::anyhow!("creating {}: {e}", parent.display())))?;
        }
        let branch = format!("--initial-branch={default_branch}");
        self.bare_cmd().args(["init", "--bare", "--quiet", &branch]).arg(&path).run().await?;
        for (k, v) in repo_config(&self.hooks_dir) {
            self.cmd(id).args(["config", &k, &v]).run().await?;
        }
        // No sample hooks, no description file clutter.
        let _ = std::fs::remove_dir_all(path.join("hooks"));
        Ok(())
    }

    /// Moves a repository out of the way. The directory is removed later by the housekeeping
    /// worker, so a slow disk never holds up an API call.
    pub fn trash(&self, id: RepoId) -> std::io::Result<()> {
        let from = self.repo_path(id);
        if !from.exists() {
            return Ok(());
        }
        let to = self.trash_dir.join(format!("{}-{}.git", id.0, chrono::Utc::now().timestamp_millis()));
        std::fs::rename(from, to)
    }

    /// Removes everything in the trash. Returns how many entries went.
    pub fn empty_trash(&self) -> usize {
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(&self.trash_dir) {
            for e in rd.flatten() {
                if std::fs::remove_dir_all(e.path()).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_creates_a_bare_repo_with_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        store.init_bare(RepoId(300), "trunk").await.unwrap();
        let p = store.repo_path(RepoId(300));
        assert!(p.ends_with("2c/300.git"), "{}", p.display());
        assert!(store.exists(RepoId(300)));
        let head = std::fs::read_to_string(p.join("HEAD")).unwrap();
        assert_eq!(head.trim(), "ref: refs/heads/trunk");
        let hooks = store.cmd(RepoId(300)).args(["config", "core.hooksPath"]).run().await.unwrap().stdout_str();
        assert_eq!(hooks, store.hooks_dir.to_string_lossy());
        assert!(store.init_bare(RepoId(300), "main").await.is_err(), "no clobbering");
    }

    #[test]
    fn hook_scripts_quote_the_program_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/opt/it's here/codehaven")).unwrap();
        let s = std::fs::read_to_string(store.hooks_dir.join("pre-receive")).unwrap();
        assert!(s.contains("exec '/opt/it'\\''s here/codehaven' hook pre-receive"), "{s}");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.hooks_dir.join("post-receive")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn verify_hooks_passes_after_install() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        store.verify_hooks().unwrap();
        assert!(store.hook_script_problems().is_empty());
    }

    #[test]
    fn verify_hooks_refuses_a_hook_without_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        std::fs::set_permissions(store.hooks_dir.join("pre-receive"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.verify_hooks().unwrap_err();
        assert!(err.contains("pre-receive"), "{err}");
        assert!(!store.hook_script_problems().is_empty());
    }

    #[test]
    fn verify_hooks_refuses_a_missing_hook() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        std::fs::remove_file(store.hooks_dir.join("post-receive")).unwrap();
        let err = store.verify_hooks().unwrap_err();
        assert!(err.contains("post-receive"), "{err}");
    }

    #[test]
    fn exec_probe_refuses_a_probe_that_cannot_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        // A successful verify leaves the probe script in place.
        store.verify_hooks().unwrap();
        std::fs::set_permissions(store.hooks_dir.join(".exec-probe"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.exec_probe().unwrap_err();
        let hooks = store.hooks_dir.to_string_lossy();
        assert!(err.contains(hooks.as_ref()), "{err}");
    }

    #[test]
    fn install_hooks_repairs_a_lost_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        let program = Path::new("/usr/bin/true");
        store.install_hooks(program).unwrap();
        std::fs::set_permissions(store.hooks_dir.join("pre-receive"), std::fs::Permissions::from_mode(0o644)).unwrap();
        store.install_hooks(program).unwrap();
        let mode = std::fs::metadata(store.hooks_dir.join("pre-receive")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[tokio::test]
    async fn trash_moves_and_empties() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("git"));
        store.init_dirs().unwrap();
        store.init_bare(RepoId(5), "main").await.unwrap();
        store.trash(RepoId(5)).unwrap();
        assert!(!store.exists(RepoId(5)));
        assert_eq!(store.empty_trash(), 1);
        store.trash(RepoId(5)).unwrap(); // already gone is fine
    }
}
