//! The audit log (step 15c, ADR 0049): one [`AuditEntry`] per login,
//! logout, user, grant, token, namespace and catalog change, and per
//! refusal (`unauthenticated`, `permission_denied`) of any operation.
//!
//! Entries are made in one place, the authorisation point
//! ([`Authorized`](crate::Authorized), [`login`](crate::auth::login), and
//! the server's gate for requests it refuses before an operation runs),
//! never in an adapter. Where they go is the [`AuditSink`]'s business:
//! the server writes them as `tracing` events of target `iwdb::audit`.
//!
//! An entry holds names (users, namespaces, token names, roles), never a
//! secret, a token's hash, a certificate, an error message or a value of
//! the data.

use std::net::IpAddr;
use std::sync::Arc;

use crate::Code;
use crate::auth::{Operation, Role, Via};

/// What happened: who did what, from where, on what, and how it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditEntry {
    /// The operation; `None` for a request the gate refused before it
    /// could tell which operation it was (an unknown route).
    pub operation: Option<Operation>,
    /// `None`: it succeeded. Otherwise the error's code (never its
    /// message, which may quote data).
    pub code: Option<Code>,
    /// The principal's user; for `Login`, the user name tried (only if it
    /// is a valid user name). `None` when the request had no principal.
    pub user: Option<String>,
    /// How the principal authenticated.
    pub via: Option<Via>,
    /// The client's address.
    pub client: Option<IpAddr>,
    /// The namespace the operation is on.
    pub namespace: Option<String>,
    /// The user the operation is about (user, grant and token changes).
    pub subject: Option<String>,
    /// The API token's name (token changes).
    pub token_name: Option<String>,
    /// The role granted.
    pub role: Option<Role>,
    /// The admin flag set (`CreateUser`, `SetAdmin`).
    pub admin: Option<bool>,
    /// The namespace's commit seq of a catalog change.
    pub seq: Option<u64>,
    /// The event number of a namespace's creation or drop in the
    /// namespaces log.
    pub namespace_event: Option<u64>,
}

impl AuditEntry {
    /// An entry of `operation`, so far a success.
    pub fn of(operation: Operation) -> Self {
        AuditEntry { operation: Some(operation), ..AuditEntry::default() }
    }

    pub fn succeeded(&self) -> bool {
        self.code.is_none()
    }

    /// `success` or `failure`.
    pub fn outcome(&self) -> &'static str {
        if self.succeeded() { "success" } else { "failure" }
    }

    pub(crate) fn namespace(mut self, namespace: &str) -> Self {
        self.namespace = Some(namespace.to_owned());
        self
    }

    pub(crate) fn subject(mut self, user: &str) -> Self {
        self.subject = Some(user.to_owned());
        self
    }
}

/// Where audit entries go. `record` is called once per entry, after the
/// outcome is known, on the request's task: it must not block for long.
pub trait AuditSink: Send + Sync {
    fn record(&self, entry: &AuditEntry);
}

/// A sink that drops every entry: for services used in code without an
/// audit log.
pub struct NoAudit;

impl AuditSink for NoAudit {
    fn record(&self, _entry: &AuditEntry) {}
}

/// A request's audit context: the sink, and the client's address.
#[derive(Clone)]
pub struct Audit {
    pub sink: Arc<dyn AuditSink>,
    pub client: Option<IpAddr>,
}

impl Audit {
    pub fn new(sink: Arc<dyn AuditSink>, client: Option<IpAddr>) -> Self {
        Audit { sink, client }
    }

    /// No audit log.
    pub fn none() -> Self {
        Audit { sink: Arc::new(NoAudit), client: None }
    }

    /// Record `entry`, with this request's client.
    pub fn record(&self, mut entry: AuditEntry) {
        entry.client = self.client;
        self.sink.record(&entry);
    }
}

impl std::fmt::Debug for Audit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audit").field("client", &self.client).finish_non_exhaustive()
    }
}
