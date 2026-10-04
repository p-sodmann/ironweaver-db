//! Command-line parsing, by hand: a few commands and flags don't need a
//! parser crate (ADR 0012).

use std::path::PathBuf;

use iwdb::import::{ExportFormat, ImportFormat};
use iwdb::{CommitTime, ConstraintKind, FsyncPolicy, RestoreTarget};
use iwdb_server::client::ClientTls;

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
  restore <dest> [--backup <dir>] [--archive <archive>] [--seq <n> | --time <rfc3339>] [-n <ns>]...
                                restore into a new directory <dest> (the latest seq by
                                default), then verify it; -n restores only the named
                                namespaces (--seq needs exactly one)
  verify <dir>                  check every file and invariant of a data directory,
                                backup or archive; changes nothing
  namespaces <dir>              list the namespaces with their seq, size and indexes
  create-namespace <dir> <name> [--key <k>]
  drop-namespace <dir> <name> (--archive <archive> | --no-archive) [--key <k>]
                                create or drop a namespace (`default` can't be dropped);
                                with a key, a retry changes nothing
  indexes <dir> [-n <ns>]       list a namespace's indexes (with their state) and constraints
  create-index <dir> <path> [-n <ns>] [--key <k>]
  drop-index <dir> <path> [-n <ns>] [--key <k>]
                                <path> is an attribute path with dots: address.city
  add-constraint <dir> unique|required <label> <path> [-n <ns>] [--key <k>]
  drop-constraint <dir> unique|required <label> <path> [-n <ns>] [--key <k>]
  import <dir> <name> <file> [--format json|binary|lgf] [--merge]
                                create the namespace <name> from a graph file (a core JSON
                                or binary file, or LGF; detected unless --format) as one
                                checkpoint (with --archive, archived too; otherwise the
                                store's next open with its archive does it); with --merge,
                                upsert the file's nodes and edges into the existing
                                namespace <name> through commits
  export <dir> <file> [-n <ns>] [--format json|binary]
                                write a namespace's graph to <file> as a core file (JSON for
                                a .json file, binary otherwise, unless --format)

users, grants and API tokens (step 15a), on a data directory <dir> (the server
stopped) or, with --server <endpoint> instead of <dir>, on a running server:
  user create <dir> <name> [--admin]
  user passwd <dir> <name>      set a password (ends the user's sessions)
  user delete <dir> <name>
  user admin <dir> <name> on|off
                                make a user a server-wide admin, or not
  user grant <dir> <name> <namespace> read|write|admin
  user revoke <dir> <name> <namespace>
  user list <dir>
  token create <dir> <user> <name> [--expires <seconds>]
                                make an API token (printed once)
  token revoke <dir> <user> <name>
  token list <dir> <user>
  Passwords are read without echo from a terminal, or one line each from stdin.

query shell (a server, over gRPC):
  shell <endpoint> [-n <ns>]    an interactive client of the iwdb-server at <endpoint>
                                (https://host:port, or http:// without TLS): match patterns, lookups, commits and
                                catalog commands, one per line from stdin (\\help lists
                                them); with --json, one JSON object per answer; log in
                                with --token, --user (a password prompt) or \\login

  help, --help                  this text
  --version                     the version

options:
  --json                        machine-readable output (one JSON object)
  --fsync always|group|off      the fsync policy to open a store with (default always)
  --keep <n>                    checkpoints to keep (checkpoint; default 2)
  -n, --namespace <name>        the namespace (default: `default`; checkpoint: all unless
                                given; restore: only those given)
  --key <k>                     an idempotency key (1 to 255 bytes) for the change
  --no-verify                   don't verify after backup or restore
  --format <f>                  the file format of import or export
  --merge                       import into an existing namespace
  --server <endpoint>           user and token commands: act on a server, not a directory
  --token <token>               the API or session token to send (shell, --server); also
                                IWDB_TOKEN
  --user <name>                 log in as <name> (shell, --server); prompts for the password
  --tls-ca <file>               the CA (PEM) to verify the server against (shell, --server;
                                default: the system's trust store)
  --tls-cert <file>             a client certificate (PEM) for mTLS (shell, --server): it
                                authenticates as the user it names, without a token
  --tls-key <file>              the client certificate's private key (PEM)
  --admin                       user create: a server-wide admin
  --expires <seconds>           token create: the token expires after this long

exit codes: 0 ok, 1 damage found, 2 usage error, 3 locked (a store has the
directory open), 4 any other failure";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Status { dir: PathBuf },
    Checkpoint { dir: PathBuf },
    Backup { dir: PathBuf, dest: PathBuf },
    Restore { dest: PathBuf, backup: Option<PathBuf>, archive: Option<PathBuf>, target: RestoreTarget },
    Verify { dir: PathBuf },
    Namespaces { dir: PathBuf },
    CreateNamespace { dir: PathBuf, name: String },
    DropNamespace { dir: PathBuf, name: String },
    Indexes { dir: PathBuf },
    CreateIndex { dir: PathBuf, path: Vec<String> },
    DropIndex { dir: PathBuf, path: Vec<String> },
    AddConstraint { dir: PathBuf, kind: ConstraintKind, label: String, path: Vec<String> },
    DropConstraint { dir: PathBuf, kind: ConstraintKind, label: String, path: Vec<String> },
    Import { dir: PathBuf, name: String, file: PathBuf, format: Option<ImportFormat>, merge: bool },
    Export { dir: PathBuf, file: PathBuf, format: Option<ExportFormat> },
    Shell { endpoint: String },
    Accounts { action: crate::users::Action, target: crate::users::Target },
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
    /// `-n`: namespaces (one for the catalog commands, any number for restore and checkpoint).
    pub namespaces: Vec<String>,
    pub key: Option<String>,
    /// `--no-archive` was given.
    pub no_archive: bool,
    /// `--token` (shell).
    pub token: Option<String>,
    /// `--user` (shell).
    pub user: Option<String>,
    /// `--tls-ca`, `--tls-cert`, `--tls-key` (shell).
    pub tls: ClientTls,
}

fn dotted(path: &str) -> Vec<String> {
    path.split('.').map(str::to_owned).collect()
}

fn constraint_kind(word: &str) -> Result<ConstraintKind, String> {
    match word {
        "unique" => Ok(ConstraintKind::Unique),
        "required" => Ok(ConstraintKind::Required),
        other => Err(format!("a constraint is 'unique' or 'required', not '{}'", other)),
    }
}

pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut words = Vec::new();
    let (mut json, mut no_verify, mut no_archive, mut merge) = (false, false, false, false);
    let mut fsync = FsyncPolicy::Always;
    let mut keep = 2;
    let (mut archive, mut backup, mut seq, mut time) = (None, None, None, None);
    let (mut namespaces, mut key): (Vec<String>, Option<String>) = (Vec::new(), None);
    let mut format: Option<String> = None;
    let (mut server, mut token, mut user, mut expires): (Option<String>, Option<String>, Option<String>, Option<u64>) =
        (None, None, None, None);
    let mut admin = false;
    let mut tls = ClientTls::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().cloned().ok_or_else(|| format!("{} needs a value", flag));
        match arg.as_str() {
            "--json" => json = true,
            "--no-verify" => no_verify = true,
            "--no-archive" => no_archive = true,
            "--merge" => merge = true,
            "--admin" => admin = true,
            "--server" => server = Some(value("--server")?),
            "--token" => token = Some(value("--token")?),
            "--user" => user = Some(value("--user")?),
            "--tls-ca" => tls.ca = Some(PathBuf::from(value("--tls-ca")?)),
            "--tls-cert" => tls.cert = Some(PathBuf::from(value("--tls-cert")?)),
            "--tls-key" => tls.key = Some(PathBuf::from(value("--tls-key")?)),
            "--expires" => {
                expires =
                    Some(value("--expires")?.parse().map_err(|_| "--expires needs a number of seconds".to_owned())?)
            }
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
            "-n" | "--namespace" => namespaces.push(value("--namespace")?),
            "--key" => key = Some(value("--key")?),
            "--format" => format = Some(value("--format")?),
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
    if name == "user" || name == "token" {
        let command = accounts(name, rest, server, Credentials { token, user, tls }, admin, expires)?;
        if format.is_some() || merge || backup.is_some() || seq.is_some() || time.is_some() || key.is_some() {
            return Err(format!("{} takes none of --format, --merge, --backup, --seq, --time, --key", name));
        }
        if !namespaces.is_empty() {
            return Err(format!("{} takes no --namespace", name));
        }
        return Ok(Parsed {
            command,
            json,
            fsync,
            keep,
            archive,
            no_verify,
            namespaces,
            key,
            no_archive,
            token: None,
            user: None,
            tls: ClientTls::default(),
        });
    }
    if admin || expires.is_some() || server.is_some() {
        return Err(format!("{} takes none of --admin, --expires, --server", name));
    }
    if (token.is_some() || user.is_some() || tls.is_set()) && name != "shell" {
        return Err(format!("{} takes no --token, --user or --tls-*", name));
    }
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
            if seq.is_some() && namespaces.len() > 1 {
                return Err("restore --seq takes one namespace".into());
            }
            if format.is_some() {
                return Err("restore takes no --format".into());
            }
            return Ok(Parsed {
                command,
                json,
                fsync,
                keep,
                archive: None,
                no_verify,
                namespaces,
                key,
                no_archive,
                token: None,
                user: None,
                tls: ClientTls::default(),
            });
        }
        "shell" => {
            expect(1)?;
            Command::Shell { endpoint: rest.first().cloned().ok_or("shell needs an endpoint (https://host:port)")? }
        }
        "verify" => {
            expect(1)?;
            Command::Verify { dir: path(0, "a directory")? }
        }
        "namespaces" => {
            expect(1)?;
            Command::Namespaces { dir: path(0, "a directory")? }
        }
        "create-namespace" => {
            expect(2)?;
            Command::CreateNamespace {
                dir: path(0, "a directory")?,
                name: rest.get(1).cloned().ok_or("create-namespace needs a name")?,
            }
        }
        "drop-namespace" => {
            expect(2)?;
            if archive.is_none() == !no_archive {
                return Err("drop-namespace archives the namespace's remaining WAL: pass --archive <archive> if the store archives its WAL, or --no-archive".into());
            }
            Command::DropNamespace {
                dir: path(0, "a directory")?,
                name: rest.get(1).cloned().ok_or("drop-namespace needs a name")?,
            }
        }
        "indexes" => {
            expect(1)?;
            Command::Indexes { dir: path(0, "a directory")? }
        }
        "create-index" | "drop-index" => {
            expect(2)?;
            let dir = path(0, "a directory")?;
            let path = dotted(rest.get(1).ok_or_else(|| format!("{} needs an attribute path", name))?);
            if name == "create-index" { Command::CreateIndex { dir, path } } else { Command::DropIndex { dir, path } }
        }
        "add-constraint" | "drop-constraint" => {
            expect(4)?;
            let dir = path(0, "a directory")?;
            let kind = constraint_kind(rest.get(1).ok_or_else(|| format!("{} needs unique or required", name))?)?;
            let label = rest.get(2).cloned().ok_or_else(|| format!("{} needs a label", name))?;
            let path = dotted(rest.get(3).ok_or_else(|| format!("{} needs an attribute path", name))?);
            if name == "add-constraint" {
                Command::AddConstraint { dir, kind, label, path }
            } else {
                Command::DropConstraint { dir, kind, label, path }
            }
        }
        "import" => {
            expect(3)?;
            Command::Import {
                dir: path(0, "a directory")?,
                name: rest.get(1).cloned().ok_or("import needs a namespace name")?,
                file: path(2, "a file")?,
                format: format.take().map(|f| f.parse::<ImportFormat>()).transpose()?,
                merge: std::mem::take(&mut merge),
            }
        }
        "export" => {
            expect(2)?;
            Command::Export {
                dir: path(0, "a directory")?,
                file: path(1, "a file")?,
                format: format.take().map(|f| f.parse::<ExportFormat>()).transpose()?,
            }
        }
        other => return Err(format!("unknown command '{}'", other)),
    };
    if format.is_some() {
        return Err(format!("{} takes no --format", name));
    }
    if merge {
        return Err(format!("{} takes no --merge", name));
    }
    if backup.is_some() || seq.is_some() || time.is_some() {
        return Err(format!("{} takes no --backup, --seq or --time", name));
    }
    let takes_namespace = matches!(
        name.as_str(),
        "checkpoint"
            | "indexes"
            | "create-index"
            | "drop-index"
            | "add-constraint"
            | "drop-constraint"
            | "export"
            | "shell"
    );
    if !namespaces.is_empty() && !takes_namespace {
        return Err(format!("{} takes no --namespace", name));
    }
    let takes_key = matches!(
        name.as_str(),
        "create-namespace" | "drop-namespace" | "create-index" | "drop-index" | "add-constraint" | "drop-constraint"
    );
    if key.is_some() && !takes_key {
        return Err(format!("{} takes no --key", name));
    }
    Ok(Parsed { command, json, fsync, keep, archive, no_verify, namespaces, key, no_archive, token, user, tls })
}

/// `--token`, `--user` and `--tls-*`: how to reach a server and log in.
struct Credentials {
    token: Option<String>,
    user: Option<String>,
    tls: ClientTls,
}

/// `user ...` and `token ...`: the action and where it acts.
fn accounts(
    name: &str,
    rest: &[String],
    server: Option<String>,
    credentials: Credentials,
    admin: bool,
    expires: Option<u64>,
) -> Result<Command, String> {
    use crate::users::{Action, Target, TokenAction, UserAction};
    let Some((action, rest)) = rest.split_first() else {
        return Err(format!("{} needs an action (see --help)", name));
    };
    let Credentials { token, user, tls } = credentials;
    let (target, args) = match server {
        Some(endpoint) => (Target::Server { endpoint, token, user, tls }, rest),
        None => {
            if token.is_some() || user.is_some() || tls.is_set() {
                return Err("--token, --user and --tls-* go with --server".into());
            }
            let (dir, args) =
                rest.split_first().ok_or_else(|| format!("{} {} needs a directory or --server", name, action))?;
            (Target::Dir(PathBuf::from(dir)), args)
        }
    };
    let usage = |n: usize, what: &str| -> Result<(), String> {
        if args.len() == n { Ok(()) } else { Err(format!("usage: iwctl {} {} <dir> {}", name, action, what)) }
    };
    let arg = |i: usize| args[i].clone();
    let role =
        |word: &str| iwdb::Role::parse(word).ok_or_else(|| format!("a role is read, write or admin, not '{}'", word));
    if admin && !(name == "user" && action == "create") {
        return Err("--admin goes with user create".into());
    }
    if expires.is_some() && !(name == "token" && action == "create") {
        return Err("--expires goes with token create".into());
    }
    let action = match (name, action.as_str()) {
        ("user", "create") => {
            usage(1, "<name> [--admin]").map(|()| Action::User(UserAction::Create { name: arg(0), admin }))?
        }
        ("user", "passwd") => usage(1, "<name>").map(|()| Action::User(UserAction::Passwd { name: arg(0) }))?,
        ("user", "delete") => usage(1, "<name>").map(|()| Action::User(UserAction::Delete { name: arg(0) }))?,
        ("user", "admin") => {
            usage(2, "<name> on|off")?;
            let admin = match args[1].as_str() {
                "on" => true,
                "off" => false,
                other => return Err(format!("user admin takes on or off, not '{}'", other)),
            };
            Action::User(UserAction::Admin { name: arg(0), admin })
        }
        ("user", "grant") => {
            usage(3, "<name> <namespace> read|write|admin")?;
            Action::User(UserAction::Grant { name: arg(0), namespace: arg(1), role: role(&args[2])? })
        }
        ("user", "revoke") => {
            usage(2, "<name> <namespace>")?;
            Action::User(UserAction::Revoke { name: arg(0), namespace: arg(1) })
        }
        ("user", "list") => usage(0, "").map(|()| Action::User(UserAction::List))?,
        ("token", "create") => {
            usage(2, "<user> <name> [--expires <seconds>]")?;
            Action::Token(TokenAction::Create {
                user: arg(0),
                name: arg(1),
                expires: expires.map(std::time::Duration::from_secs),
            })
        }
        ("token", "revoke") => {
            usage(2, "<user> <name>").map(|()| Action::Token(TokenAction::Revoke { user: arg(0), name: arg(1) }))?
        }
        ("token", "list") => usage(1, "<user>").map(|()| Action::Token(TokenAction::List { user: arg(0) }))?,
        (_, other) => return Err(format!("unknown action '{} {}' (see --help)", name, other)),
    };
    Ok(Command::Accounts { action, target })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwdb::Role;
    use std::assert_matches;

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
        assert_matches!(p.command, Command::Restore { target: RestoreTarget::Time(_), .. });
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
    }

    #[test]
    fn user_and_token_commands() {
        use crate::users::{Action, Target, TokenAction, UserAction};
        let p = parse_words("user create d ann --admin").expect("parse");
        assert_eq!(
            p.command,
            Command::Accounts {
                action: Action::User(UserAction::Create { name: "ann".into(), admin: true }),
                target: Target::Dir("d".into())
            }
        );
        let p = parse_words("user grant --server http://h:1 ann social write --token t").expect("parse");
        assert_eq!(
            p.command,
            Command::Accounts {
                action: Action::User(UserAction::Grant {
                    name: "ann".into(),
                    namespace: "social".into(),
                    role: Role::Write
                }),
                target: Target::Server {
                    endpoint: "http://h:1".into(),
                    token: Some("t".into()),
                    user: None,
                    tls: ClientTls::default()
                }
            }
        );
        let p = parse_words("token create d ann ci --expires 60").expect("parse");
        assert_matches!(
            p.command,
            Command::Accounts { action: Action::Token(TokenAction::Create { expires: Some(_), .. }), .. }
        );
        for bad in [
            "user",
            "user create",
            "user create d",
            "user frob d",
            "user grant d ann ns owner",
            "user admin d ann maybe",
            "user list d --admin",
            "user list d --token t",
            "token create d ann ci x",
            "user list d --expires 3",
            "status d --admin",
            "status d --token t",
        ] {
            assert!(parse_words(bad).is_err(), "{}", bad);
        }
        let p = parse_words("shell http://h:1 --user ann").expect("parse");
        assert_eq!(p.user.as_deref(), Some("ann"));
        let p = parse_words("shell https://h:1 --tls-ca ca.pem --tls-cert c.pem --tls-key k.pem").expect("parse");
        let tls = ClientTls { ca: Some("ca.pem".into()), cert: Some("c.pem".into()), key: Some("k.pem".into()) };
        assert_eq!(p.tls, tls);
        let p = parse_words("user list --server https://h:1 --tls-ca ca.pem").expect("parse");
        assert_matches!(
            p.command,
            Command::Accounts { target: Target::Server { tls: ClientTls { ca: Some(_), .. }, .. }, .. }
        );
        for bad in ["status d --tls-ca ca.pem", "user list d --tls-cert c.pem", "shell https://h:1 --tls-key"] {
            assert!(parse_words(bad).is_err(), "{}", bad);
        }
        assert_eq!(parse_words("status d --version").expect("parse").command, Command::Version);
    }
}
