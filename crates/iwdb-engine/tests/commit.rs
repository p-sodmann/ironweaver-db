//! Step 3: the commit pipeline. Version semantics, conflicts, constraints,
//! catalog changes, reserved names and limits, each as a typed error that
//! leaves the namespace unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ironweaver_core::format;
use ironweaver_core::{Attrs, EdgeId, Graph, Op, Record, Value};
use iwdb_engine::catalog::{AttrPath, Constraint, ConstraintKind, IndexDef, Label, NamespaceName};
use iwdb_engine::codec::{self, GraphMeta};
use iwdb_engine::reserved::VERSION_KEY;
use iwdb_engine::testutil::canonical;
use iwdb_engine::{
    CatalogChange, Change, CommitRecord, DbGraph, DbRecord, EdgeKey, Entity, Error, Mutation, Namespace, Target,
};

// Builders

fn ns() -> Namespace {
    Namespace::new(NamespaceName::new("test").unwrap())
}

fn attrs(entries: &[(&str, Value)]) -> Attrs {
    entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn upsert(id: &str, labels: &[&str], attr: &[(&str, Value)]) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: labels.iter().map(|l| l.to_string()).collect(),
        attr: attrs(attr),
        meta: Attrs::new(),
        expected_version: None,
    }
}

fn expect(mut m: Mutation, v: u64) -> Mutation {
    match &mut m {
        Mutation::UpsertNode { expected_version, .. }
        | Mutation::DeleteNode { expected_version, .. }
        | Mutation::UpsertEdge { expected_version, .. }
        | Mutation::DeleteEdge { expected_version, .. }
        | Mutation::SetAttr { expected_version, .. }
        | Mutation::RemoveAttr { expected_version, .. }
        | Mutation::AppendAttr { expected_version, .. }
        | Mutation::AddLabel { expected_version, .. }
        | Mutation::RemoveLabel { expected_version, .. }
        | Mutation::SetEdgeType { expected_version, .. } => *expected_version = Some(v),
        Mutation::AddEdge { .. } => panic!("AddEdge has no expected version"),
    }
    m
}

fn delete(id: &str) -> Mutation {
    Mutation::DeleteNode { id: id.into(), expected_version: None }
}

fn edge(from: &str, to: &str, ty: Option<&str>) -> Mutation {
    Mutation::AddEdge {
        from: from.into(),
        to: to.into(),
        ty: ty.map(str::to_owned),
        attr: Attrs::new(),
        meta: Attrs::new(),
    }
}

fn node(id: &str) -> Target {
    Target::Node(id.into())
}

fn set(target: Target, key: &str, value: Value) -> Mutation {
    Mutation::SetAttr { target, key: key.into(), value, expected_version: None }
}

fn remove(target: Target, key: &str) -> Mutation {
    Mutation::RemoveAttr { target, key: key.into(), expected_version: None }
}

fn append(target: Target, key: &str, value: Value) -> Mutation {
    Mutation::AppendAttr { target, key: key.into(), value, expected_version: None }
}

fn add_label(id: &str, label: &str) -> Mutation {
    Mutation::AddLabel { id: id.into(), label: label.into(), expected_version: None }
}

fn remove_label(id: &str, label: &str) -> Mutation {
    Mutation::RemoveLabel { id: id.into(), label: label.into(), expected_version: None }
}

fn path(keys: &[&str]) -> AttrPath {
    AttrPath::new(keys.iter().copied()).unwrap()
}

fn constraint(kind: ConstraintKind, label: &str, keys: &[&str]) -> Constraint {
    Constraint { kind, label: Label::new(label).unwrap(), path: path(keys) }
}

fn unique(label: &str, keys: &[&str]) -> Constraint {
    constraint(ConstraintKind::Unique, label, keys)
}

fn required(label: &str, keys: &[&str]) -> Constraint {
    constraint(ConstraintKind::Required, label, keys)
}

fn version_of(ns: &Namespace, target: &Target) -> Option<u64> {
    let g = ns.graph();
    match target {
        Target::Node(id) => g.node_by_id(id).map(|n| n.data.version),
        Target::Edge(id) => g.edge_ix(*id).and_then(|e| g.edge(e)).map(|e| e.data.version),
    }
}

/// Commit `mutations`, expect `error`, and check that nothing changed.
fn rejects(ns: &mut Namespace, mutations: &[Mutation], error: Error) {
    let (before, seq, next_edge) = (canonical(ns.graph()), ns.seq(), ns.graph().next_edge_id());
    assert_eq!(ns.commit(mutations), Err(error));
    assert_eq!(canonical(ns.graph()), before);
    assert_eq!(ns.seq(), seq, "a failed commit uses no seq");
    assert_eq!(ns.graph().next_edge_id(), next_edge, "a rejected commit never reaches the graph");
    assert!(!ns.is_poisoned());
}

fn violation(c: &Constraint, node: &str, other: Option<&str>) -> Error {
    Error::ConstraintViolation { constraint: c.clone(), node: node.into(), other: other.map(str::to_owned) }
}

// Versions and seq

#[test]
fn versions_start_at_one_and_grow_by_one_per_commit() {
    let mut ns = ns();
    let r = ns.commit(&[upsert("a", &["Person"], &[("name", Value::from("alice"))])]).unwrap();
    assert_eq!((r.seq, r.versions), (1, vec![(node("a"), 1)]));

    // Several mutations of one node in one commit: one bump
    let r = ns
        .commit(&[set(node("a"), "x", Value::Int(1)), set(node("a"), "y", Value::Int(2)), add_label("a", "Admin")])
        .unwrap();
    assert_eq!((r.seq, r.versions), (2, vec![(node("a"), 2)]));
    assert_eq!(version_of(&ns, &node("a")), Some(2));

    // A write that changes nothing is still a write
    let r = ns.commit(&[add_label("a", "Admin"), remove(node("a"), "missing")]).unwrap();
    assert_eq!(r.versions, vec![(node("a"), 3)]);

    // expected_version is the version before the commit, for every mutation in it
    let r = ns.commit(&[expect(set(node("a"), "x", Value::Int(5)), 3), expect(add_label("a", "X"), 3)]).unwrap();
    assert_eq!(r.versions, vec![(node("a"), 4)]);
    rejects(
        &mut ns,
        &[expect(set(node("a"), "x", Value::Int(6)), 3)],
        Error::Conflict { target: node("a"), expected: 3, actual: 4 },
    );

    // 0: must not exist
    rejects(
        &mut ns,
        &[expect(upsert("a", &[], &[]), 0)],
        Error::Conflict { target: node("a"), expected: 0, actual: 4 },
    );
    let r = ns.commit(&[expect(upsert("b", &[], &[]), 0), expect(set(node("b"), "k", Value::Int(1)), 0)]).unwrap();
    assert_eq!(r.versions, vec![(node("b"), 1)]);

    // Deleted in one commit, created again in a later one: starts at 1
    ns.commit(&[delete("a")]).unwrap();
    rejects(&mut ns, &[expect(delete("a"), 4)], Error::Conflict { target: node("a"), expected: 4, actual: 0 });
    let r = ns.commit(&[upsert("a", &[], &[])]).unwrap();
    assert_eq!(r.versions, vec![(node("a"), 1)]);

    // Deleted and created again in one commit: a write like any other
    let r = ns.commit(&[delete("b"), upsert("b", &[], &[("new", Value::Bool(true))])]).unwrap();
    assert_eq!(r.versions, vec![(node("b"), 2)]);
    assert_eq!(ns.seq(), 8);

    // Created and deleted in one commit: no version
    let r = ns.commit(&[upsert("tmp", &[], &[]), delete("tmp")]).unwrap();
    assert!(r.versions.is_empty());
    assert!(ns.graph().node_by_id("tmp").is_none());
}

#[test]
fn edges_have_versions_and_new_ids_from_the_counter() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &[], &[]), upsert("b", &[], &[])]).unwrap();
    let r = ns.commit(&[edge("a", "b", Some("KNOWS")), edge("a", "b", None)]).unwrap();
    assert_eq!(r.edge_ids, vec![EdgeId(0), EdgeId(1)]);
    assert_eq!(r.versions, vec![(Target::Edge(EdgeId(0)), 1), (Target::Edge(EdgeId(1)), 1)]);

    // Upsert by endpoints matches the type exactly
    let upsert_edge = |ty: Option<&str>, attr: &[(&str, Value)]| Mutation::UpsertEdge {
        key: EdgeKey::Endpoints { from: "a".into(), to: "b".into(), ty: ty.map(str::to_owned) },
        attr: attrs(attr),
        meta: Attrs::new(),
        expected_version: None,
    };
    let r = ns.commit(&[upsert_edge(Some("KNOWS"), &[("w", Value::Int(2))])]).unwrap();
    assert_eq!((r.edge_ids, r.versions), (vec![EdgeId(0)], vec![(Target::Edge(EdgeId(0)), 2)]));
    let r = ns.commit(&[upsert_edge(None, &[])]).unwrap();
    assert_eq!(r.edge_ids, vec![EdgeId(1)]);
    // No match: added
    let r = ns.commit(&[upsert_edge(Some("LIKES"), &[])]).unwrap();
    assert_eq!((r.edge_ids, r.versions), (vec![EdgeId(2)], vec![(Target::Edge(EdgeId(2)), 1)]));
    let r = ns
        .commit(&[
            Mutation::SetEdgeType { id: EdgeId(2), ty: Some("KNOWS".into()), expected_version: Some(1) },
            set(Target::Edge(EdgeId(2)), "w", Value::Float(0.5)),
        ])
        .unwrap();
    assert_eq!(r.versions, vec![(Target::Edge(EdgeId(2)), 2)]);

    // Two KNOWS edges now
    rejects(
        &mut ns,
        &[upsert_edge(Some("KNOWS"), &[])],
        Error::AmbiguousEdge { from: "a".into(), to: "b".into(), ty: Some("KNOWS".into()), count: 2 },
    );
    let mut stale = upsert_edge(Some("HATES"), &[]);
    if let Mutation::UpsertEdge { expected_version, .. } = &mut stale {
        *expected_version = Some(3);
    }
    rejects(
        &mut ns,
        &[stale],
        Error::NoMatchingEdge { from: "a".into(), to: "b".into(), ty: Some("HATES".into()), expected: 3 },
    );
    rejects(&mut ns, &[edge("a", "zz", None)], Error::NotFound { target: node("zz") });

    // Deleting a node deletes its edges, within the transaction too
    rejects(
        &mut ns,
        &[delete("b"), set(Target::Edge(EdgeId(0)), "w", Value::Int(1))],
        Error::NotFound { target: Target::Edge(EdgeId(0)) },
    );
    // The second upsert updates the edge the first one added (id 3). Adding
    // an edge doesn't write its endpoints
    let r = ns
        .commit(&[
            delete("b"),
            upsert("b", &[], &[]),
            upsert_edge(Some("LIKES"), &[]),
            upsert_edge(Some("LIKES"), &[("again", Value::Bool(true))]),
        ])
        .unwrap();
    assert_eq!(r.edge_ids, vec![EdgeId(3), EdgeId(3)]);
    assert_eq!(r.versions, vec![(node("b"), 2), (Target::Edge(EdgeId(3)), 1)]);
    assert_eq!(ns.graph().edge_count(), 1);
    rejects(
        &mut ns,
        &[Mutation::DeleteEdge { id: EdgeId(0), expected_version: None }],
        Error::NotFound { target: Target::Edge(EdgeId(0)) },
    );
}

#[test]
fn a_failing_transaction_changes_nothing() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &["L"], &[("x", Value::Int(1))]), upsert("b", &[], &[])]).unwrap();
    ns.commit(&[edge("a", "b", None)]).unwrap();
    // Several good mutations, then a bad one
    rejects(
        &mut ns,
        &[
            upsert("c", &["L"], &[]),
            edge("a", "c", Some("T")),
            set(node("a"), "x", Value::Int(2)),
            delete("b"),
            remove_label("a", "L"),
            Mutation::SetEdgeType { id: EdgeId(0), ty: Some("T".into()), expected_version: None },
        ],
        Error::NotFound { target: Target::Edge(EdgeId(0)) },
    );
    rejects(&mut ns, &[], Error::EmptyTransaction);
    // The next edge gets the id the failed commit would have used
    assert_eq!(ns.commit(&[edge("b", "a", None)]).unwrap().edge_ids, vec![EdgeId(1)]);
}

// Constraints

#[test]
fn unique_constraints_see_the_state_after_the_transaction() {
    let mut ns = ns();
    let email = unique("Person", &["email"]);
    ns.commit_catalog(CatalogChange::AddConstraint(email.clone())).unwrap();
    assert!(ns.graph().has_index(&["email".to_owned()]), "unique constraints are backed by an index");

    ns.commit(&[upsert("a", &["Person"], &[("email", Value::from("x"))])]).unwrap();
    rejects(&mut ns, &[upsert("b", &["Person"], &[("email", Value::from("x"))])], violation(&email, "b", Some("a")));
    // Within one transaction
    rejects(
        &mut ns,
        &[
            upsert("c", &["Person"], &[("email", Value::from("y"))]),
            upsert("d", &["Person"], &[("email", Value::from("y"))]),
        ],
        violation(&email, "c", Some("d")),
    );
    // Numbers compare across int and float
    ns.commit(&[upsert("n1", &["Person"], &[("email", Value::Int(1))])]).unwrap();
    rejects(
        &mut ns,
        &[upsert("n2", &["Person"], &[("email", Value::Float(1.0))])],
        violation(&email, "n2", Some("n1")),
    );
    ns.commit(&[upsert("n2", &["Person"], &[("email", Value::Float(1.5))])]).unwrap();
    // Missing and none values never conflict; nor do nodes without the label
    ns.commit(&[
        upsert("m1", &["Person"], &[]),
        upsert("m2", &["Person"], &[]),
        upsert("m3", &["Person"], &[("email", Value::None)]),
        upsert("m4", &["Person"], &[("email", Value::None)]),
        upsert("other", &["Robot"], &[("email", Value::from("x"))]),
    ])
    .unwrap();
    // Adding the label makes it count
    rejects(&mut ns, &[add_label("other", "Person")], violation(&email, "other", Some("a")));

    // Swapping two values in one transaction is fine
    ns.commit(&[upsert("b", &["Person"], &[("email", Value::from("y"))])]).unwrap();
    ns.commit(&[set(node("a"), "email", Value::from("y")), set(node("b"), "email", Value::from("x"))]).unwrap();
    // So is taking over the value of a node deleted, or relabeled, in the same transaction
    ns.commit(&[delete("a"), upsert("c", &["Person"], &[("email", Value::from("y"))])]).unwrap();
    ns.commit(&[
        remove_label("c", "Person"),
        add_label("other", "Person"),
        set(node("other"), "email", Value::from("y")),
    ])
    .unwrap();
    // A node deleted and created again keeps conflicting with others
    rejects(
        &mut ns,
        &[delete("b"), upsert("b", &["Person"], &[("email", Value::from("y"))])],
        violation(&email, "b", Some("other")),
    );
}

#[test]
fn required_constraints_hold_for_new_labels_and_removed_attributes() {
    let mut ns = ns();
    let name = required("Person", &["name"]);
    let city = required("Person", &["address", "city"]);
    ns.commit_catalog(CatalogChange::AddConstraint(name.clone())).unwrap();
    ns.commit_catalog(CatalogChange::AddConstraint(city.clone())).unwrap();
    let address = Value::Dict(attrs(&[("city", Value::from("Berlin"))]));

    rejects(&mut ns, &[upsert("a", &["Person"], &[("address", address.clone())])], violation(&name, "a", None));
    rejects(
        &mut ns,
        &[upsert("a", &["Person"], &[("name", Value::None), ("address", address.clone())])],
        violation(&name, "a", None),
    );
    rejects(&mut ns, &[upsert("a", &["Person"], &[("name", Value::from("x"))])], violation(&city, "a", None));
    ns.commit(&[upsert("a", &["Person"], &[("name", Value::from("x")), ("address", address.clone())])]).unwrap();
    rejects(&mut ns, &[remove(node("a"), "name")], violation(&name, "a", None));
    rejects(&mut ns, &[set(node("a"), "address", Value::from("Berlin"))], violation(&city, "a", None));
    // Removing an attribute and setting it again in one commit is fine
    ns.commit(&[remove(node("a"), "name"), set(node("a"), "name", Value::from("y"))]).unwrap();

    ns.commit(&[upsert("b", &[], &[])]).unwrap();
    // Constraints are checked in catalog order
    rejects(&mut ns, &[add_label("b", "Person")], violation(&city, "b", None));
    ns.commit(&[
        add_label("b", "Person"),
        set(node("b"), "name", Value::from("b")),
        set(node("b"), "address", address.clone()),
    ])
    .unwrap();
    // Dropping the label lifts the constraint
    ns.commit(&[remove_label("b", "Person"), remove(node("b"), "name")]).unwrap();
}

// Catalog changes

#[test]
fn catalog_changes_are_validated_against_the_data() {
    let mut ns = ns();
    ns.commit(&[
        upsert("a", &["Person"], &[("email", Value::from("x"))]),
        upsert("b", &["Person"], &[("email", Value::from("x"))]),
        upsert("c", &["Person"], &[]),
    ])
    .unwrap();
    let email = unique("Person", &["email"]);
    let before = (canonical(ns.graph()), ns.catalog().clone(), ns.seq());
    assert_eq!(ns.commit_catalog(CatalogChange::AddConstraint(email.clone())), Err(violation(&email, "b", Some("a"))));
    let name = required("Person", &["name"]);
    assert_eq!(ns.commit_catalog(CatalogChange::AddConstraint(name.clone())), Err(violation(&name, "a", None)));
    assert_eq!((canonical(ns.graph()), ns.catalog().clone(), ns.seq()), before);
    assert!(ns.graph().index_paths().is_empty());

    // Without the duplicate, it works; a unique constraint on other labels is independent
    ns.commit(&[set(node("b"), "email", Value::from("y"))]).unwrap();
    let r = ns.commit_catalog(CatalogChange::AddConstraint(email.clone())).unwrap();
    assert_eq!((r.seq, r.edge_ids.len(), r.versions.len()), (3, 0, 0));
    ns.commit_catalog(CatalogChange::AddConstraint(unique("Robot", &["email"]))).unwrap();

    let err = |ns: &Namespace, change: CatalogChange| ns.prepare_catalog(change).unwrap_err();
    assert_eq!(
        err(&ns, CatalogChange::AddConstraint(email.clone())),
        Error::ConstraintExists { constraint: email.clone() }
    );
    assert_eq!(
        err(&ns, CatalogChange::DropConstraint(name.clone())),
        Error::NoSuchConstraint { constraint: name.clone() }
    );
    let x = IndexDef { path: path(&["x"]) };
    assert_eq!(err(&ns, CatalogChange::DropIndex(x.clone())), Error::NoSuchIndex { path: path(&["x"]) });
    let labels = path(&["labels"]);
    assert_eq!(
        err(&ns, CatalogChange::CreateIndex(IndexDef { path: labels.clone() })),
        Error::UnindexablePath { path: labels.clone() }
    );
    assert_eq!(
        err(&ns, CatalogChange::AddConstraint(unique("Person", &["labels"]))),
        Error::UnindexablePath { path: labels.clone() }
    );
    // A required constraint on an attribute named "labels" needs no index
    ns.commit_catalog(CatalogChange::AddConstraint(required("Thing", &["labels"]))).unwrap();

    // The graph's indexes follow the catalog: declared indexes plus unique paths
    let paths = |ns: &Namespace| {
        let mut p: Vec<String> = ns.graph().index_paths().iter().map(|p| p.join(".")).collect();
        p.sort();
        p
    };
    assert_eq!(paths(&ns), ["email"]);
    ns.commit_catalog(CatalogChange::CreateIndex(x.clone())).unwrap();
    assert_eq!(err(&ns, CatalogChange::CreateIndex(x.clone())), Error::IndexExists { path: path(&["x"]) });
    let email_index = IndexDef { path: path(&["email"]) };
    ns.commit_catalog(CatalogChange::CreateIndex(email_index.clone())).unwrap();
    assert_eq!(paths(&ns), ["email", "x"]);
    ns.commit_catalog(CatalogChange::DropIndex(email_index)).unwrap();
    assert_eq!(paths(&ns), ["email", "x"], "the unique constraints still need it");
    ns.commit_catalog(CatalogChange::DropConstraint(email)).unwrap();
    assert_eq!(paths(&ns), ["email", "x"], "Robot's unique constraint still needs it");
    ns.commit_catalog(CatalogChange::DropConstraint(unique("Robot", &["email"]))).unwrap();
    assert_eq!(paths(&ns), ["x"]);
    assert!(!ns.graph().indexes_dirty());
    assert_eq!(ns.seq(), 10);
}

// Reserved names and limits

#[test]
fn reserved_keys_are_rejected_in_every_mutation() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &[], &[]), upsert("b", &[], &[]), edge("a", "b", None)]).unwrap();
    let reserved = |key: &str| Error::ReservedName { key: key.into() };
    let meta = attrs(&[("ok", Value::Int(1)), ("iwdb.version", Value::Int(9))]);
    let bad_attr = attrs(&[("iwdb.x", Value::Int(1))]);

    let cases = [
        (
            Mutation::UpsertNode {
                id: "a".into(),
                labels: vec![],
                attr: Attrs::new(),
                meta: meta.clone(),
                expected_version: None,
            },
            "iwdb.version",
        ),
        (
            Mutation::UpsertNode {
                id: "a".into(),
                labels: vec![],
                attr: bad_attr.clone(),
                meta: Attrs::new(),
                expected_version: None,
            },
            "iwdb.x",
        ),
        (
            Mutation::AddEdge { from: "a".into(), to: "b".into(), ty: None, attr: Attrs::new(), meta: meta.clone() },
            "iwdb.version",
        ),
        (
            Mutation::AddEdge {
                from: "a".into(),
                to: "b".into(),
                ty: None,
                attr: bad_attr.clone(),
                meta: Attrs::new(),
            },
            "iwdb.x",
        ),
        (
            Mutation::UpsertEdge {
                key: EdgeKey::Id(EdgeId(0)),
                attr: Attrs::new(),
                meta: meta.clone(),
                expected_version: None,
            },
            "iwdb.version",
        ),
        (
            Mutation::UpsertEdge {
                key: EdgeKey::Endpoints { from: "a".into(), to: "b".into(), ty: Some("NEW".into()) },
                attr: Attrs::new(),
                meta,
                expected_version: None,
            },
            "iwdb.version",
        ),
        (set(node("a"), VERSION_KEY, Value::Int(100)), VERSION_KEY),
        (set(Target::Edge(EdgeId(0)), "iwdb.x", Value::Int(1)), "iwdb.x"),
        (remove(node("a"), VERSION_KEY), VERSION_KEY),
        (append(node("a"), "iwdb.list", Value::Int(1)), "iwdb.list"),
    ];
    for (mutation, key) in cases {
        rejects(&mut ns, &[mutation], reserved(key));
    }
    // Nested keys and labels are the user's
    let nested = Value::Dict(attrs(&[("iwdb.version", Value::Int(1))]));
    ns.commit(&[set(node("a"), "d", nested), add_label("a", "iwdb.label")]).unwrap();
    assert_eq!(version_of(&ns, &node("a")), Some(2));
}

/// A namespace holding node "a" and edge 0 at `version`, through replay
/// (versions near the limit can't be reached by commits in a test).
fn at_version(version: u64) -> Namespace {
    let mut ns = ns();
    let rec = DbRecord { version, ..DbRecord::default() };
    let ops = vec![
        Op::AddNode { id: "a".into(), labels: vec![], data: rec.clone() },
        Op::AddEdge { id: EdgeId(0), from: "a".into(), to: "a".into(), ty: None, data: rec },
    ];
    ns.replay(CommitRecord::new(1, Change::Data(ops)), None).unwrap();
    ns
}

#[test]
fn versions_never_wrap() {
    let max = i64::MAX as u64;
    let mut ns = at_version(max - 1);
    let r = ns.commit(&[set(node("a"), "x", Value::Int(1)), set(Target::Edge(EdgeId(0)), "x", Value::Int(1))]).unwrap();
    assert_eq!(r.versions, vec![(node("a"), max), (Target::Edge(EdgeId(0)), max)]);
    rejects(&mut ns, &[set(node("a"), "x", Value::Int(2))], Error::VersionOverflow { target: node("a") });
    rejects(&mut ns, &[add_label("a", "L")], Error::VersionOverflow { target: node("a") });
    rejects(&mut ns, &[upsert("a", &[], &[])], Error::VersionOverflow { target: node("a") });
    let edge0 = Target::Edge(EdgeId(0));
    rejects(&mut ns, &[set(edge0.clone(), "x", Value::Int(2))], Error::VersionOverflow { target: edge0.clone() });
    rejects(
        &mut ns,
        &[Mutation::SetEdgeType { id: EdgeId(0), ty: None, expected_version: None }],
        Error::VersionOverflow { target: edge0 },
    );
    // The maximum still saves (versions are Ints in files)
    let meta = ns.graph_meta();
    let loaded = codec::from_binary(&codec::to_binary(ns.graph(), &meta).unwrap()).unwrap();
    assert_eq!(canonical(&loaded.graph), canonical(ns.graph()));
    // Deleting needs no new version; a node created again starts at 1
    ns.commit(&[delete("a"), upsert("b", &[], &[])]).unwrap();
    assert_eq!(ns.commit(&[upsert("a", &[], &[])]).unwrap().versions, vec![(node("a"), 1)]);

    // Beyond what a file can hold: rejected too, never wrapped
    let mut ns = at_version(u64::MAX);
    rejects(&mut ns, &[set(node("a"), "x", Value::Int(1))], Error::VersionOverflow { target: node("a") });
}

fn nest(levels: usize, inner: Value) -> Value {
    (0..levels).fold(inner, |v, _| Value::List(vec![v]))
}

#[test]
fn values_nested_too_deep_for_the_log_are_rejected() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &[], &[])]).unwrap();
    let too_deep = |key: &str| Error::ValueTooDeep { target: node("a"), key: key.into() };
    // Depth 100 (a scalar in 99 lists) is the limit
    let prepared = ns.prepare(&[set(node("a"), "ok", nest(99, Value::Int(1)))]).unwrap();
    let bytes = postcard::to_allocvec(prepared.record()).unwrap();
    assert_eq!(&postcard::from_bytes::<CommitRecord>(&bytes).unwrap(), prepared.record());
    ns.apply(prepared, None).unwrap();

    rejects(&mut ns, &[set(node("a"), "deep", nest(100, Value::Int(1)))], too_deep("deep"));
    // An empty list at depth 100 is fine, and the log encodes it; at 101
    // it is too deep
    let prepared = ns.prepare(&[set(node("a"), "empty", nest(99, Value::List(vec![])))]).unwrap();
    let bytes = postcard::to_allocvec(prepared.record()).unwrap();
    assert_eq!(&postcard::from_bytes::<CommitRecord>(&bytes).unwrap(), prepared.record());
    ns.apply(prepared, None).unwrap();
    rejects(&mut ns, &[set(node("a"), "deep", nest(100, Value::List(vec![])))], too_deep("deep"));
    rejects(&mut ns, &[upsert("a", &[], &[("deep", nest(100, Value::Int(1)))])], too_deep("deep"));
    let meta = attrs(&[("m", nest(100, Value::Int(1)))]);
    rejects(
        &mut ns,
        &[Mutation::UpsertNode { id: "a".into(), labels: vec![], attr: Attrs::new(), meta, expected_version: None }],
        too_deep("m"),
    );
    // Appending nests the value one level deeper
    ns.commit(&[append(node("a"), "list", nest(98, Value::Int(1)))]).unwrap();
    rejects(&mut ns, &[append(node("a"), "other", nest(99, Value::Int(1)))], too_deep("other"));
}

#[test]
fn append_builds_lists() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &[], &[("none", Value::None), ("s", Value::from("x"))])]).unwrap();
    ns.commit(&[
        append(node("a"), "new", Value::Int(1)),
        append(node("a"), "new", Value::Int(2)),
        append(node("a"), "none", Value::from("v")),
    ])
    .unwrap();
    let data = &ns.graph().node_by_id("a").unwrap().data;
    assert_eq!(data.attr["new"], Value::List(vec![Value::Int(1), Value::Int(2)]));
    assert_eq!(data.attr["none"], Value::List(vec![Value::from("v")]));
    assert_eq!(data.version, 2);
    rejects(&mut ns, &[append(node("a"), "s", Value::Int(1))], Error::NotAList { target: node("a"), key: "s".into() });
    rejects(&mut ns, &[append(node("zz"), "s", Value::Int(1))], Error::NotFound { target: node("zz") });
}

// The commit record

#[test]
fn records_hold_attribute_ops_and_version_ops() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &["L"], &[("x", Value::Int(1))]), upsert("b", &[], &[]), edge("a", "b", None)]).unwrap();
    let prepared = ns
        .prepare(&[
            set(node("a"), "x", Value::Int(2)),
            append(node("b"), "l", Value::Int(1)),
            set(Target::Edge(EdgeId(0)), "w", Value::Float(0.5)),
            upsert("c", &["L", "L"], &[]),
        ])
        .unwrap();
    let version = |v: i64| Some(Value::Int(v));
    let expected: Vec<Op<DbRecord, DbRecord>> = vec![
        Op::SetNodeAttr { id: "a".into(), key: "x".into(), value: Some(Value::Int(2)) },
        Op::SetNodeAttr { id: "b".into(), key: "l".into(), value: Some(Value::List(vec![Value::Int(1)])) },
        Op::SetEdgeAttr { id: EdgeId(0), key: "w".into(), value: Some(Value::Float(0.5)) },
        Op::AddNode { id: "c".into(), labels: vec!["L".into()], data: DbRecord { version: 1, ..DbRecord::default() } },
        // Nodes by id, then edges by id; "c" has its version in its record
        Op::SetNodeAttr { id: "a".into(), key: VERSION_KEY.into(), value: version(2) },
        Op::SetNodeAttr { id: "b".into(), key: VERSION_KEY.into(), value: version(2) },
        Op::SetEdgeAttr { id: EdgeId(0), key: VERSION_KEY.into(), value: version(2) },
    ];
    assert_eq!(prepared.record(), &CommitRecord::new(2, Change::Data(expected)));

    // Whole-record upserts carry the version themselves
    ns.apply(prepared, None).unwrap();
    let prepared = ns.prepare(&[upsert("a", &["M"], &[])]).unwrap();
    let expected: Vec<Op<DbRecord, DbRecord>> = vec![
        Op::SetNode { id: "a".into(), data: DbRecord { version: 3, ..DbRecord::default() } },
        Op::AddLabel { id: "a".into(), label: "M".into() },
    ];
    assert_eq!(prepared.record().change, Change::Data(expected));
}

#[test]
fn new_edge_ids_come_from_the_graphs_counter() {
    // A replayed history that added and removed edge 100 leaves the counter at 101
    let mut ns = ns();
    let ops = vec![
        Op::AddNode { id: "a".into(), labels: vec![], data: DbRecord { version: 1, ..DbRecord::default() } },
        Op::AddEdge { id: EdgeId(100), from: "a".into(), to: "a".into(), ty: None, data: DbRecord::default() },
        Op::RemoveEdge { id: EdgeId(100) },
    ];
    ns.replay(CommitRecord::new(1, Change::Data(ops)), None).unwrap();
    assert_eq!(
        ns.commit(&[edge("a", "a", None), edge("a", "a", None)]).unwrap().edge_ids,
        vec![EdgeId(101), EdgeId(102)]
    );
}

#[test]
fn replay_rejects_gaps_and_repeats() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &[], &[])]).unwrap();
    let record = |seq| CommitRecord::new(seq, Change::Data(vec![]));
    assert_eq!(ns.replay(record(1), None), Err(Error::OutOfOrder { expected: 2, found: 1 }));
    assert_eq!(ns.replay(record(3), None), Err(Error::OutOfOrder { expected: 2, found: 3 }));
    assert!(!ns.is_poisoned());
    ns.replay(record(2), None).unwrap();
    assert_eq!(ns.seq(), 2);
}

// Saved files

#[test]
fn a_namespace_from_a_loaded_file_continues_where_the_original_was() {
    let mut original = ns();
    original.commit_catalog(CatalogChange::AddConstraint(unique("P", &["k"]))).unwrap();
    original.commit(&[upsert("a", &["P"], &[("k", Value::Int(1))]), edge("a", "a", None)]).unwrap();
    let bytes = codec::to_binary(original.graph(), &original.graph_meta()).unwrap();
    let mut loaded = Namespace::from_loaded(codec::from_binary_reader(&bytes[..]).unwrap());
    assert_eq!(loaded.seq(), 2);
    assert_eq!(loaded.name(), original.name());
    assert_eq!(loaded.catalog(), original.catalog());
    assert_eq!(canonical(loaded.graph()), canonical(original.graph()));

    // Same results, including edge ids, versions and the constraint
    let next = [upsert("b", &["P"], &[("k", Value::Int(2))]), edge("a", "b", Some("T"))];
    assert_eq!(loaded.commit(&next).unwrap(), original.commit(&next).unwrap());
    let clash = [upsert("c", &["P"], &[("k", Value::Int(2))])];
    assert_eq!(loaded.commit(&clash), original.commit(&clash));
    assert!(loaded.commit(&clash).is_err());
    assert_eq!(canonical(loaded.graph()), canonical(original.graph()));
}

#[test]
fn files_with_reserved_attribute_keys_are_rejected() {
    // Written by another tool as a plain Record graph
    let mut g: Graph<Record, Record> = Graph::new();
    let mut record = Record::with_attr([("iwdb.version", Value::Int(1))]);
    record.meta.insert(VERSION_KEY.into(), Value::Int(1));
    g.add_node("a", record).unwrap();
    let meta = GraphMeta {
        namespace: NamespaceName::new("n").unwrap(),
        catalog: Default::default(),
        seq: 0,
        keys: Default::default(),
    }
    .to_attrs();
    let bytes = format::to_binary(&g, &meta, false).unwrap();
    assert_eq!(
        codec::from_binary(&bytes).unwrap_err(),
        Error::UnknownReservedKey { entity: Entity::Node("a".into()), key: "iwdb.version".into() }
    );

    // And saving one fails too
    let mut g = DbGraph::new();
    g.add_node("a", DbRecord::with_attr([("iwdb.x", Value::Int(1))])).unwrap();
    let meta = GraphMeta {
        namespace: NamespaceName::new("n").unwrap(),
        catalog: Default::default(),
        seq: 0,
        keys: Default::default(),
    };
    let err = codec::to_binary(&g, &meta).unwrap_err().to_string();
    assert!(err.contains("'iwdb.x' is reserved"), "{}", err);
}

/// An online index build (ADR 0019) with commits between its scan chunks:
/// changed, added and deleted nodes, a node changed after it was scanned,
/// and a node added after the handles were listed. After the install and
/// the flush the index equals a scan (the invariants), and lookups agree.
#[test]
fn an_online_index_build_sees_commits_made_during_the_scan() {
    let mut ns = ns();
    let n = |i: i64| upsert(&format!("n{}", i), &[], &[("v", Value::Int(i))]);
    ns.commit(&(0..100).map(n).collect::<Vec<_>>()).unwrap();
    let path = AttrPath::new(["v"]).unwrap();
    let change = CatalogChange::CreateIndex(IndexDef { path: path.clone() });
    let mut build = ns.begin_index_build(path.clone()).unwrap();
    // A node added before the handles are listed
    ns.commit(&[n(100)]).unwrap();
    let handles = ns.node_handles();
    let (first, rest) = handles.split_at(50);
    ns.scan_index_keys(first, &mut build).unwrap();
    ns.commit(&[
        set(node("n1"), "v", Value::Int(1001)),  // scanned, then changed
        set(node("n60"), "v", Value::Int(1060)), // changed, then scanned
        remove(node("n2"), "v"),
        delete("n3"),
        delete("n70"),
        n(101), // after the listing
    ])
    .unwrap();
    ns.scan_index_keys(rest, &mut build).unwrap();
    let prepared = ns.prepare_catalog(change).unwrap();
    ns.apply_built(prepared, None, Some(build)).unwrap();

    assert_eq!(iwdb_engine::invariants::check(&ns), Vec::<String>::new());
    let find = |v: i64| {
        let g = ns.graph();
        let mut ids: Vec<String> = g
            .find_nodes(path.keys(), &Value::Int(v))
            .unwrap()
            .expect("indexed")
            .into_iter()
            .map(|ix| g.node(ix).unwrap().id().to_owned())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(find(1001), ["n1"]);
    assert_eq!(find(1060), ["n60"]);
    assert!(find(1).is_empty() && find(2).is_empty() && find(3).is_empty() && find(70).is_empty());
    assert_eq!(find(100), ["n100"]);
    assert_eq!(find(101), ["n101"]);
    assert_eq!(find(99), ["n99"]);
    assert_eq!(ns.graph().index_stats(path.keys()).unwrap().entries, 99);
}

/// A build whose commit doesn't come (an error, a dropped namespace) is
/// dropped: the graph is unchanged and a later build works.
#[test]
fn an_abandoned_index_build_leaves_nothing_behind() {
    let mut ns = ns();
    ns.commit(&[upsert("a", &[], &[("v", Value::Int(1))])]).unwrap();
    let path = AttrPath::new(["v"]).unwrap();
    let build = ns.begin_index_build(path.clone()).unwrap();
    drop(build);
    ns.commit(&[upsert("b", &[], &[("v", Value::Int(2))])]).unwrap();
    assert!(!ns.graph().has_index(path.keys()));
    assert_eq!(ns.graph().open_index_builds(), 0);
    let mut build = ns.begin_index_build(path.clone()).unwrap();
    let handles = ns.node_handles();
    ns.scan_index_keys(&handles, &mut build).unwrap();
    let prepared = ns.prepare_catalog(CatalogChange::CreateIndex(IndexDef { path: path.clone() })).unwrap();
    ns.apply_built(prepared, None, Some(build)).unwrap();
    assert_eq!(iwdb_engine::invariants::check(&ns), Vec::<String>::new());
    assert_eq!(ns.graph().index_stats(path.keys()).unwrap().entries, 2);
}
