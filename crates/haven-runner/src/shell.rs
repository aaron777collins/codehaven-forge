//! Which shell runs a `run:` step, and how.
//!
//! `shell: bash` means `bash --noprofile --norc -eo pipefail {0}`, `sh` means `sh -e {0}`, and
//! the default is bash when the image has it, `sh -e` when it does not. A custom shell is a
//! command line with `{0}` standing for the script file.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPlan {
    /// The command and its arguments, with `{0}` still in place.
    pub template: Vec<String>,
    /// File extension for the script (`sh`, `py`, `ps1`).
    pub ext: &'static str,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShellError {
    #[error("shell {0:?} has no {{0}} for the script file")]
    NoPlaceholder(String),
    #[error("shell {0:?} could not be read: {1}")]
    Syntax(String, String),
    #[error("shell is empty")]
    Empty,
}

pub fn plan(shell: Option<&str>, has_bash: bool) -> Result<ShellPlan, ShellError> {
    let name = shell.map(str::trim).filter(|s| !s.is_empty());
    let words = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    Ok(match name {
        None if has_bash => ShellPlan { template: words("bash -e -o pipefail {0}"), ext: "sh" },
        None => ShellPlan { template: words("sh -e {0}"), ext: "sh" },
        Some("bash") => ShellPlan { template: words("bash --noprofile --norc -eo pipefail {0}"), ext: "sh" },
        Some("sh") => ShellPlan { template: words("sh -e {0}"), ext: "sh" },
        Some("python") | Some("python3") => ShellPlan { template: words("python {0}"), ext: "py" },
        Some("pwsh") => ShellPlan { template: words("pwsh -command . '{0}'"), ext: "ps1" },
        Some(custom) => {
            if !custom.contains("{0}") {
                return Err(ShellError::NoPlaceholder(custom.to_string()));
            }
            let t = shell_words::split(custom).map_err(|e| ShellError::Syntax(custom.to_string(), e.to_string()))?;
            if t.is_empty() {
                return Err(ShellError::Empty);
            }
            ShellPlan { template: t, ext: "sh" }
        }
    })
}

impl ShellPlan {
    /// The argv with the script path filled in.
    pub fn argv(&self, script: &str) -> Vec<String> {
        self.template.iter().map(|w| w.replace("{0}", script)).collect()
    }
}

/// Wraps a step command so its process id lands in `pidfile` and then becomes the step itself.
/// A timeout or cancel reads the file and kills the step's whole process tree.
pub fn wrap_with_pidfile(pidfile: &str, argv: &[String]) -> Vec<String> {
    let mut v = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"echo $$ > "$1"; shift; exec "$@""#.to_string(),
        "haven-step".to_string(),
        pidfile.to_string(),
    ];
    v.extend(argv.iter().cloned());
    v
}

/// POSIX sh that kills the process in `$1` and all its descendants, found through `/proc`
/// (no `ps`, `pkill` or `kill -- -pgid` needed, so it works in busybox images too).
pub const KILL_TREE: &str = r#"
pidfile="$1"
[ -s "$pidfile" ] || exit 0
read -r root < "$pidfile"
case "$root" in ''|*[!0-9]*) exit 0;; esac
[ "$root" -gt 1 ] || exit 0
pairs=$(for d in /proc/[0-9]*; do
  [ -r "$d/stat" ] || continue
  read -r line < "$d/stat" 2>/dev/null || continue
  rest=${line##*) }
  set -- $rest
  echo "${d#/proc/} $2"
done)
all="$root"
frontier="$root"
while [ -n "$frontier" ]; do
  next=""
  for f in $frontier; do
    for c in $(echo "$pairs" | while read -r p pp; do [ "$pp" = "$f" ] && echo "$p"; done); do
      next="$next $c"
      all="$all $c"
    done
  done
  frontier="$next"
done
for p in $all; do kill -STOP "$p" 2>/dev/null; done
for p in $all; do kill -KILL "$p" 2>/dev/null; done
exit 0
"#;

/// The argv that runs `KILL_TREE` for a pid file.
pub fn kill_tree_argv(pidfile: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), KILL_TREE.into(), "haven-kill".into(), pidfile.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!(plan(None, true).unwrap().argv("/s.sh"), ["bash", "-e", "-o", "pipefail", "/s.sh"]);
        assert_eq!(plan(None, false).unwrap().argv("/s.sh"), ["sh", "-e", "/s.sh"]);
        assert_eq!(plan(Some(""), true).unwrap().argv("/s.sh")[0], "bash");
    }

    #[test]
    fn named_shells() {
        assert_eq!(plan(Some("bash"), false).unwrap().argv("/s.sh"), ["bash", "--noprofile", "--norc", "-eo", "pipefail", "/s.sh"]);
        assert_eq!(plan(Some("sh"), true).unwrap().argv("/s.sh"), ["sh", "-e", "/s.sh"]);
        let py = plan(Some("python"), true).unwrap();
        assert_eq!(py.ext, "py");
        assert_eq!(py.argv("/s.py"), ["python", "/s.py"]);
    }

    #[test]
    fn custom_shells() {
        let p = plan(Some("perl {0}"), true).unwrap();
        assert_eq!(p.argv("/x"), ["perl", "/x"]);
        let p = plan(Some("bash -x -e {0}"), true).unwrap();
        assert_eq!(p.argv("/x"), ["bash", "-x", "-e", "/x"]);
        assert_eq!(plan(Some("zsh"), true), Err(ShellError::NoPlaceholder("zsh".into())));
        assert!(matches!(plan(Some("'unterminated {0}"), true), Err(ShellError::Syntax(..))));
    }

    #[test]
    fn script_paths_with_spaces_stay_one_argument() {
        let p = plan(Some("bash"), true).unwrap();
        assert_eq!(p.argv("/work/a b/s.sh").last().unwrap(), "/work/a b/s.sh");
    }

    #[test]
    fn pidfile_wrapper() {
        let w = wrap_with_pidfile("/p/1.pid", &["bash".into(), "/s.sh".into()]);
        assert_eq!(&w[..2], ["/bin/sh", "-c"]);
        assert_eq!(&w[3..], ["haven-step", "/p/1.pid", "bash", "/s.sh"]);
    }
}
