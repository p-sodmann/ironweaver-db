//! `explain`: how `find` would read a filter, from the core's
//! `Graph::index_plan` (the plan `index_candidates` follows).

use ironweaver_core::{Expr, IndexPlan};
use iwdb_engine::Namespace;

use super::ReadContext;
use crate::{Answer, Error, ExplainRequest, Work};

/// How an index is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// One value (`Eq`).
    Point,
    /// Several values (`In`).
    In { values: usize },
    /// A range (`Lt`, `Le`, `Gt`, `Ge`, or a lower and an upper bound on
    /// the same path inside an `And`).
    Range,
}

/// Where `find` gets its candidates from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The filter is `Const(false)`: nothing to read.
    Empty,
    /// The label index.
    Label { label: String },
    /// A property index.
    Index { path: Vec<String>, lookup: Lookup },
    /// The union of the plans of an `Or` (each branch is served by an index).
    Union(Vec<Plan>),
    /// No index narrows the filter down: every node is a candidate.
    Scan,
    /// A plan of a newer core this version doesn't know (its debug form).
    Other(String),
}

impl Plan {
    fn of(plan: &IndexPlan) -> Plan {
        let index = |path: &Vec<String>, lookup| Plan::Index { path: path.clone(), lookup };
        match plan {
            IndexPlan::Empty => Plan::Empty,
            IndexPlan::Label(label) => Plan::Label { label: label.clone() },
            IndexPlan::Point { path, .. } => index(path, Lookup::Point),
            IndexPlan::In { path, values } => index(path, Lookup::In { values: values.len() }),
            IndexPlan::Range { path, .. } => index(path, Lookup::Range),
            IndexPlan::Union(plans) => Plan::Union(plans.iter().map(Plan::of).collect()),
            other => Plan::Other(format!("{:?}", other)),
        }
    }
}

/// The answer of [`explain`].
#[derive(Clone, Debug, PartialEq)]
pub struct Explain {
    pub plan: Plan,
    /// The estimated number of candidates `find` checks (each counts as
    /// visited), from the core's `index_plan_estimate`: exact for labels
    /// and point lookups, for a range exact if it spans a few dozen keys
    /// and the index's size otherwise, the sum of the parts for a union,
    /// every node for a scan.
    pub estimated_candidates: usize,
    /// With `analyze`: the exact number of candidates.
    pub candidates: Option<usize>,
    /// The namespace's node count.
    pub nodes: usize,
    /// Attribute paths of the filter whose index is being built: `find`
    /// can't use them until the build is installed.
    pub building: Vec<Vec<String>>,
}

/// How `find` would read `request.filter`. `building` lists the paths of
/// index builds in progress (from the namespace's status). O(size of the
/// filter); with `analyze`, also O(candidates).
pub fn explain(
    ns: &Namespace,
    request: &ExplainRequest,
    building: &[Vec<String>],
    _cx: &ReadContext,
) -> Result<Answer<Explain>, Error> {
    let g = ns.graph();
    let core = g.index_plan(&request.filter);
    let (plan, estimated_candidates) = match &core {
        Some(p) => (Plan::of(p), g.index_plan_estimate(p)),
        None => (Plan::Scan, g.node_count()),
    };
    let candidates = match (&core, request.analyze) {
        (_, false) => None,
        (Some(p), true) => Some(g.execute_index_plan(p)?.len()),
        (None, true) => Some(g.node_count()),
    };
    let mut paths = Vec::new();
    referenced_paths(&request.filter, &mut paths);
    let building = building.iter().filter(|b| paths.contains(b)).cloned().collect();
    let value = Explain { plan, estimated_candidates, candidates, nodes: g.node_count(), building };
    let work = Work { visited: candidates.unwrap_or(0), edges: 0 };
    Ok(Answer { work, ..Answer::at(ns.seq(), value) })
}

fn referenced_paths(expr: &Expr, out: &mut Vec<Vec<String>>) {
    match expr {
        Expr::Compare { path, .. } | Expr::In { path, .. } | Expr::Exists { path } => {
            if !out.contains(path) {
                out.push(path.clone());
            }
        }
        Expr::And(items) | Expr::Or(items) => items.iter().for_each(|e| referenced_paths(e, out)),
        Expr::Not(inner) => referenced_paths(inner, out),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ironweaver_core::{CmpOp, Value};
    use iwdb_engine::catalog::{AttrPath, IndexDef, NamespaceName};
    use iwdb_engine::{CatalogChange, Mutation};
    use iwdb_storage::HistoryId;

    use super::*;
    use crate::{Bounds, QueryOptions};

    fn cmp(path: &str, op: CmpOp, v: i64) -> Expr {
        Expr::Compare { path: vec![path.into()], op, value: Value::Int(v) }
    }

    /// 40 nodes, `age` 0..10 (indexed, 4 nodes each), `x` 0..40, every
    /// fourth labelled `Four`.
    fn namespace() -> Namespace {
        let mut ns = Namespace::new(NamespaceName::new("t").unwrap());
        let mutations: Vec<Mutation> = (0..40)
            .map(|i| Mutation::UpsertNode {
                id: format!("n{:02}", i),
                labels: if i % 4 == 0 { vec!["Four".into()] } else { vec![] },
                attr: [("age".to_owned(), Value::Int(i % 10)), ("x".to_owned(), Value::Int(i))].into(),
                meta: Default::default(),
                expected_version: None,
            })
            .collect();
        ns.commit(&mutations).unwrap();
        let path = AttrPath::new(["age"]).unwrap();
        ns.commit_catalog(CatalogChange::CreateIndex(IndexDef { path })).unwrap();
        ns
    }

    const BOUNDS: Bounds = Bounds { max_results: 100, max_visited: 1_000, max_edges: 1_000 };

    fn run(ns: &Namespace, filter: Expr) -> Explain {
        let cx = ReadContext::new(BOUNDS, &QueryOptions::default(), 1, HistoryId::default());
        explain(ns, &ExplainRequest { filter, analyze: true }, &[], &cx).unwrap().value
    }

    #[test]
    fn the_plan_and_its_estimate_come_from_the_core() {
        let ns = namespace();
        let age = || vec!["age".to_owned()];
        let index = |lookup| Plan::Index { path: age(), lookup };
        let cases = [
            (Expr::Const(false), Plan::Empty, 0),
            (Expr::Label("Four".into()), Plan::Label { label: "Four".into() }, 10),
            (cmp("age", CmpOp::Eq, 3), index(Lookup::Point), 4),
            (Expr::In { path: age(), values: vec![Value::Int(1), Value::Int(2)] }, index(Lookup::In { values: 2 }), 8),
            (Expr::And(vec![cmp("age", CmpOp::Ge, 2), cmp("age", CmpOp::Lt, 4)]), index(Lookup::Range), 8),
            // The smaller estimate wins: the point lookup, not the label
            (Expr::And(vec![Expr::Label("Four".into()), cmp("age", CmpOp::Eq, 4)]), index(Lookup::Point), 4),
            (
                Expr::Or(vec![cmp("age", CmpOp::Eq, 1), Expr::Label("Four".into())]),
                Plan::Union(vec![index(Lookup::Point), Plan::Label { label: "Four".into() }]),
                14,
            ),
            (cmp("age", CmpOp::Ne, 3), Plan::Scan, 40),
            (cmp("x", CmpOp::Eq, 3), Plan::Scan, 40),
            (Expr::Or(vec![cmp("age", CmpOp::Eq, 1), cmp("x", CmpOp::Eq, 1)]), Plan::Scan, 40),
        ];
        for (filter, plan, estimate) in cases {
            let explained = run(&ns, filter.clone());
            assert_eq!((&explained.plan, explained.estimated_candidates), (&plan, estimate), "{:?}", filter);
            // Exact estimates here (no range spans more than a few keys)
            assert_eq!(explained.candidates, Some(estimate.min(40)), "{:?}", filter);
        }
    }
}
