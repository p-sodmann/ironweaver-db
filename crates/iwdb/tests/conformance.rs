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

/// Its data in `data/`, its WAL archive in `archive/` and its backup
/// directory `backups/` (step 16e).
fn fresh_authorized() -> FreshAuthorized {
    let dir = tempfile::tempdir().unwrap();
    let options = iwdb::StoreOptions { archive: Some(dir.path().join("archive")), ..support::options(2) };
    let store = Store::open(&dir.path().join("data"), options).unwrap();
    std::fs::create_dir(dir.path().join("backups")).unwrap();
    let db =
        Embedded::new(store, QueryConfig::default()).unwrap().with_backup_dir(&dir.path().join("backups")).unwrap();
    let principal = Arc::new(Principal::unauthenticated());
    FreshAuthorized { db: Authorized::new(Arc::new(db), principal, Audit::none()), _dir: dir }
}

mod admin {
    use super::fresh_authorized;
    iwdb_query::admin_conformance_tests!(fresh_authorized());
}
