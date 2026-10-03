//! Translation between the protos and the trait's types, both ways: the
//! server reads requests and writes answers, the client (feature `client`)
//! does the opposite with the same functions. Translation only (design rule
//! 8): the checks here are that a message is complete and that its values
//! decode; everything else is the trait's.
//!
//! `Value`, `Expr` and `Pattern` are the core's serde form in postcard
//! (ADR 0023). A decode error is `invalid_argument` with the core's message
//! (upstream #29: postcard drops custom serde messages, so the core
//! remembers them for `format::take_error`).

// The client half is used only with the `client` feature
#![cfg_attr(not(feature = "client"), allow(dead_code))]

use std::collections::BTreeMap;
use std::time::Duration;

use ironweaver_core::format::take_error;
use ironweaver_core::pathfinding::EdgeCost;
use ironweaver_core::query::Pattern;
use ironweaver_core::{Attrs, Direction, Expr, Value};
use iwdb_engine::catalog::AttrPath;
use iwdb_engine::CommitTime;
use iwdb_query::{Answer, Cursor, Error, Limits, QueryOptions, Work};
use iwdb_storage::HistoryId;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::proto as pb;

mod answers;
mod catalog;
mod entities;
mod mutations;
mod requests;

pub(crate) use answers::*;
pub(crate) use catalog::*;
pub(crate) use entities::*;
pub(crate) use mutations::*;
pub(crate) use requests::*;

pub(crate) type PbAttrs = BTreeMap<String, pb::Value>;

// ---- numbers ----

/// A count or limit from the wire; above `usize::MAX` (on 32-bit targets)
/// it saturates, and the database lowers it to its cap.
fn size(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn missing(what: &str) -> Error {
    Error::invalid(format!("{} is missing", what))
}

// ---- the core's types in postcard (ADR 0023) ----

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>, Error> {
    take_error();
    postcard::to_stdvec(value)
        .map_err(|e| Error::invalid(format!("invalid {}: {}", what, take_error().unwrap_or_else(|| e.to_string()))))
}

fn decode<T: DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, Error> {
    // A message left by an earlier failure on this thread isn't ours
    take_error();
    match postcard::take_from_bytes::<T>(bytes) {
        Ok((value, [])) => Ok(value),
        Ok((_, rest)) => {
            Err(Error::invalid(format!("invalid {}: {} bytes after its postcard encoding", what, rest.len())))
        }
        Err(e) => Err(Error::invalid(format!("invalid {}: {}", what, take_error().unwrap_or_else(|| e.to_string())))),
    }
}

pub(crate) fn value_to_pb(value: &Value) -> Result<pb::Value, Error> {
    Ok(pb::Value { form: Some(pb::value::Form::Postcard(encode(value, "value")?)) })
}

pub(crate) fn value_from_pb(value: pb::Value, what: &str) -> Result<Value, Error> {
    match value.form {
        Some(pb::value::Form::Postcard(bytes)) => decode(&bytes, what),
        None => Err(Error::invalid(format!("{} has no form", what))),
    }
}

fn value_field(value: Option<pb::Value>, what: &str) -> Result<Value, Error> {
    value_from_pb(value.ok_or_else(|| missing(what))?, what)
}

pub(crate) fn attrs_to_pb(attrs: &Attrs) -> Result<PbAttrs, Error> {
    attrs.iter().map(|(k, v)| Ok((k.clone(), value_to_pb(v)?))).collect()
}

pub(crate) fn attrs_from_pb(attrs: PbAttrs) -> Result<Attrs, Error> {
    attrs
        .into_iter()
        .map(|(k, v)| {
            let value = value_from_pb(v, &format!("value of '{}'", k))?;
            Ok((k, value))
        })
        .collect()
}

pub(crate) fn expr_to_pb(expr: &Expr) -> Result<pb::Expr, Error> {
    Ok(pb::Expr { form: Some(pb::expr::Form::Postcard(encode(expr, "filter")?)) })
}

fn expr_from_pb(expr: pb::Expr, what: &str) -> Result<Expr, Error> {
    match expr.form {
        Some(pb::expr::Form::Postcard(bytes)) => decode(&bytes, what),
        None => Err(Error::invalid(format!("{} has no form", what))),
    }
}

fn opt_expr_to_pb(expr: &Option<Expr>) -> Result<Option<pb::Expr>, Error> {
    expr.as_ref().map(expr_to_pb).transpose()
}

fn opt_expr_from_pb(expr: Option<pb::Expr>, what: &str) -> Result<Option<Expr>, Error> {
    expr.map(|e| expr_from_pb(e, what)).transpose()
}

/// The text if the core can write the pattern as text, postcard otherwise.
pub(crate) fn pattern_to_pb(pattern: &Pattern) -> Result<pb::Pattern, Error> {
    let form = match pattern.to_text() {
        Ok(text) => pb::pattern::Form::Text(text),
        Err(_) => pb::pattern::Form::Postcard(encode(pattern, "pattern")?),
    };
    Ok(pb::Pattern { form: Some(form) })
}

pub(crate) fn pattern_from_pb(pattern: Option<pb::Pattern>) -> Result<Pattern, Error> {
    match pattern.and_then(|p| p.form) {
        Some(pb::pattern::Form::Text(text)) => Ok(Pattern::parse(&text)?),
        Some(pb::pattern::Form::Postcard(bytes)) => decode(&bytes, "pattern"),
        None => Err(missing("the pattern")),
    }
}

// ---- small shared types ----

fn path_to_pb(keys: &[String]) -> pb::AttrPath {
    pb::AttrPath { keys: keys.to_vec() }
}

fn keys_from_pb(path: Option<pb::AttrPath>) -> Vec<String> {
    path.map(|p| p.keys).unwrap_or_default()
}

fn attr_path_from_pb(path: Option<pb::AttrPath>) -> Result<AttrPath, Error> {
    AttrPath::new(keys_from_pb(path)).map_err(|e| Error::invalid(e.to_string()))
}

fn direction_to_pb(direction: Direction) -> i32 {
    match direction {
        Direction::Out => pb::Direction::Out,
        Direction::In => pb::Direction::In,
        Direction::Both => pb::Direction::Both,
    }
    .into()
}

fn direction_from_pb(direction: i32) -> Result<Direction, Error> {
    match pb::Direction::try_from(direction) {
        Ok(pb::Direction::Unspecified | pb::Direction::Out) => Ok(Direction::Out),
        Ok(pb::Direction::In) => Ok(Direction::In),
        Ok(pb::Direction::Both) => Ok(Direction::Both),
        Err(_) => Err(Error::invalid(format!("unknown direction {}", direction))),
    }
}

fn cost_to_pb(cost: &EdgeCost) -> pb::EdgeCost {
    let kind = match cost {
        EdgeCost::Unit => pb::edge_cost::Kind::Unit(pb::UnitCost {}),
        EdgeCost::Weighted { key, default } => {
            pb::edge_cost::Kind::Weighted(pb::WeightedCost { key: key.clone(), default_weight: *default })
        }
    };
    pb::EdgeCost { kind: Some(kind) }
}

fn cost_from_pb(cost: Option<pb::EdgeCost>) -> EdgeCost {
    match cost.and_then(|c| c.kind) {
        None | Some(pb::edge_cost::Kind::Unit(_)) => EdgeCost::Unit,
        Some(pb::edge_cost::Kind::Weighted(w)) => EdgeCost::Weighted { key: w.key, default: w.default_weight },
    }
}

fn time_from_pb(micros: i64) -> CommitTime {
    CommitTime(micros)
}

// ---- options and answers ----

/// Whole milliseconds, rounded up so that a positive timeout stays positive.
fn millis(timeout: Duration) -> u32 {
    let ms = timeout.as_millis() + u128::from(timeout.subsec_nanos() % 1_000_000 != 0);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// The trait's options from the request's and the `grpc-timeout` header's
/// (`deadline`): the smaller timeout applies (ADR 0026).
pub(crate) fn options_from_pb(
    options: Option<pb::QueryOptions>,
    deadline: Option<Duration>,
) -> Result<QueryOptions, Error> {
    let o = options.unwrap_or_default();
    let history = match o.history.as_str() {
        "" => None,
        text => Some(text.parse::<HistoryId>().map_err(Error::invalid)?),
    };
    let asked = o.timeout_ms.map(|ms| Duration::from_millis(u64::from(ms)));
    let timeout = match (asked, deadline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let l = o.limits.unwrap_or_default();
    Ok(QueryOptions {
        min_seq: o.min_seq,
        history,
        timeout,
        limits: Limits {
            max_results: l.max_results.map(size),
            max_visited: l.max_visited.map(size),
            max_edges: l.max_edges.map(size),
        },
        partial: o.partial,
        cursor: (!o.cursor.is_empty()).then(|| Cursor::new(o.cursor)),
    })
}

pub(crate) fn options_to_pb(o: &QueryOptions) -> pb::QueryOptions {
    let l = &o.limits;
    pb::QueryOptions {
        min_seq: o.min_seq,
        history: o.history.map(|h| h.to_string()).unwrap_or_default(),
        timeout_ms: o.timeout.map(millis),
        limits: Some(pb::Limits {
            max_results: l.max_results.map(wide),
            max_visited: l.max_visited.map(wide),
            max_edges: l.max_edges.map(wide),
        }),
        partial: o.partial,
        cursor: o.cursor.as_ref().map(|c| c.as_str().to_owned()).unwrap_or_default(),
    }
}

pub(crate) fn meta_to_pb<T>(answer: &Answer<T>) -> pb::AnswerMeta {
    pb::AnswerMeta {
        seq: answer.seq,
        next: answer.next.as_ref().map(|c| c.as_str().to_owned()).unwrap_or_default(),
        truncated: answer.truncated,
        work: Some(pb::Work { visited: wide(answer.work.visited), edges: wide(answer.work.edges) }),
    }
}

pub(crate) fn answer_from_pb<T>(value: T, meta: pb::AnswerMeta) -> Answer<T> {
    let work = meta.work.unwrap_or_default();
    Answer {
        value,
        seq: meta.seq,
        next: (!meta.next.is_empty()).then(|| Cursor::new(meta.next)),
        truncated: meta.truncated,
        work: Work { visited: size(work.visited), edges: size(work.edges) },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use iwdb_query::MatchRequest;

    fn nest(depth: usize, leaf: Value) -> Value {
        (0..depth).fold(leaf, |v, _| Value::List(vec![v]))
    }

    /// A filter of `depth` levels (`Expr::depth`): `Not`s around a `Const`.
    fn nest_expr(depth: usize) -> Expr {
        let e = (1..depth).fold(Expr::Const(true), |e, _| Expr::Not(Box::new(e)));
        assert_eq!(e.depth(), depth);
        e
    }

    #[test]
    fn values_nested_100_levels_pass_and_101_fail_with_the_cores_message() {
        let ok = nest(99, Value::Int(1));
        assert_eq!(value_from_pb(value_to_pb(&ok).unwrap(), "value").unwrap(), ok);
        // Encoding refuses a value the core refuses
        let deep = nest(100, Value::Int(1));
        let e = value_to_pb(&deep).unwrap_err();
        assert!(e.message().contains("nested more than 100 levels"), "{}", e);
        // So does decoding bytes made without the limit: a list of one item
        // is the variant index, a length of 1, and the item
        let list = postcard::to_stdvec(&Value::List(vec![])).unwrap()[0];
        let mut bytes = Vec::new();
        for _ in 0..100 {
            bytes.extend_from_slice(&[list, 1]);
        }
        bytes.extend_from_slice(&postcard::to_stdvec(&Value::Int(1)).unwrap());
        let e = value_from_pb(pb::Value { form: Some(pb::value::Form::Postcard(bytes)) }, "value").unwrap_err();
        assert_eq!(e.code(), iwdb_query::Code::InvalidArgument);
        assert!(e.message().contains("nested more than 100 levels"), "#29: the core's message, got: {}", e);
    }

    #[test]
    fn filters_nested_100_levels_pass_and_101_fail_with_the_cores_message() {
        let ok = nest_expr(100);
        assert_eq!(find_from_pb(Some(expr_to_pb(&ok).unwrap())).unwrap().filter, ok);
        let e = expr_to_pb(&nest_expr(101)).unwrap_err();
        assert!(e.message().contains("expression nested more than 100 levels"), "{}", e);
    }

    #[test]
    fn a_stale_message_of_an_earlier_failure_is_not_reported() {
        assert!(expr_to_pb(&nest_expr(101)).is_err());
        let e = value_from_pb(pb::Value { form: Some(pb::value::Form::Postcard(vec![0xff, 0xff])) }, "value");
        assert!(!e.unwrap_err().message().contains("nested"));
    }

    #[test]
    fn garbage_and_trailing_bytes_are_invalid() {
        let mut bytes = postcard::to_stdvec(&Value::Int(1)).unwrap();
        bytes.push(0);
        let e = value_from_pb(pb::Value { form: Some(pb::value::Form::Postcard(bytes)) }, "value").unwrap_err();
        assert!(e.message().contains("1 bytes after"), "{}", e);
        assert_eq!(
            value_from_pb(pb::Value { form: None }, "value").unwrap_err().code(),
            iwdb_query::Code::InvalidArgument
        );
        assert!(pattern_from_pb(Some(pb::Pattern { form: Some(pb::pattern::Form::Text("(a)-[".into())) })).is_err());
    }

    #[test]
    fn patterns_go_as_text_when_the_text_can_express_them() {
        let text = MatchRequest::parse("(a:Person {age: 30})-[:knows*1..3]->(b)").unwrap();
        let pb = pattern_to_pb(&text.pattern).unwrap();
        assert!(matches!(pb.form, Some(pb::pattern::Form::Text(_))));
        assert_eq!(pattern_from_pb(Some(pb)).unwrap(), text.pattern);
        let mut bound = text.pattern.clone();
        bound.bind_ids("b", vec!["x".into()]).unwrap();
        let pb = pattern_to_pb(&bound).unwrap();
        assert!(matches!(pb.form, Some(pb::pattern::Form::Postcard(_))));
        assert_eq!(pattern_from_pb(Some(pb)).unwrap(), bound);
    }

    #[test]
    fn timeouts_take_the_smaller_and_round_up() {
        let o = |ms| Some(pb::QueryOptions { timeout_ms: ms, ..Default::default() });
        let ms = Duration::from_millis;
        assert_eq!(options_from_pb(o(Some(50)), Some(ms(20))).unwrap().timeout, Some(ms(20)));
        assert_eq!(options_from_pb(o(Some(10)), Some(ms(20))).unwrap().timeout, Some(ms(10)));
        assert_eq!(options_from_pb(o(None), Some(ms(20))).unwrap().timeout, Some(ms(20)));
        assert_eq!(options_from_pb(o(None), None).unwrap().timeout, None);
        assert_eq!(millis(Duration::ZERO), 0);
        assert_eq!(millis(Duration::from_micros(1)), 1);
        assert_eq!(millis(Duration::MAX), u32::MAX);
        let bad = Some(pb::QueryOptions { history: "nope".into(), ..Default::default() });
        assert_eq!(options_from_pb(bad, None).unwrap_err().code(), iwdb_query::Code::InvalidArgument);
    }
}
