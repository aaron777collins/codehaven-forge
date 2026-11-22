//! What a step can say to the runner through its output: workflow commands such as
//! `::add-mask::value`, and the files `$GITHUB_OUTPUT`, `$GITHUB_ENV` and `$GITHUB_PATH`.

use std::collections::BTreeMap;

/// What to do with one line of step output.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Handled {
    /// The line to show in the log, if any.
    pub show: Option<String>,
    /// A value to mask from now on.
    pub mask: Option<String>,
    /// `::set-output name=x::value`, the old way to set an output.
    pub output: Option<(String, String)>,
}

/// Parses workflow commands out of step output, keeping the `stop-commands` state.
#[derive(Default)]
pub struct CommandParser {
    /// While set, commands are off until a line `::<token>::` turns them back on.
    stopped_until: Option<String>,
    pub debug: bool,
}

impl CommandParser {
    pub fn new(debug: bool) -> CommandParser {
        CommandParser { stopped_until: None, debug }
    }

    pub fn handle(&mut self, line: &str) -> Handled {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(token) = &self.stopped_until {
            if line.trim() == format!("::{token}::") {
                self.stopped_until = None;
                return Handled::default();
            }
            return shown(line);
        }
        let Some(rest) = line.strip_prefix("::") else { return shown(line) };
        // `::name key=value,key2=value2::data`, or `::name::data`.
        let Some(end) = rest.find("::") else { return shown(line) };
        let (head, data) = (&rest[..end], &rest[end + 2..]);
        let (name, props) = match head.split_once(' ') {
            Some((n, p)) => (n, p),
            None => (head, ""),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return shown(line);
        }
        let data = unescape_data(data);
        match name.to_ascii_lowercase().as_str() {
            "add-mask" => Handled { mask: Some(data), ..Default::default() },
            "debug" => {
                if self.debug {
                    shown(&format!("##[debug]{data}"))
                } else {
                    Handled::default()
                }
            }
            "error" => shown(&annotation("error", props, &data)),
            "warning" => shown(&annotation("warning", props, &data)),
            "notice" => shown(&annotation("notice", props, &data)),
            "group" => shown(&format!("##[group]{data}")),
            "endgroup" => shown("##[endgroup]"),
            "stop-commands" => {
                if !data.is_empty() {
                    self.stopped_until = Some(data);
                }
                Handled::default()
            }
            "set-output" => {
                let props = parse_props(props);
                match props.get("name") {
                    Some(n) if !n.is_empty() => Handled { output: Some((n.clone(), data)), ..Default::default() },
                    _ => shown("##[warning]::set-output needs a name"),
                }
            }
            "save-state" => Handled::default(),
            // GitHub turned this off because a log line must not be able to change the
            // environment of later steps. So did we.
            "set-env" | "add-path" => shown(&format!("##[warning]::{name}:: is disabled. Write to $GITHUB_ENV or $GITHUB_PATH instead.")),
            _ => shown(line),
        }
    }
}

fn shown(s: &str) -> Handled {
    Handled { show: Some(s.to_string()), ..Default::default() }
}

fn annotation(level: &str, props: &str, data: &str) -> String {
    let p = parse_props(props);
    let mut loc = String::new();
    if let Some(f) = p.get("file").filter(|f| !f.is_empty()) {
        loc.push_str(f);
        if let Some(l) = p.get("line").filter(|l| !l.is_empty()) {
            loc.push(':');
            loc.push_str(l);
            if let Some(c) = p.get("col").filter(|c| !c.is_empty()) {
                loc.push(':');
                loc.push_str(c);
            }
        }
        loc.push_str(": ");
    }
    let title = p.get("title").filter(|t| !t.is_empty()).map(|t| format!("{t}: ")).unwrap_or_default();
    format!("##[{level}]{loc}{title}{data}")
}

fn parse_props(props: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for kv in props.split(',') {
        if let Some((k, v)) = kv.split_once('=') {
            m.insert(k.trim().to_string(), unescape_property(v));
        }
    }
    m
}

fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0d", "\r").replace("%0A", "\n").replace("%0a", "\n").replace("%25", "%")
}

fn unescape_property(s: &str) -> String {
    unescape_data(s).replace("%3A", ":").replace("%3a", ":").replace("%2C", ",").replace("%2c", ",")
}

/// A problem with a `$GITHUB_OUTPUT` or `$GITHUB_ENV` file.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct EnvFileError(pub String);

/// Reads the `name=value` and `name<<DELIMITER` forms of `$GITHUB_OUTPUT` and `$GITHUB_ENV`.
/// A later assignment of the same name wins.
pub fn parse_env_file(text: &str) -> Result<Vec<(String, String)>, EnvFileError> {
    let mut out: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() {
            continue;
        }
        let eq = line.find('=');
        let heredoc = line.find("<<");
        match (eq, heredoc) {
            // `name<<DELIM` when `<<` comes before any `=`.
            (e, Some(h)) if e.is_none_or(|e| h < e) => {
                let name = &line[..h];
                let delim = &line[h + 2..];
                if name.is_empty() || delim.is_empty() {
                    return Err(EnvFileError(format!("invalid format {line:?}: a multi-line value needs a name and a delimiter")));
                }
                let mut value = String::new();
                let mut closed = false;
                let mut first = true;
                while i < lines.len() {
                    let l = lines[i];
                    i += 1;
                    if l == delim {
                        closed = true;
                        break;
                    }
                    if !first {
                        value.push('\n');
                    }
                    value.push_str(l);
                    first = false;
                }
                if !closed {
                    return Err(EnvFileError(format!("the value of {name} is not closed by {delim:?}")));
                }
                set(&mut out, name, value);
            }
            (Some(e), _) => {
                let name = &line[..e];
                if name.is_empty() {
                    return Err(EnvFileError(format!("invalid format {line:?}: the name is empty")));
                }
                set(&mut out, name, line[e + 1..].to_string());
            }
            _ => return Err(EnvFileError(format!("invalid format {line:?}: expected name=value"))),
        }
    }
    Ok(out)
}

fn set(out: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some(slot) = out.iter_mut().find(|(n, _)| n == name) {
        slot.1 = value;
    } else {
        out.push((name.to_string(), value));
    }
}

/// `$GITHUB_PATH`: one directory per line, blank lines ignored.
pub fn parse_path_file(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Names a step may not set through `$GITHUB_ENV`: the ones the runner owns.
pub fn is_reserved_env_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    up.starts_with("GITHUB_") || up.starts_with("CODEHAVEN_") || up.starts_with("RUNNER_") || up == "CI"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(line: &str) -> Handled {
        CommandParser::new(false).handle(line)
    }

    #[test]
    fn plain_lines_pass_through() {
        assert_eq!(h("hello"), shown("hello"));
        assert_eq!(h(""), shown(""));
        assert_eq!(h("::"), shown("::"));
        assert_eq!(h("not ::a command"), shown("not ::a command"));
        assert_eq!(h("::bad name::x"), shown("::bad name::x"));
    }

    #[test]
    fn add_mask() {
        let r = h("::add-mask::s3cr3t");
        assert_eq!(r.mask.as_deref(), Some("s3cr3t"));
        assert_eq!(r.show, None, "the value is never shown");
        assert_eq!(h("::add-mask::a%0Ab").mask.as_deref(), Some("a\nb"));
    }

    #[test]
    fn annotations() {
        assert_eq!(h("::error::boom").show.as_deref(), Some("##[error]boom"));
        assert_eq!(h("::warning file=a.rs,line=3,col=4::careful").show.as_deref(), Some("##[warning]a.rs:3:4: careful"));
        assert_eq!(h("::notice title=Hi::there").show.as_deref(), Some("##[notice]Hi: there"));
        assert_eq!(h("::error::100%25 done%0Anext").show.as_deref(), Some("##[error]100% done\nnext"));
    }

    #[test]
    fn groups() {
        assert_eq!(h("::group::Build").show.as_deref(), Some("##[group]Build"));
        assert_eq!(h("::endgroup::").show.as_deref(), Some("##[endgroup]"));
    }

    #[test]
    fn debug_lines_need_debug_on() {
        assert_eq!(h("::debug::x"), Handled::default());
        assert_eq!(CommandParser::new(true).handle("::debug::x").show.as_deref(), Some("##[debug]x"));
    }

    #[test]
    fn stop_commands_until_the_token() {
        let mut p = CommandParser::new(false);
        assert_eq!(p.handle("::stop-commands::tok123"), Handled::default());
        assert_eq!(p.handle("::add-mask::leak").mask, None, "commands are off");
        assert_eq!(p.handle("::add-mask::leak").show.as_deref(), Some("::add-mask::leak"));
        assert_eq!(p.handle("::tok123::"), Handled::default());
        assert_eq!(p.handle("::add-mask::x").mask.as_deref(), Some("x"), "commands are back");
    }

    #[test]
    fn set_output_old_style() {
        assert_eq!(h("::set-output name=ver::1.2").output, Some(("ver".into(), "1.2".into())));
        assert!(h("::set-output ::1.2").show.unwrap().contains("needs a name"));
    }

    #[test]
    fn set_env_is_refused() {
        let r = h("::set-env name=X::1");
        assert!(r.show.unwrap().contains("disabled"));
        assert_eq!(r.output, None);
    }

    #[test]
    fn env_file_simple_and_heredoc() {
        let t = "a=1\nb=two words\nempty=\nmulti<<EOF\nline1\nline2\nEOF\nc=x=y\n";
        let r = parse_env_file(t).unwrap();
        assert_eq!(
            r,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "two words".into()),
                ("empty".into(), "".into()),
                ("multi".into(), "line1\nline2".into()),
                ("c".into(), "x=y".into()),
            ]
        );
    }

    #[test]
    fn env_file_later_wins_and_crlf_is_fine() {
        let r = parse_env_file("a=1\r\na=2\r\n").unwrap();
        assert_eq!(r, vec![("a".into(), "2".into())]);
    }

    #[test]
    fn env_file_heredoc_value_may_contain_equals_and_ltlt() {
        let r = parse_env_file("k<<D\na=b\nx<<y\nD\n").unwrap();
        assert_eq!(r, vec![("k".into(), "a=b\nx<<y".into())]);
        assert_eq!(parse_env_file("k<<D\nD\n").unwrap(), vec![("k".into(), "".into())]);
    }

    #[test]
    fn env_file_errors() {
        assert!(parse_env_file("novalue").is_err());
        assert!(parse_env_file("=x").is_err());
        assert!(parse_env_file("k<<EOF\nnever closed").is_err());
        assert!(parse_env_file("<<EOF\nx\nEOF").is_err());
        assert!(parse_env_file("").unwrap().is_empty());
        assert!(parse_env_file("\n\n").unwrap().is_empty());
    }

    #[test]
    fn path_file() {
        assert_eq!(parse_path_file("/a/bin\n\n  /b/bin  \n"), vec!["/a/bin", "/b/bin"]);
    }

    #[test]
    fn reserved_names() {
        assert!(is_reserved_env_name("GITHUB_TOKEN"));
        assert!(is_reserved_env_name("github_output"));
        assert!(is_reserved_env_name("RUNNER_TEMP"));
        assert!(is_reserved_env_name("CODEHAVEN_SHA"));
        assert!(is_reserved_env_name("CI"));
        assert!(!is_reserved_env_name("MY_VAR"));
        assert!(!is_reserved_env_name("CIRCLE"));
    }
}
