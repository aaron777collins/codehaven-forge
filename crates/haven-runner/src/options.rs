//! `container.options`: the one place a workflow can pass flags to `docker create`, and the
//! reason it is a short allow list. ARCHITECTURE.md section 2.5.
//!
//! Allowed: `--cpus`, `--memory`, `--env`, `--workdir`, `--user`, `--shm-size` and their short
//! forms `-m`, `-e`, `-w`, `-u`. Anything else is an error, and the server checks the same list
//! when it reads the workflow file; the runner checks again because it is the one that would
//! otherwise run the flag.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerOptions {
    pub cpus: Option<f64>,
    pub memory: Option<u64>,
    pub shm_size: Option<u64>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    #[error("container.options: {flag} is not allowed. Only --cpus, --memory, --env, --workdir, --user and --shm-size are")]
    NotAllowed { flag: String },
    #[error("container.options: {flag} needs a value")]
    MissingValue { flag: String },
    #[error("container.options: {flag} {value:?} is not valid: {why}")]
    BadValue { flag: String, value: String, why: String },
    #[error("container.options: {0}")]
    Syntax(String),
}

/// Parses an options string such as `--cpus 2 --memory=4g -e FOO=bar`.
pub fn parse(options: &str) -> Result<ContainerOptions, OptionsError> {
    let words = shell_words::split(options).map_err(|e| OptionsError::Syntax(e.to_string()))?;
    let mut out = ContainerOptions::default();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if !w.starts_with('-') {
            return Err(OptionsError::Syntax(format!("{w:?} is not a flag")));
        }
        // `--flag=value` and `--flag value`. Short flags take a separate value only.
        let (flag, inline): (String, Option<String>) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (w.clone(), None),
        };
        let canonical = match flag.as_str() {
            "--cpus" => "--cpus",
            "--memory" | "-m" => "--memory",
            "--shm-size" => "--shm-size",
            "--env" | "-e" => "--env",
            "--workdir" | "-w" => "--workdir",
            "--user" | "-u" => "--user",
            _ => return Err(OptionsError::NotAllowed { flag }),
        };
        let value = match inline {
            Some(v) => v,
            None => {
                let Some(v) = words.get(i) else { return Err(OptionsError::MissingValue { flag }) };
                i += 1;
                v.clone()
            }
        };
        let bad = |why: &str| OptionsError::BadValue { flag: canonical.to_string(), value: value.clone(), why: why.to_string() };
        match canonical {
            "--cpus" => {
                let n: f64 = value.parse().map_err(|_| bad("not a number"))?;
                if !n.is_finite() || n <= 0.0 {
                    return Err(bad("must be above 0"));
                }
                out.cpus = Some(n);
            }
            "--memory" => out.memory = Some(parse_size(&value).ok_or_else(|| bad("expected a size such as 512m or 4g"))?),
            "--shm-size" => out.shm_size = Some(parse_size(&value).ok_or_else(|| bad("expected a size such as 64m or 1g"))?),
            "--env" => {
                let (k, v) = match value.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => return Err(bad("expected NAME=value")),
                };
                if k.is_empty() || k.contains(char::is_whitespace) {
                    return Err(bad("the name is empty or has spaces"));
                }
                out.env.insert(k, v);
            }
            "--workdir" => {
                if !value.starts_with('/') {
                    return Err(bad("must be an absolute path"));
                }
                out.workdir = Some(value);
            }
            "--user" => {
                if value.is_empty() || value.starts_with('-') || value.contains(char::is_whitespace) {
                    return Err(bad("expected a user name or id"));
                }
                out.user = Some(value);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `512`, `512b`, `64k`, `512m`, `4g`, `1t`. Returns bytes. Binary units, like Docker.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (digits, mult): (&str, u64) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        't' => (&s[..s.len() - 1], 1 << 40),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let bytes = n * mult as f64;
    if bytes >= u64::MAX as f64 { None } else { Some(bytes as u64) }
}

/// What the runner will allow one job container.
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub cpus: f64,
    pub memory: u64,
    pub pids: i64,
    pub shm_size: u64,
}

/// What actually gets applied: the request, capped by the runner's limits.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub cpus: f64,
    pub memory: u64,
    pub pids: i64,
    pub shm_size: u64,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    /// One line per cap applied, for the job log.
    pub notes: Vec<String>,
}

pub fn apply(opts: &ContainerOptions, limits: &Limits) -> Effective {
    let mut notes = Vec::new();
    let cpus = match opts.cpus {
        Some(c) if c > limits.cpus => {
            notes.push(format!("container.options: --cpus {c} is capped to {} on this runner", limits.cpus));
            limits.cpus
        }
        Some(c) => c,
        None => limits.cpus,
    };
    let memory = match opts.memory {
        Some(m) if m > limits.memory => {
            notes.push(format!("container.options: --memory {m} bytes is capped to {} bytes on this runner", limits.memory));
            limits.memory
        }
        Some(m) => m,
        None => limits.memory,
    };
    let shm_size = match opts.shm_size {
        Some(s) if s > limits.shm_size => {
            notes.push(format!("container.options: --shm-size {s} bytes is capped to {} bytes on this runner", limits.shm_size));
            limits.shm_size
        }
        Some(s) => s,
        None => limits.shm_size,
    };
    Effective {
        cpus,
        memory,
        pids: limits.pids,
        shm_size,
        env: opts.env.clone(),
        workdir: opts.workdir.clone(),
        user: opts.user.clone(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits { cpus: 4.0, memory: 8 << 30, pids: 4096, shm_size: 1 << 30 }
    }

    #[test]
    fn parses_the_allowed_flags() {
        let o = parse("--cpus 2 --memory=4g -e FOO=bar --env BAZ=a=b --workdir /src -u 1000:1000 --shm-size 64m").unwrap();
        assert_eq!(o.cpus, Some(2.0));
        assert_eq!(o.memory, Some(4 << 30));
        assert_eq!(o.env["FOO"], "bar");
        assert_eq!(o.env["BAZ"], "a=b");
        assert_eq!(o.workdir.as_deref(), Some("/src"));
        assert_eq!(o.user.as_deref(), Some("1000:1000"));
        assert_eq!(o.shm_size, Some(64 << 20));
    }

    #[test]
    fn empty_options_are_fine() {
        assert_eq!(parse("").unwrap(), ContainerOptions::default());
        assert_eq!(parse("   ").unwrap(), ContainerOptions::default());
    }

    #[test]
    fn quoting_is_honoured() {
        let o = parse(r#"-e "GREETING=hello world" --env 'Q=it works'"#).unwrap();
        assert_eq!(o.env["GREETING"], "hello world");
        assert_eq!(o.env["Q"], "it works");
    }

    #[test]
    fn dangerous_flags_are_refused() {
        for bad in [
            "--privileged",
            "--privileged=true",
            "-v /:/host",
            "--volume /var/run/docker.sock:/var/run/docker.sock",
            "--mount type=bind,src=/,dst=/h",
            "--device /dev/sda",
            "--cap-add SYS_ADMIN",
            "--cap-add=ALL",
            "--network host",
            "--net=host",
            "--pid host",
            "--ipc host",
            "--security-opt seccomp=unconfined",
            "--userns host",
            "--add-host a:b",
            "--entrypoint sh",
            "--tmpfs /x",
            "-p 80:80",
            "--restart always",
            "--cgroup-parent x",
            "--ulimit nofile=1",
            "--sysctl a=b",
        ] {
            assert!(matches!(parse(bad), Err(OptionsError::NotAllowed { .. })), "{bad:?} must be refused, got {:?}", parse(bad));
        }
    }

    #[test]
    fn a_flag_hidden_behind_an_allowed_one_is_still_refused() {
        // `-e` takes one value; the next word is parsed as a flag of its own.
        assert!(matches!(parse("-e A=1 --privileged"), Err(OptionsError::NotAllowed { .. })));
        assert!(matches!(parse("--user root -v /:/h"), Err(OptionsError::NotAllowed { .. })));
        // A value that merely looks like a flag is a value, not a flag.
        assert_eq!(parse("-e A=--privileged").unwrap().env["A"], "--privileged");
        assert!(matches!(parse("--user --privileged"), Err(OptionsError::BadValue { .. })));
    }

    #[test]
    fn bad_values() {
        assert!(matches!(parse("--cpus abc"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("--cpus 0"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("--cpus -1"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("--cpus NaN"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("--memory lots"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("-e NOEQUALS"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("-e =x"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("--workdir relative"), Err(OptionsError::BadValue { .. })));
        assert!(matches!(parse("--cpus"), Err(OptionsError::MissingValue { .. })));
        assert!(matches!(parse("stray"), Err(OptionsError::Syntax(_))));
        assert!(matches!(parse("'unterminated"), Err(OptionsError::Syntax(_))));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("512b"), Some(512));
        assert_eq!(parse_size("64k"), Some(64 << 10));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("4GB"), Some(4 << 30));
        assert_eq!(parse_size("1t"), Some(1 << 40));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("g"), None);
        assert_eq!(parse_size("-1g"), None);
        assert_eq!(parse_size("1x"), None);
        assert_eq!(parse_size("99999999999999999999g"), None);
    }

    #[test]
    fn requests_are_capped_by_the_runner() {
        let o = parse("--cpus 16 --memory 64g --shm-size 8g").unwrap();
        let e = apply(&o, &limits());
        assert_eq!(e.cpus, 4.0);
        assert_eq!(e.memory, 8 << 30);
        assert_eq!(e.shm_size, 1 << 30);
        assert_eq!(e.notes.len(), 3);
        let o = parse("--cpus 1 --memory 1g").unwrap();
        let e = apply(&o, &limits());
        assert_eq!((e.cpus, e.memory), (1.0, 1 << 30));
        assert!(e.notes.is_empty());
        let e = apply(&ContainerOptions::default(), &limits());
        assert_eq!((e.cpus, e.memory, e.pids), (4.0, 8 << 30, 4096));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn never_panics(s in "\\PC{0,80}") {
                let _ = parse(&s);
            }

            #[test]
            fn only_allowed_flags_ever_parse(s in "(-[a-z-]{1,12}[ =][a-z0-9/:=.]{0,8} ?){0,4}") {
                if let Ok(o) = parse(&s) {
                    // Whatever got through can only have set the six allowed things.
                    let _ = (o.cpus, o.memory, o.shm_size, o.env, o.workdir, o.user);
                }
            }

            #[test]
            fn applied_limits_never_exceed_the_runner(c in 0.01f64..1000.0, m in 1u64..(1u64 << 50)) {
                let o = ContainerOptions { cpus: Some(c), memory: Some(m), ..Default::default() };
                let e = apply(&o, &limits());
                prop_assert!(e.cpus <= 4.0 && e.memory <= (8u64 << 30));
            }
        }
    }
}
