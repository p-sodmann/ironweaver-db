//! The change stream (ADR 0031) as Python dicts.
//!
//! A batch is `{"events": [...], "next_seq": int, "first_seq": int, "seq":
//! int}`. An event is `{"seq": int, "time": datetime | None, "key": str |
//! None}` plus `"ops"` (a data commit) or `"catalog"` (a catalog change).
//! An op is a dict with its name in `"op"`: `add_node`, `remove_node`,
//! `rename_node`, `add_label`, `remove_label`, `set_node`,
//! `set_node_attr`, `remove_node_attr`, `set_node_version`, and the same
//! for edges (`add_edge`, `remove_edge`, `set_edge_type`, `set_edge`,
//! `set_edge_attr`, `remove_edge_attr`, `set_edge_version`). Attribute
//! removal is an op of its own because `None` is a value.

use ironweaver_core::{Op, Value};
use iwdb::{CatalogChange, ConstraintKind};
use iwdb_engine::reserved::VERSION_KEY;
use iwdb_engine::{Change, DbRecord};
use iwdb_query::{Answer, ChangeEvent, Changes};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::convert::{from_attrs, from_value};
use crate::reports;

fn op_dict<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("op", name)?;
    Ok(dict)
}

fn set_record(py: Python<'_>, dict: &Bound<'_, PyDict>, record: &DbRecord) -> PyResult<()> {
    dict.set_item("attr", from_attrs(py, &record.attr)?)?;
    dict.set_item("meta", from_attrs(py, &record.meta)?)?;
    dict.set_item("version", record.version)
}

/// An attribute op of node or edge `id`: set, remove, or (on
/// `iwdb.version`, ADR 0004) set the version.
fn attr_op<'py, I: IntoPyObject<'py>>(
    py: Python<'py>,
    kind: &str,
    id: I,
    key: &str,
    value: &Option<Value>,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = match (key, value) {
        (VERSION_KEY, Some(Value::Int(v))) => {
            let dict = op_dict(py, &format!("set_{}_version", kind))?;
            dict.set_item("version", *v as u64)?;
            dict
        }
        (_, Some(value)) => {
            let dict = op_dict(py, &format!("set_{}_attr", kind))?;
            dict.set_item("key", key)?;
            dict.set_item("value", from_value(py, value)?)?;
            dict
        }
        (_, None) => {
            let dict = op_dict(py, &format!("remove_{}_attr", kind))?;
            dict.set_item("key", key)?;
            dict
        }
    };
    dict.set_item("id", id)?;
    Ok(dict)
}

fn op<'py>(py: Python<'py>, op: &Op<DbRecord, DbRecord>) -> PyResult<Bound<'py, PyDict>> {
    let dict = match op {
        Op::AddNode { id, labels, data } => {
            let dict = op_dict(py, "add_node")?;
            dict.set_item("id", id)?;
            dict.set_item("labels", PyList::new(py, labels)?)?;
            set_record(py, &dict, data)?;
            dict
        }
        Op::RemoveNode { id } => {
            let dict = op_dict(py, "remove_node")?;
            dict.set_item("id", id)?;
            dict
        }
        Op::RenameNode { id, new_id } => {
            let dict = op_dict(py, "rename_node")?;
            dict.set_item("id", id)?;
            dict.set_item("new_id", new_id)?;
            dict
        }
        Op::AddLabel { id, label } | Op::RemoveLabel { id, label } => {
            let name = if matches!(op, Op::AddLabel { .. }) { "add_label" } else { "remove_label" };
            let dict = op_dict(py, name)?;
            dict.set_item("id", id)?;
            dict.set_item("label", label)?;
            dict
        }
        Op::SetNode { id, data } => {
            let dict = op_dict(py, "set_node")?;
            dict.set_item("id", id)?;
            set_record(py, &dict, data)?;
            dict
        }
        Op::SetNodeAttr { id, key, value } => attr_op(py, "node", id, key, value)?,
        Op::AddEdge { id, from, to, ty, data } => {
            let dict = op_dict(py, "add_edge")?;
            dict.set_item("id", id.0)?;
            dict.set_item("from", from)?;
            dict.set_item("to", to)?;
            dict.set_item("type", ty)?;
            set_record(py, &dict, data)?;
            dict
        }
        Op::RemoveEdge { id } => {
            let dict = op_dict(py, "remove_edge")?;
            dict.set_item("id", id.0)?;
            dict
        }
        Op::SetEdgeType { id, ty } => {
            let dict = op_dict(py, "set_edge_type")?;
            dict.set_item("id", id.0)?;
            dict.set_item("type", ty)?;
            dict
        }
        Op::SetEdge { id, data } => {
            let dict = op_dict(py, "set_edge")?;
            dict.set_item("id", id.0)?;
            set_record(py, &dict, data)?;
            dict
        }
        Op::SetEdgeAttr { id, key, value } => attr_op(py, "edge", id.0, key, value)?,
    };
    Ok(dict)
}

fn catalog_change<'py>(py: Python<'py>, change: &CatalogChange) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    match change {
        CatalogChange::CreateIndex(index) | CatalogChange::DropIndex(index) => {
            let name = if matches!(change, CatalogChange::CreateIndex(_)) { "create_index" } else { "drop_index" };
            dict.set_item("change", name)?;
            dict.set_item("path", PyList::new(py, index.path.keys())?)?;
        }
        CatalogChange::AddConstraint(c) | CatalogChange::DropConstraint(c) => {
            let name =
                if matches!(change, CatalogChange::AddConstraint(_)) { "add_constraint" } else { "drop_constraint" };
            dict.set_item("change", name)?;
            let kind = match c.kind {
                ConstraintKind::Unique => "unique",
                ConstraintKind::Required => "required",
            };
            dict.set_item("kind", kind)?;
            dict.set_item("label", c.label.as_str())?;
            dict.set_item("path", PyList::new(py, c.path.keys())?)?;
        }
    }
    Ok(dict)
}

fn event<'py>(py: Python<'py>, e: &ChangeEvent) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("seq", e.seq)?;
    dict.set_item("time", reports::commit_time(py, e.time)?)?;
    dict.set_item("key", e.key.as_ref().map(|k| k.as_str()))?;
    match &e.change {
        Change::Data(ops) => {
            let list = PyList::empty(py);
            for o in ops {
                list.append(op(py, o)?)?;
            }
            dict.set_item("ops", list)?;
        }
        Change::Catalog(change) => dict.set_item("catalog", catalog_change(py, change)?)?,
    }
    Ok(dict)
}

/// A batch of the change stream as a dict (see the module docs).
pub fn batch(py: Python<'_>, answer: &Answer<Changes>) -> PyResult<Py<PyAny>> {
    let events = PyList::empty(py);
    for e in &answer.value.events {
        events.append(event(py, e)?)?;
    }
    let dict = PyDict::new(py);
    dict.set_item("events", events)?;
    dict.set_item("next_seq", answer.value.next_seq)?;
    dict.set_item("first_seq", answer.value.first_seq)?;
    dict.set_item("seq", answer.seq)?;
    Ok(dict.into_any().unbind())
}
