//! Command-line parsing, by hand: five commands and a few flags don't need
//! a parser crate (ADR 0012).

use std::path::PathBuf;

use iwdb::{CommitTime, FsyncPolicy, RestoreTarget};

pub const USAGE: &str = "\
usage: iwctl [--json] <command> [options]

commands (local data directories; a store must not have them open, except
for status, which then shows what the files say):
  status <dir>                  what a data directory, backup or archive holds; opens a
                                data directory (runs recovery) unless a store has it open
  checkpoint <dir> (--archive <archive> | --no-archive)
                                write a checkpoint and cut the WAL; if the store archives
                                its WAL, name the archive, or the segments it removes are
                                not archived
  backup <dir> <dest>           back up a data directory into a new directory <dest>,
                                then verify it
  restore <dest> [--backup <dir>] [--archive <archive>] [--seq <n> | --time <rfc3339>]
                                restore into a new directory <dest> (the latest seq by
                                default), then verify it
  verify <dir>                  check every file and invariant of a data directory,
                                backup or archive; changes nothing
  help, --help                  this text
  --version                     the version

options:
  --json                        machine-readable output (one JSON object)
  --fsync always|group|off      the fsync policy to open a store with (default always)
  --keep <n>                    checkpoints to keep (checkpoint; default 2)
  --no-verify                   don't verify after backup or restore

exit codes: 0 ok, 1 damage found, 2 usage error, 3 locked (a store has the
directory open), 4 any other failure";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Status { dir: PathBuf },
    Checkpoint { dir: PathBuf },
    Backup { dir: PathBuf, dest: PathBuf },
    Restore { dest: PathBuf, backup: Option<PathBuf>, archive: Option<PathBuf>, target: RestoreTarget },
    Verify { dir: PathBuf },
    Help,
    Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub command: Command,
    pub json: bool,
    pub fsync: FsyncPolicy,
    pub keep: usize,
    /// The store's WAL archive (status, checkpoint, backup).
    pub archive: Option<PathBuf>,
    pub no_verify: bool,
}

pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut words = Vec::new();
    let (mut json, mut no_verify, mut no_archive) = (false, false, false);
    let mut fsync = FsyncPolicy::Always;
    let mut keep = 2;
    let (mut archive, mut backup, mut seq, mut time) = (None, None, None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().cloned().ok_or_else(|| format!("{} needs a value", flag));
        match arg.as_str() {
            "--json" => json = true,
            "--no-verify" => no_verify = true,
            "--no-archive" => no_archive = true,
            "--help" | "-h" => words.insert(0, "help".to_owned()),
            "--version" | "-V" => words.insert(0, "version".to_owned()),
            "--fsync" => {
                fsync = match value("--fsync")?.as_str() {
                    "always" => FsyncPolicy::Always,
                    // The store's default group commit window
                    "group" => FsyncPolicy::Group { max_delay: std::time::Duration::from_millis(10), max_batch: 64 },
                    "off" => FsyncPolicy::Off,
                    other => return Err(format!("unknown fsync policy '{}' (always, group or off)", other)),
                }
            }
            "--keep" => {
                keep = value("--keep")?.parse().map_err(|_| "--keep needs a number of at least 1".to_owned())?;
                if keep == 0 {
                    return Err("--keep needs a number of at least 1".into());
                }
            }
            "--archive" => archive = Some(PathBuf::from(value("--archive")?)),
            "--backup" => backup = Some(PathBuf::from(value("--backup")?)),
            "--seq" => seq = Some(value("--seq")?.parse::<u64>().map_err(|_| "--seq needs a number".to_owned())?),
            "--time" => time = Some(value("--time")?.parse::<CommitTime>()?),
            flag if flag.starts_with('-') && flag.len() > 1 => return Err(format!("unknown option '{}'", flag)),
            word => words.push(word.to_owned()),
        }
    }
    let Some((name, rest)) = words.split_first() else { return Err("no command given".into()) };
    let path = |i: usize, what: &str| rest.get(i).map(PathBuf::from).ok_or_else(|| format!("{} needs {}", name, what));
    let expect = |n: usize| {
        if rest.len() > n {
            return Err(format!("{}: unexpected argument '{}'", name, rest[n]));
        }
        Ok(())
    };
    let command = match name.as_str() {
        "help" => Command::Help,
        "version" => Command::Version,
        "status" => {
            expect(1)?;
            Command::Status { dir: path(0, "a directory")? }
        }
        "checkpoint" => {
            expect(1)?;
            if archive.is_none() == !no_archive {
                return Err("checkpoint removes WAL segments: pass --archive <archive> if the store archives its WAL, or --no-archive".into());
            }
            Command::Checkpoint { dir: path(0, "a directory")? }
        }
        "backup" => {
            expect(2)?;
            Command::Backup { dir: path(0, "a data directory")?, dest: path(1, "a destination")? }
        }
        "restore" => {
            expect(1)?;
            if backup.is_none() && archive.is_none() {
                return Err("restore needs --backup <dir>, --archive <archive>, or both".into());
            }
            let target = match (seq, time) {
                (Some(_), Some(_)) => return Err("restore takes --seq or --time, not both".into()),
                (Some(seq), None) => RestoreTarget::Seq(seq),
                (None, Some(time)) => RestoreTarget::Time(time),
                (None, None) => RestoreTarget::Latest,
            };
            let command = Command::Restore { dest: path(0, "a destination")?, backup, archive: archive.take(), target };
            return Ok(Parsed { command, json, fsync, keep, archive: None, no_verify });
        }
        "verify" => {
            expect(1)?;
            Command::Verify { dir: path(0, "a directory")? }
        }
        other => return Err(format!("unknown command '{}'", other)),
    };
    if backup.is_some() || seq.is_some() || time.is_some() {
        return Err(format!("{} takes no --backup, --seq or --time", name));
    }
    Ok(Parsed { command, json, fsync, keep, archive, no_verify })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(s: &str) -> Result<Parsed, String> {
        parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    }

    #[test]
    fn commands_and_flags() {
        let p = parse_words("--json status d").expect("parse");
        assert_eq!((p.command, p.json), (Command::Status { dir: "d".into() }, true));
        let p = parse_words("checkpoint d --keep 3 --archive a --fsync off").expect("parse");
        assert_eq!((p.keep, p.archive, p.fsync), (3, Some("a".into()), FsyncPolicy::Off));
        assert!(parse_words("checkpoint d").is_err(), "an explicit choice about archiving");
        assert!(parse_words("checkpoint d --no-archive").is_ok());
        let p = parse_words("restore r --backup b --seq 7").expect("parse");
        assert_eq!(
            p.command,
            Command::Restore {
                dest: "r".into(),
                backup: Some("b".into()),
                archive: None,
                target: RestoreTarget::Seq(7)
            }
        );
        let p = parse_words("restore r --archive a --time 2026-10-01T12:00:00Z").expect("parse");
        assert!(matches!(p.command, Command::Restore { target: RestoreTarget::Time(_), .. }));
        for bad in [
            "",
            "frobnicate",
            "status",
            "status a b",
            "backup d",
            "restore r",
            "restore r --backup b --seq 1 --time 2026-10-01T12:00:00Z",
            "restore r --backup b --time yesterday",
            "verify d --seq 3",
            "status d --keep 0",
            "status d --fsync sometimes",
            "status d --wat",
        ] {
            assert!(parse_words(bad).is_err(), "{}", bad);
        }
        assert_eq!(parse_words("--help").expect("parse").command, Command::Help);
        assert_eq!(parse_words("status d --version").expect("parse").command, Command::Version);
    }
}
