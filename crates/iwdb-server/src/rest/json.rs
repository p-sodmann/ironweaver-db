//! The JSON form of the three messages that carry the core's types (ADR
//! 0023, ADR 0030): a `Value` or an `Expr` is the core's serde form
//! (`{"Int": 30}`, `{"Label": "Person"}`), a `Pattern` its text
//! (`"(a:Person)-[:knows]->(b)"`) or, for patterns the text can't express,
//! its serde form (an object). pbjson generates the serde of every other
//! message, and calls these for fields of these types.
//!
//! **Reading.** The field's JSON is first captured as raw text: serde_json
//! skips over it without recursion and without counting depth. The text is
//! then read by the core (`Value::from_json_str`, ...), which lifts
//! serde_json's own recursion limit and counts every level itself (upstream
//! #57): values and filters nested up to 100 levels are read wherever they
//! sit in a request, deeper ones are refused with the core's message. The
//! result is stored as postcard, the form `convert` decodes.
//!
//! These impls only work with serde_json (raw values are a serde_json
//! feature); every other deserializer fails with an error.

use ironweaver_core::query::Pattern;
use ironweaver_core::{Expr, Value};
use serde::de::{DeserializeOwned, Error as _};
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

use crate::convert::{decode, encode};
use crate::proto as pb;

/// The core's value of a wrapper message, for writing it.
fn core<T: DeserializeOwned, E: serde::ser::Error>(bytes: &[u8], what: &str) -> Result<T, E> {
    decode(bytes, what).map_err(|e| E::custom(e.message()))
}

/// The postcard of what the core reads from `raw`.
fn postcard<T: Serialize, E: serde::de::Error>(
    raw: &RawValue,
    read: impl FnOnce(&str) -> Result<T, ironweaver_core::GraphError>,
    what: &str,
) -> Result<Vec<u8>, E> {
    let value = read(raw.get()).map_err(|e| E::custom(format!("invalid {}: {}", what, e)))?;
    encode(&value, what).map_err(|e| E::custom(e.message()))
}

impl Serialize for pb::Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.form {
            Some(pb::value::Form::Postcard(bytes)) => core::<Value, _>(bytes, "value")?.serialize(s),
            None => Err(S::Error::custom("a value without a form")),
        }
    }
}

impl<'de> Deserialize<'de> for pb::Value {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        let bytes = postcard(&raw, Value::from_json_str, "value")?;
        Ok(pb::Value { form: Some(pb::value::Form::Postcard(bytes)) })
    }
}

impl Serialize for pb::Expr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.form {
            Some(pb::expr::Form::Postcard(bytes)) => core::<Expr, _>(bytes, "filter")?.serialize(s),
            None => Err(S::Error::custom("a filter without a form")),
        }
    }
}

impl<'de> Deserialize<'de> for pb::Expr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        let bytes = postcard(&raw, Expr::from_json_str, "filter")?;
        Ok(pb::Expr { form: Some(pb::expr::Form::Postcard(bytes)) })
    }
}

impl Serialize for pb::Pattern {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.form {
            Some(pb::pattern::Form::Text(text)) => s.serialize_str(text),
            Some(pb::pattern::Form::Postcard(bytes)) => {
                let pattern: Pattern = core(bytes, "pattern")?;
                match pattern.to_text() {
                    Ok(text) => s.serialize_str(&text),
                    Err(_) => pattern.serialize(s),
                }
            }
            None => Err(S::Error::custom("a pattern without a form")),
        }
    }
}

impl<'de> Deserialize<'de> for pb::Pattern {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        let form = if raw.get().starts_with('"') {
            // The text is parsed where the request is decoded, as over gRPC
            pb::pattern::Form::Text(serde_json::from_str(raw.get()).map_err(D::Error::custom)?)
        } else {
            pb::pattern::Form::Postcard(postcard(&raw, Pattern::from_json_str, "pattern")?)
        };
        Ok(pb::Pattern { form: Some(form) })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use ironweaver_core::CmpOp;

    use super::*;
    use crate::convert::{expr_to_pb, pattern_from_pb, value_from_pb, value_to_pb};

    fn nest(depth: usize) -> Value {
        (1..depth).fold(Value::Int(1), |v, _| Value::List(vec![v]))
    }

    fn read_value(json: &str) -> Result<Value, String> {
        let pb: pb::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        value_from_pb(pb, "value").map_err(|e| e.to_string())
    }

    #[test]
    fn values_are_the_cores_serde_form() {
        let dict = Value::Dict(HashMap::from([("b".into(), Value::Float(f64::NAN)), ("a".into(), Value::None)]));
        for (value, json) in [
            (Value::Int(30), r#"{"Int":30}"#),
            (Value::String("ann".into()), r#"{"String":"ann"}"#),
            (Value::Float(-0.0), r#"{"Float":-0.0}"#),
            (Value::Float(f64::INFINITY), r#"{"Float":"Infinity"}"#),
            (Value::None, r#""None""#),
            (Value::Bytes(vec![1, 2, 255]), r#"{"Bytes":"AQL/"}"#),
            (Value::List(vec![Value::Bool(true)]), r#"{"List":[{"Bool":true}]}"#),
            // Dict keys are written sorted
            (dict, r#"{"Dict":{"a":"None","b":{"Float":"NaN"}}}"#),
        ] {
            let pb = value_to_pb(&value).unwrap();
            assert_eq!(serde_json::to_string(&pb).unwrap(), json);
            let back = read_value(json).unwrap();
            // NaN != NaN: compare the encodings
            assert_eq!(value_to_pb(&back).unwrap(), pb, "{}", json);
        }
        // Every float is read exactly
        let x = 0.1 + 0.2;
        assert_eq!(read_value(&format!(r#"{{"Float":{}}}"#, x)).unwrap(), Value::Float(x));
    }

    #[test]
    fn values_nested_100_levels_are_read_anywhere_and_101_are_refused() {
        let deep = nest(100);
        let json = serde_json::to_string(&value_to_pb(&deep).unwrap()).unwrap();
        assert_eq!(read_value(&json).unwrap(), deep);
        // Inside a request, the request's own levels don't count
        let body = format!(r#"{{"mutations":[{{"upsertNode":{{"id":"a","attr":{{"x":{}}}}}}}]}}"#, json);
        let request: pb::CommitRequest = serde_json::from_str(&body).unwrap();
        let attrs = match &request.mutations[0].kind {
            Some(pb::mutation::Kind::UpsertNode(n)) => n.attr.clone(),
            other => panic!("{:?}", other),
        };
        assert_eq!(value_from_pb(attrs["x"].clone(), "value").unwrap(), deep);
        let e = read_value(&format!(r#"{{"List":[{}]}}"#, json)).unwrap_err();
        assert!(e.contains("nested more than 100 levels"), "{}", e);
    }

    #[test]
    fn filters_nested_100_levels_are_read_and_101_are_refused() {
        let deep = (1..100).fold(Expr::Const(true), |e, _| Expr::Or(vec![e]));
        let json = serde_json::to_string(&expr_to_pb(&deep).unwrap()).unwrap();
        let request: pb::FindRequest = serde_json::from_str(&format!(r#"{{"filter":{}}}"#, json)).unwrap();
        assert_eq!(crate::convert::find_from_pb(request.filter).unwrap().filter, deep);
        let e = serde_json::from_str::<pb::Expr>(&format!(r#"{{"Not":{}}}"#, json)).unwrap_err();
        assert!(e.to_string().contains("expression nested more than 100 levels"), "{}", e);
        // Unknown fields are refused, not skipped, however deep they nest
        let junk = format!(r#"{{"Exists":{{"path":["a"],"junk":{}1{}}}}}"#, "[".repeat(50_000), "]".repeat(50_000));
        let e = serde_json::from_str::<pb::Expr>(&junk).unwrap_err();
        assert!(e.to_string().contains("unknown field"), "{}", e);
        let compare = Expr::Compare { path: vec!["age".into()], op: CmpOp::Ge, value: Value::Int(18) };
        assert_eq!(
            serde_json::to_string(&expr_to_pb(&compare).unwrap()).unwrap(),
            r#"{"Compare":{"path":["age"],"op":"Ge","value":{"Int":18}}}"#
        );
    }

    #[test]
    fn patterns_are_text_or_the_serde_form() {
        let text = "(a:Person {age: 30})-[:knows*1..3]->(b)";
        let pb: pb::Pattern = serde_json::from_str(&serde_json::to_string(text).unwrap()).unwrap();
        assert_eq!(pb.form, Some(pb::pattern::Form::Text(text.into())));
        assert_eq!(serde_json::to_string(&pb).unwrap(), serde_json::to_string(text).unwrap());
        // A pattern the text can't express travels as an object
        let mut bound = Pattern::parse(text).unwrap();
        bound.bind_ids("b", vec!["x".into()]).unwrap();
        let pb = crate::convert::pattern_to_pb(&bound).unwrap();
        let json = serde_json::to_string(&pb).unwrap();
        assert!(json.starts_with('{'), "{}", json);
        let back: pb::Pattern = serde_json::from_str(&json).unwrap();
        assert_eq!(pattern_from_pb(Some(back)).unwrap(), bound);
        // A text that doesn't parse fails where the request is decoded
        let bad: pb::Pattern = serde_json::from_str(r#""(a)-[""#).unwrap();
        assert!(pattern_from_pb(Some(bad)).is_err());
        assert!(serde_json::from_str::<pb::Pattern>(r#"{"nodes":[],"edges":[],"x":1}"#).is_err());
    }
}
