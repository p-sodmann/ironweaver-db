//! The conformance suite of the `Database` trait (`iwdb_query::conformance`)
//! against the embedded store, and of the `Admin` trait through the
//! authorisation point, which registers the calls (as a server does).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ops::Deref;
use std::sync::Arc;

use iwdb::{Embedded, QueryConfig, Store};
use iwdb_query::audit::Audit;
use iwdb_query::{Authorized, Principal};

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

/// A fresh store as a server-wide admin sees it through the authorisation
/// point.
struct FreshAuthorized {
    db: Authorized<Embedded>,
    _dir: tempfile::TempDir,
}

impl Deref for FreshAuthorized {
    type Target = Authorized<Embedded>;

    fn deref(&self) -> &Authorized<Embedded> {
        &self.db
    }
}

fn fresh_authorized() -> FreshAuthorized {
    let Fresh { db, _dir } = fresh();
    FreshAuthorized { db: Authorized::new(Arc::new(db), Arc::new(Principal::unauthenticated()), Audit::none()), _dir }
}

mod admin {
    use super::fresh_authorized;
    iwdb_query::admin_conformance_tests!(fresh_authorized());
}
