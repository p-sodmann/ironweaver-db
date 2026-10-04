//! `iwctl shell <endpoint>`: an interactive client of a server, like
//! `psql` or `redis-cli` (step 14a, ADR 0036). It reads one command per
//! line from stdin and runs it through the `Database` trait over gRPC
//! (`iwdb_server::client::Remote`), so it works the same piped from a
//! script. Errors print their code and message and don't end the shell.
//!
//! The shell only translates (design rule 8): every command is one trait
//! call, and a page (`\next`) is the same call with the previous answer's
//! cursor.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use ironweaver_core::query::Pattern;
use ironweaver_core::{EdgeId, Expr, Value};
use iwdb::{
    AttrPath, Attrs, CatalogChange, CommitOptions, ConstraintKind, Edge, IdempotencyKey, IndexDef, Mutation,
    NamespaceStatus, Node,
};
use iwdb_query::exec::block_on;
use iwdb_query::{Answer, Cursor, Database, ExplainRequest, FindRequest, Limits, MatchRequest, MatchRow, QueryOptions};
use iwdb_server::client::Remote;
use serde_json::{Map, Value as Json, json};

use crate::output::{Out, ns_status_json, ns_status_text};

pub const HELP: &str = "\
commands (one per line; -- starts a comment):
  match <pattern>                 every match of a pattern: (a:Person {age: 30})-[:KNOWS*1..2]->(b)
  find <filter>                   nodes matching a filter, in the core's JSON form:
                                    {\"Label\": \"Person\"}
                                    {\"Compare\": {\"path\": [\"age\"], \"op\": \"Ge\", \"value\": {\"Int\": 18}}}
  explain <filter>                how find would read the filter (with the index it uses)
  node <id>...                    nodes by id
  edge <id>...                    edges by id
  upsert-node <id> [:Label]... [{attributes as JSON}]
  add-edge <from> <to> [:type] [{attributes as JSON}]
  delete-node <id>                delete a node and its edges
  delete-edge <id>
  namespaces                      the namespaces
  use <name>                      work in another namespace
  create-namespace <name>, drop-namespace <name>
  status                          the namespace's seq, counts, memory and indexes
  indexes                         its indexes and constraints
  create-index <path>, drop-index <path>
                                  <path> is an attribute path with dots: address.city
  add-constraint unique|required <label> <path>, drop-constraint unique|required <label> <path>
settings:
  \\next                           the next page of the last find or match
  \\limit <n>|off                  results per answer (max_results; off: the server's default)
  \\partial on|off                 answer with what was found when a limit is reached
  \\timeout <seconds>|off          the timeout of each request
  \\json, \\table                   print JSON (one object per line) or tables
  \\help, \\quit (or end of input)";

/// Why a command failed: a mistake in the command, or the database's error
/// (printed with its code).
enum Failure {
    Usage(String),
    Db(iwdb_query::Error),
}

impl From<iwdb_query::Error> for Failure {
    fn from(e: iwdb_query::Error) -> Self {
        Failure::Db(e)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Usage(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Failure::Usage(message.to_owned())
    }
}

type Done = Result<(), Failure>;

/// A paginated request whose next page `\next` reads.
enum Paged {
    Find(FindRequest),
    Match(MatchRequest),
}

struct Shell {
    db: Remote,
    namespace: String,
    out: Out,
    limit: Option<usize>,
    partial: bool,
    timeout: Option<Duration>,
    /// The last paginated request and its next page.
    next: Option<(Paged, Cursor)>,
}

/// Run the shell on stdin until `\quit` or the end of input. Exit code 0
/// if every command succeeded, 4 if one failed, 2 for an invalid endpoint.
pub fn run(endpoint: &str, namespace: &str, json: bool) -> u8 {
    let db = match Remote::connect(endpoint) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("iwctl: {}", e.message());
            return crate::exit::USAGE;
        }
    };
    let mut shell = Shell {
        db,
        namespace: namespace.to_owned(),
        out: Out { json },
        limit: None,
        partial: false,
        timeout: None,
        next: None,
    };
    let interactive = io::stdin().is_terminal();
    if interactive {
        eprintln!("iwctl shell: {} (\\help for help, \\quit to leave)", endpoint);
    }
    let mut failed = false;
    let mut lines = io::stdin().lock().lines();
    loop {
        if interactive {
            eprint!("{}> ", shell.namespace);
            let _ = io::stderr().flush();
        }
        let Some(Ok(line)) = lines.next() else { break };
        let line = line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        if matches!(line, "\\quit" | "\\q" | "quit" | "exit") {
            break;
        }
        if let Err(failure) = shell.execute(line) {
            failed = true;
            shell.report(failure);
        }
    }
    if failed { crate::exit::FAILED } else { crate::exit::OK }
}

/// The first word of `line` and the rest, trimmed.
fn split_word(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (line, ""),
    }
}

/// The words of `rest`, exactly `n` of them.
fn words<'a>(rest: &'a str, n: usize, usage: &str) -> Result<Vec<&'a str>, Failure> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.len() != n {
        return Err(format!("usage: {}", usage).into());
    }
    Ok(words)
}

fn dotted(path: &str) -> Result<AttrPath, Failure> {
    AttrPath::new(path.split('.').map(str::to_owned)).map_err(|e| Failure::Usage(e.to_string()))
}

fn constraint_kind(word: &str) -> Result<ConstraintKind, Failure> {
    match word {
        "unique" => Ok(ConstraintKind::Unique),
        "required" => Ok(ConstraintKind::Required),
        other => Err(format!("a constraint is 'unique' or 'required', not '{}'", other).into()),
    }
}

fn filter(text: &str) -> Result<Expr, Failure> {
    if text.is_empty() {
        return Err("a filter is the core's JSON form, e.g. {\"Label\": \"Person\"} (see \\help)".into());
    }
    Expr::from_json_str(text).map_err(|e| Failure::Usage(format!("invalid filter: {}", e)))
}

/// Plain JSON as a value: objects as dicts, integers that fit as `Int`,
/// other numbers as `Float`.
fn value(json: Json) -> Value {
    match json {
        Json::Null => Value::None,
        Json::Bool(b) => Value::Bool(b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::String(s),
        Json::Array(items) => Value::List(items.into_iter().map(value).collect()),
        Json::Object(entries) => Value::Dict(entries.into_iter().map(|(k, v)| (k, value(v))).collect()),
    }
}

/// A value as plain JSON, for printing: bytes as `0x…`, dates and times as
/// text, floats that JSON can't hold as text.
fn plain(v: &Value) -> Json {
    match v {
        Value::None => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or_else(|| json!(f.to_string()), Json::Number),
        Value::Half(h) => plain(&Value::Float(f64::from(h.to_f32()))),
        Value::String(s) => json!(s),
        Value::Bytes(b) => json!(format!("0x{}", b.iter().map(|b| format!("{:02x}", b)).collect::<String>())),
        Value::Date(d) => json!(d.to_string()),
        Value::DateTime(t) => json!(t.to_string()),
        Value::List(items) => Json::Array(items.iter().map(plain).collect()),
        Value::Dict(entries) => {
            let mut keys: Vec<&String> = entries.keys().collect();
            keys.sort();
            Json::Object(keys.into_iter().map(|k| (k.clone(), plain(&entries[k]))).collect::<Map<_, _>>())
        }
    }
}

fn attrs_json(attrs: &Attrs) -> Json {
    plain(&Value::Dict(attrs.clone()))
}

/// `<words> [{json}]`: the words before the first `{`, and the attributes.
fn words_and_attrs(rest: &str) -> Result<(Vec<&str>, Attrs), Failure> {
    let (head, json) = match rest.find('{') {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let attrs = match json {
        None => Attrs::new(),
        Some(text) => match serde_json::from_str::<Json>(text) {
            Ok(Json::Object(entries)) => entries.into_iter().map(|(k, v)| (k, value(v))).collect(),
            Ok(_) => return Err("the attributes are a JSON object".into()),
            Err(e) => return Err(format!("invalid attributes: {}", e).into()),
        },
    };
    Ok((head.split_whitespace().collect(), attrs))
}

fn node_json(n: &Node) -> Json {
    json!({"id": n.id, "labels": n.labels, "attr": attrs_json(&n.attr), "meta": attrs_json(&n.meta), "version": n.version})
}

fn edge_json(e: &Edge) -> Json {
    json!({"id": e.id.0, "from": e.from, "to": e.to, "type": e.ty, "attr": attrs_json(&e.attr), "meta": attrs_json(&e.meta), "version": e.version})
}

/// Columns aligned to their widest cell, under a header and a rule.
fn table(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let padded: Vec<String> = cells.iter().zip(&widths).map(|(c, w)| format!("{:<w$}", c, w = *w)).collect();
        padded.join(" | ").trim_end().to_owned()
    };
    let mut text = line(headers);
    text.push('\n');
    text += &widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-");
    for row in rows {
        text.push('\n');
        text += &line(row);
    }
    text += &format!("\n({} row{})", rows.len(), if rows.len() == 1 { "" } else { "s" });
    text
}

fn node_rows(nodes: &[Node]) -> Vec<Vec<String>> {
    nodes
        .iter()
        .map(|n| vec![n.id.clone(), n.labels.join(","), attrs_json(&n.attr).to_string(), n.version.to_string()])
        .collect()
}

fn headers(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

/// The column names of a match: the pattern's node variables (`_n` for an
/// anonymous one), then its edge variables (`_e` likewise).
fn match_headers(pattern: &Pattern, rows: &[MatchRow]) -> Vec<String> {
    let nodes = rows.first().map_or(pattern.nodes.len(), |r| r.nodes.len());
    let mut names: Vec<String> = if nodes == pattern.nodes.len() {
        pattern.nodes.iter().enumerate().map(|(i, n)| n.name.clone().unwrap_or_else(|| format!("_{}", i))).collect()
    } else {
        // Only the named nodes are reported
        pattern.nodes.iter().filter_map(|n| n.name.clone()).collect()
    };
    names.extend(pattern.edges.iter().enumerate().map(|(i, e)| e.name.clone().unwrap_or_else(|| format!("_e{}", i))));
    names
}

impl Shell {
    fn options(&self, cursor: Option<Cursor>) -> QueryOptions {
        QueryOptions {
            timeout: self.timeout,
            limits: Limits { max_results: self.limit, ..Limits::default() },
            partial: self.partial,
            cursor,
            ..QueryOptions::default()
        }
    }

    fn report(&self, failure: Failure) {
        let (code, message) = match failure {
            Failure::Usage(message) => ("usage".to_owned(), message),
            Failure::Db(e) => (e.code().to_string(), e.message().to_owned()),
        };
        if self.out.json {
            println!("{}", json!({"error": {"code": code, "message": message}}));
        } else {
            eprintln!("error ({}): {}", code, message);
        }
    }

    /// Print `value` in JSON mode, `text` otherwise.
    fn emit(&self, value: Json, text: impl FnOnce() -> String) {
        if self.out.json {
            println!("{}", value);
        } else {
            println!("{}", text());
        }
    }

    /// Print an answer's footer facts into its JSON, or below its table.
    fn emit_answer<T>(&self, mut value: Map<String, Json>, a: &Answer<T>, text: String) {
        value.insert("seq".into(), json!(a.seq));
        value.insert("cursor".into(), json!(a.next.as_ref().map(Cursor::as_str)));
        value.insert("truncated".into(), json!(a.truncated));
        value.insert("work".into(), json!({"visited": a.work.visited, "edges": a.work.edges}));
        self.emit(Json::Object(value), || {
            let mut text = text;
            if a.truncated {
                text += "\n(truncated: a limit was reached)";
            }
            if a.next.is_some() {
                text += "\n(more: \\next)";
            }
            text
        });
    }

    fn execute(&mut self, line: &str) -> Done {
        let (word, rest) = split_word(line);
        match word {
            "\\help" | "help" => {
                println!("{}", HELP);
                Ok(())
            }
            "\\json" => {
                self.out.json = true;
                Ok(())
            }
            "\\table" => {
                self.out.json = false;
                Ok(())
            }
            "\\limit" => {
                self.limit = match rest {
                    "off" => None,
                    n => Some(n.parse().ok().filter(|n| *n > 0).ok_or("usage: \\limit <n>|off (n at least 1)")?),
                };
                Ok(())
            }
            "\\partial" => {
                self.partial = match rest {
                    "on" => true,
                    "off" => false,
                    _ => return Err("usage: \\partial on|off".into()),
                };
                Ok(())
            }
            "\\timeout" => {
                self.timeout = match rest {
                    "off" => None,
                    s => Some(
                        s.parse::<f64>()
                            .ok()
                            .and_then(|s| Duration::try_from_secs_f64(s).ok())
                            .ok_or("usage: \\timeout <seconds>|off")?,
                    ),
                };
                Ok(())
            }
            "\\next" => self.next_page(),
            "use" => {
                let [name] = words(rest, 1, "use <namespace>")?[..] else { unreachable!() };
                block_on(self.db.namespace_status(name))?;
                self.namespace = name.to_owned();
                self.next = None;
                Ok(())
            }
            "match" => {
                let pattern = Pattern::parse(rest).map_err(|e| Failure::Usage(format!("invalid pattern: {}", e)))?;
                self.run_match(MatchRequest { pattern, filters: Vec::new() }, None)
            }
            "find" => self.run_find(FindRequest { filter: filter(rest)? }, None),
            "explain" => self.explain(rest),
            "node" => self.nodes(rest),
            "edge" => self.edges(rest),
            "upsert-node" => {
                let (words, attr) = words_and_attrs(rest)?;
                let usage = "usage: upsert-node <id> [:Label]... [{attributes}]";
                let (id, labels) = words.split_first().ok_or(usage)?;
                let labels = labels
                    .iter()
                    .map(|l| l.strip_prefix(':').map(str::to_owned).ok_or(usage))
                    .collect::<Result<Vec<_>, _>>()?;
                let id = (*id).to_owned();
                self.commit(Mutation::UpsertNode { id, labels, attr, meta: Attrs::new(), expected_version: None })
            }
            "add-edge" => {
                let (words, attr) = words_and_attrs(rest)?;
                let usage = "usage: add-edge <from> <to> [:type] [{attributes}]";
                let (from, to, ty) = match words[..] {
                    [from, to] => (from, to, None),
                    [from, to, ty] => (from, to, Some(ty.strip_prefix(':').ok_or(usage)?.to_owned())),
                    _ => return Err(usage.into()),
                };
                let (from, to) = (from.to_owned(), to.to_owned());
                self.commit(Mutation::AddEdge { from, to, ty, attr, meta: Attrs::new() })
            }
            "delete-node" => {
                let [id] = words(rest, 1, "delete-node <id>")?[..] else { unreachable!() };
                self.commit(Mutation::DeleteNode { id: id.to_owned(), expected_version: None })
            }
            "delete-edge" => {
                let [id] = words(rest, 1, "delete-edge <id>")?[..] else { unreachable!() };
                let id = id.parse().map_err(|_| "an edge id is a number")?;
                self.commit(Mutation::DeleteEdge { id: EdgeId(id), expected_version: None })
            }
            "namespaces" => self.namespaces(),
            "create-namespace" | "drop-namespace" => {
                let [name] = words(rest, 1, &format!("{} <name>", word))?[..] else { unreachable!() };
                let (result, what) = if word == "create-namespace" {
                    (block_on(self.db.create_namespace(name, None))?, "created")
                } else {
                    (block_on(self.db.drop_namespace(name, None))?, "dropped")
                };
                self.out.namespace_result(what, &result);
                Ok(())
            }
            "status" => {
                let status = block_on(self.db.namespace_status(&self.namespace))?;
                self.status(&status);
                Ok(())
            }
            "indexes" => {
                let status = block_on(self.db.namespace_status(&self.namespace))?;
                let catalog = block_on(self.db.catalog(&self.namespace, self.options(None)))?;
                self.out.indexes(&status, &catalog.value);
                Ok(())
            }
            "create-index" | "drop-index" => {
                let [path] = words(rest, 1, &format!("{} <path>", word))?[..] else { unreachable!() };
                let index = IndexDef { path: dotted(path)? };
                let change = if word == "create-index" {
                    CatalogChange::CreateIndex(index)
                } else {
                    CatalogChange::DropIndex(index)
                };
                self.commit_catalog(&format!("{} {}", word, path), change)
            }
            "add-constraint" | "drop-constraint" => {
                let usage = format!("{} unique|required <label> <path>", word);
                let [kind, label, path] = words(rest, 3, &usage)?[..] else { unreachable!() };
                let constraint = crate::constraint(constraint_kind(kind)?, label, dotted(path)?.keys())
                    .map_err(|e| Failure::Usage(e.to_string()))?;
                let change = if word == "add-constraint" {
                    CatalogChange::AddConstraint(constraint)
                } else {
                    CatalogChange::DropConstraint(constraint)
                };
                self.commit_catalog(&format!("{} {} {} {}", word, kind, label, path), change)
            }
            other => Err(format!("unknown command '{}' (\\help lists them)", other).into()),
        }
    }

    fn next_page(&mut self) -> Done {
        match self.next.take() {
            Some((Paged::Find(request), cursor)) => self.run_find(request, Some(cursor)),
            Some((Paged::Match(request), cursor)) => self.run_match(request, Some(cursor)),
            None => Err("no more pages: \\next continues the last find or match that had more".into()),
        }
    }

    fn run_find(&mut self, request: FindRequest, cursor: Option<Cursor>) -> Done {
        self.next = None;
        let a = block_on(self.db.find(&self.namespace, request.clone(), self.options(cursor)))?;
        let mut value = Map::new();
        value.insert("nodes".into(), Json::Array(a.value.iter().map(node_json).collect()));
        let text = table(&headers(&["id", "labels", "attr", "version"]), &node_rows(&a.value));
        self.emit_answer(value, &a, text);
        if let Some(next) = a.next {
            self.next = Some((Paged::Find(request), next));
        }
        Ok(())
    }

    fn run_match(&mut self, request: MatchRequest, cursor: Option<Cursor>) -> Done {
        self.next = None;
        let a = block_on(self.db.match_pattern(&self.namespace, request.clone(), self.options(cursor)))?;
        let names = match_headers(&request.pattern, &a.value);
        let ids = |path: &[EdgeId]| path.iter().map(|e| e.0.to_string()).collect::<Vec<_>>().join(",");
        let rows: Vec<Vec<String>> =
            a.value.iter().map(|r| r.nodes.iter().cloned().chain(r.edges.iter().map(|p| ids(p))).collect()).collect();
        let json_rows: Vec<Json> = a
            .value
            .iter()
            .map(|r| {
                let edges: Vec<Vec<u64>> = r.edges.iter().map(|p| p.iter().map(|e| e.0).collect()).collect();
                json!({"nodes": r.nodes, "edges": edges})
            })
            .collect();
        let mut value = Map::new();
        value.insert("columns".into(), json!(names));
        value.insert("rows".into(), Json::Array(json_rows));
        self.emit_answer(value, &a, table(&names, &rows));
        if let Some(next) = a.next {
            self.next = Some((Paged::Match(request), next));
        }
        Ok(())
    }

    fn explain(&self, rest: &str) -> Done {
        let request = ExplainRequest { filter: filter(rest)?, analyze: true };
        let a = block_on(self.db.explain(&self.namespace, request, self.options(None)))?;
        let e = &a.value;
        let plan = format!("{:?}", e.plan);
        let value = json!({"plan": plan, "estimated_candidates": e.estimated_candidates, "candidates": e.candidates, "nodes": e.nodes, "building": e.building});
        self.emit(value, || {
            let mut text = format!(
                "plan: {}\ncandidates: {} estimated, {} exact, of {} nodes",
                plan,
                e.estimated_candidates,
                e.candidates.map_or("?".to_owned(), |c| c.to_string()),
                e.nodes
            );
            for path in &e.building {
                text += &format!("\nindex on {} is being built: not used yet", path.join("."));
            }
            text
        });
        Ok(())
    }

    fn nodes(&self, rest: &str) -> Done {
        let ids: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();
        if ids.is_empty() {
            return Err("usage: node <id>...".into());
        }
        let a = block_on(self.db.get_nodes(&self.namespace, ids.clone(), self.options(None)))?;
        let found: Vec<Node> = a.value.iter().flatten().cloned().collect();
        let value =
            json!({"nodes": a.value.iter().map(|n| n.as_ref().map_or(Json::Null, node_json)).collect::<Vec<_>>()});
        self.emit(value, || {
            let missing: Vec<&String> =
                ids.iter().zip(&a.value).filter(|(_, n)| n.is_none()).map(|(id, _)| id).collect();
            let mut text = table(&headers(&["id", "labels", "attr", "version"]), &node_rows(&found));
            if !missing.is_empty() {
                text += &format!("\nnot found: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
            }
            text
        });
        Ok(())
    }

    fn edges(&self, rest: &str) -> Done {
        let ids = rest
            .split_whitespace()
            .map(|w| w.parse().map(EdgeId).map_err(|_| format!("an edge id is a number, not '{}'", w)))
            .collect::<Result<Vec<_>, _>>()?;
        if ids.is_empty() {
            return Err("usage: edge <id>...".into());
        }
        let a = block_on(self.db.get_edges(&self.namespace, ids, self.options(None)))?;
        let value =
            json!({"edges": a.value.iter().map(|e| e.as_ref().map_or(Json::Null, edge_json)).collect::<Vec<_>>()});
        self.emit(value, || {
            let rows: Vec<Vec<String>> = a
                .value
                .iter()
                .flatten()
                .map(|e| {
                    vec![
                        e.id.0.to_string(),
                        e.from.clone(),
                        e.to.clone(),
                        e.ty.clone().unwrap_or_default(),
                        attrs_json(&e.attr).to_string(),
                        e.version.to_string(),
                    ]
                })
                .collect();
            table(&headers(&["id", "from", "to", "type", "attr", "version"]), &rows)
        });
        Ok(())
    }

    fn commit(&mut self, mutation: Mutation) -> Done {
        let result = block_on(self.db.commit(&self.namespace, vec![mutation], CommitOptions::default()))?;
        let edges: Vec<u64> = result.edge_ids.iter().map(|e| e.0).collect();
        let value = json!({"seq": result.seq, "edge_ids": edges});
        self.emit(value, || {
            let mut text = format!("committed at seq {}", result.seq);
            if !edges.is_empty() {
                text += &format!(" (edge {})", edges.iter().map(u64::to_string).collect::<Vec<_>>().join(", "));
            }
            text
        });
        Ok(())
    }

    fn commit_catalog(&mut self, what: &str, change: CatalogChange) -> Done {
        let options = CommitOptions { idempotency_key: None::<IdempotencyKey> };
        let result = block_on(self.db.commit_catalog(&self.namespace, change, options))?;
        self.out.commit(what, result.seq, result.deduplicated);
        Ok(())
    }

    fn namespaces(&self) -> Done {
        let list = block_on(self.db.namespaces())?;
        let value = json!({"namespaces": list.iter().map(|n| json!({"id": n.id, "name": n.name.as_str(), "created": n.created.to_string()})).collect::<Vec<_>>()});
        self.emit(value, || {
            let rows: Vec<Vec<String>> =
                list.iter().map(|n| vec![n.name.to_string(), n.id.to_string(), n.created.to_string()]).collect();
            table(&headers(&["name", "id", "created"]), &rows)
        });
        Ok(())
    }

    fn status(&self, status: &NamespaceStatus) {
        self.emit(ns_status_json(status), || ns_status_text(status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align_their_columns() {
        let rows = vec![vec!["a".to_owned(), "long cell".to_owned()], vec!["bbb".to_owned(), "x".to_owned()]];
        assert_eq!(
            table(&headers(&["id", "v"]), &rows),
            "id  | v\n----+----------\na   | long cell\nbbb | x\n(2 rows)"
        );
        assert_eq!(table(&headers(&["id"]), &[]), "id\n--\n(0 rows)");
    }

    #[test]
    fn attributes_follow_the_words() {
        let (words, attrs) = words_and_attrs("a :P :Q {\"age\": 3, \"tags\": [\"x\", 1.5]}").ok().unwrap();
        assert_eq!(words, ["a", ":P", ":Q"]);
        assert_eq!(attrs["age"], Value::Int(3));
        assert_eq!(attrs["tags"], Value::List(vec![Value::String("x".into()), Value::Float(1.5)]));
        assert!(words_and_attrs("a [1]").ok().unwrap().1.is_empty());
        assert!(words_and_attrs("a {1}").is_err());
        assert!(words_and_attrs("a {\"k\": 1} {").is_err());
    }

    #[test]
    fn values_print_as_plain_json() {
        let dict = Value::Dict([("b".to_owned(), Value::Bytes(vec![0, 255])), ("a".to_owned(), Value::None)].into());
        assert_eq!(plain(&dict).to_string(), r#"{"a":null,"b":"0x00ff"}"#);
        assert_eq!(plain(&Value::Float(f64::NAN)), json!("NaN"));
        assert_eq!(
            value(json!({"n": [1, 2.5, null]})),
            Value::Dict([("n".to_owned(), Value::List(vec![Value::Int(1), Value::Float(2.5), Value::None]))].into())
        );
    }

    #[test]
    fn match_columns_are_the_pattern_s_variables() {
        let pattern = Pattern::parse("(a)-[r:KNOWS]->(b)").ok().unwrap();
        assert_eq!(match_headers(&pattern, &[]), ["a", "b", "r"]);
    }
}
