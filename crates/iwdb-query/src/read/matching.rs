//! `match`: every occurrence of a pattern.

use ironweaver_core::EdgeId;
use ironweaver_core::query::{Bound, Match, for_each_match_limited};
use iwdb_engine::{DbGraph, Namespace};

use super::{ReadContext, TopK};
use crate::{Answer, Error, MatchRequest, MatchRow, Work};

/// Every match of the pattern (with the request's `where` filters), as
/// rows sorted by node ids then edge ids, a page of `max_results` at a
/// time.
///
/// Bounded by `max_visited` and `max_edges` of the search (the core's
/// `for_each_match_limited`: every node checked against a node variable
/// and every step of a variable-length edge counts as visited, every edge
/// looked at from a bound node as examined), with cancellation checked per
/// step. Every page enumerates the matches again, at the seq of the first.
pub fn match_pattern(ns: &Namespace, request: &MatchRequest, cx: &ReadContext) -> Result<Answer<Vec<MatchRow>>, Error> {
    let fingerprint = request.fingerprint();
    let mut pattern = request.pattern.clone();
    for (name, filter) in &request.filters {
        pattern.add_filter(name, filter.clone())?;
    }
    let after = match cx.resume(ns, fingerprint)? {
        Some(keys) => Some(decode(keys, pattern.nodes.len(), pattern.edges.len())?),
        None => None,
    };
    let g = ns.graph();
    let mut top = TopK::new(cx.bounds.max_results);
    let limited = for_each_match_limited::<_, _, Error>(g, &pattern, cx.budget(false), |m| {
        let row = row(g, m);
        if after.as_ref().is_none_or(|a| &row > a) {
            top.push(row, ());
        }
        Ok(true)
    })?;
    let truncated = limited.truncated;
    let (page, more) = top.finish();
    let next = match page.last() {
        Some((last, ())) if more && !truncated => Some(cx.next(ns, fingerprint, encode(last))),
        _ => None,
    };
    let value = page.into_iter().map(|(row, ())| row).collect();
    let work = Work { visited: limited.visited, edges: limited.edges };
    Ok(Answer { value, seq: ns.seq(), next, truncated, work })
}

fn row(g: &DbGraph, m: &Match) -> MatchRow {
    let edge_id = |e| g.edge(e).map_or(EdgeId(u64::MAX), |edge| edge.id());
    MatchRow {
        nodes: m.nodes.iter().map(|&ix| g.node(ix).map_or_else(String::new, |n| n.id().to_owned())).collect(),
        edges: m
            .edges
            .iter()
            .map(|b| match b {
                Bound::Edge(e) => vec![edge_id(*e)],
                Bound::Path(path) => path.iter().map(|&e| edge_id(e)).collect(),
            })
            .collect(),
    }
}

/// A row as cursor keys: the node ids, then each edge binding as its ids
/// joined by commas.
fn encode(row: &MatchRow) -> Vec<String> {
    let edges = row.edges.iter().map(|ids| ids.iter().map(|e| e.0.to_string()).collect::<Vec<_>>().join(","));
    row.nodes.iter().cloned().chain(edges).collect()
}

fn decode(keys: Vec<String>, nodes: usize, edges: usize) -> Result<MatchRow, Error> {
    let invalid = || Error::invalid("the cursor doesn't fit the pattern");
    if keys.len() != nodes + edges {
        return Err(invalid());
    }
    let mut keys = keys.into_iter();
    let node_ids: Vec<String> = keys.by_ref().take(nodes).collect();
    let edge_ids = keys
        .map(|k| {
            if k.is_empty() {
                return Ok(Vec::new());
            }
            k.split(',').map(|n| n.parse().map(EdgeId).map_err(|_| invalid())).collect()
        })
        .collect::<Result<_, Error>>()?;
    Ok(MatchRow { nodes: node_ids, edges: edge_ids })
}
