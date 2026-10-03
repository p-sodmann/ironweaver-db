//! The catalog: namespaces, index definitions and constraints.
//!
//! Like Postgres' `pg_catalog`, the catalog describes the data rather than
//! holding it. A store has namespaces, each with its own graph. Each
//! namespace has property indexes (by attribute path) and constraints
//! (unique or required, per label and attribute path).
//!
//! Every catalog type validates on construction and on deserialization,
//! so an invalid definition can't exist in memory. Errors are
//! [`CatalogError`]s, never panics.
//!
//! Storage (ADR 0003): a namespace's catalog is saved in the graph meta of
//! that namespace's file, under [`CATALOG_KEY`](crate::reserved::CATALOG_KEY),
//! as a versioned JSON document (see [`NamespaceCatalog::to_meta_value`]).
//! On load it is the source of truth for indexes: the core also saves index
//! definitions (`metadata.indexes`), and [`NamespaceCatalog::apply_indexes`]
//! makes the graph's indexes match the catalog.

use std::collections::BTreeSet;
use std::fmt;

use ironweaver_core::{GraphError, Value};
use serde::{Deserialize, Serialize};

use crate::{DbGraph, Error};

/// Version of the catalog document stored in graph meta.
pub const CATALOG_FORMAT: u32 = 1;

/// Longest attribute path: the core's value nesting limit, so a longer
/// path could never match a saved value.
pub const MAX_PATH_LEN: usize = ironweaver_core::format::MAX_DEPTH;

/// Longest namespace name, in bytes.
pub const MAX_NAMESPACE_LEN: usize = 64;

/// An invalid catalog definition, or a catalog that can't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    #[error("attribute path is empty")]
    EmptyPath,
    #[error("attribute path has an empty key at position {position}")]
    EmptyPathKey { position: usize },
    #[error("attribute path has {len} keys, more than the maximum of {MAX_PATH_LEN}")]
    PathTooLong { len: usize },
    #[error("label is empty")]
    EmptyLabel,
    #[error("invalid namespace name '{name}': {reason}")]
    InvalidNamespaceName { name: String, reason: &'static str },
    /// The graph meta of a saved file has no catalog.
    #[error("graph meta has no catalog ('{}')", crate::reserved::CATALOG_KEY)]
    Missing,
    /// The stored catalog is not a string value.
    #[error("the stored catalog is not a string but {found}")]
    NotAString { found: String },
    /// The stored catalog document can't be decoded (or holds an invalid
    /// definition).
    #[error("the stored catalog can't be decoded: {0}")]
    Decode(String),
    #[error("catalog format {found} is not supported (this version reads format {CATALOG_FORMAT})")]
    UnsupportedFormat { found: u32 },
}

/// An attribute path: an attribute name, then keys into nested dicts
/// (`["address", "city"]` reads `attr["address"]["city"]`). Non-empty, no
/// empty keys, at most [`MAX_PATH_LEN`] keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct AttrPath(Vec<String>);

impl AttrPath {
    pub fn new<S: Into<String>>(keys: impl IntoIterator<Item = S>) -> Result<Self, CatalogError> {
        Self::try_from(keys.into_iter().map(Into::into).collect::<Vec<String>>())
    }

    /// The keys, as the core's index and filter functions take them.
    pub fn keys(&self) -> &[String] {
        &self.0
    }
}

impl TryFrom<Vec<String>> for AttrPath {
    type Error = CatalogError;

    fn try_from(keys: Vec<String>) -> Result<Self, CatalogError> {
        if keys.is_empty() {
            return Err(CatalogError::EmptyPath);
        }
        if keys.len() > MAX_PATH_LEN {
            return Err(CatalogError::PathTooLong { len: keys.len() });
        }
        if let Some(position) = keys.iter().position(String::is_empty) {
            return Err(CatalogError::EmptyPathKey { position });
        }
        Ok(AttrPath(keys))
    }
}

impl From<AttrPath> for Vec<String> {
    fn from(path: AttrPath) -> Self {
        path.0
    }
}

impl fmt::Display for AttrPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("."))
    }
}

/// A node label. Non-empty.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Label(String);

impl Label {
    pub fn new(name: impl Into<String>) -> Result<Self, CatalogError> {
        Self::try_from(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Label {
    type Error = CatalogError;

    fn try_from(name: String) -> Result<Self, CatalogError> {
        if name.is_empty() {
            return Err(CatalogError::EmptyLabel);
        }
        Ok(Label(name))
    }
}

impl From<Label> for String {
    fn from(label: Label) -> Self {
        label.0
    }
}

/// A namespace name: 1 to [`MAX_NAMESPACE_LEN`] ASCII letters, digits, `_`
/// or `-`, starting with a letter or digit. Names end up in file and
/// directory names, hence the small alphabet.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NamespaceName(String);

impl NamespaceName {
    pub fn new(name: impl Into<String>) -> Result<Self, CatalogError> {
        Self::try_from(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NamespaceName {
    type Error = CatalogError;

    fn try_from(name: String) -> Result<Self, CatalogError> {
        let reason = if name.is_empty() {
            Some("empty")
        } else if name.len() > MAX_NAMESPACE_LEN {
            Some("longer than 64 bytes")
        } else if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            Some("only ASCII letters, digits, '_' and '-' are allowed")
        } else if !name.as_bytes()[0].is_ascii_alphanumeric() {
            Some("must start with a letter or digit")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(CatalogError::InvalidNamespaceName { name, reason }),
            None => Ok(NamespaceName(name)),
        }
    }
}

impl From<NamespaceName> for String {
    fn from(name: NamespaceName) -> Self {
        name.0
    }
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A node property index on an attribute path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDef {
    pub path: AttrPath,
}

/// What a [`Constraint`] requires of the nodes with its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    /// No two nodes with the label have equal values at the path (numbers
    /// compare across int and float, as in the core's indexes). Nodes
    /// without a value don't conflict. Backed by an index on the path.
    Unique,
    /// Every node with the label has a (non-none) value at the path.
    Required,
}

/// A constraint on the nodes with `label`, at attribute `path`. Checked by
/// the commit pipeline (step 3).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub label: Label,
    pub path: AttrPath,
}

impl fmt::Display for Constraint {
    /// `unique constraint on :Person(email)`, `required constraint on :Person(address.city)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ConstraintKind::Unique => "unique",
            ConstraintKind::Required => "required",
        };
        write!(f, "{} constraint on :{}({})", kind, self.label.as_str(), self.path)
    }
}

/// The catalog of one namespace: its indexes and constraints, sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceCatalog {
    #[serde(default)]
    indexes: BTreeSet<IndexDef>,
    #[serde(default)]
    constraints: BTreeSet<Constraint>,
}

/// The catalog document stored in graph meta, as written: a format
/// version, the namespace it belongs to and its catalog.
#[derive(Serialize)]
struct StoredRef<'a> {
    format: u32,
    namespace: &'a NamespaceName,
    indexes: &'a BTreeSet<IndexDef>,
    constraints: &'a BTreeSet<Constraint>,
}

/// The same, as read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    #[allow(dead_code)] // checked through `StoredFormat` first
    format: u32,
    namespace: NamespaceName,
    #[serde(default)]
    indexes: BTreeSet<IndexDef>,
    #[serde(default)]
    constraints: BTreeSet<Constraint>,
}

/// Just the format version of a stored catalog document, read first so
/// that a newer format is reported as such and not as a decoding error.
#[derive(Deserialize)]
struct StoredFormat {
    format: u32,
}

/// How [`NamespaceCatalog::apply_indexes`] changed a graph's indexes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexChanges {
    /// Catalog indexes the graph didn't have, now built.
    pub created: Vec<AttrPath>,
    /// Indexes the graph had but the catalog doesn't list, now dropped
    /// (as the core reports them: they need not be valid paths).
    pub dropped: Vec<Vec<String>>,
}

impl NamespaceCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The declared indexes, sorted.
    pub fn indexes(&self) -> impl Iterator<Item = &IndexDef> {
        self.indexes.iter()
    }

    /// The constraints, sorted.
    pub fn constraints(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints.iter()
    }

    pub fn has_index(&self, index: &IndexDef) -> bool {
        self.indexes.contains(index)
    }

    pub fn has_constraint(&self, constraint: &Constraint) -> bool {
        self.constraints.contains(constraint)
    }

    /// Add an index definition; false if it exists already.
    pub fn add_index(&mut self, index: IndexDef) -> bool {
        self.indexes.insert(index)
    }

    /// Remove an index definition; false if there was none.
    pub fn remove_index(&mut self, index: &IndexDef) -> bool {
        self.indexes.remove(index)
    }

    /// Add a constraint; false if it exists already.
    pub fn add_constraint(&mut self, constraint: Constraint) -> bool {
        self.constraints.insert(constraint)
    }

    /// Remove a constraint; false if there was none.
    pub fn remove_constraint(&mut self, constraint: &Constraint) -> bool {
        self.constraints.remove(constraint)
    }

    /// The paths the graph must have property indexes on: the declared
    /// indexes and the paths of unique constraints, sorted, each once.
    pub fn index_paths(&self) -> BTreeSet<&AttrPath> {
        let unique = self.constraints.iter().filter(|c| c.kind == ConstraintKind::Unique).map(|c| &c.path);
        self.indexes.iter().map(|i| &i.path).chain(unique).collect()
    }

    /// The catalog as stored in graph meta under
    /// [`CATALOG_KEY`](crate::reserved::CATALOG_KEY): a string holding a
    /// JSON document `{"format": 1, "namespace": .., "indexes": [..],
    /// "constraints": [..]}`, with sorted entries, so equal catalogs give
    /// equal bytes.
    pub fn to_meta_value(&self, namespace: &NamespaceName) -> Value {
        let doc =
            StoredRef { format: CATALOG_FORMAT, namespace, indexes: &self.indexes, constraints: &self.constraints };
        // Serializing structs, strings and sets to JSON can't fail (an empty
        // string would still be caught as undecodable on load)
        Value::String(serde_json::to_string(&doc).unwrap_or_default())
    }

    /// Read a catalog stored by [`to_meta_value`](Self::to_meta_value),
    /// with the namespace it belongs to. Every definition is validated.
    pub fn from_meta_value(value: &Value) -> Result<(NamespaceName, NamespaceCatalog), CatalogError> {
        let Value::String(json) = value else {
            return Err(CatalogError::NotAString { found: format!("{:?}", value) });
        };
        let decode = |e: serde_json::Error| CatalogError::Decode(e.to_string());
        let StoredFormat { format } = serde_json::from_str(json).map_err(decode)?;
        if format != CATALOG_FORMAT {
            return Err(CatalogError::UnsupportedFormat { found: format });
        }
        let doc: Stored = serde_json::from_str(json).map_err(decode)?;
        Ok((doc.namespace, NamespaceCatalog { indexes: doc.indexes, constraints: doc.constraints }))
    }

    /// Make `graph`'s property indexes match the catalog, then bring them
    /// up to date: drop the indexes the catalog doesn't list, build the
    /// missing ones ([`index_paths`](Self::index_paths)), flush the rest.
    ///
    /// Call after loading a file: the core recreates the index definitions
    /// saved in the file (`metadata.indexes`) empty and dirty, and the
    /// catalog is the source of truth when they disagree. O(n log n) per
    /// index built, O(n) per dirty index flushed.
    pub fn apply_indexes(&self, graph: &mut DbGraph) -> Result<IndexChanges, Error> {
        let wanted = self.index_paths();
        let dropped: Vec<Vec<String>> = graph
            .index_paths()
            .into_iter()
            .filter(|&have| !wanted.iter().any(|w| w.keys() == have))
            .map(<[String]>::to_vec)
            .collect();
        for path in &dropped {
            graph.drop_index(path);
        }
        let mut created = Vec::new();
        for path in wanted {
            if graph.create_index::<GraphError>(path.keys())? {
                created.push(path.clone());
            }
        }
        graph.flush_indexes()?;
        Ok(IndexChanges { created, dropped })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(keys: &[&str]) -> AttrPath {
        AttrPath::new(keys.iter().copied()).expect("valid path")
    }

    fn sample() -> NamespaceCatalog {
        let mut c = NamespaceCatalog::new();
        assert!(c.add_index(IndexDef { path: path(&["age"]) }));
        assert!(!c.add_index(IndexDef { path: path(&["age"]) }));
        c.add_index(IndexDef { path: path(&["address", "city"]) });
        let person = Label::new("Person").expect("label");
        c.add_constraint(Constraint { kind: ConstraintKind::Unique, label: person.clone(), path: path(&["email"]) });
        c.add_constraint(Constraint { kind: ConstraintKind::Required, label: person, path: path(&["name"]) });
        c
    }

    #[test]
    fn paths_validate() {
        assert_eq!(AttrPath::new(Vec::<String>::new()), Err(CatalogError::EmptyPath));
        assert_eq!(AttrPath::new(["a", ""]), Err(CatalogError::EmptyPathKey { position: 1 }));
        let long = vec!["k"; MAX_PATH_LEN + 1];
        assert_eq!(AttrPath::new(long), Err(CatalogError::PathTooLong { len: MAX_PATH_LEN + 1 }));
        assert_eq!(path(&["a", "b"]).keys(), ["a", "b"]);
        assert_eq!(path(&["a", "b"]).to_string(), "a.b");
    }

    #[test]
    fn labels_and_namespace_names_validate() {
        assert_eq!(Label::new(""), Err(CatalogError::EmptyLabel));
        assert_eq!(Label::new("Person").expect("label").as_str(), "Person");
        for ok in ["default", "a", "A-1_b", "0x", &"n".repeat(64)] {
            assert!(NamespaceName::new(ok).is_ok(), "{}", ok);
        }
        for bad in ["", "-a", "_a", "a b", "a/b", "..", "ä", &"n".repeat(65)] {
            assert!(matches!(NamespaceName::new(bad), Err(CatalogError::InvalidNamespaceName { .. })), "{}", bad);
        }
    }

    #[test]
    fn deserializing_validates() {
        let err = |json: &str| serde_json::from_str::<NamespaceCatalog>(json).expect_err(json).to_string();
        assert!(err(r#"{"indexes": [{"path": []}]}"#).contains("attribute path is empty"));
        assert!(err(r#"{"indexes": [{"path": ["a", ""]}]}"#).contains("empty key"));
        assert!(err(r#"{"constraints": [{"kind": "unique", "label": "", "path": ["a"]}]}"#).contains("label is empty"));
        assert!(
            err(r#"{"constraints": [{"kind": "exists", "label": "A", "path": ["a"]}]}"#).contains("unknown variant")
        );
        assert!(err(r#"{"indexes": [], "views": []}"#).contains("unknown field"));
        assert!(err(r#"{"indexes": [{"path": ["a"], "kind": "hash"}]}"#).contains("unknown field"));
        let bad_name = serde_json::from_str::<NamespaceName>(r#""a/b""#).expect_err("name").to_string();
        assert!(bad_name.contains("invalid namespace"), "{}", bad_name);
    }

    #[test]
    fn stored_form_round_trips_and_is_deterministic() {
        let ns = NamespaceName::new("social").expect("name");
        let catalog = sample();
        let value = catalog.to_meta_value(&ns);
        let Value::String(json) = &value else { panic!("not a string: {:?}", value) };
        assert_eq!(
            json,
            r#"{"format":1,"namespace":"social","indexes":[{"path":["address","city"]},{"path":["age"]}],"constraints":[{"kind":"unique","label":"Person","path":["email"]},{"kind":"required","label":"Person","path":["name"]}]}"#
        );
        assert_eq!(NamespaceCatalog::from_meta_value(&value), Ok((ns.clone(), catalog)));
        let empty = NamespaceCatalog::new().to_meta_value(&ns);
        assert_eq!(NamespaceCatalog::from_meta_value(&empty), Ok((ns, NamespaceCatalog::new())));
    }

    #[test]
    fn stored_form_errors_are_typed() {
        let read = |v: Value| NamespaceCatalog::from_meta_value(&v);
        assert!(matches!(read(Value::Int(1)), Err(CatalogError::NotAString { .. })));
        assert!(matches!(read(Value::from("not json")), Err(CatalogError::Decode(_))));
        assert!(matches!(read(Value::from(r#"{"namespace": "a"}"#)), Err(CatalogError::Decode(_))));
        assert_eq!(
            read(Value::from(r#"{"format": 2, "namespace": "a", "graphs": {}}"#)),
            Err(CatalogError::UnsupportedFormat { found: 2 })
        );
        assert!(matches!(
            read(Value::from(r#"{"format": 1, "namespace": "a", "indexes": [{"path": []}]}"#)),
            Err(CatalogError::Decode(msg)) if msg.contains("attribute path is empty")
        ));
        assert!(matches!(
            read(Value::from(r#"{"format": 1, "namespace": "", "indexes": []}"#)),
            Err(CatalogError::Decode(msg)) if msg.contains("invalid namespace name")
        ));
    }

    #[test]
    fn constraints_display() {
        let c = Constraint {
            kind: ConstraintKind::Required,
            label: Label::new("Person").expect("label"),
            path: path(&["address", "city"]),
        };
        assert_eq!(c.to_string(), "required constraint on :Person(address.city)");
    }

    #[test]
    fn unique_constraints_need_an_index() {
        let c = sample();
        let paths: Vec<String> = c.index_paths().into_iter().map(ToString::to_string).collect();
        assert_eq!(paths, ["address.city", "age", "email"]);
    }
}
