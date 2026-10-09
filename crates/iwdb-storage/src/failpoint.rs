//! Failpoints on every write-side file operation (feature `failpoints`,
//! step 6, ADR 0007).
//!
//! [`FailFs`] wraps any [`LogFs`] and runs each call through a list of
//! [`Rule`]s: at the chosen occurrence of a [`Call`] (optionally only for
//! paths containing a string), at a chosen point of it ([`When`]), it
//! fails the call, reports a full disk, pauses the thread, panics or
//! aborts the process ([`Action`]). It also records every call. Tests use
//! it in-process; the crash harness (`tests/crash`) uses it in a child
//! process that it kills at a pause.
//!
//! The rules belong to one `FailFs` (and its clones), not to the process,
//! so tests with their own `FailFs` run in parallel. Without the feature
//! this module doesn't exist, and [`StdFs`] has no failpoints: normal
//! builds pay nothing.
//!
//! **What a failpoint can't reach.** The core's `write_atomic` is one
//! call: a rule can act before it, while our writer writes into the
//! temporary file ([`When::Midway`]), when the writer is done
//! ([`When::WriterDone`]) and after it returned ([`When::After`]), but not
//! between the core's fsync of the temporary file and its rename. For a
//! process crash that point is the same as [`When::WriterDone`] (the
//! temporary file is complete, flushed into the page cache, and not
//! renamed; an fsync changes nothing a kill can observe).

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::io::{LogFile, LogFs, StdFs};

/// An enum of names used on command lines: the enum, `ALL`, `name()` and
/// `FromStr`, from one list of variants and their names.
macro_rules! named_enum {
    ($(#[$meta:meta])* $vis:vis enum $ty:ident ($what:literal) {
        $($(#[$vmeta:meta])* $variant:ident => $name:literal,)*
    }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        $vis enum $ty {
            $($(#[$vmeta])* $variant,)*
        }

        impl $ty {
            pub const ALL: [$ty; [$($name),*].len()] = [$($ty::$variant),*];

            pub fn name(self) -> &'static str {
                match self {
                    $($ty::$variant => $name,)*
                }
            }
        }

        impl FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                Self::ALL.into_iter().find(|v| v.name() == s).ok_or_else(|| format!("unknown {} '{}'", $what, s))
            }
        }
    };
}

named_enum! {
    /// A file operation of [`LogFs`] / [`LogFile`].
    pub enum Call("call") {
        Create => "create",
        OpenAppend => "open_append",
        Rename => "rename",
        SyncDir => "sync_dir",
        /// [`LogFile::write_all`].
        Write => "write",
        /// [`LogFile::sync`].
        Sync => "sync",
        WriteAtomic => "write_atomic",
        RemoveFile => "remove_file",
        Truncate => "truncate",
        /// [`LogFs::create_dir`] (a namespace's directory).
        CreateDir => "create_dir",
        /// [`LogFs::remove_dir_all`] (a dropped namespace's directory).
        RemoveDirAll => "remove_dir_all",
    }
}

named_enum! {
    /// Where in a call a rule acts.
    pub enum When("point") {
        /// Before the operation: if the action fails the call, nothing was done.
        Before => "before",
        /// Halfway: for [`Call::Write`], after the first half of the bytes is
        /// written; for [`Call::WriteAtomic`], after the writer wrote half of
        /// its first write (at most 4 KiB), flushed into the temporary file.
        /// A failing action leaves a torn frame, or a partial temporary file
        /// (which `write_atomic` removes).
        Midway => "midway",
        /// [`Call::WriteAtomic`] only: the writer is done and its output is
        /// flushed into the temporary file, which is not yet fsynced or
        /// renamed. A failing action makes `write_atomic` remove the file.
        WriterDone => "writer_done",
        /// After the operation succeeded. A failing action reports an error
        /// although the operation happened (a write or fsync that reached the
        /// disk and still failed).
        After => "after",
    }
}

named_enum! {
    /// What a rule does when it fires.
    pub enum Action("action") {
        /// Fail the call with an I/O error.
        Fail => "fail",
        /// Fail the call with `ENOSPC` (a full disk).
        NoSpace => "nospace",
        /// Call the pause handler ([`FailFs::set_pause`]; by default the thread
        /// blocks forever), then go on. A harness kills the process meanwhile.
        Pause => "pause",
        /// Panic.
        Panic => "panic",
        /// Abort the process (`std::process::abort`): no destructor runs, like
        /// `kill -9`.
        Abort => "abort",
    }
}

/// A failpoint: `action` at the `skip + 1`-th call of kind `call` whose
/// path contains `path`, at the point `when`. It fires once.
///
/// Its text form (for command lines) is
/// `<call>:<when>:<action>[:skip=<n>][:path=<substring>]`, for example
/// `remove_file:before:pause:skip=1:path=/wal/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rule {
    pub call: Call,
    pub when: When,
    pub action: Action,
    /// Matching calls to let through first.
    pub skip: u64,
    /// Only calls on a path containing this ("" matches every path), with
    /// `/` as the separator on every platform (a `\\` in the path counts
    /// as `/`, ADR 0058). Writes and fsyncs have the path of their file.
    pub path: String,
}

impl Rule {
    pub fn new(call: Call, when: When, action: Action) -> Self {
        Rule { call, when, action, skip: 0, path: String::new() }
    }

    pub fn skip(mut self, skip: u64) -> Self {
        self.skip = skip;
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    fn matches(&self, call: Call, when: When, path: &Path) -> bool {
        self.call == call
            && self.when == when
            && (self.path.is_empty() || path.to_string_lossy().replace('\\', "/").contains(&self.path))
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.call.name(), self.when.name(), self.action.name())?;
        if self.skip > 0 {
            write!(f, ":skip={}", self.skip)?;
        }
        if !self.path.is_empty() {
            write!(f, ":path={}", self.path)?;
        }
        Ok(())
    }
}

impl FromStr for Rule {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let mut parts = s.splitn(4, ':');
        let mut next = |what| parts.next().ok_or_else(|| format!("rule '{}' has no {}", s, what));
        let mut rule = Rule::new(next("call")?.parse()?, next("point")?.parse()?, next("action")?.parse()?);
        let mut rest = parts.next().unwrap_or("");
        while !rest.is_empty() {
            if let Some(path) = rest.strip_prefix("path=") {
                rule.path = path.to_owned();
                break;
            }
            let (field, tail) = rest.split_once(':').unwrap_or((rest, ""));
            let skip =
                field.strip_prefix("skip=").ok_or_else(|| format!("unknown field '{}' in rule '{}'", field, s))?;
            rule.skip = skip.parse().map_err(|_| format!("invalid skip '{}' in rule '{}'", skip, s))?;
            rest = tail;
        }
        Ok(rule)
    }
}

/// How the next matching call fails: the rules tests use most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fail before doing anything ([`When::Before`], [`Action::Fail`]).
    Fail,
    /// Fail halfway ([`When::Midway`], [`Action::Fail`]): a write writes
    /// the first half of its bytes, an atomic write fails in the middle of
    /// the temporary file. Other calls fail before doing anything.
    Partial,
}

/// Called before every call, with its path, outside the state's lock.
pub type Hook = Arc<dyn Fn(Call, &Path) + Send + Sync>;
/// Called when an [`Action::Pause`] fires.
pub type PauseHandler = Arc<dyn Fn(&Rule, &Path) + Send + Sync>;

/// What a [`FailFs`] has seen, and its rules.
#[derive(Default)]
pub struct State {
    /// Every call, in order (failed ones included).
    pub calls: Vec<Call>,
    /// Calls that an action failed.
    pub failed: Vec<Call>,
    /// Rules that fired, in order.
    pub fired: Vec<Rule>,
    /// Rules not fired yet, with the matching calls seen so far.
    rules: Vec<(Rule, u64)>,
    hook: Option<Hook>,
    pause: Option<PauseHandler>,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("calls", &self.calls.len())
            .field("failed", &self.failed)
            .field("fired", &self.fired)
            .field("rules", &self.rules)
            .finish_non_exhaustive()
    }
}

impl State {
    pub fn count(&self, call: Call) -> usize {
        self.calls.iter().filter(|c| **c == call).count()
    }

    /// Count a matching call for every rule; take the first rule that
    /// fires now.
    fn take(&mut self, call: Call, when: When, path: &Path) -> Option<Rule> {
        let mut fire = None;
        for (i, (rule, seen)) in self.rules.iter_mut().enumerate() {
            if rule.matches(call, when, path) {
                *seen += 1;
                if fire.is_none() && *seen > rule.skip {
                    fire = Some(i);
                }
            }
        }
        let (rule, _) = self.rules.remove(fire?);
        self.fired.push(rule.clone());
        Some(rule)
    }
}

/// A [`LogFs`] with failpoints: every call goes to `inner` unless a
/// [`Rule`] acts on it. Clones share the rules and the record of calls.
#[derive(Debug)]
pub struct FailFs<F: LogFs = StdFs> {
    inner: F,
    state: Arc<Mutex<State>>,
}

impl<F: LogFs + Clone> Clone for FailFs<F> {
    fn clone(&self) -> Self {
        FailFs { inner: self.inner.clone(), state: self.state.clone() }
    }
}

impl Default for FailFs<StdFs> {
    fn default() -> Self {
        FailFs::wrap(StdFs)
    }
}

impl FailFs<StdFs> {
    /// Failpoints on the real file system.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<F: LogFs> FailFs<F> {
    pub fn wrap(inner: F) -> Self {
        FailFs { inner, state: Arc::default() }
    }

    /// The rules and the record of calls.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add a rule.
    pub fn add(&self, rule: Rule) {
        self.state().rules.push((rule, 0));
    }

    /// Fail the next call of kind `call`.
    pub fn inject(&self, call: Call, fault: Fault) {
        let when = match (fault, call) {
            (Fault::Partial, Call::Write | Call::WriteAtomic) => When::Midway,
            _ => When::Before,
        };
        self.add(Rule::new(call, when, Action::Fail));
    }

    /// Remove every rule that hasn't fired.
    pub fn clear(&self) {
        self.state().rules.clear();
    }

    /// Rules that haven't fired yet.
    pub fn pending(&self) -> Vec<Rule> {
        self.state().rules.iter().map(|(r, _)| r.clone()).collect()
    }

    pub fn count(&self, call: Call) -> usize {
        self.state().count(call)
    }

    pub fn set_hook(&self, hook: Option<Hook>) {
        self.state().hook = hook;
    }

    /// Replace what [`Action::Pause`] does (by default: block forever).
    pub fn set_pause(&self, pause: Option<PauseHandler>) {
        self.state().pause = pause;
    }

    /// A call begins: run the hook, record it, and act on a `Before` rule.
    fn enter(&self, call: Call, path: &Path) -> io::Result<()> {
        let hook = self.state().hook.clone();
        if let Some(hook) = hook {
            hook(call, path);
        }
        self.state().calls.push(call);
        self.point(call, When::Before, path)
    }

    /// Act on a rule for `call` at `when`, if one fires.
    fn point(&self, call: Call, when: When, path: &Path) -> io::Result<()> {
        let rule = self.state().take(call, when, path);
        match rule {
            Some(rule) => self.act(&rule, path),
            None => Ok(()),
        }
    }

    /// Take the rule that fires at `when` of this call, to act on later
    /// in the call (halfway through a write).
    fn take(&self, call: Call, when: When, path: &Path) -> Option<Rule> {
        self.state().take(call, when, path)
    }

    fn act(&self, rule: &Rule, path: &Path) -> io::Result<()> {
        match rule.action {
            Action::Fail => {
                self.state().failed.push(rule.call);
                Err(io::Error::other(format!("injected {:?} failure", rule.call)))
            }
            Action::NoSpace => {
                self.state().failed.push(rule.call);
                Err(no_space())
            }
            Action::Pause => {
                let pause = self.state().pause.clone();
                match pause {
                    Some(pause) => pause(rule, path),
                    None => loop {
                        std::thread::park();
                    },
                }
                Ok(())
            }
            Action::Panic => panic!("injected panic at {} ({})", rule, path.display()),
            Action::Abort => std::process::abort(),
        }
    }

    fn file(&self, inner: F::File, path: &Path) -> FailFile<F>
    where
        F: Clone,
    {
        FailFile { inner, path: path.to_path_buf(), fs: self.clone() }
    }
}

/// A full disk, as the OS reports it: `ENOSPC` on Unix,
/// `ERROR_DISK_FULL` on Windows.
pub fn no_space() -> io::Error {
    #[cfg(unix)]
    {
        // ENOSPC is 28 on Linux, macOS and the BSDs
        io::Error::from_raw_os_error(28)
    }
    #[cfg(windows)]
    {
        // ERROR_DISK_FULL (winerror.h)
        io::Error::from_raw_os_error(112)
    }
    #[cfg(not(any(unix, windows)))]
    {
        io::Error::new(io::ErrorKind::StorageFull, "no space left on device")
    }
}

impl<F: LogFs + Clone> LogFs for FailFs<F> {
    type File = FailFile<F>;

    fn create(&self, path: &Path) -> io::Result<FailFile<F>> {
        self.enter(Call::Create, path)?;
        let file = self.inner.create(path)?;
        self.point(Call::Create, When::After, path)?;
        Ok(self.file(file, path))
    }

    fn open_append(&self, path: &Path) -> io::Result<FailFile<F>> {
        self.enter(Call::OpenAppend, path)?;
        let file = self.inner.open_append(path)?;
        self.point(Call::OpenAppend, When::After, path)?;
        Ok(self.file(file, path))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.enter(Call::Rename, to)?;
        self.inner.rename(from, to)?;
        self.point(Call::Rename, When::After, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.enter(Call::SyncDir, dir)?;
        self.inner.sync_dir(dir)?;
        self.point(Call::SyncDir, When::After, dir)
    }

    fn write_atomic(&self, path: &Path, write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
        self.enter(Call::WriteAtomic, path)?;
        let midway = self.take(Call::WriteAtomic, When::Midway, path);
        let done = self.take(Call::WriteAtomic, When::WriterDone, path);
        self.inner.write_atomic(path, &mut |out| {
            let mut tap = Tap { inner: out, budget: None, midway: midway.clone(), fs: self, path };
            write(&mut tap)?;
            if let Some(rule) = &done {
                tap.flush()?;
                self.act(rule, path)?;
            }
            Ok(())
        })?;
        self.point(Call::WriteAtomic, When::After, path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.enter(Call::RemoveFile, path)?;
        self.inner.remove_file(path)?;
        self.point(Call::RemoveFile, When::After, path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.enter(Call::Truncate, path)?;
        self.inner.truncate(path, len)?;
        self.point(Call::Truncate, When::After, path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.enter(Call::CreateDir, path)?;
        self.inner.create_dir(path)?;
        self.point(Call::CreateDir, When::After, path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.enter(Call::RemoveDirAll, path)?;
        self.inner.remove_dir_all(path)?;
        self.point(Call::RemoveDirAll, When::After, path)
    }
}

/// The writer `write_atomic` gets: passes everything through, and acts on
/// a `Midway` rule after half of the first write (at most 4 KiB).
struct Tap<'a, F: LogFs> {
    inner: &'a mut dyn Write,
    budget: Option<usize>,
    midway: Option<Rule>,
    fs: &'a FailFs<F>,
    path: &'a Path,
}

impl<F: LogFs> Write for Tap<'_, F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.midway.is_none() {
            return self.inner.write(buf);
        }
        let budget = *self.budget.get_or_insert((buf.len() / 2).min(4096));
        if budget == 0 {
            self.inner.flush()?;
            if let Some(rule) = self.midway.take() {
                self.fs.act(&rule, self.path)?;
            }
            return self.inner.write(buf);
        }
        let n = self.inner.write(&buf[..budget.min(buf.len())])?;
        self.budget = Some(budget - n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A file opened through a [`FailFs`].
pub struct FailFile<F: LogFs> {
    inner: F::File,
    path: PathBuf,
    fs: FailFs<F>,
}

impl<F: LogFs> fmt::Debug for FailFile<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailFile").field("path", &self.path).finish_non_exhaustive()
    }
}

impl<F: LogFs> LogFile for FailFile<F> {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.fs.enter(Call::Write, &self.path)?;
        if let Some(rule) = self.fs.take(Call::Write, When::Midway, &self.path) {
            self.inner.write_all(&bytes[..bytes.len() / 2])?;
            self.fs.act(&rule, &self.path)?;
            self.inner.write_all(&bytes[bytes.len() / 2..])?;
        } else {
            self.inner.write_all(bytes)?;
        }
        self.fs.point(Call::Write, When::After, &self.path)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.fs.enter(Call::Sync, &self.path)?;
        self.inner.sync()?;
        self.fs.point(Call::Sync, When::After, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_round_trip_through_text() {
        let rules = [
            Rule::new(Call::Write, When::Midway, Action::Pause),
            Rule::new(Call::RemoveFile, When::Before, Action::Abort).skip(3).path("/wal/"),
            Rule::new(Call::WriteAtomic, When::WriterDone, Action::NoSpace).path("a:b"),
            Rule::new(Call::Sync, When::After, Action::Fail).skip(1),
        ];
        for rule in rules {
            assert_eq!(rule.to_string().parse::<Rule>(), Ok(rule.clone()), "{}", rule);
        }
        assert_eq!("truncate:before:panic".parse::<Rule>(), Ok(Rule::new(Call::Truncate, When::Before, Action::Panic)));
        for bad in ["write", "write:before", "write:soon:fail", "write:before:fail:skip=x", "write:before:fail:x=1"] {
            assert!(bad.parse::<Rule>().is_err(), "{}", bad);
        }
    }

    #[test]
    fn a_rule_fires_once_at_its_occurrence_and_path() {
        let mut state = State::default();
        state.rules.push((Rule::new(Call::Sync, When::Before, Action::Fail).skip(1).path("wal"), 0));
        let (wal, other) = (Path::new("/d/wal/1.wal"), Path::new("/d/checkpoints"));
        assert!(state.take(Call::Sync, When::Before, other).is_none());
        assert!(state.take(Call::Sync, When::After, wal).is_none());
        assert!(state.take(Call::Write, When::Before, wal).is_none());
        assert!(state.take(Call::Sync, When::Before, wal).is_none(), "skipped");
        assert!(state.take(Call::Sync, When::Before, wal).is_some());
        assert!(state.take(Call::Sync, When::Before, wal).is_none(), "fires once");
        assert_eq!(state.fired.len(), 1);
        // Paths match with `/` on every platform (Windows separators, ADR 0058)
        let rule = Rule::new(Call::Sync, When::Before, Action::Fail).path("/wal/");
        assert!(rule.matches(Call::Sync, When::Before, Path::new(r"C:\d\wal\1.wal")));
        assert!(rule.matches(Call::Sync, When::Before, Path::new("/d/wal/1.wal")));
        assert!(!rule.matches(Call::Sync, When::Before, Path::new(r"C:\d\walx\1.wal")));
    }

    #[test]
    fn failpoints_act_where_they_say() {
        let dir = tempfile::tempdir().unwrap();
        let fs = FailFs::new();
        let path = dir.path().join("f");

        // A halfway failure writes half the bytes
        fs.add(Rule::new(Call::Write, When::Midway, Action::NoSpace));
        let mut file = fs.create(&path).unwrap();
        let err = file.write_all(b"abcdef").unwrap_err();
        assert_eq!(err.raw_os_error(), no_space().raw_os_error());
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");

        // After: done, and reported as failed
        fs.add(Rule::new(Call::Write, When::After, Action::Fail));
        assert!(file.write_all(b"gh").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"abcgh");
        assert_eq!(fs.state().failed, vec![Call::Write, Call::Write]);

        // A pause runs the handler, then the call goes on
        let paused = Arc::new(Mutex::new(Vec::new()));
        let seen = paused.clone();
        fs.set_pause(Some(Arc::new(move |rule: &Rule, _: &Path| seen.lock().unwrap().push(rule.clone()))));
        fs.add(Rule::new(Call::Sync, When::Before, Action::Pause));
        file.sync().unwrap();
        assert_eq!(paused.lock().unwrap().len(), 1);

        // An atomic write that fails when its writer is done leaves nothing
        let target = dir.path().join("atomic");
        fs.add(Rule::new(Call::WriteAtomic, When::WriterDone, Action::Fail));
        assert!(fs.write_atomic(&target, &mut |out| out.write_all(b"data")).is_err());
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "the temporary file was removed");
        // and one paused halfway has written part of its temporary file
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let (seen, tmp_dir) = (sizes.clone(), dir.path().to_path_buf());
        fs.set_pause(Some(Arc::new(move |_: &Rule, _: &Path| {
            for entry in std::fs::read_dir(&tmp_dir).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().ends_with(".tmp") {
                    // The file's own size: on Windows a directory entry's
                    // lags behind an open file
                    seen.lock().unwrap().push(std::fs::metadata(entry.path()).unwrap().len());
                }
            }
        })));
        fs.add(Rule::new(Call::WriteAtomic, When::Midway, Action::Pause));
        fs.write_atomic(&target, &mut |out| out.write_all(&[7; 100])).unwrap();
        assert_eq!(*sizes.lock().unwrap(), vec![50]);
        assert_eq!(std::fs::read(&target).unwrap(), vec![7; 100]);
        assert!(fs.pending().is_empty());
        assert_eq!(fs.count(Call::WriteAtomic), 2);
    }
}
