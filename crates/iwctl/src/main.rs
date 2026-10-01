//! `iwctl`: the admin CLI for local data directories (step 7; the query
//! shell comes in step 14). It parses arguments, calls the library
//! (`iwdb`) and prints the result; every operation lives in the library
//! (design rule 8). `documentation/iwctl.md` describes the commands and
//! the exit codes.

mod args;
mod output;

use std::process::ExitCode;

use iwdb::{Error, RestoreSources, Store, StoreOptions};

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
fn exit_code(error: &Error) -> u8 {
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
        Command::Status { dir } => {
            let status = iwdb::status(dir, store_options(parsed))?;
            out.status(&status);
            Ok(if status.files.in_use { exit::LOCKED } else { exit::OK })
        }
        Command::Checkpoint { dir } => {
            let store = Store::open(dir, store_options(parsed))?;
            let outcome = store.checkpoint()?;
            store.close()?;
            out.checkpoint(dir, &outcome);
            Ok(exit::OK)
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
            let report = iwdb::restore(dest, &sources, *target)?;
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
