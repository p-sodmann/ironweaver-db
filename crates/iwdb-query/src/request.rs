//! The requests of the read operations, and what some of them return.
//! Every request is plain data: filters are `Expr`s and patterns are
//! `Pattern`s, never closures (design rule 5).

use ironweaver_core::algo::{Leiden, PageRank};
use ironweaver_core::pathfinding::{Coords, EdgeCost, Metric};
use ironweaver_core::query::Pattern;
use ironweaver_core::{Direction, EdgeId, Expr};

use crate::cursor::Fingerprint;
use crate::model::{Edge, Node};

/// A batch of the change stream ([`Database::changes`](crate::Database::changes),
/// ADR 0031).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangesRequest {
    /// The first seq to return (0 is read as 1). To resume, pass the
    /// `next_seq` of the last batch, or the seq of the last commit
    /// processed plus one.
    pub from_seq: u64,
    /// If no commit from `from_seq` on is streamable yet, wait for one
    /// (a long poll) instead of answering with an empty batch at once.
    pub wait: bool,
}

/// About how many bytes of WAL payload a batch of the change stream holds
/// at most (always at least one commit, which can be up to 64 MiB).
pub const CHANGES_BATCH_BYTES: usize = 4 << 20;

/// Nodes matching a filter ([`Database::find`](crate::Database::find)).
#[derive(Clone, Debug, PartialEq)]
pub struct FindRequest {
    pub filter: Expr,
}

impl FindRequest {
    pub(crate) fn fingerprint(&self) -> u64 {
        Fingerprint::new().str("find").json(&self.filter).finish()
    }
}

/// How [`find`](crate::Database::find) would read a filter
/// ([`Database::explain`](crate::Database::explain)).
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainRequest {
    pub filter: Expr,
    /// Also run the index lookup and report the exact number of candidates
    /// (like Postgres' `EXPLAIN ANALYZE`). Costs O(candidates).
    pub analyze: bool,
}

/// Every node within `depth` edges of the seeds
/// ([`Database::neighbourhood`](crate::Database::neighbourhood)).
#[derive(Clone, Debug, PartialEq)]
pub struct NeighbourhoodRequest {
    /// Where to start; ids that don't exist are skipped.
    pub seeds: Vec<String>,
    /// How many edges away a node may be (0: the seeds only).
    pub depth: usize,
    pub direction: Direction,
    /// Follow only edges with one of these types (all if empty).
    pub edge_types: Vec<String>,
    /// Follow only edges matching this filter.
    pub edge_filter: Option<Expr>,
    /// Return only nodes matching this filter. It doesn't prune the search:
    /// nodes it rejects are still expanded.
    pub node_filter: Option<Expr>,
}

impl NeighbourhoodRequest {
    /// Outgoing edges of any type, no filters.
    pub fn new(seeds: impl IntoIterator<Item = impl Into<String>>, depth: usize) -> Self {
        NeighbourhoodRequest {
            seeds: seeds.into_iter().map(Into::into).collect(),
            depth,
            direction: Direction::Out,
            edge_types: Vec::new(),
            edge_filter: None,
            node_filter: None,
        }
    }

    pub(crate) fn fingerprint(&self) -> u64 {
        let mut f = Fingerprint::new();
        f.str("neighbourhood").u64(self.seeds.len() as u64);
        for seed in &self.seeds {
            f.str(seed);
        }
        f.u64(self.depth as u64).u64(direction_code(self.direction)).u64(self.edge_types.len() as u64);
        for ty in &self.edge_types {
            f.str(ty);
        }
        f.json(&self.edge_filter).json(&self.node_filter).finish()
    }
}

pub(crate) fn direction_code(direction: Direction) -> u64 {
    match direction {
        Direction::Out => 0,
        Direction::In => 1,
        Direction::Both => 2,
    }
}

/// The order of a [`TraverseRequest`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// Breadth first: nodes by distance from the start.
    #[default]
    Bfs,
    /// Depth first, pre-order.
    Dfs,
}

/// A breadth- or depth-first traversal
/// ([`Database::traverse`](crate::Database::traverse)).
#[derive(Clone, Debug, PartialEq)]
pub struct TraverseRequest {
    pub start: String,
    pub order: Order,
    /// How many edges away from the start a node may be (`None`: any).
    pub depth: Option<usize>,
    /// Which edges to follow (default `Out`).
    pub direction: Direction,
    pub edge_types: Vec<String>,
    pub edge_filter: Option<Expr>,
}

impl TraverseRequest {
    pub fn new(start: impl Into<String>, order: Order) -> Self {
        TraverseRequest {
            start: start.into(),
            order,
            depth: None,
            direction: Direction::Out,
            edge_types: Vec::new(),
            edge_filter: None,
        }
    }
}

/// The algorithm of a [`PathRequest`].
#[derive(Clone, Debug, PartialEq)]
pub enum PathMethod {
    /// Fewest edges (bidirectional breadth-first search). `cost` must be
    /// `EdgeCost::Unit`.
    Bfs,
    /// Cheapest path by `cost`.
    Dijkstra,
    /// Cheapest path by `cost`, guided by the distance between node
    /// coordinates (which must not overestimate the cost).
    AStar { coords: Coords, metric: Metric },
}

/// A shortest path between two nodes
/// ([`Database::shortest_path`](crate::Database::shortest_path)).
#[derive(Clone, Debug, PartialEq)]
pub struct PathRequest {
    pub from: String,
    pub to: String,
    pub method: PathMethod,
    pub cost: EdgeCost,
    pub direction: Direction,
    /// BFS only: at most this many edges.
    pub max_depth: Option<usize>,
    /// Ignore paths that cost more (for BFS: have more edges).
    pub max_cost: Option<f64>,
}

impl PathRequest {
    /// Fewest edges, following outgoing edges.
    pub fn bfs(from: impl Into<String>, to: impl Into<String>) -> Self {
        PathRequest {
            from: from.into(),
            to: to.into(),
            method: PathMethod::Bfs,
            cost: EdgeCost::Unit,
            direction: Direction::Out,
            max_depth: None,
            max_cost: None,
        }
    }
}

/// A path found by [`Database::shortest_path`](crate::Database::shortest_path).
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    /// From the start to the end, both included.
    pub nodes: Vec<String>,
    /// The sum of the edge costs (for BFS: the number of edges).
    pub cost: f64,
}

/// Random walks from a node ([`Database::random_walks`](crate::Database::random_walks)).
#[derive(Clone, Debug, PartialEq)]
pub struct WalkRequest {
    pub start: String,
    /// Most nodes per walk (at least 1).
    pub max_length: usize,
    /// How many walks to attempt; duplicates are removed.
    pub walks: usize,
    /// Drop walks with fewer nodes.
    pub min_length: usize,
    /// Whether a walk may visit a node twice.
    pub allow_revisit: bool,
    /// The same seed gives the same walks for the same graph state in the
    /// same process; random if `None`.
    pub seed: Option<u64>,
}

impl WalkRequest {
    pub fn new(start: impl Into<String>, max_length: usize, walks: usize) -> Self {
        WalkRequest { start: start.into(), max_length, walks, min_length: 1, allow_revisit: false, seed: None }
    }
}

/// The nodes within `depth` edges of the seeds and the edges between them
/// ([`Database::subgraph`](crate::Database::subgraph)).
#[derive(Clone, Debug, PartialEq)]
pub struct SubgraphRequest {
    pub seeds: Vec<String>,
    pub depth: usize,
    pub direction: Direction,
    /// Follow, and return, only edges with one of these types (all if empty).
    pub edge_types: Vec<String>,
    /// Follow, and return, only edges matching this filter.
    pub edge_filter: Option<Expr>,
}

impl SubgraphRequest {
    pub fn new(seeds: impl IntoIterator<Item = impl Into<String>>, depth: usize) -> Self {
        SubgraphRequest {
            seeds: seeds.into_iter().map(Into::into).collect(),
            depth,
            direction: Direction::Out,
            edge_types: Vec::new(),
            edge_filter: None,
        }
    }
}

/// A subgraph: nodes sorted by id, edges sorted by id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Every occurrence of a pattern
/// ([`Database::match_pattern`](crate::Database::match_pattern)).
#[derive(Clone, Debug, PartialEq)]
pub struct MatchRequest {
    pub pattern: Pattern,
    /// `where`: a filter per named node variable, added to the pattern's
    /// own filter of that node (both must hold).
    pub filters: Vec<(String, Expr)>,
}

impl MatchRequest {
    /// A request for the pattern text `text` (the core's Cypher-like
    /// syntax, `Pattern::parse`). Errors: `invalid_argument`.
    pub fn parse(text: &str) -> Result<Self, crate::Error> {
        Ok(MatchRequest { pattern: Pattern::parse(text)?, filters: Vec::new() })
    }

    pub(crate) fn fingerprint(&self) -> u64 {
        let mut f = Fingerprint::new();
        f.str("match").json(&self.pattern).u64(self.filters.len() as u64);
        for (name, filter) in &self.filters {
            f.str(name).json(filter);
        }
        f.finish()
    }
}

/// One match: a node id per pattern node and the edge ids per pattern
/// edge (one for a single edge, the path for a variable-length one), in
/// the pattern's order. Rows are sorted by nodes, then edges.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchRow {
    pub nodes: Vec<String>,
    pub edges: Vec<Vec<EdgeId>>,
}

/// What an analytics job runs on: every node, and the edges followed in
/// `direction`, weighted by `cost`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionSpec {
    pub direction: Direction,
    pub cost: EdgeCost,
}

impl Default for ProjectionSpec {
    /// Outgoing edges, unweighted.
    fn default() -> Self {
        ProjectionSpec { direction: Direction::Out, cost: EdgeCost::Unit }
    }
}

/// An analytics job ([`Database::analyze`](crate::Database::analyze)), run
/// by the core's algorithms on a projection.
#[derive(Clone, Debug)]
pub enum Job {
    /// Scores; `personalization` must be `None` (it is by dense index,
    /// which callers can't know).
    PageRank(PageRank),
    /// Scores: the number of edges per node, normalised by `n - 1`.
    Degree { incoming: bool },
    /// Groups.
    WeaklyConnectedComponents,
    /// Groups.
    StronglyConnectedComponents,
    /// Groups (communities).
    Leiden(Leiden),
    /// Groups (communities).
    LabelPropagation { max_iter: usize },
    /// Counts: each node's core number.
    CoreNumber,
    /// Counts: the triangles through each node.
    Triangles,
}

impl Job {
    /// The job's name, as the protos' `Job` names it: `page_rank`,
    /// `degree`, `weakly_connected_components`, ...
    pub fn name(&self) -> &'static str {
        match self {
            Job::PageRank(_) => "page_rank",
            Job::Degree { .. } => "degree",
            Job::WeaklyConnectedComponents => "weakly_connected_components",
            Job::StronglyConnectedComponents => "strongly_connected_components",
            Job::Leiden(_) => "leiden",
            Job::LabelPropagation { .. } => "label_propagation",
            Job::CoreNumber => "core_number",
            Job::Triangles => "triangles",
        }
    }
}

/// The result of a [`Job`], ranked, at most `max_results` rows.
#[derive(Clone, Debug, PartialEq)]
pub enum JobResult {
    /// Per node, by score (highest first), then id.
    Scores(Vec<(String, f64)>),
    /// Groups of node ids (each sorted), biggest first, then by first id.
    Groups(Vec<Vec<String>>),
    /// Per node, by count (highest first), then id.
    Counts(Vec<(String, u64)>),
}

/// An analytics job on a projection of a namespace.
#[derive(Clone, Debug)]
pub struct AnalyticsRequest {
    pub projection: ProjectionSpec,
    pub job: Job,
}
