//! [`DbRecord`], the payload of the database's graphs.

use ironweaver_core::{AttrPatch, Attributes, Attrs, GraphError, Lookup, Value};
use serde::{Deserialize, Deserializer, Serialize};

use crate::reserved::VERSION_KEY;

/// Node and edge payload of the database: user attributes, user metadata
/// and the entity's version.
///
/// - `attr` and `meta` behave as in the core's `Record`: algorithms,
///   filters and indexes read `attr` through [`Attributes`], with the same
///   path rules; ops change `attr` through [`AttrPatch`].
/// - `meta` never contains reserved keys (`iwdb.*`, see
///   [`reserved`](crate::reserved)), and neither do the top-level keys of
///   `attr`: the commit pipeline checks user input, and saving, loading or
///   deserializing a record with one fails.
/// - `version` is the entity's version for optimistic concurrency. The
///   commit pipeline sets it (see [`Namespace`](crate::Namespace)); saved
///   files store it under the `iwdb.version` meta key. Only values up to
///   `i64::MAX` can be saved.
///
/// Serde (used for WAL records) writes `attr` and `meta` sorted by key, so
/// equal records encode to equal bytes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DbRecord {
    #[serde(serialize_with = "ironweaver_core::value::serialize_sorted", deserialize_with = "user_attr")]
    pub attr: Attrs,
    #[serde(serialize_with = "ironweaver_core::value::serialize_sorted", deserialize_with = "user_meta")]
    pub meta: Attrs,
    pub version: u64,
}

impl DbRecord {
    /// A record with the given attributes, no metadata and version 0.
    pub fn with_attr<K: Into<String>>(attr: impl IntoIterator<Item = (K, Value)>) -> Self {
        DbRecord { attr: attr.into_iter().map(|(k, v)| (k.into(), v)).collect(), ..DbRecord::default() }
    }

    // Copy of the private `Record::at` in ironweaver-core (record.rs,
    // a14149e), so that `DbRecord` answers every `Attributes` method exactly
    // like `Record` for the same attribute map. Keep in sync on core bumps;
    // `tests/db_record.rs` checks the equivalence. Upstream #30 asks for a
    // public `record::lookup(&Attrs, path)`.
    fn at(&self, path: &[String]) -> Option<&Value> {
        let (first, rest) = path.split_first()?;
        let mut value = self.attr.get(first)?;
        for key in rest {
            value = match value {
                Value::Dict(d) => d.get(key)?,
                _ => return None,
            };
        }
        if value.is_none() {
            None
        } else {
            Some(value)
        }
    }
}

/// Deserialize a meta map, rejecting reserved keys.
fn user_meta<'de, D: Deserializer<'de>>(d: D) -> Result<Attrs, D::Error> {
    let meta = Attrs::deserialize(d)?;
    crate::reserved::check_user_meta(&meta).map_err(serde::de::Error::custom)?;
    Ok(meta)
}

/// Deserialize an attribute map, rejecting reserved top-level keys.
fn user_attr<'de, D: Deserializer<'de>>(d: D) -> Result<Attrs, D::Error> {
    let attr = Attrs::deserialize(d)?;
    crate::reserved::check_user_attrs(&attr).map_err(serde::de::Error::custom)?;
    Ok(attr)
}

impl Attributes for DbRecord {
    type Error = GraphError;

    fn with_value<R>(&self, path: &[String], f: impl FnOnce(Option<&Value>) -> R) -> Result<R, GraphError> {
        Ok(f(self.at(path)))
    }

    fn number(&self, path: &[String]) -> Result<Lookup<f64>, GraphError> {
        Ok(match self.at(path) {
            None => Lookup::Missing,
            Some(v) => v.as_f64().map_or(Lookup::Invalid, Lookup::Found),
        })
    }

    fn numbers(&self, path: &[String]) -> Result<Lookup<Vec<f64>>, GraphError> {
        Ok(match self.at(path) {
            None => Lookup::Missing,
            Some(Value::List(items)) => {
                items.iter().map(Value::as_f64).collect::<Option<Vec<f64>>>().map_or(Lookup::Invalid, Lookup::Found)
            }
            Some(_) => Lookup::Invalid,
        })
    }

    fn text(&self, key: &str) -> Result<Lookup<String>, GraphError> {
        Ok(match self.attr.get(key) {
            None | Some(Value::None) => Lookup::Missing,
            Some(Value::String(s)) => Lookup::Found(s.clone()),
            Some(_) => Lookup::Invalid,
        })
    }
}

/// Sets and removes user attributes, and sets the version through the
/// reserved key [`VERSION_KEY`] (ADR 0004).
///
/// - `set_attr(VERSION_KEY, Some(Int(v)))` sets `version` to `v` and
///   returns the old version as `Int`. The conversion is a bit cast both
///   ways, so the returned value undoes the change exactly for every `u64`
///   (the commit pipeline only writes versions up to `i64::MAX`).
/// - `set_attr(VERSION_KEY, ..)` with anything else (`None`, another
///   type) changes nothing and returns `value`, so that its undo op is the
///   same no-op.
/// - Every other key sets or removes an attribute. `meta` never changes.
///
/// User attributes never use `VERSION_KEY`: the commit pipeline rejects
/// top-level attribute keys starting with `iwdb.`.
impl AttrPatch for DbRecord {
    fn set_attr(&mut self, key: &str, value: Option<Value>) -> Option<Value> {
        if key == VERSION_KEY {
            return match value {
                Some(Value::Int(v)) => {
                    let old = std::mem::replace(&mut self.version, v as u64);
                    Some(Value::Int(old as i64))
                }
                other => other,
            };
        }
        match value {
            Some(v) => self.attr.insert(key.to_owned(), v),
            None => self.attr.remove(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_attr_changes_only_attributes() {
        let mut r = DbRecord::with_attr([("a", Value::Int(1))]);
        r.meta.insert("m".into(), Value::Bool(true));
        r.version = 7;
        assert_eq!(r.set_attr("a", Some(Value::Int(2))), Some(Value::Int(1)));
        assert_eq!(r.set_attr("b", Some(Value::Int(3))), None);
        assert_eq!(r.set_attr("a", None), Some(Value::Int(2)));
        assert_eq!(r.attr, [("b".to_owned(), Value::Int(3))].into());
        assert_eq!(r.meta.len(), 1);
        assert_eq!(r.version, 7);
    }

    #[test]
    fn the_version_key_sets_the_version_and_undoes_exactly() {
        let mut r = DbRecord::with_attr([("a", Value::Int(1))]);
        r.version = 7;
        let before = r.clone();
        let undo = r.set_attr(VERSION_KEY, Some(Value::Int(8)));
        assert_eq!(undo, Some(Value::Int(7)));
        assert_eq!(r.version, 8);
        assert_eq!(r.attr, before.attr);
        assert_eq!(r.set_attr(VERSION_KEY, undo), Some(Value::Int(8)));
        assert_eq!(r, before);

        // Exact for every u64, through the bit cast
        r.version = u64::MAX;
        let undo = r.set_attr(VERSION_KEY, Some(Value::Int(1)));
        assert_eq!(undo, Some(Value::Int(-1)));
        r.set_attr(VERSION_KEY, undo);
        assert_eq!(r.version, u64::MAX);

        // Anything else is a no-op whose undo is the same no-op
        let mut r = before.clone();
        for value in [None, Some(Value::from("8")), Some(Value::Float(8.0))] {
            assert_eq!(r.set_attr(VERSION_KEY, value.clone()), value);
            assert_eq!(r, before);
        }
    }

    #[test]
    fn serde_round_trips_and_rejects_reserved_meta() {
        let mut r = DbRecord::with_attr([("name", Value::from("x"))]);
        r.meta.insert("source".into(), Value::Int(1));
        r.version = u64::MAX;
        let json = serde_json::to_string(&r).expect("encode");
        assert_eq!(serde_json::from_str::<DbRecord>(&json).expect("decode"), r);

        let bad = r#"{"attr": {}, "meta": {"iwdb.version": {"Int": 1}}, "version": 1}"#;
        let err = serde_json::from_str::<DbRecord>(bad).expect_err("reserved key");
        assert!(err.to_string().contains("'iwdb.version' is reserved"), "{}", err);

        let bad = r#"{"attr": {"iwdb.x": {"Int": 1}}, "meta": {}, "version": 1}"#;
        let err = serde_json::from_str::<DbRecord>(bad).expect_err("reserved attribute key");
        assert!(err.to_string().contains("'iwdb.x' is reserved"), "{}", err);
    }
}
