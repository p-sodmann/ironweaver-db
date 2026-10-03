//! The inner encoding of the v1 contract (ADR 0023): `Value`, `Expr` and
//! `Pattern` travel as postcard of the core's serde form. `buf breaking`
//! guards the messages; this fixture guards the bytes inside them. If a core
//! bump changes them, v1 clients break: the change needs a new `form` in the
//! protos, not an edit of these bytes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use ironweaver_core::query::Pattern;
use ironweaver_core::{CmpOp, Date, DateTime, Expr, Value};

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn check<T>(value: &T, bytes: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = hex(bytes);
    assert_eq!(postcard::to_stdvec(value).unwrap(), bytes, "encoding of {:?}", value);
    assert_eq!(&postcard::from_bytes::<T>(&bytes).unwrap(), value, "decoding of {:?}", value);
}

#[test]
fn every_value_variant_keeps_its_bytes() {
    let dict: HashMap<String, Value> = [("b".to_owned(), Value::Int(2)), ("a".to_owned(), Value::Bool(true))].into();
    check(&Value::String("ann".into()), "0003616e6e");
    check(&Value::Int(-30), "013b");
    check(&Value::Float(1.5), "02000000000000f83f");
    check(&Value::Bool(true), "0401");
    check(&Value::None, "05");
    check(&Value::List(vec![Value::Int(1), Value::String("x".into())]), "06020102000178");
    // Dict entries are written sorted by key
    check(&Value::Dict(dict), "07020161040101620104");
    check(&Value::Bytes(vec![0, 255]), "080200ff");
    check(&Value::Date(Date(19_000)), "09f0a802");
    check(&Value::DateTime(DateTime { micros: 1_700_000_000_000_000, offset: Some(3600) }), "0a8080f2818389850601a038");
    // Half: the f16 bits as a varint (1.5 is 0x3e00); the core's API makes
    // halves only by decoding
    let half: Value = postcard::from_bytes(&hex("03807c")).unwrap();
    assert!(matches!(half, Value::Half(_)) && half.as_f64() == Some(1.5), "{:?}", half);
    assert_eq!(postcard::to_stdvec(&half).unwrap(), hex("03807c"));
}

#[test]
fn every_filter_variant_keeps_its_bytes() {
    check(&Expr::Const(false), "0000");
    check(&Expr::Compare { path: vec!["age".into()], op: CmpOp::Ge, value: Value::Int(18) }, "010103616765050124");
    check(&Expr::In { path: vec!["k".into()], values: vec![Value::Int(1), Value::Int(2)] }, "0201016b0201020104");
    check(&Expr::Exists { path: vec!["a".into(), "b".into()] }, "030201610162");
    check(&Expr::Label("Person".into()), "0406506572736f6e");
    check(&Expr::Type("knows".into()), "05056b6e6f7773");
    check(&Expr::And(vec![Expr::Const(true), Expr::Label("L".into())]), "0602000104014c");
    check(&Expr::Or(vec![Expr::Const(true)]), "07010001");
    check(&Expr::Not(Box::new(Expr::Const(true))), "080001");
}

#[test]
fn a_pattern_keeps_its_bytes() {
    let mut pattern = Pattern::parse("(a:Person)-[:knows*1..2]->(b)").unwrap();
    pattern.bind_ids("b", vec!["x".into()]).unwrap();
    check(&pattern, "020101610106506572736f6e0000010162000001010178010000010101056b6e6f77730001010102");
}
