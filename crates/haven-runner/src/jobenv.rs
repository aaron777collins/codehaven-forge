//! The job's directories, its environment variables and its expression contexts.

use ch_api_types::runner::JobPayload;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Everything a job keeps on disk, under `<work_dir>/<name>`. The whole tree is bind-mounted
/// into the job container at the same path, so a path means the same thing on both sides.
///
/// ```text
/// <root>/workspace          GITHUB_WORKSPACE, where checkout puts the code
/// <root>/_runner/temp       RUNNER_TEMP
/// <root>/_runner/scripts    one script file per `run:` step
/// <root>/_runner/files      per step: output, env, path, summary
/// <root>/_runner/pids       per step: the pid file the kill-on-timeout reads
/// <root>/_runner/tmp        scratch the job may use
/// <root>.private            the runner's own scratch (archives it builds and downloads),
///                           mode 0700, beside the tree and never mounted into the container
/// ```
///
/// The job can change anything under `<root>`, including the runner's part, so the runner reads
/// and writes there only through `crate::safefs`, which never follows a link (ch-ctk.1).
/// The most the runner reads back from one of a step's files (`$GITHUB_OUTPUT`, `$GITHUB_ENV`,
/// `$GITHUB_PATH`, `$GITHUB_STEP_SUMMARY`). A job could otherwise make one a sparse file of any
/// size and have the runner read it all into memory.
pub const MAX_STEP_FILE: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct JobDirs {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub runner: PathBuf,
    pub temp: PathBuf,
    pub scripts: PathBuf,
    pub files: PathBuf,
    pub pids: PathBuf,
    pub tmp: PathBuf,
    pub private: PathBuf,
}

impl JobDirs {
    pub fn create(work_dir: &Path, job_id: i64) -> std::io::Result<JobDirs> {
        let suffix: String = rand::random::<u32>().to_string();
        let root = work_dir.join(format!("job-{job_id}-{suffix}"));
        let d = JobDirs::at(root);
        for dir in [&d.workspace, &d.temp, &d.scripts, &d.files, &d.pids, &d.tmp] {
            std::fs::create_dir_all(dir)?;
        }
        // The container may run as any user and has to write here. Only this job's container
        // can reach the tree (it is the only one that mounts it), so open permissions do not
        // widen who can read it.
        for dir in [&d.root, &d.workspace, &d.runner, &d.temp, &d.scripts, &d.files, &d.pids, &d.tmp] {
            set_mode(dir, 0o777)?;
        }
        std::fs::create_dir(&d.private)?;
        set_mode(&d.private, 0o700)?;
        Ok(d)
    }

    /// Removes the tree and the runner's private scratch. `remove_dir_all` does not follow links.
    pub fn remove(&self) -> std::io::Result<()> {
        let private = std::fs::remove_dir_all(&self.private);
        let tree = std::fs::remove_dir_all(&self.root);
        for r in [tree, private] {
            if let Err(e) = r
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(e);
            }
        }
        Ok(())
    }

    fn at(root: PathBuf) -> JobDirs {
        let runner = root.join("_runner");
        let mut private = root.clone().into_os_string();
        private.push(".private");
        JobDirs {
            private: PathBuf::from(private),
            workspace: root.join("workspace"),
            temp: runner.join("temp"),
            scripts: runner.join("scripts"),
            files: runner.join("files"),
            pids: runner.join("pids"),
            tmp: runner.join("tmp"),
            runner,
            root,
        }
    }

    /// The four per-step files, created empty and writable by anyone. Whatever the job left at
    /// these names is replaced, and a link on the way stops the step (ch-ctk.1).
    pub fn step_files(&self, step: i32) -> std::io::Result<StepFiles> {
        let dir = self.files.join(format!("step-{step}"));
        crate::safefs::fresh_dir(&self.root, &dir, 0o777)?;
        let f = StepFiles { output: dir.join("output"), env: dir.join("env"), path: dir.join("path"), summary: dir.join("summary") };
        for p in [&f.output, &f.env, &f.path, &f.summary] {
            crate::safefs::create_new(&self.root, p, 0o666)?;
        }
        Ok(f)
    }

    /// Writes a file the runner makes in the job tree (a step's script, the event), replacing
    /// whatever the job left there and following no link.
    pub fn write_file(&self, path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
        crate::safefs::write_new(&self.root, path, data, mode)
    }

    /// Reads a file the job may have written, refusing links, anything but a regular file and
    /// anything over `MAX_STEP_FILE` bytes (an error of kind `FileTooLarge`).
    pub fn read_file(&self, path: &Path) -> std::io::Result<String> {
        crate::safefs::read_to_string(&self.root, path, MAX_STEP_FILE)
    }

    pub fn script_path(&self, step: i32, ext: &str) -> PathBuf {
        self.scripts.join(format!("step-{step}.{ext}"))
    }

    pub fn pid_path(&self, step: i32) -> PathBuf {
        self.pids.join(format!("step-{step}.pid"))
    }
}

#[derive(Debug, Clone)]
pub struct StepFiles {
    pub output: PathBuf,
    pub env: PathBuf,
    pub path: PathBuf,
    pub summary: PathBuf,
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

pub fn runner_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "X64",
        "aarch64" => "ARM64",
        "arm" => "ARM",
        "x86" => "X86",
        other => other,
    }
}

/// `refs/heads/main` into `main`, `refs/tags/v1` into `v1`, `refs/pull/7/merge` into `7/merge`.
pub fn ref_name(git_ref: &str) -> &str {
    for prefix in ["refs/heads/", "refs/tags/", "refs/pull/"] {
        if let Some(r) = git_ref.strip_prefix(prefix) {
            return r;
        }
    }
    git_ref
}

pub fn ref_type(git_ref: &str) -> &'static str {
    if git_ref.starts_with("refs/tags/") { "tag" } else { "branch" }
}

fn event_str(p: &JobPayload, path: &[&str]) -> String {
    let mut v = &p.context.event;
    for k in path {
        v = &v[*k];
    }
    v.as_str().unwrap_or("").to_string()
}

/// The `github` context (also exposed as `codehaven`).
pub fn github_context(p: &JobPayload, workspace: &Path, event_path: &Path) -> Value {
    let c = &p.context;
    let owner = c.repository.split('/').next().unwrap_or("");
    json!({
        "event_name": c.event_name,
        "event": c.event,
        "event_path": event_path.to_string_lossy(),
        "ref": c.git_ref,
        "ref_name": ref_name(&c.git_ref),
        "ref_type": ref_type(&c.git_ref),
        "sha": c.sha,
        "actor": c.actor,
        "triggering_actor": c.actor,
        "repository": c.repository,
        "repository_owner": owner,
        "run_id": c.run_id.to_string(),
        "run_number": c.run_number.to_string(),
        "run_attempt": c.run_attempt.to_string(),
        "workflow": c.workflow,
        "job": p.job_key,
        "server_url": c.server_url,
        "api_url": c.api_url,
        "workspace": workspace.to_string_lossy(),
        "token": p.token,
        "head_ref": event_str(p, &["pull_request", "head", "ref"]),
        "base_ref": event_str(p, &["pull_request", "base", "ref"]),
    })
}

/// The environment every step gets before the job's and the step's own `env:` are applied.
/// Each `GITHUB_*` variable has a `CODEHAVEN_*` twin.
pub fn base_env(p: &JobPayload, dirs: &JobDirs, runner_name: &str, event_path: &Path) -> BTreeMap<String, String> {
    let c = &p.context;
    let owner = c.repository.split('/').next().unwrap_or("");
    let mut github: Vec<(&str, String)> = vec![
        ("ACTIONS", "true".into()),
        ("ACTOR", c.actor.clone()),
        ("API_URL", c.api_url.clone()),
        ("BASE_REF", event_str(p, &["pull_request", "base", "ref"])),
        ("EVENT_NAME", c.event_name.clone()),
        ("EVENT_PATH", event_path.to_string_lossy().into_owned()),
        ("HEAD_REF", event_str(p, &["pull_request", "head", "ref"])),
        ("JOB", p.job_key.clone()),
        ("REF", c.git_ref.clone()),
        ("REF_NAME", ref_name(&c.git_ref).to_string()),
        ("REF_TYPE", ref_type(&c.git_ref).to_string()),
        ("REPOSITORY", c.repository.clone()),
        ("REPOSITORY_OWNER", owner.to_string()),
        ("RUN_ATTEMPT", c.run_attempt.to_string()),
        ("RUN_ID", c.run_id.to_string()),
        ("RUN_NUMBER", c.run_number.to_string()),
        ("SERVER_URL", c.server_url.clone()),
        ("SHA", c.sha.clone()),
        ("WORKFLOW", c.workflow.clone()),
        ("WORKSPACE", dirs.workspace.to_string_lossy().into_owned()),
    ];
    github.sort_by_key(|(k, _)| *k);
    let mut env = BTreeMap::new();
    for (k, v) in github {
        env.insert(format!("GITHUB_{k}"), v.clone());
        env.insert(format!("CODEHAVEN_{k}"), v);
    }
    env.insert("CI".into(), "true".into());
    env.insert("RUNNER_OS".into(), "Linux".into());
    env.insert("RUNNER_ARCH".into(), runner_arch().into());
    env.insert("RUNNER_NAME".into(), runner_name.into());
    env.insert("RUNNER_TEMP".into(), dirs.temp.to_string_lossy().into_owned());
    env.insert("RUNNER_TOOL_CACHE".into(), dirs.runner.join("toolcache").to_string_lossy().into_owned());
    // The workspace belongs to the runner's user, not the container's, and git refuses to work
    // in a directory owned by someone else unless told it is safe.
    env.insert("GIT_CONFIG_COUNT".into(), "1".into());
    env.insert("GIT_CONFIG_KEY_0".into(), "safe.directory".into());
    env.insert("GIT_CONFIG_VALUE_0".into(), dirs.workspace.to_string_lossy().into_owned());
    env
}

/// The `runner` context.
pub fn runner_context(runner_name: &str, dirs: &JobDirs) -> Value {
    json!({
        "os": "Linux",
        "arch": runner_arch(),
        "name": runner_name,
        "temp": dirs.temp.to_string_lossy(),
        "tool_cache": dirs.runner.join("toolcache").to_string_lossy(),
    })
}

pub fn map_to_value(m: &BTreeMap<String, String>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
}

/// `inputs` for a manual run: the `inputs` object of the event payload.
pub fn inputs_context(p: &JobPayload) -> Value {
    match &p.context.event["inputs"] {
        v @ Value::Object(_) => v.clone(),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_api_types::runner::{JobPayload, RepoInfo, RunContext};

    fn payload() -> JobPayload {
        JobPayload {
            job_id: 3,
            run_id: 9,
            repo: RepoInfo {
                id: 1,
                full_name: "alice/demo".into(),
                clone_url: "http://s/alice/demo.git".into(),
                default_branch: "main".into(),
            },
            job_key: "test".into(),
            name: "ci / test".into(),
            runs_on: vec!["ubuntu-latest".into()],
            container: None,
            env: Default::default(),
            matrix: json!({}),
            needs: Default::default(),
            steps: vec![],
            outputs: Default::default(),
            secrets: Default::default(),
            vars: Default::default(),
            context: RunContext {
                event_name: "pull_request".into(),
                git_ref: "refs/pull/7/merge".into(),
                sha: "a".repeat(40),
                actor: "alice".into(),
                repository: "alice/demo".into(),
                run_id: 9,
                run_number: 4,
                run_attempt: 2,
                workflow: "ci".into(),
                server_url: "http://s".into(),
                api_url: "http://s/api/v3".into(),
                event: json!({"pull_request": {"head": {"ref": "feature"}, "base": {"ref": "main"}}, "inputs": {"level": "debug"}}),
            },
            default_shell: None,
            default_working_directory: None,
            timeout_minutes: 60,
            token: "job_tok".into(),
            lease_seconds: 120,
        }
    }

    #[test]
    fn dirs_are_laid_out_and_writable_by_the_container() {
        let w = tempfile::tempdir().unwrap();
        let d = JobDirs::create(w.path(), 3).unwrap();
        assert!(d.workspace.is_dir() && d.temp.is_dir() && d.scripts.is_dir() && d.pids.is_dir() && d.tmp.is_dir());
        assert!(d.root.starts_with(w.path()));
        assert!(d.root.file_name().unwrap().to_string_lossy().starts_with("job-3-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&d.workspace).unwrap().permissions().mode() & 0o777, 0o777);
        }
        let f = d.step_files(2).unwrap();
        assert!(f.output.exists() && f.env.exists() && f.path.exists() && f.summary.exists());
        assert_eq!(std::fs::read_to_string(&f.output).unwrap(), "");
        assert_ne!(JobDirs::create(w.path(), 3).unwrap().root, d.root, "two runs of one job never share a directory");
        assert!(d.private.is_dir() && !d.private.starts_with(&d.root), "the runner's scratch is outside the mounted tree");
        d.remove().unwrap();
        assert!(!d.root.exists() && !d.private.exists());
    }

    #[test]
    fn ref_helpers() {
        assert_eq!(ref_name("refs/heads/feature/x"), "feature/x");
        assert_eq!(ref_name("refs/tags/v1.0"), "v1.0");
        assert_eq!(ref_name("refs/pull/7/merge"), "7/merge");
        assert_eq!(ref_name("weird"), "weird");
        assert_eq!(ref_type("refs/tags/v1"), "tag");
        assert_eq!(ref_type("refs/heads/main"), "branch");
    }

    #[test]
    fn env_has_github_and_codehaven_twins() {
        let w = tempfile::tempdir().unwrap();
        let d = JobDirs::create(w.path(), 3).unwrap();
        let e = base_env(&payload(), &d, "runner-1", Path::new("/e.json"));
        assert_eq!(e["CI"], "true");
        assert_eq!(e["GITHUB_ACTIONS"], "true");
        assert_eq!(e["GITHUB_REPOSITORY"], "alice/demo");
        assert_eq!(e["CODEHAVEN_REPOSITORY"], "alice/demo");
        assert_eq!(e["GITHUB_REPOSITORY_OWNER"], "alice");
        assert_eq!(e["GITHUB_RUN_ID"], "9");
        assert_eq!(e["GITHUB_RUN_ATTEMPT"], "2");
        assert_eq!(e["GITHUB_HEAD_REF"], "feature");
        assert_eq!(e["GITHUB_BASE_REF"], "main");
        assert_eq!(e["GITHUB_REF_NAME"], "7/merge");
        assert_eq!(e["GITHUB_SHA"].len(), 40);
        assert_eq!(e["GITHUB_WORKSPACE"], d.workspace.to_string_lossy());
        assert_eq!(e["RUNNER_OS"], "Linux");
        assert_eq!(e["RUNNER_NAME"], "runner-1");
        assert_eq!(e["GIT_CONFIG_KEY_0"], "safe.directory");
        for k in e.keys().filter(|k| k.starts_with("GITHUB_")) {
            let twin = k.replacen("GITHUB_", "CODEHAVEN_", 1);
            assert_eq!(e[k], e[&twin], "{k} and {twin} differ");
        }
        assert!(!e.contains_key("GITHUB_TOKEN"), "the job token is not in every step's environment");
        assert!(!e.values().any(|v| v == "job_tok"));
    }

    #[test]
    fn github_context_values() {
        let w = tempfile::tempdir().unwrap();
        let d = JobDirs::create(w.path(), 3).unwrap();
        let g = github_context(&payload(), &d.workspace, Path::new("/e.json"));
        assert_eq!(g["event_name"], "pull_request");
        assert_eq!(g["repository_owner"], "alice");
        assert_eq!(g["run_id"], "9", "ids are strings in the github context");
        assert_eq!(g["head_ref"], "feature");
        assert_eq!(g["token"], "job_tok");
        assert_eq!(g["event"]["pull_request"]["head"]["ref"], "feature");
        assert_eq!(inputs_context(&payload())["level"], "debug");
    }

    #[test]
    fn arch_names() {
        assert!(["X64", "ARM64", "ARM", "X86"].contains(&runner_arch()) || !runner_arch().is_empty());
    }
}
