//! Reads by id and by filter.

use ironweaver_core::EdgeId;
use iwdb_engine::Namespace;

use super::{Meter, ReadContext, TopK};
use crate::{Answer, Edge, Error, FindRequest, Node, Work};

/// The nodes `ids`, in the order asked (`None` for those that don't
/// exist). Bounded: at most `max_results` ids.
pub fn get_nodes(ns: &Namespace, ids: &[String], cx: &ReadContext) -> Result<Answer<Vec<Option<Node>>>, Error> {
    if ids.len() > cx.bounds.max_results {
        return Err(Error::budget(format!("{} results ({} ids were asked for)", cx.bounds.max_results, ids.len())));
    }
    let g = ns.graph();
    let value = ids.iter().map(|id| g.node_ix(id).and_then(|ix| Node::read(g, ix))).collect();
    Ok(Answer { work: Work { visited: ids.len(), edges: 0 }, ..Answer::at(ns.seq(), value) })
}

/// The edges `ids`, in the order asked (`None` for those that don't
/// exist). Bounded: at most `max_results` ids.
pub fn get_edges(ns: &Namespace, ids: &[EdgeId], cx: &ReadContext) -> Result<Answer<Vec<Option<Edge>>>, Error> {
    if ids.len() > cx.bounds.max_results {
        return Err(Error::budget(format!("{} results ({} ids were asked for)", cx.bounds.max_results, ids.len())));
    }
    let g = ns.graph();
    let value = ids.iter().map(|&id| g.edge_ix(id).and_then(|ix| Edge::read(g, ix))).collect();
    Ok(Answer { work: Work { visited: 0, edges: ids.len() }, ..Answer::at(ns.seq(), value) })
}

/// The nodes matching `request.filter`, sorted by id, a page of
/// `max_results` at a time.
///
/// The candidates come from the core's `index_candidates` (property and
/// label indexes) or, if no index narrows the filter down, from every
/// node; each candidate checked counts as visited, so a filter no index
/// serves needs `max_visited` of at least the number of nodes. Every page
/// checks every candidate again (O(candidates)), at the seq of the first.
pub fn find(ns: &Namespace, request: &FindRequest, cx: &ReadContext) -> Result<Answer<Vec<Node>>, Error> {
    let fingerprint = request.fingerprint();
    let after = cx.resume(ns, fingerprint)?.and_then(|keys| keys.into_iter().next());
    let g = ns.graph();
    let candidates: Box<dyn Iterator<Item = _>> = match g.index_candidates(&request.filter)? {
        Some(found) => Box::new(found.into_iter()),
        None => Box::new(g.node_indices()),
    };
    let mut meter = Meter::new(&cx.bounds);
    let mut top = TopK::new(cx.bounds.max_results);
    let stop = ironweaver_core::cancel::stop();
    for ix in candidates {
        if stop.poll() || !meter.enter() {
            break;
        }
        let Some(node) = g.node(ix) else { continue };
        if after.as_deref().is_some_and(|a| node.id() <= a) || !request.filter.matches_node(g, ix)? {
            continue;
        }
        top.push(node.id().to_owned(), ix);
    }
    if let Some(what) = &meter.hit {
        cx.reached(what)?;
    }
    let (page, more) = top.finish();
    let truncated = meter.hit.is_some();
    let next = match page.last() {
        Some((last, _)) if more && !truncated => Some(cx.next(ns, fingerprint, vec![last.clone()])),
        _ => None,
    };
    let value = page.into_iter().filter_map(|(_, ix)| Node::read(g, ix)).collect();
    Ok(Answer { value, seq: ns.seq(), next, truncated, work: meter.work() })
}
