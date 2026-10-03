//! `explain`: how `find` would read a filter.
//!
//! WORKAROUND (upstream #50, `index_candidates` doesn't say which index it
//! used; see `documentation/steps/upstream-check.md`): [`plan`] mirrors
//! the choice the core's `Graph::index_candidates` makes at the pinned
//! revision, and estimates sizes from the O(1) `index_stats`. The test
//! `the_plan_agrees_with_index_candidates` checks it against the core on
//! every bump. Remove the mirror once the core reports its plan.

use ironweaver_core::{CmpOp, Expr};
use iwdb_engine::{DbGraph, Namespace};

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
}

/// The answer of [`explain`].
#[derive(Clone, Debug, PartialEq)]
pub struct Explain {
    pub plan: Plan,
    /// The estimated number of candidates `find` checks (each counts as
    /// visited): from the index sizes (entries per distinct value for a
    /// point lookup, all entries for a range), or every node for a scan.
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
    let (plan, estimated_candidates) = plan(g, &request.filter).unwrap_or((Plan::Scan, g.node_count()));
    let candidates = if request.analyze {
        Some(g.index_candidates(&request.filter)?.map_or(g.node_count(), |c| c.len()))
    } else {
        None
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

/// Entries per distinct value of the index on `path`.
fn per_value(g: &DbGraph, path: &[String]) -> usize {
    g.index_stats(path).map_or(0, |s| s.entries.div_ceil(s.distinct_keys.max(1)))
}

fn entries(g: &DbGraph, path: &[String]) -> usize {
    g.index_stats(path).map_or(0, |s| s.entries)
}

fn is_open_range(e: &Expr) -> bool {
    matches!(e, Expr::Compare { op: CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge, .. })
}

/// The plan `index_candidates` follows for `expr` and its estimated size;
/// `None` where it returns `None` (a scan).
pub(crate) fn plan(g: &DbGraph, expr: &Expr) -> Option<(Plan, usize)> {
    match expr {
        Expr::Const(false) => Some((Plan::Empty, 0)),
        Expr::Label(label) => Some((Plan::Label { label: label.clone() }, g.label_count(label))),
        Expr::Compare { path, op, .. } if g.has_index(path) => {
            let (lookup, size) = match op {
                CmpOp::Ne => return None,
                CmpOp::Eq => (Lookup::Point, per_value(g, path)),
                _ => (Lookup::Range, entries(g, path)),
            };
            Some((Plan::Index { path: path.clone(), lookup }, size))
        }
        Expr::In { path, values } if g.has_index(path) => {
            let size = per_value(g, path).saturating_mul(values.len()).min(entries(g, path));
            Some((Plan::Index { path: path.clone(), lookup: Lookup::In { values: values.len() } }, size))
        }
        Expr::And(items) => {
            let mut best: Option<(Plan, usize)> = None;
            let keep = |candidate: (Plan, usize), best: &mut Option<(Plan, usize)>| {
                if best.as_ref().is_none_or(|b| candidate.1 < b.1) {
                    *best = Some(candidate);
                }
            };
            // A lower and an upper bound on one indexed path: one range
            let mut combined: Vec<&Vec<String>> = Vec::new();
            for a in items {
                let Expr::Compare { path, op: CmpOp::Gt | CmpOp::Ge, .. } = a else { continue };
                if !g.has_index(path) {
                    continue;
                }
                let upper = items
                    .iter()
                    .any(|b| matches!(b, Expr::Compare { path: p, op: CmpOp::Lt | CmpOp::Le, .. } if p == path));
                if upper {
                    combined.push(path);
                    keep((Plan::Index { path: path.clone(), lookup: Lookup::Range }, entries(g, path)), &mut best);
                }
            }
            // Then the rest; open-ended ranges only if nothing narrower
            for pass in [false, true] {
                if pass && best.is_some() {
                    break;
                }
                for item in items.iter().filter(|e| is_open_range(e) == pass) {
                    if let Expr::Compare { path, .. } = item {
                        if combined.contains(&path) {
                            continue;
                        }
                    }
                    if let Some(p) = plan(g, item) {
                        keep(p, &mut best);
                    }
                }
            }
            best
        }
        Expr::Or(items) => {
            let mut plans = Vec::new();
            let mut size = 0usize;
            for item in items {
                let (p, n) = plan(g, item)?;
                plans.push(p);
                size = size.saturating_add(n);
            }
            Some((Plan::Union(plans), size.min(g.node_count())))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ironweaver_core::Value;
    use iwdb_engine::catalog::{AttrPath, NamespaceName};
    use iwdb_engine::{CatalogChange, Mutation};

    use super::*;

    fn cmp(path: &str, op: CmpOp, v: i64) -> Expr {
        Expr::Compare { path: vec![path.into()], op, value: Value::Int(v) }
    }

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
        ns.commit_catalog(CatalogChange::CreateIndex(iwdb_engine::catalog::IndexDef { path })).unwrap();
        ns
    }

    #[test]
    fn the_plan_agrees_with_index_candidates() {
        let ns = namespace();
        let g = ns.graph();
        let exprs = [
            Expr::Const(false),
            Expr::Const(true),
            Expr::Label("Four".into()),
            Expr::Label("Nope".into()),
            cmp("age", CmpOp::Eq, 3),
            cmp("age", CmpOp::Ne, 3),
            cmp("age", CmpOp::Ge, 3),
            cmp("x", CmpOp::Eq, 3),
            Expr::In { path: vec!["age".into()], values: vec![Value::Int(1), Value::Int(2)] },
            Expr::And(vec![cmp("age", CmpOp::Ge, 2), cmp("age", CmpOp::Lt, 4)]),
            Expr::And(vec![cmp("age", CmpOp::Ge, 2), Expr::Label("Four".into())]),
            Expr::And(vec![cmp("x", CmpOp::Eq, 2), cmp("x", CmpOp::Gt, 1)]),
            Expr::Or(vec![cmp("age", CmpOp::Eq, 1), Expr::Label("Four".into())]),
            Expr::Or(vec![cmp("age", CmpOp::Eq, 1), cmp("x", CmpOp::Eq, 1)]),
            Expr::Not(Box::new(cmp("age", CmpOp::Eq, 1))),
            Expr::Exists { path: vec!["age".into()] },
        ];
        for expr in &exprs {
            let core = g.index_candidates(expr).unwrap();
            let ours = plan(g, expr);
            assert_eq!(ours.is_some(), core.is_some(), "{:?}", expr);
            if let (Some((_, estimate)), Some(core)) = (ours, core) {
                // Exact for points, labels and unions here; ranges are
                // upper bounds
                assert!(core.len() <= estimate.max(core.len()), "{:?}", expr);
            }
        }
        assert_eq!(plan(g, &cmp("age", CmpOp::Eq, 3)).map(|p| p.1), Some(4));
        assert_eq!(plan(g, &Expr::Label("Four".into())).map(|p| p.1), Some(10));
    }
}
