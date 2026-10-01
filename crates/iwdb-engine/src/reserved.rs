//! Reserved names: keys starting with `iwdb.` belong to the database.
//!
//! The database stores its own bookkeeping next to user data in the core's
//! meta maps: the entity version in each node's and edge's meta, the
//! commit position and the catalog in the graph meta of a saved file. The
//! commit pipeline also sets versions through an attribute op on
//! [`VERSION_KEY`] (ADR 0004). User input must not use these keys, so
//! every meta map and every top-level attribute key that comes from a user
//! goes through [`check_user_meta`] / [`check_user_attrs`] /
//! [`check_user_key`]. Keys inside nested dicts are not restricted.

use ironweaver_core::Attrs;

use crate::Error;

/// Prefix of every key the database owns.
pub const RESERVED_PREFIX: &str = "iwdb.";

/// Entity meta: the node's or edge's version (see [`DbRecord`](crate::DbRecord)).
/// Also the attribute key of the op that sets a version (ADR 0004).
pub const VERSION_KEY: &str = "iwdb.version";

/// Graph meta: the commit sequence number a saved file reflects (written
/// by checkpoints, step 5).
pub const SEQ_KEY: &str = "iwdb.seq";

/// Graph meta: the namespace's catalog (see ADR 0003).
pub const CATALOG_KEY: &str = "iwdb.catalog";

/// Graph meta: the namespace's idempotency key table (data-dir layout 3,
/// step 8; see [`KeyTable`](crate::KeyTable)).
pub const KEYS_KEY: &str = "iwdb.keys";

/// Whether `key` is owned by the database.
pub fn is_reserved(key: &str) -> bool {
    key.starts_with(RESERVED_PREFIX)
}

/// Reject `key` if the database owns it.
pub fn check_user_key(key: &str) -> Result<(), Error> {
    if is_reserved(key) {
        return Err(Error::ReservedName { key: key.to_owned() });
    }
    Ok(())
}

/// Reject `key` if the database owns it (for meta keys).
pub fn check_user_meta_key(key: &str) -> Result<(), Error> {
    check_user_key(key)
}

/// Reject a user-supplied meta map that uses a reserved key. If several
/// do, the error names the smallest. O(n) (O(n log n) only on error).
pub fn check_user_meta(meta: &Attrs) -> Result<(), Error> {
    check_keys(meta)
}

/// Reject a user-supplied attribute map whose top-level keys include a
/// reserved key. If several do, the error names the smallest. O(n).
pub fn check_user_attrs(attr: &Attrs) -> Result<(), Error> {
    check_keys(attr)
}

fn check_keys(map: &Attrs) -> Result<(), Error> {
    let first = map.keys().filter(|k| is_reserved(k)).min();
    match first {
        Some(key) => Err(Error::ReservedName { key: key.clone() }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironweaver_core::Value;

    #[test]
    fn own_keys_are_reserved() {
        for key in [VERSION_KEY, SEQ_KEY, CATALOG_KEY, KEYS_KEY, "iwdb.", "iwdb.anything"] {
            assert!(is_reserved(key), "{}", key);
            assert_eq!(check_user_meta_key(key), Err(Error::ReservedName { key: key.to_owned() }));
            assert_eq!(check_user_key(key), Err(Error::ReservedName { key: key.to_owned() }));
        }
        for key in ["", "iwdb", "iwdb_version", "IWDB.version", "x.iwdb.version", "version"] {
            assert!(!is_reserved(key), "{}", key);
            assert_eq!(check_user_meta_key(key), Ok(()));
        }
    }

    #[test]
    fn user_maps_with_a_reserved_key_are_rejected() {
        let mut meta: Attrs = [("source".to_owned(), Value::from("import"))].into();
        assert_eq!(check_user_meta(&meta), Ok(()));
        meta.insert("iwdb.z".into(), Value::Int(1));
        meta.insert("iwdb.version".into(), Value::Int(1));
        assert_eq!(check_user_meta(&meta), Err(Error::ReservedName { key: "iwdb.version".into() }));
        assert_eq!(check_user_attrs(&meta), Err(Error::ReservedName { key: "iwdb.version".into() }));

        // Nested keys are the user's
        let nested: Attrs = [("d".to_owned(), Value::Dict(meta))].into();
        assert_eq!(check_user_attrs(&nested), Ok(()));
    }
}
