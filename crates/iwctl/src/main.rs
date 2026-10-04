//! `iwctl`: the admin CLI for local data directories (step 7), a query
//! shell for servers (`iwctl shell`, step 14a), and users, grants and API
//! tokens on a directory or a server (`iwctl user`, `iwctl token`, step 15a). It parses arguments, calls
//! the library (`iwdb`, or the `Database` trait over gRPC) and prints the
//! result; every operation lives in the library (design rule 8).
//! `documentation/iwctl.md` describes the commands and the exit codes.

mod args;
mod output;
mod shell;
mod users;

use std::process::ExitCode;

use iwdb::{
    AttrPath, CatalogChange, CommitOptions, Constraint, ConstraintKind, Error, IdempotencyKey, IndexDef, Label,
    RestoreSources, Store, StoreOptions,
};

use args::{Command, Parsed, USAGE};
use output::Out;

/// Exit codes (documented in `documentation/iwctl.md`).
mod exit {
    pub const OK: u8 = 0;
    /// Verify found damage, or the operation failed on damaged data.
    pub const DAMAGE: u8 = 1;
    pub const USAGE: u8 = 2;
    /// A store has the directory open.
    pub const LOCKED: u8 = 3;
    /// Any other failure (I/O, not a data directory, a refused restore, ...).
    pub const FAILED: u8 = 4;
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse(&raw) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("iwctl: {}\n\n{}", message, USAGE);
            return ExitCode::from(exit::USAGE);
        }
    };
    let out = Out { json: parsed.json };
    ExitCode::from(match run(&parsed, &out) {
        Ok(code) => code,
        Err(error) => {
            out.error(&error);
            exit_code(&error)
        }
    })
}

/// The exit code of a failed operation.
pub(crate) fn exit_code(error: &Error) -> u8 {
    match error {
        Error::Locked { .. } => exit::LOCKED,
        Error::Corrupt { .. }
        | Error::InvalidRecord { .. }
        | Error::SeqMismatch { .. }
        | Error::HeaderMismatch { .. }
        | Error::SegmentTooLarge { .. }
        | Error::TornTail { .. }
        | Error::InvalidCheckpoint { .. }
        | Error::NoUsableCheckpoint { .. }
        | Error::ReplayFailed { .. }
        | Error::InvalidDataDir { .. }
        | Error::InvalidManifest { .. }
        | Error::InvalidNamespaceLog { .. }
        | Error::NamespaceDamaged { .. }
        | Error::ArchiveConflict { .. } => exit::DAMAGE,
        _ => exit::FAILED,
    }
}

/// Options for opening a store from the command line: no background
/// threads, no checkpoint on close (only `checkpoint` writes one), never
/// create a directory.
fn store_options(parsed: &Parsed) -> StoreOptions {
    let mut options = StoreOptions::default();
    options.wal.fsync = parsed.fsync;
    options.checkpoint.background = false;
    options.checkpoint.on_close = false;
    options.checkpoint.keep = parsed.keep;
    options.create_if_missing = false;
    options.archive = parsed.archive.clone();
    options
}

fn run(parsed: &Parsed, out: &Out) -> Result<u8, Error> {
    match &parsed.command {
        Command::Help => {
            println!("{}", USAGE);
            Ok(exit::OK)
        }
        Command::Version => {
            println!("iwctl {}", env!("CARGO_PKG_VERSION"));
            Ok(exit::OK)
        }
        Command::Shell { endpoint } => Ok(shell::run(
            endpoint,
            one_namespace(parsed),
            parsed.json,
            parsed.token.as_deref(),
            parsed.user.as_deref(),
        )),
        Command::Accounts { action, target } => Ok(users::run(action, target, store_options(parsed), out)),
        Command::Status { dir } => {
            let status = iwdb::status(dir, store_options(parsed))?;
            out.status(&status);
            Ok(if status.files.in_use { exit::LOCKED } else { exit::OK })
        }
        Command::Checkpoint { dir } => {
            let store = Store::open(dir, store_options(parsed))?;
            let outcomes: Vec<(String, iwdb::CheckpointOutcome)> = if parsed.namespaces.is_empty() {
                store.checkpoint_all()?
            } else {
                let mut all = Vec::new();
                for name in &parsed.namespaces {
                    all.push((name.clone(), store.namespace(name)?.checkpoint()?));
                }
                all
            };
            store.close()?;
            for (name, outcome) in &outcomes {
                out.checkpoint(dir, name, outcome);
            }
            Ok(exit::OK)
        }
        Command::Namespaces { dir } => {
            let store = Store::open(dir, store_options(parsed))?;
            out.namespaces(&store.status());
            Ok(exit::OK)
        }
        Command::CreateNamespace { dir, name } => {
            let store = Store::open(dir, store_options(parsed))?;
            let result = store.create_namespace(name, key(parsed)?.as_ref())?;
            store.close()?;
            out.namespace_result("created", &result);
            Ok(exit::OK)
        }
        Command::DropNamespace { dir, name } => {
            let store = Store::open(dir, store_options(parsed))?;
            let result = store.drop_namespace(name, key(parsed)?.as_ref())?;
            store.close()?;
            out.namespace_result("dropped", &result);
            Ok(exit::OK)
        }
        Command::Import { dir, name, file, format, merge } => {
            let store = Store::open(dir, store_options(parsed))?;
            let mut progress = Progress::new(parsed.json);
            let mut show = |p| progress.show(p);
            if *merge {
                let report = store.namespace(name)?.import_file(file, *format, Some(&mut show))?;
                progress.done();
                store.close()?;
                out.merge(name, &report);
            } else {
                let report = store.import_file(name, file, *format, Some(&mut show))?;
                progress.done();
                store.close()?;
                out.import(&report);
            }
            Ok(exit::OK)
        }
        Command::Export { dir, file, format } => {
            let store = Store::open(dir, store_options(parsed))?;
            let mut progress = Progress::new(parsed.json);
            let report =
                store.namespace(one_namespace(parsed))?.export_file(file, *format, Some(&mut |p| progress.show(p)))?;
            progress.done();
            store.close()?;
            out.export(file, &report);
            Ok(exit::OK)
        }
        Command::Indexes { dir } => {
            let store = Store::open(dir, store_options(parsed))?;
            let ns = store.namespace(one_namespace(parsed))?;
            out.indexes(&ns.status(), &ns.catalog());
            Ok(exit::OK)
        }
        Command::CreateIndex { dir, path } => catalog_change(parsed, out, dir, "created index", |_| {
            Ok(CatalogChange::CreateIndex(IndexDef { path: attr_path(path)? }))
        }),
        Command::DropIndex { dir, path } => catalog_change(parsed, out, dir, "dropped index", |_| {
            Ok(CatalogChange::DropIndex(IndexDef { path: attr_path(path)? }))
        }),
        Command::AddConstraint { dir, kind, label, path } => {
            catalog_change(parsed, out, dir, "added constraint", |_| {
                Ok(CatalogChange::AddConstraint(constraint(*kind, label, path)?))
            })
        }
        Command::DropConstraint { dir, kind, label, path } => {
            catalog_change(parsed, out, dir, "dropped constraint", |_| {
                Ok(CatalogChange::DropConstraint(constraint(*kind, label, path)?))
            })
        }
        Command::Backup { dir, dest } => {
            let store = Store::open(dir, store_options(parsed))?;
            let report = store.backup(dest)?;
            store.close()?;
            out.backup(&report);
            verify_after(parsed, out, dest)
        }
        Command::Restore { dest, backup, archive, target } => {
            let sources = RestoreSources { backup: backup.clone(), archive: archive.clone() };
            let only: Vec<iwdb::NamespaceName> = parsed
                .namespaces
                .iter()
                .map(|n| iwdb::NamespaceName::new(n.as_str()).map_err(iwdb_engine_error))
                .collect::<Result<_, _>>()?;
            let only = (!only.is_empty()).then_some(only.as_slice());
            let report = iwdb::restore_namespaces(dest, &sources, *target, only)?;
            out.restore(&report);
            verify_after(parsed, out, dest)
        }
        Command::Verify { dir } => {
            let report = iwdb::verify(dir)?;
            out.verify(&report);
            Ok(if report.is_ok() { exit::OK } else { exit::DAMAGE })
        }
    }
}

/// Verify what a backup or restore wrote, unless `--no-verify`.
fn verify_after(parsed: &Parsed, out: &Out, dir: &std::path::Path) -> Result<u8, Error> {
    if parsed.no_verify {
        return Ok(exit::OK);
    }
    let report = iwdb::verify(dir)?;
    out.verify(&report);
    Ok(if report.is_ok() { exit::OK } else { exit::DAMAGE })
}

fn iwdb_engine_error(e: iwdb::CatalogError) -> Error {
    Error::Engine(e.into())
}

/// Progress of an import or export on stderr, at most once a second, if
/// stderr is a terminal and the output isn't JSON.
struct Progress {
    on: bool,
    last: Option<std::time::Instant>,
}

impl Progress {
    fn new(json: bool) -> Self {
        use std::io::IsTerminal;
        Progress { on: !json && std::io::stderr().is_terminal(), last: None }
    }

    fn show(&mut self, p: iwdb::import::Progress) {
        if !self.on || self.last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1)) {
            return;
        }
        self.last = Some(std::time::Instant::now());
        match p.phase {
            iwdb::import::Phase::Reading => eprint!("\r{:.1} MiB read   ", p.bytes as f64 / (1 << 20) as f64),
            iwdb::import::Phase::Writing => eprint!("\r{:.1} MiB written   ", p.bytes as f64 / (1 << 20) as f64),
            iwdb::import::Phase::Committing => eprint!("\r{} mutations committed   ", p.mutations),
        }
    }

    fn done(&self) {
        if self.on && self.last.is_some() {
            eprintln!();
        }
    }
}

fn one_namespace(parsed: &Parsed) -> &str {
    parsed.namespaces.first().map_or(iwdb::NAMESPACE, String::as_str)
}

fn key(parsed: &Parsed) -> Result<Option<IdempotencyKey>, Error> {
    Ok(parsed.key.as_deref().map(IdempotencyKey::new).transpose()?)
}

fn attr_path(path: &[String]) -> Result<AttrPath, Error> {
    AttrPath::new(path.iter().cloned()).map_err(iwdb_engine_error)
}

fn constraint(kind: ConstraintKind, label: &str, path: &[String]) -> Result<Constraint, Error> {
    Ok(Constraint { kind, label: Label::new(label).map_err(iwdb_engine_error)?, path: attr_path(path)? })
}

/// Open the store, commit one catalog change in a namespace, close.
fn catalog_change(
    parsed: &Parsed,
    out: &Out,
    dir: &std::path::Path,
    what: &str,
    make: impl FnOnce(&Store) -> Result<CatalogChange, Error>,
) -> Result<u8, Error> {
    let store = Store::open(dir, store_options(parsed))?;
    let change = make(&store)?;
    let options = CommitOptions { idempotency_key: key(parsed)? };
    let result = store.namespace(one_namespace(parsed))?.commit_catalog_with(change, &options)?;
    store.close()?;
    out.commit(what, result.seq, result.deduplicated);
    Ok(exit::OK)
}
