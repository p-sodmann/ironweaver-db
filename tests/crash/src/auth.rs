//! The auth scenario (step 15a, ADR 0043): kill -9 a store while it
//! creates users, changes passwords, grants and revokes roles and makes and
//! revokes API tokens, then check that recovery returns the users of the
//! acknowledged changes, or of one more (the one in flight): each change
//! is one commit to the system namespace, so it is all or nothing.
//!
//! The child (`iwdb-crash auth --dir D --rule R`) runs [`script`] once,
//! saying `ack` after each change, and pauses at its failpoint (`paused`).
//! The parent ([`run`]) kills it there, opens the store, and compares the
//! [`Users`] it finds (users, admin flags, grants, tokens, and which
//! password each user's hash accepts) with the model after `acks` and
//! `acks + 1` changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use iwdb::auth::HashParams;
use iwdb::{Role, Secret, Store};
use iwdb_storage::failpoint::Rule;

use crate::child::{fail, fail_fs, say, wait_for_kill};
use crate::harness::{CHILD_TIMEOUT, ChildProcess, Plan};
use crate::script::{Policy, check_options, child_options};

/// Cheap hashes: the scenario is about commits, not hashing.
const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };

/// One change.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    CreateNamespace(&'static str),
    CreateUser(&'static str, &'static str, bool),
    SetPassword(&'static str, &'static str),
    SetAdmin(&'static str, bool),
    Grant(&'static str, &'static str, Role),
    Revoke(&'static str, &'static str),
    CreateToken(&'static str, &'static str),
    RevokeToken(&'static str, &'static str),
    DeleteUser(&'static str),
}

/// The child's changes, in order.
pub fn script() -> Vec<Change> {
    use Change::*;
    vec![
        CreateNamespace("social"),
        CreateUser("root", "root-password-1", true),
        CreateUser("ann", "ann-password-1", false),
        CreateUser("bob", "bob-password-1", false),
        Grant("ann", "social", Role::Write),
        Grant("bob", "default", Role::Read),
        CreateToken("ann", "ci"),
        CreateToken("bob", "laptop"),
        SetPassword("bob", "bob-password-2"),
        Grant("ann", "social", Role::Admin),
        Revoke("bob", "default"),
        SetAdmin("ann", true),
        RevokeToken("ann", "ci"),
        CreateToken("ann", "deploy"),
        DeleteUser("bob"),
        SetPassword("ann", "ann-password-2"),
        SetAdmin("root", false),
    ]
}

/// Every password the script gives anyone: recovery is checked for which
/// of them each user's hash accepts.
const PASSWORDS: [&str; 5] =
    ["root-password-1", "ann-password-1", "ann-password-2", "bob-password-1", "bob-password-2"];

/// What a store's users look like: per user its admin flag, grants by
/// namespace name, token names and the password its hash accepts.
pub type Users = BTreeMap<String, (bool, BTreeMap<String, Role>, Vec<String>, Option<String>)>;

/// The users after the first `n` changes of the script.
pub fn model(n: usize) -> Users {
    let mut users = Users::new();
    for change in script().into_iter().take(n) {
        match change {
            Change::CreateNamespace(_) => {}
            Change::CreateUser(name, pw, admin) => {
                users.insert(name.into(), (admin, BTreeMap::new(), Vec::new(), Some(pw.into())));
            }
            Change::SetPassword(name, pw) => users.get_mut(name).map_or((), |u| u.3 = Some(pw.into())),
            Change::SetAdmin(name, admin) => users.get_mut(name).map_or((), |u| u.0 = admin),
            Change::Grant(name, ns, role) => users.get_mut(name).map_or((), |u| {
                u.1.insert(ns.into(), role);
            }),
            Change::Revoke(name, ns) => users.get_mut(name).map_or((), |u| {
                u.1.remove(ns);
            }),
            Change::CreateToken(name, token) => users.get_mut(name).map_or((), |u| {
                u.2.push(token.into());
                u.2.sort();
            }),
            Change::RevokeToken(name, token) => users.get_mut(name).map_or((), |u| u.2.retain(|t| t != token)),
            Change::DeleteUser(name) => {
                users.remove(name);
            }
        }
    }
    users
}

/// The users of an open store.
pub fn observe<F: iwdb::LogFs + Clone + Send + Sync + 'static>(store: &Store<F>) -> Result<Users, String>
where
    F::File: Send,
{
    let users = store.users();
    let mut out = Users::new();
    for user in users.list().map_err(|e| e.to_string())? {
        let tokens = users.tokens(&user.name).map_err(|e| e.to_string())?.into_iter().map(|t| t.name).collect();
        let mut password = None;
        for pw in PASSWORDS {
            if users.verify(&user.name, &Secret::new(pw)).map_err(|e| e.to_string())? {
                password = Some(pw.to_owned());
            }
        }
        out.insert(user.name.clone(), (user.admin, user.grants, tokens, password));
    }
    Ok(out)
}

fn apply<F: iwdb::LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, change: &Change) -> Result<(), String>
where
    F::File: Send,
{
    let users = store.users().with_params(FAST);
    fn e(e: impl std::fmt::Display) -> String {
        e.to_string()
    }
    match change {
        Change::CreateNamespace(name) => store.create_namespace(name, None).map(drop).map_err(e),
        Change::CreateUser(name, pw, admin) => users.create(name, &Secret::new(*pw), *admin).map(drop).map_err(e),
        Change::SetPassword(name, pw) => users.set_password(name, &Secret::new(*pw), None).map_err(e),
        Change::SetAdmin(name, admin) => users.set_admin(name, *admin).map(drop).map_err(e),
        Change::Grant(name, ns, role) => users.grant(name, ns, *role).map(drop).map_err(e),
        Change::Revoke(name, ns) => users.revoke(name, ns).map(drop).map_err(e),
        Change::CreateToken(name, token) => users.create_token(name, token, None).map(drop).map_err(e),
        Change::RevokeToken(name, token) => users.revoke_token(name, token).map_err(e),
        Change::DeleteUser(name) => users.delete(name).map_err(e),
    }
}

/// The child's arguments.
#[derive(Clone, Debug)]
pub struct AuthArgs {
    pub dir: PathBuf,
    pub policy: Policy,
    pub rules: Vec<Rule>,
}

impl AuthArgs {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["--dir".into(), self.dir.display().to_string(), "--policy".into(), self.policy.to_string()];
        for rule in &self.rules {
            args.push("--rule".into());
            args.push(rule.to_string());
        }
        args
    }

    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = AuthArgs { dir: PathBuf::new(), policy: Policy::Always, rules: Vec::new() };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{} needs a value", flag))?;
            match flag.as_str() {
                "--dir" => parsed.dir = value.into(),
                "--policy" => parsed.policy = value.parse()?,
                "--rule" => parsed.rules.push(value.parse()?),
                other => return Err(format!("unknown auth option '{}'", other)),
            }
        }
        Ok(parsed)
    }
}

/// The auth child's main: never returns. Lines: `ack` (a change
/// returned), `paused <rule> <path>`, `done`, `error <message>`.
pub fn main(args: &AuthArgs) -> ! {
    let fs = fail_fs(&args.rules);
    let mut options = child_options(args.policy, 2, None);
    options.checkpoint.background = true;
    let store = match Store::open_with(fs, &args.dir, options) {
        Ok(store) => store,
        Err(e) => fail("open", e),
    };
    say("open");
    for change in script() {
        if let Err(e) = apply(&store, &change) {
            fail(&format!("{:?}", change), e);
        }
        say("ack");
    }
    say("done");
    wait_for_kill()
}

/// What [`run`] saw.
#[derive(Clone, Debug, Default)]
pub struct AuthSummary {
    /// Cycles whose child was killed at its failpoint.
    pub paused: usize,
    /// How many recovered with the change in flight applied.
    pub in_flight_kept: usize,
}

/// Run each plan once: a child on a new directory, killed at the plan's
/// failpoint (or after `done`), then recovery checked against the model.
pub fn run(exe: &Path, policy: Policy, work: &Path, plans: &[Plan]) -> Result<AuthSummary, String> {
    let mut summary = AuthSummary::default();
    for (i, plan) in plans.iter().enumerate() {
        let dir = work.join(format!("auth-{}", i));
        let rules = match plan {
            Plan::At(rule) => vec![rule.clone()],
            _ => Vec::new(),
        };
        // The store exists before the child starts: its failpoints count
        // the scenario's writes only
        Store::open(&dir, check_options(policy, 2, None)).and_then(Store::close).map_err(|e| e.to_string())?;
        let args = AuthArgs { dir: dir.clone(), policy, rules };
        let mut child = ChildProcess::spawn_command(exe, "auth", args.to_args(), &work.join(format!("auth-{}.err", i)))
            .map_err(|e| e.to_string())?;
        let line = child.wait_for(&["paused", "done", "error"], CHILD_TIMEOUT);
        let outcome = child.kill().map_err(|e| e.to_string())?;
        if let Some(error) = outcome.lines.iter().find(|l| l.starts_with("error")) {
            return Err(format!("plan {:?}: the child failed: {}", plan, error));
        }
        if line.as_deref().is_some_and(|l| l.starts_with("paused")) {
            summary.paused += 1;
        }
        let acks = outcome.lines.iter().filter(|l| *l == "ack").count();
        let report = iwdb::verify(&dir).map_err(|e| e.to_string())?;
        if !report.is_ok() {
            return Err(format!("plan {:?}: verify found {:#?}", plan, report.problems));
        }
        let store = Store::open(&dir, check_options(policy, 2, None)).map_err(|e| format!("plan {:?}: {}", plan, e))?;
        let found = observe(&store).map_err(|e| format!("plan {:?}: {}", plan, e))?;
        // A change that leaves the users as they were (creating the
        // namespace) can't be told apart: then the test doesn't go on
        let ambiguous = model(acks) == model(acks + 1);
        if found == model(acks) {
        } else if acks < script().len() && found == model(acks + 1) {
            summary.in_flight_kept += 1;
        } else {
            return Err(format!(
                "plan {:?}: after {} acknowledged changes the store has {:?}, expected {:?} or {:?}",
                plan,
                acks,
                found,
                model(acks),
                model(acks + 1)
            ));
        }
        // The recovered store goes on: the next change of the script works
        if !ambiguous
            && let Some(next) = script().get(acks + usize::from(found != model(acks)))
            && let Err(e) = apply(&store, next)
        {
            return Err(format!("plan {:?}: after recovery {:?} failed: {}", plan, next, e));
        }
        store.close().map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(summary)
}
