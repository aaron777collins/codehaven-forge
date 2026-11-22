//! The container options a workflow may set, and nothing else.
//!
//! `container.options` is the one place a workflow author can reach Docker flags, so it is a
//! short allow list: `--cpus`, `--memory`, `--env`, `--workdir`, `--user` and `--shm-size`.
//! Privileged mode, volumes, devices, added capabilities and host networking are refused with
//! a message that names the flag. The server checks when it reads the file and the runner checks
//! again before it starts a container, because the runner must not trust the server blindly.

/// Docker flags a workflow may use.
pub const ALLOWED_FLAGS: &[&str] = &["--cpus", "--memory", "--env", "--workdir", "--user", "--shm-size"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerOptions {
    pub cpus: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub shm_bytes: Option<u64>,
}

/// The most a job may ask for. The runner's own limits go here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_cpus: f64,
    pub max_memory_bytes: u64,
    pub max_shm_bytes: u64,
}

impl Default for Limits {
    /// The defaults from ARCHITECTURE.md section 2.5: 4 CPUs and 8 GiB per job.
    fn default() -> Self {
        Limits { max_cpus: 4.0, max_memory_bytes: 8 << 30, max_shm_bytes: 2 << 30 }
    }
}

/// Splits like a shell would for the simple cases: whitespace, single quotes, double quotes
/// and backslash escapes. No expansion of any kind happens.
fn split(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            '\'' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("unterminated quote in options".into()),
                    }
                }
            }
            '"' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$')) => cur.push(c),
                            Some(c) => {
                                cur.push('\\');
                                cur.push(c);
                            }
                            None => return Err("unterminated quote in options".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("unterminated quote in options".into()),
                    }
                }
            }
            '\\' => {
                have = true;
                match chars.next() {
                    Some(c) => cur.push(c),
                    None => return Err("options end with a backslash".into()),
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        out.push(cur);
    }
    Ok(out)
}

/// Parses a size such as `512m`, `4g`, `1048576`. Binary multiples, as Docker reads them.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mult) = match t.strip_suffix(['b']) {
        Some(rest) if rest.ends_with(['k', 'm', 'g', 't']) => {
            let m = match rest.chars().last() {
                Some('k') => 1u64 << 10,
                Some('m') => 1 << 20,
                Some('g') => 1 << 30,
                _ => 1 << 40,
            };
            (&rest[..rest.len() - 1], m)
        }
        Some(rest) => (rest, 1),
        None => match t.chars().last() {
            Some('k') => (&t[..t.len() - 1], 1u64 << 10),
            Some('m') => (&t[..t.len() - 1], 1 << 20),
            Some('g') => (&t[..t.len() - 1], 1 << 30),
            Some('t') => (&t[..t.len() - 1], 1 << 40),
            _ => (t.as_str(), 1),
        },
    };
    let n: u64 = num.trim().parse().map_err(|_| format!("`{s}` is not a size such as 512m or 4g"))?;
    n.checked_mul(mult).filter(|v| *v > 0).ok_or_else(|| format!("`{s}` is not a usable size"))
}

fn safe_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'))
}

fn norm_flag(f: &str) -> Option<&'static str> {
    Some(match f {
        "--cpus" => "--cpus",
        "--memory" | "-m" => "--memory",
        "--env" | "-e" => "--env",
        "--workdir" | "-w" => "--workdir",
        "--user" | "-u" => "--user",
        "--shm-size" => "--shm-size",
        _ => return None,
    })
}

/// Parses `container.options`. The error is a sentence for the person who wrote the file.
pub fn parse_options(s: &str) -> Result<ContainerOptions, String> {
    let words = split(s)?;
    let mut out = ContainerOptions::default();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if !w.starts_with('-') {
            return Err(format!("`{w}` is not allowed here. Options must be flags: {}", ALLOWED_FLAGS.join(", ")));
        }
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (w.as_str(), None),
        };
        // `-m4g` style attached short values are refused: they are the easy way to hide a flag.
        let Some(name) = norm_flag(flag) else {
            return Err(format!("`{flag}` is not allowed. Allowed options: {}", ALLOWED_FLAGS.join(", ")));
        };
        let value = match inline {
            Some(v) => v,
            None => {
                let Some(v) = words.get(i) else { return Err(format!("`{flag}` needs a value")) };
                i += 1;
                v.clone()
            }
        };
        match name {
            "--cpus" => {
                let c: f64 = value.parse().map_err(|_| format!("`--cpus {value}` is not a number"))?;
                if !c.is_finite() || c <= 0.0 {
                    return Err(format!("`--cpus {value}` must be above zero"));
                }
                out.cpus = Some(c);
            }
            "--memory" => out.memory_bytes = Some(parse_size(&value)?),
            "--shm-size" => out.shm_bytes = Some(parse_size(&value)?),
            "--env" => {
                let Some((k, v)) = value.split_once('=') else {
                    return Err(format!(
                        "`--env {value}` needs a value: write NAME=value. Passing through the host's environment is not allowed"
                    ));
                };
                if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') || k.as_bytes()[0].is_ascii_digit() {
                    return Err(format!("`{k}` is not a valid variable name"));
                }
                out.env.push((k.to_string(), v.to_string()));
            }
            "--workdir" => {
                if !value.starts_with('/') || value.contains("..") || value.bytes().any(|b| b < 0x20) {
                    return Err(format!("`--workdir {value}` must be an absolute path without `..`"));
                }
                out.workdir = Some(value);
            }
            "--user" => {
                if !value.split(':').all(safe_name) || value.split(':').count() > 2 {
                    return Err(format!("`--user {value}` must be a name, an id, or name:group"));
                }
                out.user = Some(value);
            }
            _ => unreachable!("norm_flag only returns allowed names"),
        }
    }
    Ok(out)
}

/// Refuses a request above the runner's limits.
pub fn check_limits(o: &ContainerOptions, l: &Limits) -> Result<(), String> {
    if let Some(c) = o.cpus
        && c > l.max_cpus
    {
        return Err(format!("`--cpus {c}` is above this runner's limit of {}", l.max_cpus));
    }
    if let Some(m) = o.memory_bytes
        && m > l.max_memory_bytes
    {
        return Err(format!("`--memory` is above this runner's limit of {} MiB", l.max_memory_bytes >> 20));
    }
    if let Some(m) = o.shm_bytes
        && m > l.max_shm_bytes
    {
        return Err(format!("`--shm-size` is above this runner's limit of {} MiB", l.max_shm_bytes >> 20));
    }
    Ok(())
}

/// Parses and checks in one step.
pub fn validate(s: &str, limits: &Limits) -> Result<ContainerOptions, String> {
    let o = parse_options(s)?;
    check_limits(&o, limits)?;
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_the_safe_list() {
        let o = parse_options("--cpus 2 --memory 4g --env A=1 -e B=two --workdir /app --user 1000:1000 --shm-size=256m").unwrap();
        assert_eq!(o.cpus, Some(2.0));
        assert_eq!(o.memory_bytes, Some(4 << 30));
        assert_eq!(o.env, vec![("A".into(), "1".into()), ("B".into(), "two".into())]);
        assert_eq!(o.workdir.as_deref(), Some("/app"));
        assert_eq!(o.user.as_deref(), Some("1000:1000"));
        assert_eq!(o.shm_bytes, Some(256 << 20));
        assert_eq!(parse_options("").unwrap(), ContainerOptions::default());
        assert_eq!(parse_options("-m 512m").unwrap().memory_bytes, Some(512 << 20));
        assert_eq!(parse_options("--env 'MSG=hello world'").unwrap().env[0].1, "hello world");
    }

    #[test]
    fn refuses_everything_that_escapes() {
        for bad in [
            "--privileged",
            "--privileged=true",
            "-v /:/host",
            "--volume /var/run/docker.sock:/var/run/docker.sock",
            "--device /dev/sda",
            "--cap-add SYS_ADMIN",
            "--cap-add=ALL",
            "--network host",
            "--net=host",
            "--pid host",
            "--ipc host",
            "--security-opt seccomp=unconfined",
            "--mount type=bind,src=/,dst=/h",
            "--userns host",
            "--cgroupns host",
            "--add-host a:b",
            "--entrypoint sh",
            "--ulimit nofile=1",
            "-p 22:22",
            "--publish 80:80",
            "--init",
            "--rm",
            "--user",
            "--cpus",
            "-m4g",
            "--cpus 1 -- --privileged",
            "stray",
        ] {
            assert!(parse_options(bad).is_err(), "{bad:?} must be refused");
        }
        let e = parse_options("--privileged").unwrap_err();
        assert!(e.contains("--privileged") && e.contains("--cpus"), "{e}");
    }

    #[test]
    fn refuses_values_that_are_not_what_they_claim() {
        assert!(parse_options("--cpus abc").is_err());
        assert!(parse_options("--cpus 0").is_err());
        assert!(parse_options("--cpus -1").is_err());
        assert!(parse_options("--cpus NaN").is_err());
        assert!(parse_options("--memory lots").is_err());
        assert!(parse_options("--memory 0").is_err());
        assert!(parse_options("--env NOVALUE").is_err(), "no passthrough of the host environment");
        assert!(parse_options("--env 1BAD=x").is_err());
        assert!(parse_options("--env =x").is_err());
        assert!(parse_options("--workdir relative").is_err());
        assert!(parse_options("--workdir /a/../etc").is_err());
        assert!(parse_options("--user root;id").is_err());
        assert!(parse_options("--user a:b:c").is_err());
        assert!(parse_options("--env 'unterminated").is_err());
    }

    #[test]
    fn quoting_cannot_smuggle_a_flag() {
        // The quotes keep this one word, and a word that is not a flag is refused.
        assert!(parse_options("'--privileged'").is_err());
        assert!(parse_options("--env A=1 '--privileged'").is_err());
        assert!(parse_options("--user \"root --privileged\"").is_err());
        assert!(parse_options("--cpus 1\n--privileged").is_err());
        assert!(parse_options("--cpus\t1\t--privileged").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("1kb").unwrap(), 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("1b").unwrap(), 1);
        assert!(parse_size("g").is_err());
        assert!(parse_size("-1g").is_err());
        assert!(parse_size("99999999999999g").is_err(), "overflow is an error, not a wrap");
    }

    #[test]
    fn limits() {
        let l = Limits::default();
        assert!(validate("--cpus 4 --memory 8g", &l).is_ok());
        assert!(validate("--cpus 5", &l).is_err());
        assert!(validate("--memory 9g", &l).is_err());
        assert!(validate("--shm-size 3g", &l).is_err());
        assert!(validate("--cpus 0.5", &l).is_ok());
    }

    proptest! {
        // No input makes the parser panic.
        #[test]
        fn never_panics(s in "[ -~\\n\\t]{0,80}") {
            let _ = parse_options(&s);
        }

        #[test]
        fn a_denied_flag_is_always_denied(flag in "--(privileged|volume|device|cap-add|network|pid|ipc|mount|entrypoint|publish)", val in "[a-z0-9/:=]{0,12}") {
            let a = format!("{flag} {val}");
            let b = format!("--cpus 1 {flag}={val}");
            let c = format!("--user x {flag} {val}");
            prop_assert!(parse_options(&a).is_err());
            prop_assert!(parse_options(&b).is_err());
            prop_assert!(parse_options(&c).is_err());
        }
    }
}
