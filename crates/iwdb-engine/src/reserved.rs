//! Reserved names: meta keys starting with `iwdb.` belong to the database.
//!
//! The database stores its own bookkeeping next to user data in the core's
//! meta maps: the entity version in each node's and edge's meta, the
//! commit position and the catalog in the graph meta of a saved file. User
//! input must not use these keys, so every meta map that comes from a user
//! goes through [`check_user_meta`]. Attribute keys (`attr`) are not
//! restricted.

use ironweaver_core::Attrs;

use crate::Error;

/// Prefix of every key the database owns.
pub const RESERVED_PREFIX: &str = "iwdb.";

/// Entity meta: the node's or edge's version (see [`DbRecord`](crate::DbRecord)).
pub const VERSION_KEY: &str = "iwdb.version";

/// Graph meta: the commit sequence number a saved file reflects (written
/// by checkpoints, step 5).
pub const SEQ_KEY: &str = "iwdb.seq";

/// Graph meta: the namespace's catalog (see ADR 0003).
pub const CATALOG_KEY: &str = "iwdb.catalog";

/// Whether `key` is owned by the database.
pub fn is_reserved(key: &str) -> bool {
    key.starts_with(RESERVED_PREFIX)
}

/// Reject `key` if the database owns it.
pub fn check_user_meta_key(key: &str) -> Result<(), Error> {
    if is_reserved(key) {
        return Err(Error::ReservedName { key: key.to_owned() });
    }
    Ok(())
}

/// Reject a user-supplied meta map that uses a reserved key. If several
/// do, the error names the smallest. O(n) (O(n log n) only on error).
pub fn check_user_meta(meta: &Attrs) -> Result<(), Error> {
    let first = meta.keys().filter(|k| is_reserved(k)).min();
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
        for key in [VERSION_KEY, SEQ_KEY, CATALOG_KEY, "iwdb.", "iwdb.anything"] {
            assert!(is_reserved(key), "{}", key);
            assert_eq!(check_user_meta_key(key), Err(Error::ReservedName { key: key.to_owned() }));
        }
        for key in ["", "iwdb", "iwdb_version", "IWDB.version", "x.iwdb.version", "version"] {
            assert!(!is_reserved(key), "{}", key);
            assert_eq!(check_user_meta_key(key), Ok(()));
        }
    }

    #[test]
    fn user_meta_with_a_reserved_key_is_rejected() {
        let mut meta: Attrs = [("source".to_owned(), Value::from("import"))].into();
        assert_eq!(check_user_meta(&meta), Ok(()));
        meta.insert("iwdb.z".into(), Value::Int(1));
        meta.insert("iwdb.version".into(), Value::Int(1));
        assert_eq!(check_user_meta(&meta), Err(Error::ReservedName { key: "iwdb.version".into() }));
    }
}
