//! `iwctl user ...` and `iwctl token ...` (step 15a): users, grants and API
//! tokens, on a data directory (offline: the server must be stopped; the
//! directory's file permissions are the boundary) or on a server
//! (`--server`, through its `AuthService` with the caller's credentials).
//! Both run through the `Accounts` trait (design rule 8): the embedded
//! store's on a directory, the gRPC client's on a server.
//!
//! Passwords are read without echo from a terminal, or as one line each
//! from stdin when it isn't one (scripts). Neither a password nor a token
//! is put on the command line or in an error.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwdb::{Embedded, QueryConfig, Role, Secret, Store, StoreOptions};
use iwdb_query::exec::block_on;
use iwdb_query::{Accounts, Error, TokenInfo, UserInfo};
use iwdb_server::client::Remote;
use serde_json::{Value, json};

use crate::output::Out;

/// What a `user` command does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserAction {
    Create { name: String, admin: bool },
    Passwd { name: String },
    Delete { name: String },
    Admin { name: String, admin: bool },
    Grant { name: String, namespace: String, role: Role },
    Revoke { name: String, namespace: String },
    List,
}

/// What a `token` command does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenAction {
    Create { user: String, name: String, expires: Option<Duration> },
    Revoke { user: String, name: String },
    List { user: String },
}

/// Where a `user` or `token` command acts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Dir(PathBuf),
    /// A server, with the caller's credentials: `--token` (or
    /// `IWDB_TOKEN`), or `--user` and a password prompt.
    Server {
        endpoint: String,
        token: Option<String>,
        user: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    User(UserAction),
    Token(TokenAction),
}

/// Read a secret: without echo from a terminal (after `prompt` on stderr),
/// or one line from stdin.
pub fn read_secret(prompt: &str) -> io::Result<Secret> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(Secret::new(line.trim_end_matches(['\n', '\r'])));
    }
    eprint!("{}", prompt);
    io::stderr().flush()?;
    let echo = set_echo(false);
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    if echo {
        set_echo(true);
    }
    eprintln!();
    read?;
    Ok(Secret::new(line.trim_end_matches(['\n', '\r'])))
}

/// Turn the terminal's echo off or on (`stty`); whether it worked.
fn set_echo(on: bool) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("stty")
            .arg(if on { "echo" } else { "-echo" })
            .stdin(std::process::Stdio::inherit())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = on;
        false
    }
}

/// A new password: twice on a terminal (they must match), once from stdin.
fn new_password(prompt: &str) -> Result<Secret, String> {
    let first = read_secret(prompt).map_err(|e| e.to_string())?;
    if io::stdin().is_terminal() {
        let again = read_secret("again: ").map_err(|e| e.to_string())?;
        if again != first {
            return Err("the passwords differ".into());
        }
    }
    Ok(first)
}

/// Run a `user` or `token` command; the exit code.
pub fn run(action: &Action, target: &Target, options: StoreOptions, out: &Out) -> u8 {
    match target {
        Target::Dir(dir) => offline(action, dir, options, out),
        Target::Server { endpoint, token, user } => {
            let remote = match connect(endpoint, token.as_deref(), user.as_deref()) {
                Ok(remote) => remote,
                Err(e) => return report(out, &e),
            };
            finish(out, execute(&remote, action))
        }
    }
}

/// A client of `endpoint` with the caller's credentials.
pub fn connect(endpoint: &str, token: Option<&str>, user: Option<&str>) -> Result<Remote, Error> {
    let remote = Remote::connect(endpoint)?;
    let token = token.map(str::to_owned).or_else(|| std::env::var("IWDB_TOKEN").ok().filter(|t| !t.is_empty()));
    if let Some(token) = token {
        remote.set_token(Some(Secret::new(token)));
    } else if let Some(user) = user {
        let password = read_secret(&format!("password for {}: ", user)).map_err(|e| Error::invalid(e.to_string()))?;
        block_on(remote.login(user, password))?;
    }
    Ok(remote)
}

fn offline(action: &Action, dir: &Path, options: StoreOptions, out: &Out) -> u8 {
    let store = match Store::open(dir, options) {
        Ok(store) => store,
        Err(e) => {
            out.error(&e);
            return crate::exit_code(&e);
        }
    };
    let config = QueryConfig { workers: 1, queue: 4, ..QueryConfig::default() };
    let db = match Embedded::new(store, config) {
        Ok(db) => db,
        Err(e) => return report(out, &e),
    };
    let result = execute(&db, action);
    if let Err(e) = db.close() {
        out.error(&e);
        return crate::exit_code(&e);
    }
    finish(out, result)
}

fn report(out: &Out, e: &Error) -> u8 {
    if out.json {
        println!("{}", json!({"error": {"code": e.code().as_str(), "message": e.message()}}));
    } else {
        eprintln!("iwctl: {}: {}", e.code(), e.message());
    }
    crate::exit::FAILED
}

/// Print a command's answer.
fn finish(out: &Out, result: Result<Value, Error>) -> u8 {
    match result {
        Ok(value) => {
            if out.json {
                println!("{}", value);
            } else if let Some(text) = value.get("text").and_then(Value::as_str) {
                println!("{}", text);
            }
            crate::exit::OK
        }
        Err(e) => report(out, &e),
    }
}

fn user_json(u: &UserInfo) -> Value {
    let grants: serde_json::Map<String, Value> = u.grants.iter().map(|(n, r)| (n.clone(), json!(r.as_str()))).collect();
    json!({"name": u.name, "admin": u.admin, "grants": grants})
}

fn user_text(u: &UserInfo) -> String {
    let grants: Vec<String> = u.grants.iter().map(|(n, r)| format!("{}={}", n, r)).collect();
    format!(
        "{}{}: {}",
        u.name,
        if u.admin { " (admin)" } else { "" },
        if grants.is_empty() { "no grants".to_owned() } else { grants.join(", ") }
    )
}

fn ms(ms: Option<u64>) -> String {
    ms.map_or_else(
        || "never".to_owned(),
        |ms| iwdb::CommitTime(i64::try_from(ms.saturating_mul(1000)).unwrap_or(i64::MAX)).to_string(),
    )
}

fn token_json(t: &TokenInfo) -> Value {
    json!({"user": t.user, "name": t.name, "created_ms": t.created_ms, "expires_ms": t.expires_ms})
}

/// One command through the `Accounts` trait: what to print, as JSON (with
/// a `text` field for people).
fn execute<A: Accounts>(db: &A, action: &Action) -> Result<Value, Error> {
    let pw = |prompt: &str| new_password(prompt).map_err(Error::invalid);
    Ok(match action {
        Action::User(UserAction::Create { name, admin }) => {
            let user = block_on(db.create_user(name, pw(&format!("password for the new user {}: ", name))?, *admin))?;
            let mut v = user_json(&user);
            v["text"] = json!(format!("created user {}", user_text(&user)));
            v
        }
        Action::User(UserAction::Passwd { name }) => {
            block_on(db.set_password(name, pw(&format!("new password for {}: ", name))?, None))?;
            json!({"user": name, "text": format!("set the password of {} (its sessions ended)", name)})
        }
        Action::User(UserAction::Delete { name }) => {
            block_on(db.delete_user(name))?;
            json!({"user": name, "deleted": true, "text": format!("deleted user {}", name)})
        }
        Action::User(UserAction::Admin { name, admin }) => {
            let user = block_on(db.set_admin(name, *admin))?;
            let mut v = user_json(&user);
            v["text"] = json!(user_text(&user));
            v
        }
        Action::User(UserAction::Grant { name, namespace, role }) => {
            let user = block_on(db.grant(name, namespace, *role))?;
            let mut v = user_json(&user);
            v["text"] = json!(user_text(&user));
            v
        }
        Action::User(UserAction::Revoke { name, namespace }) => {
            let user = block_on(db.revoke(name, namespace))?;
            let mut v = user_json(&user);
            v["text"] = json!(user_text(&user));
            v
        }
        Action::User(UserAction::List) => {
            let users = block_on(db.users())?;
            let text = if users.is_empty() {
                "no users".to_owned()
            } else {
                users.iter().map(user_text).collect::<Vec<_>>().join("\n")
            };
            json!({"users": users.iter().map(user_json).collect::<Vec<_>>(), "text": text})
        }
        Action::Token(TokenAction::Create { user, name, expires }) => {
            let token = block_on(db.create_token(user, name, *expires))?;
            let mut v = token_json(&token.info);
            v["token"] = json!(token.token.expose());
            v["text"] = json!(format!(
                "API token {} of {} (expires: {}); shown this once:\n{}",
                name,
                user,
                ms(token.info.expires_ms),
                token.token.expose()
            ));
            v
        }
        Action::Token(TokenAction::Revoke { user, name }) => {
            block_on(db.revoke_token(user, name))?;
            json!({"user": user, "name": name, "revoked": true, "text": format!("revoked token {} of {}", name, user)})
        }
        Action::Token(TokenAction::List { user }) => {
            let tokens = block_on(db.tokens(user))?;
            let text = if tokens.is_empty() {
                format!("{} has no API tokens", user)
            } else {
                tokens
                    .iter()
                    .map(|t| format!("{} (created {}, expires {})", t.name, ms(Some(t.created_ms)), ms(t.expires_ms)))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            json!({"tokens": tokens.iter().map(token_json).collect::<Vec<_>>(), "text": text})
        }
    })
}
