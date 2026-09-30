//! [`DbRecord`], the payload of the database's graphs.

use ironweaver_core::{AttrPatch, Attributes, Attrs, GraphError, Lookup, Value};
use serde::{Deserialize, Deserializer, Serialize};

/// Node and edge payload of the database: user attributes, user metadata
/// and the entity's version.
///
/// - `attr` and `meta` behave as in the core's `Record`: algorithms,
///   filters and indexes read `attr` through [`Attributes`], with the same
///   path rules; ops change `attr` through [`AttrPatch`].
/// - `meta` never contains reserved keys (`iwdb.*`, see
///   [`reserved`](crate::reserved)): the commit pipeline checks user input,
///   and saving or deserializing a record with one fails.
/// - `version` is the entity's version for optimistic concurrency. The
///   commit pipeline (step 3) sets it; saved files store it under the
///   `iwdb.version` meta key. Only values up to `i64::MAX` can be saved.
///
/// Serde (used for WAL records) writes `attr` and `meta` sorted by key, so
/// equal records encode to equal bytes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DbRecord {
    #[serde(serialize_with = "ironweaver_core::value::serialize_sorted")]
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
    // `tests/db_record.rs` checks the equivalence. Possible upstream request:
    // a public `record::lookup(&Attrs, path)`.
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

/// Sets and removes user attributes. Leaves `meta` and `version` alone:
/// version bumps are the commit pipeline's job (step 3).
impl AttrPatch for DbRecord {
    fn set_attr(&mut self, key: &str, value: Option<Value>) -> Option<Value> {
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
    fn serde_round_trips_and_rejects_reserved_meta() {
        let mut r = DbRecord::with_attr([("name", Value::from("x"))]);
        r.meta.insert("source".into(), Value::Int(1));
        r.version = u64::MAX;
        let json = serde_json::to_string(&r).expect("encode");
        assert_eq!(serde_json::from_str::<DbRecord>(&json).expect("decode"), r);

        let bad = r#"{"attr": {}, "meta": {"iwdb.version": {"Int": 1}}, "version": 1}"#;
        let err = serde_json::from_str::<DbRecord>(bad).expect_err("reserved key");
        assert!(err.to_string().contains("'iwdb.version' is reserved"), "{}", err);
    }
}
