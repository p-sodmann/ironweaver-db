//! The bounded read operations, as functions over a [`Namespace`]
//! (ADR 0021).
//!
//! The caller holds the namespace's read lock and runs the function under
//! a cancel token with a deadline (`ironweaver_core::cancel::run`); the
//! embedded store does both (`iwdb::Embedded`). Each function bounds its
//! own work by the [`ReadContext`]: the core's `Budget` where the core
//! takes one, our own counters where it doesn't (marked as workarounds,
//! with the upstream issue that removes them).
//!
//! Results whose order would otherwise follow the core's iteration order
//! are sorted by id (design rule 7), and paginated reads continue from the
//! sort key of their last result (a keyset cursor).

mod analytics;
mod explain;
mod graph;
mod lookup;
mod matching;

use std::collections::BinaryHeap;

use ironweaver_core::{Budget, EdgeIx, Expr, NodeIx, OnLimit, Symbol};
use iwdb_engine::{DbGraph, Namespace};
use iwdb_storage::HistoryId;

use crate::cursor::State;
use crate::{Bounds, Code, Cursor, Error, QueryOptions};

pub use analytics::run_job;
pub use explain::{Explain, Lookup, Plan, explain};
pub use graph::{neighbourhood, random_walks, shortest_path, subgraph, traverse};
pub use lookup::{find, get_edges, get_nodes};
pub use matching::match_pattern;

/// What a read may do and where it continues: the resolved limits, the
/// options, and the namespace's identity for cursors.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadContext {
    pub bounds: Bounds,
    pub partial: bool,
    pub cursor: Option<Cursor>,
    pub namespace_id: u64,
    pub history: HistoryId,
}

impl ReadContext {
    pub fn new(bounds: Bounds, options: &QueryOptions, namespace_id: u64, history: HistoryId) -> Self {
        ReadContext { bounds, partial: options.partial, cursor: options.cursor.clone(), namespace_id, history }
    }

    /// The core's budget for these limits, with `max_results` if `results`.
    pub(crate) fn budget(&self, results: bool) -> Budget {
        Budget {
            max_visited: Some(self.bounds.max_visited),
            max_edges: Some(self.bounds.max_edges),
            max_results: results.then_some(self.bounds.max_results),
            on_limit: if self.partial { OnLimit::Truncate } else { OnLimit::Error },
        }
    }

    /// The position of the cursor, if the read continues one: checked
    /// against the namespace, its state and the request.
    pub(crate) fn resume(&self, ns: &Namespace, request: u64) -> Result<Option<Vec<String>>, Error> {
        let Some(cursor) = &self.cursor else { return Ok(None) };
        let state = State::decode(cursor)?;
        if state.namespace_id != self.namespace_id || state.history != self.history {
            return Err(Error::invalid("the cursor belongs to another namespace"));
        }
        if state.request != request {
            return Err(Error::invalid("the cursor belongs to another request"));
        }
        if state.seq != ns.seq() {
            return Err(Error::new(
                Code::CursorExpired,
                format!(
                    "the cursor was made at seq {}, but the namespace is at seq {} now; start again without it",
                    state.seq,
                    ns.seq()
                ),
            ));
        }
        Ok(Some(state.after))
    }

    /// A cursor continuing after `after`.
    pub(crate) fn next(&self, ns: &Namespace, request: u64, after: Vec<String>) -> Cursor {
        State { namespace_id: self.namespace_id, history: self.history, seq: ns.seq(), request, after }.encode()
    }

    /// A limit was reached: fine with `partial`, an error without.
    pub(crate) fn reached(&self, what: impl std::fmt::Display) -> Result<(), Error> {
        if self.partial { Ok(()) } else { Err(Error::budget(what)) }
    }
}

/// Our own count of nodes visited and edges examined, for searches the
/// core can't bound itself.
pub(crate) struct Meter {
    max_visited: usize,
    max_edges: usize,
    pub visited: usize,
    pub edges: usize,
    /// What stopped the search, if a limit did.
    pub hit: Option<String>,
}

impl Meter {
    pub fn new(bounds: &Bounds) -> Self {
        Meter { max_visited: bounds.max_visited, max_edges: bounds.max_edges, visited: 0, edges: 0, hit: None }
    }

    /// Count a node visited; false (stop) if over the limit.
    pub fn enter(&mut self) -> bool {
        if self.visited >= self.max_visited {
            self.hit = Some(format!("{} nodes visited", self.max_visited));
            return false;
        }
        self.visited += 1;
        true
    }

    /// Count an edge examined; false (stop) if over the limit.
    pub fn examine(&mut self) -> bool {
        if self.edges >= self.max_edges {
            self.hit = Some(format!("{} edges examined", self.max_edges));
            return false;
        }
        self.edges += 1;
        true
    }

    pub fn work(&self) -> crate::Work {
        crate::Work { visited: self.visited, edges: self.edges }
    }
}

/// Which edges a search follows: by type and by filter.
pub(crate) struct EdgeFilter<'a> {
    /// `None`: any type. Names the graph doesn't know match nothing.
    types: Option<Vec<Symbol>>,
    expr: Option<&'a Expr>,
}

impl<'a> EdgeFilter<'a> {
    pub fn new(g: &DbGraph, types: &[String], expr: Option<&'a Expr>) -> Self {
        let types = (!types.is_empty()).then(|| types.iter().filter_map(|t| g.symbol(t)).collect());
        EdgeFilter { types, expr }
    }

    pub fn accepts(&self, g: &DbGraph, e: EdgeIx) -> Result<bool, Error> {
        if let Some(types) = &self.types {
            let ty = g.edge(e).and_then(|edge| edge.edge_type());
            if !ty.is_some_and(|t| types.contains(&t)) {
                return Ok(false);
            }
        }
        match self.expr {
            Some(expr) => Ok(expr.matches_edge(g, e)?),
            None => Ok(true),
        }
    }
}

/// The first `limit` items by key, keeping at most `limit + 1` in memory.
pub(crate) struct TopK<K: Ord, T: Ord> {
    heap: BinaryHeap<(K, T)>,
    limit: usize,
}

impl<K: Ord, T: Ord> TopK<K, T> {
    pub fn new(limit: usize) -> Self {
        TopK { heap: BinaryHeap::new(), limit }
    }

    pub fn push(&mut self, key: K, item: T) {
        self.heap.push((key, item));
        if self.heap.len() > self.limit.saturating_add(1) {
            self.heap.pop();
        }
    }

    /// The items in key order, and whether there were more than `limit`.
    pub fn finish(self) -> (Vec<(K, T)>, bool) {
        let mut items = self.heap.into_sorted_vec();
        let more = items.len() > self.limit;
        items.truncate(self.limit);
        (items, more)
    }
}

/// The handles of the node ids that exist.
pub(crate) fn existing(g: &DbGraph, ids: &[String]) -> Vec<NodeIx> {
    ids.iter().filter_map(|id| g.node_ix(id)).collect()
}

/// The handle of node `id`, or `not_found`.
pub(crate) fn node_ix(g: &DbGraph, id: &str) -> Result<NodeIx, Error> {
    g.node_ix(id).ok_or_else(|| Error::not_found(format!("node '{}' not found", id)))
}
