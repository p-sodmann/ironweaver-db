//! Ironweaver DB engine: the database payload, the catalog and the commit
//! pipeline, built on [`ironweaver_core`].
//!
//! Step 1 only sets up the crate: it holds the [`testutil`] helpers that
//! later steps use to compare graphs. The payload, catalog and commit
//! pipeline arrive in steps 2 and 3.

pub mod testutil;
