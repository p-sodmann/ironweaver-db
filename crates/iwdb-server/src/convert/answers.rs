//! Read answers.

use ironweaver_core::EdgeId;
use iwdb_query::read::{Explain, Lookup, Plan};
use iwdb_query::{Error, JobResult, MatchRow, Path, Subgraph};

use super::{edges_from_pb, keys_from_pb, missing, nodes_from_pb, path_to_pb, size, wide};
use crate::proto as pb;

fn plan_to_pb(plan: &Plan) -> pb::Plan {
    use pb::plan::Kind;
    let kind = match plan {
        Plan::Empty => Kind::Empty(pb::PlanEmpty {}),
        Plan::Label { label } => Kind::Label(pb::PlanLabel { label: label.clone() }),
        Plan::Index { path, lookup } => {
            let (lookup, in_values) = match lookup {
                Lookup::Point => (pb::Lookup::Point, 0),
                Lookup::In { values } => (pb::Lookup::In, wide(*values)),
                Lookup::Range => (pb::Lookup::Range, 0),
            };
            Kind::Index(pb::PlanIndex { path: Some(path_to_pb(path)), lookup: lookup.into(), in_values })
        }
        Plan::Union(plans) => Kind::Union(pb::PlanUnion { plans: plans.iter().map(plan_to_pb).collect() }),
        Plan::Scan => Kind::Scan(pb::PlanScan {}),
        Plan::Other(text) => Kind::Other(text.clone()),
    };
    pb::Plan { kind: Some(kind) }
}

fn plan_from_pb(plan: Option<pb::Plan>) -> Result<Plan, Error> {
    use pb::plan::Kind;
    Ok(match plan.and_then(|p| p.kind).ok_or_else(|| missing("the plan"))? {
        Kind::Empty(_) => Plan::Empty,
        Kind::Label(l) => Plan::Label { label: l.label },
        Kind::Index(i) => {
            let lookup = match pb::Lookup::try_from(i.lookup) {
                Ok(pb::Lookup::Point) => Lookup::Point,
                Ok(pb::Lookup::In) => Lookup::In { values: size(i.in_values) },
                Ok(pb::Lookup::Range) => Lookup::Range,
                _ => return Err(Error::invalid(format!("unknown lookup {}", i.lookup))),
            };
            Plan::Index { path: keys_from_pb(i.path), lookup }
        }
        Kind::Union(u) => Plan::Union(u.plans.into_iter().map(|p| plan_from_pb(Some(p))).collect::<Result<_, _>>()?),
        Kind::Scan(_) => Plan::Scan,
        Kind::Other(text) => Plan::Other(text),
    })
}

pub(crate) fn explain_to_pb(e: &Explain) -> pb::Explain {
    pb::Explain {
        plan: Some(plan_to_pb(&e.plan)),
        estimated_candidates: wide(e.estimated_candidates),
        candidates: e.candidates.map(wide),
        nodes: wide(e.nodes),
        building: e.building.iter().map(|p| path_to_pb(p)).collect(),
    }
}

pub(crate) fn explain_answer_from_pb(e: Option<pb::Explain>) -> Result<Explain, Error> {
    let e = e.ok_or_else(|| missing("the explanation"))?;
    Ok(Explain {
        plan: plan_from_pb(e.plan)?,
        estimated_candidates: size(e.estimated_candidates),
        candidates: e.candidates.map(size),
        nodes: size(e.nodes),
        building: e.building.into_iter().map(|p| p.keys).collect(),
    })
}

pub(crate) fn path_to_answer_pb(path: &Path) -> pb::Path {
    pb::Path { nodes: path.nodes.clone(), cost: path.cost }
}

pub(crate) fn path_from_answer_pb(path: pb::Path) -> Path {
    Path { nodes: path.nodes, cost: path.cost }
}

pub(crate) fn walks_to_pb(walks: &[Vec<String>]) -> Vec<pb::Walk> {
    walks.iter().map(|w| pb::Walk { nodes: w.clone() }).collect()
}

pub(crate) fn match_row_to_pb(row: &MatchRow) -> pb::MatchRow {
    pb::MatchRow {
        nodes: row.nodes.clone(),
        edges: row.edges.iter().map(|ids| pb::EdgePath { ids: ids.iter().map(|e| e.0).collect() }).collect(),
    }
}

pub(crate) fn match_row_from_pb(row: pb::MatchRow) -> MatchRow {
    MatchRow {
        nodes: row.nodes,
        edges: row.edges.into_iter().map(|p| p.ids.into_iter().map(EdgeId).collect()).collect(),
    }
}

/// A job result's rows, as the three row lists and their kind.
pub(crate) struct JobRows {
    pub kind: pb::JobResultKind,
    pub scores: Vec<pb::Score>,
    pub groups: Vec<pb::Group>,
    pub counts: Vec<pb::Count>,
}

pub(crate) fn job_result_to_pb(result: &JobResult) -> JobRows {
    let mut rows = JobRows { kind: pb::JobResultKind::Unspecified, scores: vec![], groups: vec![], counts: vec![] };
    match result {
        JobResult::Scores(s) => {
            rows.kind = pb::JobResultKind::Scores;
            rows.scores = s.iter().map(|(id, score)| pb::Score { id: id.clone(), score: *score }).collect();
        }
        JobResult::Groups(g) => {
            rows.kind = pb::JobResultKind::Groups;
            rows.groups = g.iter().map(|ids| pb::Group { ids: ids.clone() }).collect();
        }
        JobResult::Counts(c) => {
            rows.kind = pb::JobResultKind::Counts;
            rows.counts = c.iter().map(|(id, count)| pb::Count { id: id.clone(), count: *count }).collect();
        }
    }
    rows
}

pub(crate) fn job_result_from_pb(rows: JobRows) -> Result<JobResult, Error> {
    Ok(match rows.kind {
        pb::JobResultKind::Scores => JobResult::Scores(rows.scores.into_iter().map(|s| (s.id, s.score)).collect()),
        pb::JobResultKind::Groups => JobResult::Groups(rows.groups.into_iter().map(|g| g.ids).collect()),
        pb::JobResultKind::Counts => JobResult::Counts(rows.counts.into_iter().map(|c| (c.id, c.count)).collect()),
        pb::JobResultKind::Unspecified => return Err(missing("the kind of the job result")),
    })
}

pub(crate) fn subgraph_answer_from_pb(nodes: Vec<pb::Node>, edges: Vec<pb::Edge>) -> Result<Subgraph, Error> {
    Ok(Subgraph { nodes: nodes_from_pb(nodes)?, edges: edges_from_pb(edges)? })
}
