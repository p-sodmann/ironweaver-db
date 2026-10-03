//! Nodes and edges.

use ironweaver_core::EdgeId;
use iwdb_query::{Edge, Error, Node};

use super::{attrs_from_pb, attrs_to_pb};
use crate::proto as pb;

pub(crate) fn node_to_pb(node: &Node) -> Result<pb::Node, Error> {
    Ok(pb::Node {
        id: node.id.clone(),
        labels: node.labels.clone(),
        attr: attrs_to_pb(&node.attr)?,
        meta: attrs_to_pb(&node.meta)?,
        version: node.version,
    })
}

pub(crate) fn node_from_pb(node: pb::Node) -> Result<Node, Error> {
    Ok(Node {
        id: node.id,
        labels: node.labels,
        attr: attrs_from_pb(node.attr)?,
        meta: attrs_from_pb(node.meta)?,
        version: node.version,
    })
}

pub(crate) fn edge_to_pb(edge: &Edge) -> Result<pb::Edge, Error> {
    Ok(pb::Edge {
        id: edge.id.0,
        from: edge.from.clone(),
        to: edge.to.clone(),
        r#type: edge.ty.clone(),
        attr: attrs_to_pb(&edge.attr)?,
        meta: attrs_to_pb(&edge.meta)?,
        version: edge.version,
    })
}

pub(crate) fn edge_from_pb(edge: pb::Edge) -> Result<Edge, Error> {
    Ok(Edge {
        id: EdgeId(edge.id),
        from: edge.from,
        to: edge.to,
        ty: edge.r#type,
        attr: attrs_from_pb(edge.attr)?,
        meta: attrs_from_pb(edge.meta)?,
        version: edge.version,
    })
}

pub(crate) fn nodes_to_pb(nodes: &[Node]) -> Result<Vec<pb::Node>, Error> {
    nodes.iter().map(node_to_pb).collect()
}

pub(crate) fn nodes_from_pb(nodes: Vec<pb::Node>) -> Result<Vec<Node>, Error> {
    nodes.into_iter().map(node_from_pb).collect()
}

pub(crate) fn edges_to_pb(edges: &[Edge]) -> Result<Vec<pb::Edge>, Error> {
    edges.iter().map(edge_to_pb).collect()
}

pub(crate) fn edges_from_pb(edges: Vec<pb::Edge>) -> Result<Vec<Edge>, Error> {
    edges.into_iter().map(edge_from_pb).collect()
}

pub(crate) fn maybe_nodes_to_pb(nodes: &[Option<Node>]) -> Result<Vec<pb::MaybeNode>, Error> {
    nodes.iter().map(|n| Ok(pb::MaybeNode { node: n.as_ref().map(node_to_pb).transpose()? })).collect()
}

pub(crate) fn maybe_nodes_from_pb(nodes: Vec<pb::MaybeNode>) -> Result<Vec<Option<Node>>, Error> {
    nodes.into_iter().map(|n| n.node.map(node_from_pb).transpose()).collect()
}

pub(crate) fn maybe_edges_to_pb(edges: &[Option<Edge>]) -> Result<Vec<pb::MaybeEdge>, Error> {
    edges.iter().map(|e| Ok(pb::MaybeEdge { edge: e.as_ref().map(edge_to_pb).transpose()? })).collect()
}

pub(crate) fn maybe_edges_from_pb(edges: Vec<pb::MaybeEdge>) -> Result<Vec<Option<Edge>>, Error> {
    edges.into_iter().map(|e| e.edge.map(edge_from_pb).transpose()).collect()
}
