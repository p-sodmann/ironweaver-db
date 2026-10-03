//! A failpoint in the apply path (feature `failpoints`, tests only): make
//! the next apply of a data record on this thread fail, as the core's
//! `apply_all` does on a bug (ADR 0028).

use std::cell::RefCell;

use ironweaver_core::GraphError;

thread_local! {
    static NEXT: RefCell<Option<GraphError>> = const { RefCell::new(None) };
}

/// Make the next apply of a data record on this thread fail with `error`.
/// A `GraphError::Internal` comes after the graph applied the record's ops,
/// as a failed rollback leaves it (part of the transaction stays); any
/// other error before the graph changes, as a clean rollback leaves it.
pub fn fail_next_apply(error: GraphError) {
    NEXT.with(|next| *next.borrow_mut() = Some(error));
}

pub(crate) fn take() -> Option<GraphError> {
    NEXT.with(|next| next.borrow_mut().take())
}
