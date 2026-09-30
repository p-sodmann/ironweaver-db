//! Saving and loading [`DbGraph`]s in the core's file format (version 2).
//!
//! The file is an ordinary ironweaver graph file; the database's own data
//! lives in reserved meta keys (see [`reserved`](crate::reserved)):
//!
//! - each node's and edge's `meta` holds the user's meta entries plus
//!   `iwdb.version` (an `Int`, so versions above `i64::MAX` can't be saved);
//! - the graph meta holds `iwdb.catalog`, the namespace and its catalog
//!   (ADR 0003), and nothing else.
//!
//! So `ironweaver_core::format::from_binary` can read a database file as a
//! `Record` graph, with the version as a meta entry.
//!
//! Saves are deterministic: attribute and meta maps are written sorted by
//! key and without a timestamp, so equal graphs with equal slot order and
//! equal catalogs save to equal bytes.
//!
//! Loading rejects, with an [`Error`] and never a panic: a node or edge
//! without a valid `iwdb.version`, unknown `iwdb.*` keys, graph meta other
//! than the catalog, and an invalid catalog. Files written by other tools
//! (or in format 1) therefore don't load as database files.

use std::io::{Read, Write};

use ironweaver_core::format::{self, Codec, GraphWriter, LoadAttrs, LoadEdge, LoadGraph, LoadKind, LoadNode};
use ironweaver_core::value::{serialize_sorted, sorted_entries};
use ironweaver_core::{Attrs, Value};
use serde::{Serialize, Serializer};

use crate::catalog::{CatalogError, IndexChanges, NamespaceCatalog, NamespaceName};
use crate::reserved::{is_reserved, CATALOG_KEY, VERSION_KEY};
use crate::{DbGraph, DbRecord, Entity, Error};

/// The graph-level data a database file carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMeta {
    /// The namespace the graph belongs to.
    pub namespace: NamespaceName,
    /// The namespace's catalog.
    pub catalog: NamespaceCatalog,
}

impl GraphMeta {
    /// The graph meta map as saved.
    pub fn to_attrs(&self) -> Attrs {
        [(CATALOG_KEY.to_owned(), self.catalog.to_meta_value(&self.namespace))].into()
    }

    /// Read the graph meta of a loaded file.
    pub fn from_load(meta: &LoadAttrs<'_>) -> Result<Self, Error> {
        let mut found = None;
        for (key, value) in meta.iter() {
            if key == CATALOG_KEY {
                if found.is_some() {
                    return Err(CatalogError::Decode(format!("'{}' appears twice", CATALOG_KEY)).into());
                }
                found = Some(NamespaceCatalog::from_meta_value(&value.to_value())?);
            } else if is_reserved(key) {
                return Err(Error::UnknownReservedKey { entity: Entity::Graph, key: key.to_owned() });
            } else {
                return Err(Error::UnexpectedGraphMeta { key: key.to_owned() });
            }
        }
        let (namespace, catalog) = found.ok_or(CatalogError::Missing)?;
        Ok(GraphMeta { namespace, catalog })
    }
}

/// [`Codec`] for [`DbGraph`]: writes `attr` and `meta` sorted by key, with
/// the version merged into `meta` as `iwdb.version`, and the given graph
/// meta.
///
/// Saving fails (with a `GraphError::Format` naming the problem) if a
/// record's `meta` has a reserved key or its version is above `i64::MAX`.
pub struct DbCodec {
    meta: Attrs,
}

impl DbCodec {
    pub fn new(meta: &GraphMeta) -> Self {
        DbCodec { meta: meta.to_attrs() }
    }
}

impl Codec<DbRecord, DbRecord> for DbCodec {
    fn node_attr<S: Serializer>(&self, node: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&node.attr, s)
    }
    fn node_meta<S: Serializer>(&self, node: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        EntityMeta(node).serialize(s)
    }
    fn edge_attr<S: Serializer>(&self, edge: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&edge.attr, s)
    }
    fn edge_meta<S: Serializer>(&self, edge: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        EntityMeta(edge).serialize(s)
    }
    fn graph_meta<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&self.meta, s)
    }
}

/// A record's user meta with its version, as one map sorted by key.
struct EntityMeta<'a>(&'a DbRecord);

impl Serialize for EntityMeta<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let record = self.0;
        let version = i64::try_from(record.version).map_err(|_| {
            format::ser_error::<S::Error>(format_args!("version {} is too large to save", record.version))
        })?;
        let version = Value::Int(version);
        let mut entries: Vec<(&str, &Value)> =
            sorted_entries(&record.meta).into_iter().map(|(k, v)| (k.as_str(), v)).collect();
        if let Some((key, _)) = entries.iter().find(|(k, _)| is_reserved(k)) {
            return Err(format::ser_error(Error::ReservedName { key: (*key).to_owned() }));
        }
        let at = entries.partition_point(|(k, _)| *k < VERSION_KEY);
        entries.insert(at, (VERSION_KEY, &version));
        s.collect_map(entries)
    }
}

/// Make a node's payload while loading (for `LoadGraph::build` and
/// `LoadGraph::build_from_reader`).
pub fn node_record(node: &LoadNode<'_>) -> Result<DbRecord, Error> {
    record(node.attr(), node.meta(), || Entity::Node(node.id().to_owned()))
}

/// Make an edge's payload while loading (for `LoadGraph::build` and
/// `LoadGraph::build_from_reader`).
pub fn edge_record(edge: &LoadEdge<'_>) -> Result<DbRecord, Error> {
    record(edge.attr(), edge.meta(), || Entity::Edge(edge.id().to_owned()))
}

fn record(attr: &LoadAttrs<'_>, meta: &LoadAttrs<'_>, entity: impl Fn() -> Entity) -> Result<DbRecord, Error> {
    let mut version = None;
    let mut user = Attrs::with_capacity(meta.len().saturating_sub(1));
    for (key, value) in meta.iter() {
        if key == VERSION_KEY {
            let invalid = |found: String| Error::InvalidVersion { entity: entity(), found };
            if version.is_some() {
                return Err(invalid("the key appears twice".into()));
            }
            version = Some(match value.kind() {
                LoadKind::Int(v) => u64::try_from(v).map_err(|_| invalid(format!("{:?}", value.to_value())))?,
                _ => return Err(invalid(format!("{:?}", value.to_value()))),
            });
        } else if is_reserved(key) {
            return Err(Error::UnknownReservedKey { entity: entity(), key: key.to_owned() });
        } else {
            user.insert(key.to_owned(), value.to_value());
        }
    }
    let version = version.ok_or_else(|| Error::MissingVersion { entity: entity() })?;
    Ok(DbRecord { attr: attr.to_attrs(), meta: user, version })
}

/// A loaded database file.
#[derive(Debug)]
pub struct Loaded {
    /// The graph, its indexes matching the catalog and up to date.
    pub graph: DbGraph,
    pub meta: GraphMeta,
    /// How the indexes saved in the file differed from the catalog (empty
    /// for files written by [`to_binary`] / [`to_json`]).
    pub index_changes: IndexChanges,
}

/// Finish loading: apply the catalog's indexes.
fn finish(mut graph: DbGraph, meta: GraphMeta) -> Result<Loaded, Error> {
    let index_changes = meta.catalog.apply_indexes(&mut graph)?;
    Ok(Loaded { graph, meta, index_changes })
}

/// Load a parsed document (JSON or binary, from a slice).
pub fn load(doc: &LoadGraph<'_>) -> Result<Loaded, Error> {
    // The graph meta first: it is cheap and fails fast on foreign files
    let meta = GraphMeta::from_load(doc.meta())?;
    let graph = doc.build(node_record, edge_record)?;
    finish(graph, meta)
}

/// Load a JSON file's bytes.
pub fn from_json(bytes: &[u8]) -> Result<Loaded, Error> {
    load(&LoadGraph::from_json_slice(bytes)?)
}

/// Load a binary file's bytes (header and checksum checked first).
pub fn from_binary(bytes: &[u8]) -> Result<Loaded, Error> {
    load(&LoadGraph::from_binary_slice(bytes)?)
}

/// Load a binary file from `reader`, building the graph while decoding:
/// peak memory is the graph plus a small buffer. A checksum mismatch is
/// detected at the end, and the partly built graph is dropped.
pub fn from_binary_reader(reader: impl Read) -> Result<Loaded, Error> {
    let (graph, meta) = LoadGraph::build_from_reader(reader, node_record, edge_record)?;
    finish(graph, GraphMeta::from_load(&meta)?)
}

fn writer<'a>(graph: &'a DbGraph, codec: &'a DbCodec) -> GraphWriter<'a, DbRecord, DbRecord, DbCodec> {
    GraphWriter::new(graph, codec).with_timestamp(None)
}

/// Encode as JSON (deterministic, see the module comment).
pub fn to_json(graph: &DbGraph, meta: &GraphMeta, pretty: bool) -> Result<Vec<u8>, Error> {
    Ok(writer(graph, &DbCodec::new(meta)).to_json(pretty)?)
}

/// Write the binary format to `out` (deterministic, see the module
/// comment). Streams: nothing but the output is buffered.
pub fn write_binary(graph: &DbGraph, meta: &GraphMeta, out: impl Write) -> Result<(), Error> {
    Ok(writer(graph, &DbCodec::new(meta)).write_binary(out)?)
}

/// Encode in the binary format.
pub fn to_binary(graph: &DbGraph, meta: &GraphMeta) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    write_binary(graph, meta, &mut out)?;
    Ok(out)
}
