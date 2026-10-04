//! LGF, the LEMON Graph Format, read into a graph (ADR 0033).
//!
//! ```text
//! @nodes
//! label   coordinates  size   title
//! 0       (10,20)      10     "First node"
//! 1       (80,80)      8      "Second node"
//! @arcs
//!         capacity
//! 0   1   10
//! @attributes
//! caption "LEMON test digraph"
//! ```
//!
//! A file is a list of sections, each starting with a line `@<kind>`
//! (optionally followed by a name, which is ignored):
//!
//! - `@nodes`: a header line naming the columns, then a row per node. The
//!   `label` column (required) is the node's id; the others become its
//!   attributes.
//! - `@arcs` and `@edges`: a header line naming the columns after the two
//!   ends, then a row per edge: the labels of its source and target, then
//!   its values. Every row is one directed edge from the first node to the
//!   second (an undirected `@edges` row too, in the order written), with
//!   the columns as attributes (a `label` column too: edge ids are the
//!   database's). Edges have no type.
//! - `@attributes`: rows of a name and a value, graph-level, which a
//!   namespace has no place for: dropped and listed.
//! - `@red_nodes` and `@blue_nodes` (bipartite graphs) are refused.
//!
//! Lines are split into tokens at whitespace; a token in double quotes may
//! hold whitespace. Escapes, in and outside quotes: `\\`, `\"`, `\'`, `\?`,
//! `\a`, `\b`, `\f`, `\n`, `\r`, `\t`, `\v`, `\x` with hex digits, and up
//! to three octal digits. Empty lines (whitespace only) and lines starting
//! with `#` are skipped, so a header line can't be empty: a section of
//! edges without columns has the header `label`, as LEMON writes it. Values are typed by their look: an unquoted token that parses
//! as a 64-bit integer is an `Int`, one with a digit that parses as a
//! number is a `Float`, anything else and every quoted token a `String`.
//!
//! Nodes get no labels. The file is read line by line: memory is the graph
//! and a line.

use std::collections::HashSet;
use std::io::{self, BufRead};

use ironweaver_core::{Attrs, Value};
use iwdb_engine::{DbGraph, DbRecord};

/// An LGF file, read.
#[derive(Debug)]
pub struct Lgf {
    pub graph: DbGraph,
    /// What was left out: `@attributes` entries, as `@attributes 'name'`.
    pub dropped: Vec<String>,
}

/// Why an LGF file couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum LgfError {
    /// The file is invalid at this line (from 1).
    #[error("line {line}: {message}")]
    Invalid { line: u64, message: String },
    /// Reading failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A token of a line, unescaped.
#[derive(Clone, Debug, PartialEq)]
struct Token {
    text: String,
    quoted: bool,
}

impl Token {
    /// The value it stands for: see the module docs.
    fn value(&self) -> Value {
        if !self.quoted {
            if let Ok(v) = self.text.parse::<i64>() {
                return Value::Int(v);
            }
            if self.text.bytes().any(|b| b.is_ascii_digit())
                && let Ok(v) = self.text.parse::<f64>()
            {
                return Value::Float(v);
            }
        }
        Value::String(self.text.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Nodes,
    Edges,
    Attributes,
}

/// The section being read: its kind and, once read, its columns.
struct Section {
    kind: Kind,
    columns: Option<Vec<String>>,
    /// `@nodes`: the index of the `label` column.
    label: usize,
}

/// Read an LGF file. See the module docs for what it accepts.
pub fn read(mut input: impl BufRead) -> Result<Lgf, LgfError> {
    let mut graph = DbGraph::new();
    let mut dropped = Vec::new();
    let mut section: Option<Section> = None;
    let mut buf = Vec::new();
    let mut line_no = 0u64;
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        line_no += 1;
        let invalid = |message: String| LgfError::Invalid { line: line_no, message };
        let line = std::str::from_utf8(&buf).map_err(|_| invalid("the line isn't valid UTF-8".into()))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(head) = line.strip_prefix('@') {
            let kind = head.split_whitespace().next().unwrap_or("");
            let kind = match kind {
                "nodes" => Kind::Nodes,
                "arcs" | "edges" => Kind::Edges,
                "attributes" => Kind::Attributes,
                "red_nodes" | "blue_nodes" => {
                    return Err(invalid(format!("@{} sections (bipartite graphs) aren't supported", kind)));
                }
                other => return Err(invalid(format!("unknown section '@{}'", other))),
            };
            let columns = (kind == Kind::Attributes).then(Vec::new);
            section = Some(Section { kind, columns, label: 0 });
            continue;
        }
        let tokens = tokenize(line).map_err(invalid)?;
        let Some(section) = section.as_mut() else {
            return Err(invalid("a line before the first section (@nodes, @arcs, ...)".into()));
        };
        let Some(columns) = &section.columns else {
            // The header
            let mut seen = HashSet::new();
            for t in &tokens {
                if !seen.insert(t.text.as_str()) {
                    return Err(invalid(format!("column '{}' appears twice", t.text)));
                }
            }
            let names: Vec<String> = tokens.into_iter().map(|t| t.text).collect();
            if section.kind == Kind::Nodes {
                section.label = names
                    .iter()
                    .position(|n| n == "label")
                    .ok_or_else(|| invalid("@nodes needs a 'label' column (the node ids)".into()))?;
            }
            section.columns = Some(names);
            continue;
        };
        match section.kind {
            Kind::Nodes => {
                if tokens.len() != columns.len() {
                    return Err(invalid(format!("{} values for {} columns", tokens.len(), columns.len())));
                }
                let id = &tokens[section.label].text;
                if id.is_empty() {
                    return Err(invalid("a node's label is empty".into()));
                }
                if graph.contains_node(id) {
                    return Err(invalid(format!("node '{}' appears twice", id)));
                }
                let attr = attrs(columns, &tokens, Some(section.label));
                graph.add_node(id.clone(), record(attr)).map_err(|e| invalid(e.to_string()))?;
            }
            Kind::Edges => {
                if tokens.len() != columns.len() + 2 {
                    return Err(invalid(format!(
                        "{} values for an edge's two ends and {} columns",
                        tokens.len(),
                        columns.len()
                    )));
                }
                let end = |t: &Token| graph.node_ix(&t.text).ok_or_else(|| invalid(format!("no node '{}'", t.text)));
                let (from, to) = (end(&tokens[0])?, end(&tokens[1])?);
                let attr = attrs(columns, &tokens[2..], None);
                graph.add_edge(from, to, record(attr)).map_err(|e| invalid(e.to_string()))?;
            }
            Kind::Attributes => {
                if tokens.len() != 2 {
                    return Err(invalid(format!("an attribute is a name and a value, not {} tokens", tokens.len())));
                }
                dropped.push(format!("@attributes '{}'", tokens[0].text));
            }
        }
    }
    Ok(Lgf { graph, dropped })
}

fn record(attr: Attrs) -> DbRecord {
    DbRecord { attr, meta: Attrs::new(), version: 1 }
}

/// The attributes of a row: each column's value, except `skip`'s.
fn attrs(columns: &[String], tokens: &[Token], skip: Option<usize>) -> Attrs {
    columns
        .iter()
        .zip(tokens)
        .enumerate()
        .filter(|(i, _)| Some(*i) != skip)
        .map(|(_, (name, token))| (name.clone(), token.value()))
        .collect()
}

/// Split a line into tokens.
fn tokenize(line: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        let mut text = String::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err("a quoted value has no closing '\"'".into()),
                    Some('"') => break,
                    Some('\\') => text.push(escape(&mut chars)?),
                    Some(c) => text.push(c),
                }
            }
            if chars.peek().is_some_and(|c| !c.is_whitespace()) {
                return Err("a quoted value must be followed by whitespace".into());
            }
            tokens.push(Token { text, quoted: true });
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                if c == '\\' {
                    text.push(escape(&mut chars)?);
                } else {
                    text.push(c);
                }
            }
            tokens.push(Token { text, quoted: false });
        }
    }
    Ok(tokens)
}

/// The character of an escape sequence, after its `\`.
fn escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<char, String> {
    let c = chars.next().ok_or("a '\\' at the end of the line")?;
    let code = match c {
        '\\' | '"' | '\'' | '?' => return Ok(c),
        'a' => 0x07,
        'b' => 0x08,
        'f' => 0x0c,
        'n' => 0x0a,
        'r' => 0x0d,
        't' => 0x09,
        'v' => 0x0b,
        'x' => {
            let mut code = 0u32;
            let mut digits = 0;
            while let Some(d) = chars.next_if(char::is_ascii_hexdigit) {
                code = code.saturating_mul(16).saturating_add(d.to_digit(16).unwrap_or(0));
                digits += 1;
            }
            if digits == 0 {
                return Err("'\\x' without hex digits".into());
            }
            code
        }
        '0'..='7' => {
            let mut code = c.to_digit(8).unwrap_or(0);
            for _ in 0..2 {
                match chars.next_if(|d| ('0'..='7').contains(d)) {
                    Some(d) => code = code * 8 + d.to_digit(8).unwrap_or(0),
                    None => break,
                }
            }
            code
        }
        other => return Err(format!("unknown escape '\\{}'", other)),
    };
    char::from_u32(code).ok_or_else(|| format!("escape for {:#x}, which isn't a character", code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(line: &str) -> Vec<(String, bool)> {
        tokenize(line).unwrap().into_iter().map(|t| (t.text, t.quoted)).collect()
    }

    fn lgf(text: &str) -> Result<Lgf, LgfError> {
        read(text.as_bytes())
    }

    fn error(text: &str) -> String {
        lgf(text).unwrap_err().to_string()
    }

    #[test]
    fn lines_split_into_tokens_with_quotes_and_escapes() {
        assert_eq!(tokens("  a  bb\tc "), [("a".into(), false), ("bb".into(), false), ("c".into(), false)]);
        assert_eq!(tokens(r#""x y" "" z"#), [("x y".into(), true), ("".into(), true), ("z".into(), false)]);
        assert_eq!(tokens(r#""a\"b\\c\n\t" d\x41\101\0"#), [("a\"b\\c\n\t".into(), true), ("dAA\0".into(), false)]);
        assert_eq!(tokens(r#""\x263a" \?\'"#), [("\u{263a}".into(), true), ("?'".into(), false)]);
        for (line, message) in [
            (r#""open"#, "no closing"),
            (r#""a"b"#, "followed by whitespace"),
            (r"a\q", "unknown escape"),
            (r"a\", "end of the line"),
            (r"\x", "without hex digits"),
            (r"\xd800", "isn't a character"),
        ] {
            let e = tokenize(line).unwrap_err();
            assert!(e.contains(message), "{}: {}", line, e);
        }
    }

    #[test]
    fn values_are_typed_by_their_look() {
        let value = |text: &str, quoted| Token { text: text.into(), quoted }.value();
        assert_eq!(value("42", false), Value::Int(42));
        assert_eq!(value("-7", false), Value::Int(-7));
        assert_eq!(value("2.5", false), Value::Float(2.5));
        assert_eq!(value("1e3", false), Value::Float(1000.0));
        assert_eq!(value("42", true), Value::String("42".into()));
        for s in ["inf", "NaN", "(10,20)", "abc", "", "1.2.3"] {
            assert_eq!(value(s, false), Value::String(s.into()), "{}", s);
        }
    }

    /// The example of LEMON's documentation.
    #[test]
    fn the_lemon_example_reads() {
        let read = lgf(r#"
# A comment
@nodes
label   coordinates size    title
0       (10,20)     10      "First node"
1       (80,80)     8       "Second node"
2       (20,80)     10      "Third node"
@arcs
        capacity
0   1   10
1   2   20
2   0   8
@attributes
source 0
caption "LEMON test digraph"
"#)
        .unwrap();
        let g = &read.graph;
        assert_eq!((g.node_count(), g.edge_count()), (3, 3));
        let first = g.node_by_id("0").unwrap();
        assert_eq!(first.data.attr["coordinates"], Value::String("(10,20)".into()));
        assert_eq!(first.data.attr["size"], Value::Int(10));
        assert_eq!(first.data.attr["title"], Value::String("First node".into()));
        assert!(!first.data.attr.contains_key("label"));
        assert!(first.labels().is_empty());
        let (_, edge) = g.edges().find(|(_, e)| g.node(e.source()).unwrap().id() == "1").unwrap();
        assert_eq!(g.node(edge.target()).unwrap().id(), "2");
        assert_eq!((edge.data.attr["capacity"].clone(), edge.edge_type()), (Value::Int(20), None));
        assert_eq!(read.dropped, ["@attributes 'source'", "@attributes 'caption'"]);
    }

    #[test]
    fn edges_sections_arc_labels_and_section_names() {
        let read = lgf(
            "@nodes cities\nlabel\na\nb\n@edges roads\n\t\tlabel length\na b 0 1.5\nb a 1 2\n\n@arcs\n\t\tweight\na a 3\n",
        )
        .unwrap();
        let g = &read.graph;
        assert_eq!(g.edge_count(), 3);
        let labels: Vec<Value> = g.edges().filter_map(|(_, e)| e.data.attr.get("label").cloned()).collect();
        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&Value::Int(0)) && labels.contains(&Value::Int(1)));
        assert!(g.edges().any(|(_, e)| e.data.attr.get("length") == Some(&Value::Float(1.5))));
        assert!(read.dropped.is_empty());
        // A header line of whitespace is an empty line: the next line is
        // taken as the header, as in LEMON
        assert!(error("@nodes\nlabel\na\n@arcs\n\t\t\na a\n").contains("column 'a' appears twice"));
    }

    #[test]
    fn invalid_files_name_the_line() {
        for (text, line, message) in [
            ("label\na\n", 1, "before the first section"),
            ("@nodes\nid\n", 2, "needs a 'label' column"),
            ("@nodes\nlabel x x\n", 2, "column 'x' appears twice"),
            ("@nodes\nlabel x\na\n", 3, "1 values for 2 columns"),
            ("@nodes\nlabel\na\na\n", 4, "node 'a' appears twice"),
            ("@nodes\nlabel\n\"\"\n", 3, "label is empty"),
            ("@nodes\nlabel\na\n@arcs\n-\na b\n", 6, "2 values for an edge's two ends and 1 columns"),
            ("@nodes\nlabel\na\n@arcs\nw\na b 1\n", 6, "no node 'b'"),
            ("@red_nodes\nlabel\n", 1, "bipartite"),
            ("@things\n", 1, "unknown section '@things'"),
            ("@attributes\ncaption\n", 2, "a name and a value"),
            ("@nodes\nlabel\n\"open\n", 3, "no closing"),
        ] {
            let e = error(text);
            assert!(e.starts_with(&format!("line {}: ", line)) && e.contains(message), "{:?}: {}", text, e);
        }
        let e = lgf_bytes(b"@nodes\nlabel\n\xff\n");
        assert!(e.starts_with("line 3: ") && e.contains("UTF-8"), "{}", e);
    }

    fn lgf_bytes(bytes: &[u8]) -> String {
        read(bytes).unwrap_err().to_string()
    }

    #[test]
    fn empty_sections_and_files_read() {
        assert_eq!(lgf("").unwrap().graph.node_count(), 0);
        assert_eq!(lgf("@nodes\n@arcs\n").unwrap().graph.node_count(), 0);
        let read = lgf("@nodes\r\nlabel\r\nx\r\n").unwrap();
        assert!(read.graph.contains_node("x"));
    }
}
