//! Catalog input validation shared by the CLI adapters. Callers keep their
//! own error wrappers and exit-code policy.

use iwdb::{AttrPath, CatalogError, Constraint, ConstraintKind, Label};

pub(crate) fn attr_path(path: &[String]) -> Result<AttrPath, CatalogError> {
    AttrPath::new(path.iter().cloned())
}

pub(crate) fn constraint(kind: ConstraintKind, label: &str, path: &[String]) -> Result<Constraint, CatalogError> {
    Ok(Constraint { kind, label: Label::new(label)?, path: attr_path(path)? })
}

pub(crate) fn constraint_kind(word: &str) -> Result<ConstraintKind, String> {
    match word {
        "unique" => Ok(ConstraintKind::Unique),
        "required" => Ok(ConstraintKind::Required),
        other => Err(format!("a constraint is 'unique' or 'required', not '{}'", other)),
    }
}
