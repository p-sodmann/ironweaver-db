//! `iwctl --server <endpoint> <command>` (step 16e, ADR 0055): the admin
//! commands against a running server, without its data directory. Each is
//! one call of the `Database` or `Admin` trait through the gRPC client
//! (`client::Remote`), printed like its local form; `iwctl` decides nothing
//! (design rule 8). Credentials work as for `user` and `token`.

use iwdb::{CatalogChange, CommitOptions, IdempotencyKey, IndexDef};
use iwdb_query::exec::block_on;
use iwdb_query::{Admin, BackupRequest, Code, Database, Error, QueryOptions, VerifyTarget};
use iwdb_server::client::{ClientTls, Remote};

use crate::args::RemoteFlags;
use crate::output::Out;

/// What a command against a server does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Status,
    Checkpoint {
        namespace: Option<String>,
    },
    Backup {
        name: String,
        max_bytes_per_second: Option<u64>,
        verify: bool,
    },
    Verify(VerifyTarget),
    PruneArchive {
        before: String,
        dry_run: bool,
    },
    Namespaces,
    CreateNamespace {
        name: String,
        key: Option<String>,
    },
    DropNamespace {
        name: String,
        key: Option<String>,
    },
    Indexes {
        namespace: String,
    },
    CreateIndex {
        namespace: String,
        path: Vec<String>,
        key: Option<String>,
    },
    DropIndex {
        namespace: String,
        path: Vec<String>,
        key: Option<String>,
    },
    AddConstraint {
        namespace: String,
        kind: iwdb::ConstraintKind,
        label: String,
        path: Vec<String>,
        key: Option<String>,
    },
    DropConstraint {
        namespace: String,
        kind: iwdb::ConstraintKind,
        label: String,
        path: Vec<String>,
        key: Option<String>,
    },
    Requests {
        user: Option<String>,
    },
    Cancel {
        id: u64,
    },
}

/// Why restore, import and export refuse `--server` (ADR 0055).
pub const OFFLINE: &str = "restore, import and export are offline: run them on the server's host (restore writes a new data directory and doesn't touch a running store; start a server on it afterwards)";

/// Parse `<command> <args>` of `iwctl --server <endpoint> ...`.
pub fn parse(name: &str, rest: &[String], flags: RemoteFlags<'_>) -> Result<Action, String> {
    let RemoteFlags { namespaces, key, before, dry_run, rate, no_verify } = flags;
    let expect = |n: usize, usage: &str| {
        if rest.len() == n { Ok(()) } else { Err(format!("usage: iwctl --server <endpoint> {} {}", name, usage)) }
    };
    let arg = |i: usize| rest[i].clone();
    let one_namespace = || -> Result<String, String> {
        match namespaces {
            [] => Ok(iwdb::NAMESPACE.to_owned()),
            [one] => Ok(one.clone()),
            _ => Err(format!("{} takes one --namespace", name)),
        }
    };
    let takes_key = matches!(
        name,
        "create-namespace" | "drop-namespace" | "create-index" | "drop-index" | "add-constraint" | "drop-constraint"
    );
    if key.is_some() && !takes_key {
        return Err(format!("{} takes no --key", name));
    }
    let takes_namespace =
        matches!(name, "checkpoint" | "indexes" | "create-index" | "drop-index" | "add-constraint" | "drop-constraint");
    if !namespaces.is_empty() && !takes_namespace {
        return Err(format!("{} takes no --namespace", name));
    }
    if (before.is_some() || dry_run) && name != "archive" {
        return Err(format!("{} takes no --before or --dry-run", name));
    }
    if (rate.is_some() || no_verify) && name != "backup" {
        return Err(format!("{} takes no --max-bytes-per-second or --no-verify", name));
    }
    let constraint = || -> Result<(iwdb::ConstraintKind, String, Vec<String>), String> {
        expect(3, "unique|required <label> <path> [-n <ns>] [--key <k>]")?;
        let kind = crate::args::constraint_kind(&rest[0])?;
        Ok((kind, arg(1), crate::args::dotted(&rest[2])))
    };
    Ok(match name {
        "status" => expect(0, "").map(|()| Action::Status)?,
        "checkpoint" => {
            expect(0, "[-n <ns>]")?;
            match namespaces {
                [] => Action::Checkpoint { namespace: None },
                [one] => Action::Checkpoint { namespace: Some(one.clone()) },
                _ => return Err("checkpoint takes one --namespace, or none for every namespace".into()),
            }
        }
        "backup" => {
            expect(1, "<name> [--max-bytes-per-second <n>] [--no-verify]")?;
            Action::Backup { name: arg(0), max_bytes_per_second: rate, verify: !no_verify }
        }
        "verify" => match rest {
            [] => Action::Verify(VerifyTarget::Store),
            [w] if w == "store" => Action::Verify(VerifyTarget::Store),
            [w] if w == "archive" => Action::Verify(VerifyTarget::Archive),
            [w, backup] if w == "backup" => Action::Verify(VerifyTarget::Backup(backup.clone())),
            _ => return Err("usage: iwctl --server <endpoint> verify [store | archive | backup <name>]".into()),
        },
        "archive" => {
            if rest.first().map(String::as_str) != Some("prune") || rest.len() != 1 {
                return Err("usage: iwctl --server <endpoint> archive prune --before <backup> [--dry-run]".into());
            }
            let before = before.ok_or("archive prune needs --before <backup>: the oldest backup to keep")?;
            Action::PruneArchive { before, dry_run }
        }
        "namespaces" => expect(0, "").map(|()| Action::Namespaces)?,
        "create-namespace" => {
            expect(1, "<name> [--key <k>]").map(|()| Action::CreateNamespace { name: arg(0), key })?
        }
        "drop-namespace" => expect(1, "<name> [--key <k>]").map(|()| Action::DropNamespace { name: arg(0), key })?,
        "indexes" => {
            expect(0, "[-n <ns>]")?;
            Action::Indexes { namespace: one_namespace()? }
        }
        "create-index" | "drop-index" => {
            expect(1, "<path> [-n <ns>] [--key <k>]")?;
            let (namespace, path) = (one_namespace()?, crate::args::dotted(&rest[0]));
            if name == "create-index" {
                Action::CreateIndex { namespace, path, key }
            } else {
                Action::DropIndex { namespace, path, key }
            }
        }
        "add-constraint" | "drop-constraint" => {
            let (kind, label, path) = constraint()?;
            let namespace = one_namespace()?;
            if name == "add-constraint" {
                Action::AddConstraint { namespace, kind, label, path, key }
            } else {
                Action::DropConstraint { namespace, kind, label, path, key }
            }
        }
        "requests" => match rest {
            [] => Action::Requests { user: None },
            [user] => Action::Requests { user: Some(user.clone()) },
            _ => return Err("usage: iwctl --server <endpoint> requests [<user>]".into()),
        },
        "cancel" => {
            expect(1, "<request id>")?;
            Action::Cancel { id: rest[0].parse().map_err(|_| format!("a request id is a number, not '{}'", rest[0]))? }
        }
        "restore" | "import" | "export" => return Err(OFFLINE.into()),
        "shell" | "help" | "version" => return Err(format!("{} takes no --server", name)),
        other => return Err(format!("unknown command '{}' (see --help)", other)),
    })
}

/// The exit code of a server's error: damage 1, a busy or draining server 3
/// (like a locked directory), anything else 4.
fn exit_code(e: &Error) -> u8 {
    match e.code() {
        Code::Corrupt => crate::exit::DAMAGE,
        Code::Unavailable => crate::exit::LOCKED,
        _ => crate::exit::FAILED,
    }
}

fn report(out: &Out, e: &Error) -> u8 {
    out.remote_error(e);
    exit_code(e)
}

/// Run `action` on the server at `endpoint`; the exit code.
pub fn run(endpoint: &str, action: &Action, tls: &ClientTls, token: Option<&str>, user: Option<&str>, out: &Out) -> u8 {
    let remote = match crate::users::connect(endpoint, tls, token, user) {
        Ok(remote) => remote,
        Err(e) => return report(out, &e),
    };
    match execute(&remote, endpoint, action, out) {
        Ok(code) => code,
        Err(e) => report(out, &e),
    }
}

fn key(key: &Option<String>) -> Result<CommitOptions, Error> {
    let idempotency_key =
        key.as_deref().map(IdempotencyKey::new).transpose().map_err(|e| Error::invalid(e.to_string()))?;
    Ok(CommitOptions { idempotency_key })
}

fn attr_path(path: &[String]) -> Result<iwdb::AttrPath, Error> {
    iwdb::AttrPath::new(path.iter().cloned()).map_err(|e| Error::invalid(e.to_string()))
}

fn constraint(kind: iwdb::ConstraintKind, label: &str, path: &[String]) -> Result<iwdb::Constraint, Error> {
    let label = iwdb::Label::new(label).map_err(|e| Error::invalid(e.to_string()))?;
    Ok(iwdb::Constraint { kind, label, path: attr_path(path)? })
}

/// Commit one catalog change and print it.
fn catalog(
    remote: &Remote,
    out: &Out,
    namespace: &str,
    change: CatalogChange,
    k: &Option<String>,
    what: &str,
) -> Result<u8, Error> {
    let result = block_on(remote.commit_catalog(namespace, change, key(k)?))?;
    out.commit(what, result.seq, result.deduplicated);
    Ok(crate::exit::OK)
}

fn execute(remote: &Remote, endpoint: &str, action: &Action, out: &Out) -> Result<u8, Error> {
    use crate::exit::{DAMAGE, OK};
    match action {
        Action::Status => {
            out.server_status(endpoint, &block_on(remote.server_status())?);
            Ok(OK)
        }
        Action::Checkpoint { namespace } => {
            for c in block_on(remote.checkpoint(namespace.clone()))? {
                out.checkpoint(endpoint, &c.namespace, &c.outcome);
            }
            Ok(OK)
        }
        Action::Backup { name, max_bytes_per_second, verify } => {
            let request =
                BackupRequest { name: name.clone(), max_bytes_per_second: *max_bytes_per_second, verify: *verify };
            let done = block_on(remote.backup(request))?;
            out.backup(&done.report);
            match done.verify {
                Some(report) => {
                    out.verify(&report);
                    Ok(if report.is_ok() { OK } else { DAMAGE })
                }
                None => Ok(OK),
            }
        }
        Action::Verify(target) => {
            let report = block_on(remote.verify(target.clone()))?;
            out.verify(&report);
            Ok(if report.is_ok() { OK } else { DAMAGE })
        }
        Action::PruneArchive { before, dry_run } => {
            out.prune(&block_on(remote.prune_archive(before.clone(), *dry_run))?);
            Ok(OK)
        }
        Action::Namespaces => {
            out.namespaces(&block_on(remote.server_status())?.namespaces);
            Ok(OK)
        }
        Action::CreateNamespace { name, key: k } => {
            let k = key(k)?.idempotency_key;
            out.namespace_result("created", &block_on(remote.create_namespace(name, k))?);
            Ok(OK)
        }
        Action::DropNamespace { name, key: k } => {
            let k = key(k)?.idempotency_key;
            out.namespace_result("dropped", &block_on(remote.drop_namespace(name, k))?);
            Ok(OK)
        }
        Action::Indexes { namespace } => {
            let status = block_on(remote.namespace_status(namespace))?;
            let catalog = block_on(remote.catalog(namespace, QueryOptions::default()))?.value;
            out.indexes(&status, &catalog);
            Ok(OK)
        }
        Action::CreateIndex { namespace, path, key } => {
            let change = CatalogChange::CreateIndex(IndexDef { path: attr_path(path)? });
            catalog(remote, out, namespace, change, key, "created index")
        }
        Action::DropIndex { namespace, path, key } => {
            let change = CatalogChange::DropIndex(IndexDef { path: attr_path(path)? });
            catalog(remote, out, namespace, change, key, "dropped index")
        }
        Action::AddConstraint { namespace, kind, label, path, key } => {
            let change = CatalogChange::AddConstraint(constraint(*kind, label, path)?);
            catalog(remote, out, namespace, change, key, "added constraint")
        }
        Action::DropConstraint { namespace, kind, label, path, key } => {
            let change = CatalogChange::DropConstraint(constraint(*kind, label, path)?);
            catalog(remote, out, namespace, change, key, "dropped constraint")
        }
        Action::Requests { user } => {
            out.requests(&block_on(remote.active_requests(user.clone(), None))?);
            Ok(OK)
        }
        Action::Cancel { id } => {
            out.cancelled(&block_on(remote.cancel_request(*id, None))?);
            Ok(OK)
        }
    }
}
