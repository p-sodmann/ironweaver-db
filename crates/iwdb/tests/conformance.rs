//! The conformance suite of the `Database` trait (`iwdb_query::conformance`)
//! against the embedded store.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ops::Deref;

use iwdb::{Embedded, QueryConfig, Store};

mod support;

/// A fresh store in a temporary directory, served as a `Database`.
struct Fresh {
    db: Embedded,
    _dir: tempfile::TempDir,
}

impl Deref for Fresh {
    type Target = Embedded;

    fn deref(&self) -> &Embedded {
        &self.db
    }
}

fn fresh() -> Fresh {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), support::options(2)).unwrap();
    Fresh { db: Embedded::new(store, QueryConfig::default()).unwrap(), _dir: dir }
}

iwdb_query::conformance_tests!(fresh());
