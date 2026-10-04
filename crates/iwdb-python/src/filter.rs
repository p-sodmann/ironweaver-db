//! Filters from Python (`iwdb/filter.py`) as the core's `Expr`.
//!
//! A filter is an `iwdb.Filter` whose `_expr` is a tuple: `("const",
//! bool)`, `("compare", path, op, value)`, `("in", path, values)`,
//! `("exists", path)`, `("label", str)`, `("type", str)`, `("and",
//! [filters])`, `("or", [filters])`, `("not", filter)`. Values convert like
//! attribute values. A filter nests at most `MAX_EXPR_DEPTH` levels (And,
//! Or and Not around a leaf), counted like the core counts them, so a filter fails the same way
//! embedded and over the network (where the core's serde refuses deeper
//! ones), and a crafted one can't exhaust the stack.

use ironweaver_core::expr::MAX_EXPR_DEPTH;
use ironweaver_core::{CmpOp, Expr};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::convert::to_value;
use crate::store::attr_path;

fn not_a_filter() -> PyErr {
    PyTypeError::new_err("a filter is built with iwdb.attr, iwdb.label, iwdb.edge_type or iwdb.const")
}

/// The core's `Expr` for the filter `f`.
pub fn to_expr(f: &Bound<'_, PyAny>) -> PyResult<Expr> {
    expr(f, 0)
}

/// An optional filter.
pub fn to_expr_opt(f: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Expr>> {
    f.map(to_expr).transpose()
}

fn path(p: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    Ok(attr_path(p)?.keys().to_vec())
}

/// `f` inside `depth` levels of And, Or and Not.
fn expr(f: &Bound<'_, PyAny>, depth: usize) -> PyResult<Expr> {
    let py = f.py();
    let parts = f.getattr(intern!(py, "_expr")).map_err(|_| not_a_filter())?;
    let parts = parts.cast::<PyTuple>().map_err(|_| not_a_filter())?;
    let kind: String = parts.get_item(0)?.extract()?;
    let arg = |i: usize| parts.get_item(i);
    // The operands of And, Or and Not are one level deeper. Like the core,
    // the leaf counts as a level: at most MAX_EXPR_DEPTH - 1 nested
    // operators (`nest_expr` in iwdb-server's convert tests)
    let nested = || {
        if depth + 1 >= MAX_EXPR_DEPTH {
            Err(PyValueError::new_err(format!(
                "the filter is nested more than {} levels deep (And, Or and Not around a leaf)",
                MAX_EXPR_DEPTH
            )))
        } else {
            Ok(depth + 1)
        }
    };
    Ok(match kind.as_str() {
        "const" => Expr::Const(arg(1)?.extract()?),
        "compare" => {
            let op = match arg(2)?.extract::<String>()?.as_str() {
                "eq" => CmpOp::Eq,
                "ne" => CmpOp::Ne,
                "lt" => CmpOp::Lt,
                "le" => CmpOp::Le,
                "gt" => CmpOp::Gt,
                "ge" => CmpOp::Ge,
                _ => return Err(not_a_filter()),
            };
            Expr::Compare { path: path(&arg(1)?)?, op, value: to_value(&arg(3)?, 1)? }
        }
        "in" => {
            let values = arg(2)?.try_iter()?.map(|v| to_value(&v?, 1)).collect::<PyResult<_>>()?;
            Expr::In { path: path(&arg(1)?)?, values }
        }
        "exists" => Expr::Exists { path: path(&arg(1)?)? },
        "label" => Expr::Label(arg(1)?.extract()?),
        "type" => Expr::Type(arg(1)?.extract()?),
        "and" | "or" => {
            let level = nested()?;
            let operands = arg(1)?.try_iter()?.map(|f| expr(&f?, level)).collect::<PyResult<Vec<_>>>()?;
            if kind == "and" { Expr::And(operands) } else { Expr::Or(operands) }
        }
        "not" => Expr::Not(Box::new(expr(&arg(1)?, nested()?)?)),
        _ => return Err(not_a_filter()),
    })
}
