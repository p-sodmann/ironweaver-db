//! The reference model: the same commits, run in memory in the parent.
//!
//! The workload is deterministic from a seed, and so is each commit's
//! outcome given the state before it, so the parent reproduces the state
//! the child had at any seq it reached: it runs the child's commit steps
//! in order (a step that fails validation uses no seq, in the child and
//! here) until the model is at that seq. It keeps every commit record, so
//! it can also go back to an earlier seq.
//!
//! Commits with an idempotency key go through the engine's keyed
//! path, so the model applies each key once, as the store must: a retry
//! whose original is in the model's key table commits nothing.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use iwdb::{IdempotencyKey, Namespace};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::testutil::workload::Step;
use iwdb_engine::{CommitRecord, Prepare};

use crate::script::Script;

pub use iwdb_engine::testutil::{State, state};

/// A commit the child may make: its step and idempotency key.
pub type Commit = (Step, Option<IdempotencyKey>);

/// A digest of [`state`], for the child to report the state it opened
/// with. Parent and child are the same binary, so `DefaultHasher` (fixed
/// keys) gives the same digest in both.
pub fn digest(ns: &Namespace) -> u64 {
    let (graph, catalog, seq, keys) = state(ns);
    let mut hasher = DefaultHasher::new();
    graph.hash(&mut hasher);
    format!("{:?}", catalog).hash(&mut hasher);
    seq.hash(&mut hasher);
    keys.hash(&mut hasher);
    hasher.finish()
}

pub fn empty() -> Namespace {
    // The store's namespace name is valid
    #[allow(clippy::expect_used)]
    Namespace::new(NamespaceName::new(iwdb::NAMESPACE).expect("a valid name"))
}

/// The reference: a namespace at some seq, the records of every commit up
/// to it, and the commit steps of the child that may have gone further.
pub struct Model {
    ns: Namespace,
    history: Vec<CommitRecord>,
    pending: Option<Box<dyn Iterator<Item = Commit>>>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model").field("seq", &self.ns.seq()).finish_non_exhaustive()
    }
}

impl Default for Model {
    fn default() -> Self {
        Model { ns: empty(), history: Vec::new(), pending: None }
    }
}

impl Model {
    pub fn seq(&self) -> u64 {
        self.ns.seq()
    }

    pub fn namespace(&self) -> &Namespace {
        &self.ns
    }

    /// A child started at the model's seq with the script `seed`, running
    /// at most `acts` acts, after retrying the keyed commits `retries`: its
    /// commits are what the model can advance by.
    pub fn begin(&mut self, seed: u64, acts: usize, retries: &[IdempotencyKey]) -> Result<(), String> {
        let mut first = Vec::new();
        for key in retries {
            first.push((Script::keyed(key)?, Some(key.clone())));
        }
        self.pending = Some(Box::new(first.into_iter().chain(Script::commits(seed, acts))));
        Ok(())
    }

    /// Go to `seq`: forward through the pending commits, or back through
    /// the history. Fails if the pending commits don't reach it.
    pub fn at(&mut self, seq: u64) -> Result<&Namespace, String> {
        if seq < self.ns.seq() {
            let mut ns = empty();
            for record in &self.history[..seq as usize] {
                ns.replay(record.clone(), None).map_err(|e| format!("the model failed to replay: {}", e))?;
            }
            self.history.truncate(seq as usize);
            self.ns = ns;
            // The pending commits followed the later state
            self.pending = None;
        }
        while self.ns.seq() < seq {
            let Some((step, key)) = self.pending.as_mut().and_then(Iterator::next) else {
                return Err(format!(
                    "seq {} is beyond every commit the child could have made (the model ends at {})",
                    seq,
                    self.ns.seq()
                ));
            };
            let prepared = match step {
                Step::Tx(mutations) => self.ns.prepare_keyed(&mutations, key.as_ref()),
                Step::Catalog(change) => self.ns.prepare_catalog_keyed(change, key.as_ref()),
            };
            // A step that fails validation fails in the child too, and a
            // retry of a commit the model has commits nothing in either
            if let Ok(Prepare::New(prepared)) = prepared {
                let record = prepared.record().clone();
                self.ns.apply(prepared, None).map_err(|e| format!("the model failed to apply: {}", e))?;
                self.history.push(record);
            }
        }
        Ok(&self.ns)
    }

    /// The state after the commits `1 ..= seq`, without moving the model:
    /// for checking backups and restores. `seq` must be at most the
    /// model's seq.
    pub fn state_at(&self, seq: u64) -> Result<State, String> {
        if seq > self.ns.seq() {
            return Err(format!("seq {} is beyond the model's seq {}", seq, self.ns.seq()));
        }
        let mut ns = empty();
        for record in &self.history[..seq as usize] {
            ns.replay(record.clone(), None).map_err(|e| format!("the model failed to replay: {}", e))?;
        }
        Ok(state(&ns))
    }

    /// Apply `step` now, as the parent commits it to a store it opened.
    /// Returns the seq, or `None` if the step fails validation (then the
    /// store's commit fails too).
    pub fn commit(&mut self, step: &Step) -> Result<Option<u64>, String> {
        self.pending = None;
        let prepared = match step {
            Step::Tx(mutations) => self.ns.prepare(mutations),
            Step::Catalog(change) => self.ns.prepare_catalog(change.clone()),
        };
        let Ok(prepared) = prepared else { return Ok(None) };
        let record = prepared.record().clone();
        let result = self.ns.apply(prepared, None).map_err(|e| format!("the model failed to apply: {}", e))?;
        self.history.push(record);
        Ok(Some(result.seq))
    }

    /// Recovery settled at `seq` (checked): the next child starts there.
    pub fn settle(&mut self, seq: u64) -> Result<(), String> {
        self.at(seq)?;
        self.pending = None;
        Ok(())
    }
}
