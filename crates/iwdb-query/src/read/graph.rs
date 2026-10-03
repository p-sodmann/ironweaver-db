//! Graph searches: neighbourhoods, traversals, subgraphs, shortest paths
//! and random walks.

use std::collections::HashSet;

use ironweaver_core::pathfinding::{self, EdgeCost, Heuristic, PathQuery};
use ironweaver_core::random_walks::{self, WalkOptions};
use ironweaver_core::{Direction, EdgeIx, GraphError, NodeIx, traversal};
use iwdb_engine::{DbGraph, DbRecord, Namespace};

use super::{EdgeFilter, Meter, ReadContext, TopK, existing, node_ix};
use crate::{
    Answer, Edge, Error, NeighbourhoodRequest, Node, Order, Path, PathMethod, PathRequest, Subgraph, SubgraphRequest,
    TraverseRequest, WalkRequest, Work,
};

/// The nodes within `depth` of `seeds` along the edges `filter` accepts
/// (the core's `expand_limited`), the work done and whether a limit
/// truncated it.
fn reach(
    g: &DbGraph,
    seeds: &[NodeIx],
    depth: usize,
    direction: Direction,
    filter: &EdgeFilter<'_>,
    cx: &ReadContext,
) -> Result<(Vec<NodeIx>, Work, bool), Error> {
    let edge_ok = |e: EdgeIx, _: &ironweaver_core::Edge<DbRecord>| filter.accepts(g, e);
    let limited = traversal::expand_limited(g, seeds.iter().copied(), depth, direction, cx.budget(false), edge_ok)?;
    let work = Work { visited: limited.visited, edges: limited.edges };
    Ok((limited.value, work, limited.truncated))
}

fn check_seeds(seeds: &[String], cx: &ReadContext) -> Result<(), Error> {
    if seeds.len() > cx.bounds.max_visited {
        return Err(Error::budget(format!(
            "{} nodes visited ({} seeds were given)",
            cx.bounds.max_visited,
            seeds.len()
        )));
    }
    Ok(())
}

/// Every node within `depth` edges of the seeds (seeds included) that
/// passes the node filter, sorted by id, a page of `max_results` at a time.
///
/// Bounded by `max_visited` (nodes expanded) and `max_edges` (edges
/// examined) of the search, which every page runs again, at the seq of the
/// first. Seeds that don't exist are skipped.
pub fn neighbourhood(
    ns: &Namespace,
    request: &NeighbourhoodRequest,
    cx: &ReadContext,
) -> Result<Answer<Vec<Node>>, Error> {
    let fingerprint = request.fingerprint();
    let after = cx.resume(ns, fingerprint)?.and_then(|keys| keys.into_iter().next());
    check_seeds(&request.seeds, cx)?;
    let g = ns.graph();
    let filter = EdgeFilter::new(g, &request.edge_types, request.edge_filter.as_ref());
    let (reached, work, truncated) =
        reach(g, &existing(g, &request.seeds), request.depth, request.direction, &filter, cx)?;
    let mut top = TopK::new(cx.bounds.max_results);
    for ix in reached {
        let Some(node) = g.node(ix) else { continue };
        if after.as_deref().is_some_and(|a| node.id() <= a) {
            continue;
        }
        if let Some(f) = &request.node_filter {
            if !f.matches_node(g, ix)? {
                continue;
            }
        }
        top.push(node.id().to_owned(), ix);
    }
    let (page, more) = top.finish();
    let next = match page.last() {
        Some((last, _)) if more && !truncated => Some(cx.next(ns, fingerprint, vec![last.clone()])),
        _ => None,
    };
    let value = page.into_iter().filter_map(|(_, ix)| Node::read(g, ix)).collect();
    Ok(Answer { value, seq: ns.seq(), next, truncated, work })
}

/// The ids of the nodes a breadth- or depth-first traversal from `start`
/// reaches along edges in `direction`, in traversal order (the core's
/// `bfs_limited` / `dfs_limited`; with `Both`, a node's outgoing edges come
/// before its incoming ones).
///
/// Bounded by `max_visited`, `max_edges` and `max_results` (no cursor: the
/// order isn't a sort key). Among the nodes reached through the edges of
/// one node, the order follows the core's edge order, which isn't part of
/// the contract (design rule 7): it may differ after a restart, and with a
/// depth limit or a truncation, so may the set a DFS reaches.
pub fn traverse(ns: &Namespace, request: &TraverseRequest, cx: &ReadContext) -> Result<Answer<Vec<String>>, Error> {
    let g = ns.graph();
    let start = node_ix(g, &request.start)?;
    let filter = EdgeFilter::new(g, &request.edge_types, request.edge_filter.as_ref());
    let edge_ok = |e: EdgeIx, _: &ironweaver_core::Edge<DbRecord>| filter.accepts(g, e);
    let budget = cx.budget(true);
    let limited = match request.order {
        Order::Bfs => traversal::bfs_limited(g, start, request.depth, request.direction, budget, edge_ok)?,
        Order::Dfs => traversal::dfs_limited(g, start, request.depth, request.direction, budget, edge_ok)?,
    };
    let value = limited.value.iter().filter_map(|&ix| g.node(ix).map(|n| n.id().to_owned())).collect();
    let work = Work { visited: limited.visited, edges: limited.edges };
    Ok(Answer { value, seq: ns.seq(), next: None, truncated: limited.truncated, work })
}

/// The nodes within `depth` edges of the seeds and the edges between them
/// that pass the edge filter (an induced subgraph), sorted by id.
///
/// Bounded by `max_visited` and `max_edges` (the search, then every
/// outgoing edge of the nodes reached, counted together) and by
/// `max_results` (nodes and edges together; a partial answer keeps the
/// first nodes by id and the edges between them).
pub fn subgraph(ns: &Namespace, request: &SubgraphRequest, cx: &ReadContext) -> Result<Answer<Subgraph>, Error> {
    check_seeds(&request.seeds, cx)?;
    let g = ns.graph();
    let filter = EdgeFilter::new(g, &request.edge_types, request.edge_filter.as_ref());
    let (reached, mut work, mut truncated) =
        reach(g, &existing(g, &request.seeds), request.depth, request.direction, &filter, cx)?;
    let mut nodes: Vec<(String, NodeIx)> =
        reached.iter().filter_map(|&ix| g.node(ix).map(|n| (n.id().to_owned(), ix))).collect();
    nodes.sort_unstable();
    let set: HashSet<NodeIx> = reached.into_iter().collect();
    let mut bounds = cx.bounds;
    bounds.max_edges = bounds.max_edges.saturating_sub(work.edges);
    let mut meter = Meter::new(&bounds);
    let mut edges: Vec<EdgeIx> = Vec::new();
    let stop = ironweaver_core::cancel::stop();
    'scan: for (_, ix) in &nodes {
        let Some(node) = g.node(*ix) else { continue };
        for &e in node.out_edges() {
            if stop.poll() || !meter.examine() {
                break 'scan;
            }
            if g.edge(e).is_some_and(|edge| set.contains(&edge.target())) && filter.accepts(g, e)? {
                edges.push(e);
            }
        }
    }
    work.edges += meter.edges;
    if meter.hit.is_some() {
        cx.reached(format!("{} edges examined", cx.bounds.max_edges))?;
        truncated = true;
    }
    let max = cx.bounds.max_results;
    if nodes.len() + edges.len() > max {
        cx.reached(format!("{} results (nodes and edges)", max))?;
        truncated = true;
        nodes.truncate(max);
    }
    let kept: HashSet<NodeIx> = nodes.iter().map(|(_, ix)| *ix).collect();
    let mut edges: Vec<Edge> = edges
        .into_iter()
        .filter(|&e| g.edge(e).is_some_and(|edge| kept.contains(&edge.source()) && kept.contains(&edge.target())))
        .filter_map(|e| Edge::read(g, e))
        .collect();
    edges.sort_unstable_by_key(|e| e.id);
    edges.truncate(max - nodes.len());
    let nodes = nodes.into_iter().filter_map(|(_, ix)| Node::read(g, ix)).collect();
    Ok(Answer { value: Subgraph { nodes, edges }, seq: ns.seq(), next: None, truncated, work })
}

/// A shortest path from `from` to `to`, or `None` if there is none within
/// the request's limits (`max_depth`, `max_cost`).
///
/// Bounded by `max_visited` and `max_edges` (the core's
/// `bidirectional_bfs_limited` for BFS, `find_path_limited` for Dijkstra
/// and A*: nodes settled or frontier nodes expanded, edges relaxed or
/// listed), with cancellation checked per edge. With `partial`, a search a
/// limit stopped answers `None` with `truncated`.
pub fn shortest_path(ns: &Namespace, request: &PathRequest, cx: &ReadContext) -> Result<Answer<Option<Path>>, Error> {
    let g = ns.graph();
    let from = node_ix(g, &request.from)?;
    let to = node_ix(g, &request.to)?;
    pathfinding::check_max_cost(request.max_cost)?;
    let budget = cx.budget(false);
    let (found, work, truncated) = match &request.method {
        PathMethod::Bfs => {
            if request.cost != EdgeCost::Unit {
                return Err(Error::invalid("a BFS path counts edges; use Dijkstra or A* for a weighted cost"));
            }
            // As the core's BFS method: max_cost counts edges
            let by_cost = request.max_cost.map(|c| c.floor() as usize);
            let max_depth = match (request.max_depth, by_cost) {
                (Some(d), Some(c)) => Some(d.min(c)),
                (d, c) => d.or(c),
            };
            let all = |_: EdgeIx, _: &ironweaver_core::Edge<DbRecord>| Ok::<_, GraphError>(true);
            let limited = traversal::bidirectional_bfs_limited(g, from, to, max_depth, request.direction, budget, all)?;
            let found = limited.value.map(|nodes| {
                let cost = nodes.len().saturating_sub(1) as f64;
                (nodes, cost)
            });
            (found, Work { visited: limited.visited, edges: limited.edges }, limited.truncated)
        }
        PathMethod::Dijkstra | PathMethod::AStar { .. } => {
            if request.max_depth.is_some() {
                return Err(Error::invalid("max_depth limits BFS paths; use max_cost for Dijkstra and A*"));
            }
            let mut query: PathQuery<'_, DbRecord, GraphError> = match &request.method {
                PathMethod::AStar { coords, metric } => {
                    PathQuery::astar(Heuristic::coords(g, to, *metric, coords.clone())?)
                }
                _ => PathQuery::dijkstra(),
            };
            query.cost = request.cost.clone();
            query.direction = request.direction;
            query.max_cost = request.max_cost;
            let limited = pathfinding::find_path_limited(g, from, to, &mut query, budget)?;
            let found = limited.value.map(|r| (r.nodes, r.cost));
            (found, Work { visited: limited.visited, edges: limited.edges }, limited.truncated)
        }
    };
    let value = found.map(|(nodes, cost)| Path {
        nodes: nodes.iter().filter_map(|&ix| g.node(ix).map(|n| n.id().to_owned())).collect(),
        cost,
    });
    Ok(Answer { value, seq: ns.seq(), next: None, truncated, work })
}

/// Random walks from `start`, as lists of node ids.
///
/// The core's `plan_limited` first indexes the nodes and edges the walks
/// can reach (within `max_length - 1` steps of the start); then
/// `WalkPlan::run_limited` walks with what is left of the budget:
/// `max_results` walks, `max_visited` nodes walked through, `max_edges`
/// steps. The work reports both. A plan is all or nothing: with `partial`,
/// a limit reached while indexing answers no walks with `truncated`. Walks
/// with a seed repeat for the same state in the same process; the edge
/// order they depend on isn't part of the contract.
pub fn random_walks(
    ns: &Namespace,
    request: &WalkRequest,
    cx: &ReadContext,
) -> Result<Answer<Vec<Vec<String>>>, Error> {
    let g = ns.graph();
    node_ix(g, &request.start)?;
    let mut options = WalkOptions::new(request.max_length, request.walks);
    options.min_length = request.min_length;
    options.allow_revisit = request.allow_revisit;
    options.seed = request.seed;
    let planned = random_walks::plan_limited::<_, _, GraphError>(g, Some(&request.start), options, cx.budget(false))?;
    let mut work = Work { visited: planned.visited, edges: planned.edges };
    let Some(plan) = planned.value else {
        return Ok(Answer { value: Vec::new(), seq: ns.seq(), next: None, truncated: true, work });
    };
    let mut budget = cx.budget(true);
    budget.max_visited = budget.max_visited.map(|m| m.saturating_sub(planned.visited));
    budget.max_edges = budget.max_edges.map(|m| m.saturating_sub(planned.edges));
    let limited = plan.run_limited(budget)?;
    let value = limited.value.iter().map(|w| plan.items(w).map(str::to_owned).collect()).collect();
    work.visited += limited.visited;
    work.edges += limited.edges;
    Ok(Answer { value, seq: ns.seq(), next: None, truncated: limited.truncated, work })
}
