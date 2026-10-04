//! Plain graph files: the core's file format (version 2, JSON or binary)
//! as the Ironweaver library and other tools write it, without the
//! database's reserved keys (step 13, ADR 0033).
//!
//! - Import reads one into a graph ([`from_json`], [`from_binary_reader`])
//!   and makes it a namespace's first state ([`imported`]): every node and
//!   edge at version 1, the file's indexes as the catalog's, at seq
//!   [`IMPORT_SEQ`].
//! - Export writes a namespace's graph as one ([`to_json`],
//!   [`write_binary`]): attributes and user meta, sorted by key, no
//!   versions and no graph meta.
//!
//! A plain file has no `iwdb.*` keys: a reserved key in an attribute or
//! meta map is refused, so a database checkpoint doesn't load as a plain
//! file (nor a plain file as a checkpoint), and so is one in the graph
//! meta. Other graph meta has no place in a namespace; loading drops it
//! and returns its keys.

use std::io::{Read, Write};

use ironweaver_core::format::{Codec, GraphWriter, LoadAttrs, LoadEdge, LoadGraph, LoadNode};
use ironweaver_core::value::serialize_sorted;
use ironweaver_core::{Attrs, GraphError};
use serde::Serializer;

use crate::catalog::{AttrPath, IndexDef, NamespaceCatalog, NamespaceName};
use crate::codec::{GraphMeta, Loaded};
use crate::idempotency::KeyTable;
use crate::mark::MarkTable;
use crate::reserved::is_reserved;
use crate::{DbGraph, DbRecord, Entity, Error, Namespace};

/// The seq of an imported namespace's first state: its WAL starts at
/// `IMPORT_SEQ + 1`. Not 0, so that a change stream asking for seq 1 learns
/// that the import isn't in the WAL (`not_retained`) instead of missing it.
pub const IMPORT_SEQ: u64 = 1;

/// A plain file, loaded.
#[derive(Debug)]
pub struct Plain {
    /// The graph: every record at version 1, the file's index definitions
    /// recreated (not flushed; [`imported`] applies them).
    pub graph: DbGraph,
    /// The keys of the file's graph meta, which were dropped, sorted.
    pub dropped_meta: Vec<String>,
}

/// A node's payload from a plain file: its attributes and meta, version 1.
pub fn node_record(node: &LoadNode<'_>) -> Result<DbRecord, Error> {
    record(node.attr(), node.meta(), || Entity::Node(node.id().to_owned()))
}

/// An edge's payload from a plain file.
pub fn edge_record(edge: &LoadEdge<'_>) -> Result<DbRecord, Error> {
    record(edge.attr(), edge.meta(), || Entity::Edge(edge.id().to_owned()))
}

fn record(attr: &LoadAttrs<'_>, meta: &LoadAttrs<'_>, entity: impl Fn() -> Entity) -> Result<DbRecord, Error> {
    for map in [attr, meta] {
        if let Some((key, _)) = map.iter().find(|(k, _)| is_reserved(k)) {
            return Err(Error::UnknownReservedKey { entity: entity(), key: key.to_owned() });
        }
    }
    Ok(DbRecord { attr: attr.to_attrs(), meta: meta.to_attrs(), version: 1 })
}

fn plain(graph: DbGraph, meta: &LoadAttrs<'_>) -> Result<Plain, Error> {
    if let Some((key, _)) = meta.iter().find(|(k, _)| is_reserved(k)) {
        return Err(Error::UnknownReservedKey { entity: Entity::Graph, key: key.to_owned() });
    }
    let mut dropped_meta: Vec<String> = meta.iter().map(|(k, _)| k.to_owned()).collect();
    dropped_meta.sort_unstable();
    Ok(Plain { graph, dropped_meta })
}

/// Load a plain JSON file's bytes (version 1 files are migrated by the
/// core).
pub fn from_json(bytes: &[u8]) -> Result<Plain, Error> {
    let doc = LoadGraph::from_json_slice(bytes)?;
    let graph = doc.build(node_record, edge_record)?;
    plain(graph, doc.meta())
}

/// Load a plain binary file from `reader`, building the graph while
/// decoding: peak memory is the graph plus a small buffer.
pub fn from_binary_reader(reader: impl Read) -> Result<Plain, Error> {
    let (graph, meta) = LoadGraph::build_from_reader(reader, node_record, edge_record)?;
    plain(graph, &meta)
}

/// The namespace `name` with `graph` as its state at [`IMPORT_SEQ`]: the
/// graph's index definitions become the catalog's indexes (and are
/// built), the catalog has no constraints, there are no idempotency keys
/// and no marks. The caller should check the result with
/// [`invariants::check`](crate::invariants::check) before trusting it.
///
/// Errors: an index path that isn't a valid attribute path, or `labels`
/// ([`Error::UnindexablePath`]); the core failing to build an index.
pub fn imported(name: NamespaceName, mut graph: DbGraph) -> Result<Namespace, Error> {
    let mut catalog = NamespaceCatalog::new();
    let paths: Vec<Vec<String>> = graph.index_paths().into_iter().map(<[String]>::to_vec).collect();
    for keys in paths {
        let path = AttrPath::new(keys)?;
        if path.keys() == ["labels"] {
            return Err(Error::UnindexablePath { path });
        }
        catalog.add_index(IndexDef { path });
    }
    let meta = GraphMeta { namespace: name, catalog, seq: IMPORT_SEQ, keys: KeyTable::new(), marks: MarkTable::new() };
    let index_changes = meta.catalog.apply_indexes(&mut graph)?;
    Ok(Namespace::from_loaded(Loaded { graph, meta, index_changes }))
}

/// [`Codec`] for exports: attributes and user meta sorted by key, no
/// versions, no graph meta.
struct PlainCodec;

impl Codec<DbRecord, DbRecord> for PlainCodec {
    fn node_attr<S: Serializer>(&self, node: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&node.attr, s)
    }
    fn node_meta<S: Serializer>(&self, node: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&node.meta, s)
    }
    fn edge_attr<S: Serializer>(&self, edge: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&edge.attr, s)
    }
    fn edge_meta<S: Serializer>(&self, edge: &DbRecord, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&edge.meta, s)
    }
    fn graph_meta<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_sorted(&Attrs::new(), s)
    }
}

fn writer(graph: &DbGraph) -> GraphWriter<'_, DbRecord, DbRecord, PlainCodec> {
    GraphWriter::new(graph, &PlainCodec).with_timestamp(None)
}

/// `graph` as a plain JSON file (deterministic: no timestamp, maps sorted).
pub fn to_json(graph: &DbGraph, pretty: bool) -> Result<Vec<u8>, GraphError> {
    writer(graph).to_json(pretty)
}

/// Write `graph` as a plain binary file to `out`, streaming.
pub fn write_binary(graph: &DbGraph, out: impl Write) -> Result<(), GraphError> {
    writer(graph).write_binary(out)
}
