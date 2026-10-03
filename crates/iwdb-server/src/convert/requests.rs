//! Read requests.

use ironweaver_core::algo::{Leiden, PageRank};
use ironweaver_core::pathfinding::{Coords, Metric};
use iwdb_query::{
    AnalyticsRequest, Error, ExplainRequest, FindRequest, Job, MatchRequest, NeighbourhoodRequest, Order, PathMethod,
    PathRequest, ProjectionSpec, QueryOptions, SubgraphRequest, TraverseRequest, WalkRequest,
};

use super::{
    cost_from_pb, cost_to_pb, direction_from_pb, direction_to_pb, expr_from_pb, expr_to_pb, missing, opt_expr_from_pb,
    opt_expr_to_pb, options_to_pb, path_to_pb, pattern_from_pb, pattern_to_pb, size, wide,
};
use crate::proto as pb;

pub(crate) fn find_to_pb(request: &FindRequest) -> Result<pb::Expr, Error> {
    expr_to_pb(&request.filter)
}

pub(crate) fn find_from_pb(filter: Option<pb::Expr>) -> Result<FindRequest, Error> {
    Ok(FindRequest { filter: expr_from_pb(filter.ok_or_else(|| missing("the filter"))?, "filter")? })
}

pub(crate) fn explain_from_pb(filter: Option<pb::Expr>, analyze: bool) -> Result<ExplainRequest, Error> {
    Ok(ExplainRequest { filter: find_from_pb(filter)?.filter, analyze })
}

pub(crate) fn neighbourhood_to_pb(
    namespace: &str,
    r: &NeighbourhoodRequest,
    options: &QueryOptions,
) -> Result<pb::NeighbourhoodRequest, Error> {
    Ok(pb::NeighbourhoodRequest {
        namespace: namespace.to_owned(),
        seeds: r.seeds.clone(),
        depth: wide(r.depth),
        direction: direction_to_pb(r.direction),
        edge_types: r.edge_types.clone(),
        edge_filter: opt_expr_to_pb(&r.edge_filter)?,
        node_filter: opt_expr_to_pb(&r.node_filter)?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn neighbourhood_from_pb(r: pb::NeighbourhoodRequest) -> Result<NeighbourhoodRequest, Error> {
    Ok(NeighbourhoodRequest {
        seeds: r.seeds,
        depth: size(r.depth),
        direction: direction_from_pb(r.direction)?,
        edge_types: r.edge_types,
        edge_filter: opt_expr_from_pb(r.edge_filter, "edge filter")?,
        node_filter: opt_expr_from_pb(r.node_filter, "node filter")?,
    })
}

pub(crate) fn traverse_to_pb(
    namespace: &str,
    r: &TraverseRequest,
    options: &QueryOptions,
) -> Result<pb::TraverseRequest, Error> {
    let order = match r.order {
        Order::Bfs => pb::Order::Bfs,
        Order::Dfs => pb::Order::Dfs,
    };
    Ok(pb::TraverseRequest {
        namespace: namespace.to_owned(),
        start: r.start.clone(),
        order: order.into(),
        depth: r.depth.map(wide),
        direction: direction_to_pb(r.direction),
        edge_types: r.edge_types.clone(),
        edge_filter: opt_expr_to_pb(&r.edge_filter)?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn traverse_from_pb(r: pb::TraverseRequest) -> Result<TraverseRequest, Error> {
    let order = match pb::Order::try_from(r.order) {
        Ok(pb::Order::Unspecified | pb::Order::Bfs) => Order::Bfs,
        Ok(pb::Order::Dfs) => Order::Dfs,
        Err(_) => return Err(Error::invalid(format!("unknown order {}", r.order))),
    };
    Ok(TraverseRequest {
        start: r.start,
        order,
        depth: r.depth.map(size),
        direction: direction_from_pb(r.direction)?,
        edge_types: r.edge_types,
        edge_filter: opt_expr_from_pb(r.edge_filter, "edge filter")?,
    })
}

fn coords_to_pb(coords: &Coords) -> pb::Coords {
    let kind = match coords {
        Coords::Sequence(path) => pb::coords::Kind::Sequence(path_to_pb(path)),
        Coords::PerDimension(paths) => pb::coords::Kind::PerDimension(pb::CoordsPerDimension {
            paths: paths.iter().map(|p| path_to_pb(p)).collect(),
        }),
    };
    pb::Coords { kind: Some(kind) }
}

fn coords_from_pb(coords: Option<pb::Coords>) -> Coords {
    match coords.and_then(|c| c.kind) {
        None => Coords::default(),
        Some(pb::coords::Kind::Sequence(path)) => Coords::Sequence(path.keys),
        Some(pb::coords::Kind::PerDimension(d)) => Coords::PerDimension(d.paths.into_iter().map(|p| p.keys).collect()),
    }
}

pub(crate) fn path_request_to_pb(namespace: &str, r: &PathRequest, options: &QueryOptions) -> pb::ShortestPathRequest {
    use pb::path_method::Kind;
    let method = match &r.method {
        PathMethod::Bfs => Kind::Bfs(pb::PathBfs {}),
        PathMethod::Dijkstra => Kind::Dijkstra(pb::PathDijkstra {}),
        PathMethod::AStar { coords, metric } => {
            let metric = match metric {
                Metric::Euclidean => pb::Metric::Euclidean,
                Metric::Manhattan => pb::Metric::Manhattan,
            };
            Kind::AStar(pb::PathAStar { coords: Some(coords_to_pb(coords)), metric: metric.into() })
        }
    };
    pb::ShortestPathRequest {
        namespace: namespace.to_owned(),
        from: r.from.clone(),
        to: r.to.clone(),
        method: Some(pb::PathMethod { kind: Some(method) }),
        cost: Some(cost_to_pb(&r.cost)),
        direction: direction_to_pb(r.direction),
        max_depth: r.max_depth.map(wide),
        max_cost: r.max_cost,
        options: Some(options_to_pb(options)),
    }
}

pub(crate) fn path_request_from_pb(r: pb::ShortestPathRequest) -> Result<PathRequest, Error> {
    use pb::path_method::Kind;
    let method = match r.method.and_then(|m| m.kind) {
        None | Some(Kind::Bfs(_)) => PathMethod::Bfs,
        Some(Kind::Dijkstra(_)) => PathMethod::Dijkstra,
        Some(Kind::AStar(a)) => {
            let metric = match pb::Metric::try_from(a.metric) {
                Ok(pb::Metric::Unspecified | pb::Metric::Euclidean) => Metric::Euclidean,
                Ok(pb::Metric::Manhattan) => Metric::Manhattan,
                Err(_) => return Err(Error::invalid(format!("unknown metric {}", a.metric))),
            };
            PathMethod::AStar { coords: coords_from_pb(a.coords), metric }
        }
    };
    Ok(PathRequest {
        from: r.from,
        to: r.to,
        method,
        cost: cost_from_pb(r.cost),
        direction: direction_from_pb(r.direction)?,
        max_depth: r.max_depth.map(size),
        max_cost: r.max_cost,
    })
}

pub(crate) fn walk_to_pb(namespace: &str, r: &WalkRequest, options: &QueryOptions) -> pb::RandomWalksRequest {
    pb::RandomWalksRequest {
        namespace: namespace.to_owned(),
        start: r.start.clone(),
        max_length: wide(r.max_length),
        walks: wide(r.walks),
        min_length: wide(r.min_length),
        allow_revisit: r.allow_revisit,
        seed: r.seed,
        options: Some(options_to_pb(options)),
    }
}

pub(crate) fn walk_from_pb(r: pb::RandomWalksRequest) -> WalkRequest {
    WalkRequest {
        start: r.start,
        max_length: size(r.max_length),
        walks: size(r.walks),
        min_length: size(r.min_length),
        allow_revisit: r.allow_revisit,
        seed: r.seed,
    }
}

pub(crate) fn subgraph_to_pb(
    namespace: &str,
    r: &SubgraphRequest,
    options: &QueryOptions,
) -> Result<pb::SubgraphRequest, Error> {
    Ok(pb::SubgraphRequest {
        namespace: namespace.to_owned(),
        seeds: r.seeds.clone(),
        depth: wide(r.depth),
        direction: direction_to_pb(r.direction),
        edge_types: r.edge_types.clone(),
        edge_filter: opt_expr_to_pb(&r.edge_filter)?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn subgraph_from_pb(r: pb::SubgraphRequest) -> Result<SubgraphRequest, Error> {
    Ok(SubgraphRequest {
        seeds: r.seeds,
        depth: size(r.depth),
        direction: direction_from_pb(r.direction)?,
        edge_types: r.edge_types,
        edge_filter: opt_expr_from_pb(r.edge_filter, "edge filter")?,
    })
}

pub(crate) fn match_to_pb(
    namespace: &str,
    r: &MatchRequest,
    options: &QueryOptions,
) -> Result<pb::MatchPatternRequest, Error> {
    Ok(pb::MatchPatternRequest {
        namespace: namespace.to_owned(),
        pattern: Some(pattern_to_pb(&r.pattern)?),
        filters: r
            .filters
            .iter()
            .map(|(name, filter)| Ok(pb::VariableFilter { name: name.clone(), filter: Some(expr_to_pb(filter)?) }))
            .collect::<Result<_, Error>>()?,
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn match_from_pb(r: pb::MatchPatternRequest) -> Result<MatchRequest, Error> {
    let filters = r
        .filters
        .into_iter()
        .map(|f| {
            let filter = f.filter.ok_or_else(|| missing(&format!("the filter of '{}'", f.name)))?;
            Ok((f.name, expr_from_pb(filter, "filter")?))
        })
        .collect::<Result<_, Error>>()?;
    Ok(MatchRequest { pattern: pattern_from_pb(r.pattern)?, filters })
}

fn job_to_pb(job: &Job) -> Result<pb::Job, Error> {
    use pb::job::Kind;
    let kind = match job {
        Job::PageRank(p) => {
            if p.personalization.is_some() {
                return Err(Error::invalid("PageRank's personalization (by dense index) can't be sent to a server"));
            }
            Kind::PageRank(pb::PageRankJob { alpha: Some(p.alpha), max_iter: Some(wide(p.max_iter)), tol: Some(p.tol) })
        }
        Job::Degree { incoming } => Kind::Degree(pb::DegreeJob { incoming: *incoming }),
        Job::WeaklyConnectedComponents => Kind::WeaklyConnectedComponents(pb::WeaklyConnectedComponentsJob {}),
        Job::StronglyConnectedComponents => Kind::StronglyConnectedComponents(pb::StronglyConnectedComponentsJob {}),
        Job::Leiden(l) => Kind::Leiden(pb::LeidenJob {
            resolution: Some(l.resolution),
            randomness: Some(l.randomness),
            max_iter: Some(wide(l.max_iter)),
            seed: Some(l.seed),
        }),
        Job::LabelPropagation { max_iter } => {
            Kind::LabelPropagation(pb::LabelPropagationJob { max_iter: wide(*max_iter) })
        }
        Job::CoreNumber => Kind::CoreNumber(pb::CoreNumberJob {}),
        Job::Triangles => Kind::Triangles(pb::TrianglesJob {}),
    };
    Ok(pb::Job { kind: Some(kind) })
}

fn job_from_pb(job: Option<pb::Job>) -> Result<Job, Error> {
    use pb::job::Kind;
    Ok(match job.and_then(|j| j.kind).ok_or_else(|| missing("the job"))? {
        Kind::PageRank(p) => {
            let d = PageRank::default();
            Job::PageRank(PageRank {
                alpha: p.alpha.unwrap_or(d.alpha),
                personalization: None,
                max_iter: p.max_iter.map_or(d.max_iter, size),
                tol: p.tol.unwrap_or(d.tol),
            })
        }
        Kind::Degree(d) => Job::Degree { incoming: d.incoming },
        Kind::WeaklyConnectedComponents(_) => Job::WeaklyConnectedComponents,
        Kind::StronglyConnectedComponents(_) => Job::StronglyConnectedComponents,
        Kind::Leiden(l) => {
            let d = Leiden::default();
            Job::Leiden(Leiden {
                resolution: l.resolution.unwrap_or(d.resolution),
                randomness: l.randomness.unwrap_or(d.randomness),
                max_iter: l.max_iter.map_or(d.max_iter, size),
                seed: l.seed.unwrap_or(d.seed),
            })
        }
        Kind::LabelPropagation(l) => Job::LabelPropagation { max_iter: size(l.max_iter) },
        Kind::CoreNumber(_) => Job::CoreNumber,
        Kind::Triangles(_) => Job::Triangles,
    })
}

pub(crate) fn analyze_to_pb(
    namespace: &str,
    r: &AnalyticsRequest,
    options: &QueryOptions,
) -> Result<pb::AnalyzeRequest, Error> {
    Ok(pb::AnalyzeRequest {
        namespace: namespace.to_owned(),
        projection: Some(pb::Projection {
            direction: direction_to_pb(r.projection.direction),
            cost: Some(cost_to_pb(&r.projection.cost)),
        }),
        job: Some(job_to_pb(&r.job)?),
        options: Some(options_to_pb(options)),
    })
}

pub(crate) fn analyze_from_pb(r: pb::AnalyzeRequest) -> Result<AnalyticsRequest, Error> {
    let p = r.projection.unwrap_or_default();
    Ok(AnalyticsRequest {
        projection: ProjectionSpec { direction: direction_from_pb(p.direction)?, cost: cost_from_pb(p.cost) },
        job: job_from_pb(r.job)?,
    })
}
