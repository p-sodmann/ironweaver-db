//! `DbRecord` reads attributes exactly like the core's `Record`: it carries
//! a copy of the core's private path lookup (`Record::at`), and this test
//! keeps the copy honest across core bumps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use ironweaver_core::{Attributes, Record, Value};
use iwdb_engine::DbRecord;
use proptest::collection::vec;
use proptest::prelude::*;

proptest! {
    #[test]
    fn every_attributes_method_answers_like_record(
        attr in common::attrs(),
        meta in common::user_meta(),
        version in common::version(),
        // Paths of every length, the empty one included
        path in vec(common::key(), 0..4),
    ) {
        let record = Record { attr: attr.clone(), meta: meta.clone() };
        let db = DbRecord { attr, meta, version };

        let copy = |v: Option<&Value>| v.cloned();
        prop_assert_eq!(db.with_value(&path, copy), record.with_value(&path, copy));
        prop_assert_eq!(db.number(&path), record.number(&path));
        prop_assert_eq!(db.numbers(&path), record.numbers(&path));
        let key = path.first().map_or("", String::as_str);
        prop_assert_eq!(db.text(key), record.text(key));
    }
}

#[test]
fn lookups_follow_the_core_path_rules() {
    let nested: ironweaver_core::Attrs =
        [("lat".to_owned(), Value::Float(52.5)), ("none".to_owned(), Value::None)].into();
    let db = DbRecord::with_attr([
        ("pos", Value::Dict(nested)),
        ("xs", Value::List(vec![Value::Int(1), Value::Float(2.5)])),
        ("name", Value::from("Ada")),
    ]);
    let p = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    assert_eq!(db.number(&p(&["pos", "lat"])).unwrap(), ironweaver_core::Lookup::Found(52.5));
    // None counts as missing, anywhere on the path
    assert_eq!(db.number(&p(&["pos", "none"])).unwrap(), ironweaver_core::Lookup::Missing);
    assert_eq!(db.number(&p(&["name"])).unwrap(), ironweaver_core::Lookup::Invalid);
    assert_eq!(db.numbers(&p(&["xs"])).unwrap(), ironweaver_core::Lookup::Found(vec![1.0, 2.5]));
    assert_eq!(db.text("name").unwrap(), ironweaver_core::Lookup::Found("Ada".into()));
    assert!(db.with_value(&p(&[]), |v| v.is_none()).unwrap());
}
