//! YAML anchors and aliases (ch-ctk.7). The YAML reader the workflow parser uses
//! (`marked-yaml`) turns a file with `&name` or `*name` away, and GitHub's workflow syntax allows
//! both, so a file that has them is rewritten first: every alias is replaced by what its anchor
//! names, and the result is written out again as YAML with no anchors, which the parser then reads
//! as it reads every other file.
//!
//! Nothing here is trusted. The file comes from a repository, and aliases can name a collection that
//! holds aliases of a collection that holds aliases (the "billion laughs" file). The expanded size
//! is counted as each node is made, from sizes that are shared rather than copied, so a file that
//! would expand past [`MAX_NODES`] (counting text by its bytes) is refused before anything that big exists. Merge keys (`<<`) are
//! refused too, as GitHub refuses them.
//!
//! Line numbers in the errors of a file that had aliases refer to the rewritten text, not to the
//! original, because that is the text the parser read.

use std::collections::HashMap;
use std::rc::Rc;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

/// The most weight a file may expand to, aliases counted at every use. A node (a scalar or a
/// collection) weighs 1, and a scalar weighs 1 more for every [`BYTES_PER_NODE`] bytes of text, so
/// one big string aliased many times is counted by the bytes it writes out, not as one node each.
/// At the limit the written file is at most about 640 KB.
pub const MAX_NODES: u64 = 10_000;

/// Bytes of scalar text that weigh as much as one node.
const BYTES_PER_NODE: u64 = 64;

enum Kind {
    Scalar(String, TScalarStyle),
    Seq(Vec<Rc<Node>>),
    Map(Vec<(Rc<Node>, Rc<Node>)>),
}

struct Node {
    kind: Kind,
    /// This node and everything under it, aliases counted at every use.
    size: u64,
}

enum Frame {
    Seq { anchor: usize, items: Vec<Rc<Node>>, size: u64 },
    Map { anchor: usize, pairs: Vec<(Rc<Node>, Rc<Node>)>, key: Option<Rc<Node>>, size: u64 },
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    anchors: HashMap<usize, Rc<Node>>,
    root: Option<Rc<Node>>,
    error: Option<String>,
}

impl Builder {
    fn fail(&mut self, msg: &str) {
        if self.error.is_none() {
            self.error = Some(msg.to_string());
        }
    }

    /// A finished node goes to its parent, or becomes the root, and is remembered under its anchor.
    fn finish(&mut self, node: Node, anchor: usize) {
        if node.size > MAX_NODES {
            self.fail(&format!("the aliases expand to more than {MAX_NODES} nodes (a node is one value, or {BYTES_PER_NODE} bytes of text). Write the value out in full"));
            return;
        }
        let node = Rc::new(node);
        if anchor != 0 {
            self.anchors.insert(anchor, node.clone());
        }
        self.attach(node);
    }

    fn attach(&mut self, node: Rc<Node>) {
        let size = node.size;
        let mut merge_key = false;
        match self.stack.last_mut() {
            None => self.root = Some(node),
            Some(Frame::Seq { items, size: s, .. }) => {
                *s = s.saturating_add(size);
                items.push(node);
            }
            Some(Frame::Map { pairs, key, size: s, .. }) => {
                *s = s.saturating_add(size);
                match key.take() {
                    None => {
                        merge_key = matches!(&node.kind, Kind::Scalar(t, TScalarStyle::Plain) if t == "<<");
                        *key = Some(node);
                    }
                    Some(k) => pairs.push((k, node)),
                }
            }
        }
        if merge_key {
            self.fail("merge keys (`<<`) are not supported. Write the keys out in full");
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, ev: Event, _mark: Marker) {
        if self.error.is_some() {
            return;
        }
        match ev {
            Event::Scalar(text, style, anchor, _) => {
                let size = 1 + text.len() as u64 / BYTES_PER_NODE;
                self.finish(Node { kind: Kind::Scalar(text, style), size }, anchor)
            }
            Event::SequenceStart(anchor, _) => self.stack.push(Frame::Seq { anchor, items: Vec::new(), size: 1 }),
            Event::MappingStart(anchor, _) => self.stack.push(Frame::Map { anchor, pairs: Vec::new(), key: None, size: 1 }),
            Event::SequenceEnd => {
                if let Some(Frame::Seq { anchor, items, size }) = self.stack.pop() {
                    self.finish(Node { kind: Kind::Seq(items), size }, anchor);
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map { anchor, pairs, size, .. }) = self.stack.pop() {
                    self.finish(Node { kind: Kind::Map(pairs), size }, anchor);
                }
            }
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(n) => {
                    if n.size > MAX_NODES {
                        self.fail(&format!("the aliases expand to more than {MAX_NODES} nodes (a node is one value, or {BYTES_PER_NODE} bytes of text). Write the value out in full"));
                    } else {
                        self.attach(n);
                    }
                }
                None => self.fail("an alias names an anchor that is not defined before it (an anchor cannot be used inside its own value)"),
            },
            _ => {}
        }
        // The running total of every open collection counts too: a loop of aliases adds up here
        // before its collection is finished.
        let over = self.stack.iter().any(|f| match f {
            Frame::Seq { size, .. } | Frame::Map { size, .. } => *size > MAX_NODES,
        });
        if over {
            self.fail(&format!("the aliases expand to more than {MAX_NODES} nodes (a node is one value, or {BYTES_PER_NODE} bytes of text). Write the value out in full"));
        }
    }
}

/// The same YAML with every anchor and alias replaced by the value it names.
pub fn expand(text: &str) -> Result<String, String> {
    let mut b = Builder::default();
    let mut parser = Parser::new_from_str(text);
    parser.load(&mut b, false).map_err(|e| format!("the YAML is not valid: {}", e.info()))?;
    if let Some(e) = b.error {
        return Err(e);
    }
    let Some(root) = b.root else { return Ok(String::new()) };
    let mut out = String::new();
    write_block(&root, 0, &mut out)?;
    Ok(out)
}

fn indent(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push(' ');
    }
}

fn quote(s: &str) -> String {
    // A JSON string is a YAML double-quoted scalar.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A scalar as YAML text. A plain scalar stays plain when that is safe (so `on`, numbers and
/// `${{ ... }}` read as they did), anything else is double-quoted.
fn scalar(text: &str, style: TScalarStyle) -> String {
    if style != TScalarStyle::Plain {
        return quote(text);
    }
    if text.is_empty() {
        return "~".into();
    }
    let first = text.chars().next().unwrap_or(' ');
    let unsafe_start = "-?:,[]{}#&*!|>'\"%@`".contains(first);
    let unsafe_inside = text.contains(": ") || text.contains(" #") || text.ends_with(':') || text.chars().any(|c| c.is_control());
    if unsafe_start || unsafe_inside || text != text.trim() {
        return quote(text);
    }
    text.to_string()
}

fn write_block(node: &Node, at: usize, out: &mut String) -> Result<(), String> {
    match &node.kind {
        Kind::Map(pairs) => {
            for (k, v) in pairs {
                let Kind::Scalar(key, ks) = &k.kind else { return Err("a key must be plain text".into()) };
                indent(out, at);
                out.push_str(&scalar(key, *ks));
                out.push(':');
                write_value(v, at + 2, out)?;
            }
        }
        Kind::Seq(items) => {
            for item in items {
                indent(out, at);
                out.push('-');
                write_value(item, at + 2, out)?;
            }
        }
        Kind::Scalar(t, s) => {
            indent(out, at);
            out.push_str(&scalar(t, *s));
            out.push('\n');
        }
    }
    Ok(())
}

/// What follows `key:` or `-`: a scalar or an empty collection on the same line, a collection on
/// the lines below.
fn write_value(node: &Node, at: usize, out: &mut String) -> Result<(), String> {
    match &node.kind {
        Kind::Scalar(t, s) => {
            out.push(' ');
            out.push_str(&scalar(t, *s));
            out.push('\n');
        }
        Kind::Seq(items) if items.is_empty() => out.push_str(" []\n"),
        Kind::Map(pairs) if pairs.is_empty() => out.push_str(" {}\n"),
        _ => {
            out.push('\n');
            write_block(node, at, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_become_the_values_they_name() {
        let out = expand("env: &shared\n  A: '1'\n  B: two\njobs:\n  a:\n    env: *shared\n    steps:\n      - run: echo ${{ env.A }}\n")
            .unwrap();
        // Written out again with no anchors, and the parser reads it.
        assert!(!out.contains('&') && !out.contains('*'), "{out}");
        assert_eq!(out.matches("A: \"1\"").count(), 2, "{out}");
        assert!(out.contains("run: echo ${{ env.A }}"), "{out}");
    }

    #[test]
    fn scalars_and_sequences_can_be_anchored_too() {
        let out = expand("on: push\nx: &v 10\ny: *v\nlist: &l [a, b]\nagain: *l\nempty:\n").unwrap();
        assert!(out.contains("y: 10") && out.contains("on: push"), "{out}");
        assert_eq!(out.matches("- a").count(), 2, "{out}");
        assert!(out.contains("empty: ~"), "{out}");
    }

    #[test]
    fn a_file_that_expands_without_end_is_refused_before_it_is_built() {
        // Each level lists the one below it ten times: a million scalars at the sixth.
        let mut t = String::from("l0: &l0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..8 {
            let prev = format!("*l{}", i - 1);
            t.push_str(&format!("l{i}: &l{i} [{}]\n", vec![prev; 10].join(", ")));
        }
        let e = expand(&t).unwrap_err();
        assert!(e.contains("expand to more than"), "{e}");
    }

    #[test]
    fn one_big_string_aliased_many_times_is_refused() {
        // 200,000 bytes aliased 9,950 times is about 2 GB when written out, with only ~10,000 nodes.
        let big = "A".repeat(200_000);
        let mut t = format!("big: &a \"{big}\"\nlist: [");
        t.push_str(&vec!["*a"; 9_950].join(", "));
        t.push_str("]\n");
        let r = expand(&t);
        assert!(r.as_ref().is_err_and(|e| e.contains("expand to more than")), "accepted, {} bytes", r.map(|o| o.len()).unwrap_or(0));
    }

    #[test]
    fn the_written_output_stays_small_whatever_is_accepted() {
        // Many mid-sized strings, each alias fine on its own: what is accepted stays under 1 MiB.
        let s = "B".repeat(1_000);
        let mut t = format!("s: &s \"{s}\"\nl: [");
        t.push_str(&vec!["*s"; 500].join(", "));
        t.push_str("]\n");
        let out = expand(&t).unwrap();
        assert!(out.len() < 1 << 20, "{}", out.len());
    }

    #[test]
    fn merge_keys_undefined_and_self_aliases_are_refused() {
        assert!(expand("a: &a {x: 1}\nb:\n  <<: *a\n").unwrap_err().contains("merge keys"));
        assert!(expand("a: *nope\n").unwrap_err().contains("not valid") || expand("a: *nope\n").is_err());
        assert!(expand("a: &a [*a]\n").is_err());
    }

    #[test]
    fn text_that_needs_quotes_gets_them() {
        let out = expand("a: &a \"- not a list\"\nb: *a\nc: &c 'x: y'\nd: *c\ne: |\n  one\n  two\n").unwrap();
        assert!(out.contains("b: \"- not a list\"") && out.contains("d: \"x: y\"") && out.contains("e: \"one\\ntwo\\n\""), "{out}");
    }
}
