//! Resource limits per client and namespace (step 15d, ADR 0059): what a
//! user ([`ClientLimits`]) and a namespace ([`NamespaceLimits`]) may use,
//! and the [`Limiter`] that admits or refuses a call before it starts.
//!
//! **Values.** Every limit is unset (`None`: the default applies), 0 (no
//! limit, whatever the default) or a number. A user's or namespace's own
//! value replaces the default ([`ClientLimits::over`]).
//!
//! **Admission** ([`Limiter::admit`]) happens at the authorisation point,
//! when a call is checked: before its future exists, so a refused call
//! holds no worker, queue slot or namespace lock. It is all or nothing:
//! the user's running count, the namespace's running count and the user's
//! token bucket are checked under one lock, and only if all pass are the
//! counts raised and a token taken. The counts fall when the call ends
//! ([`Limiter::release`]). Refusals are `resource_exhausted`.
//!
//! **Refusals are counted** per user and per namespace for the metrics,
//! with bounded labels: the first [`MAX_USER_SERIES`] users refused get a
//! series each; later ones are counted as [`OTHER`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::options::Budget;
use crate::{Code, Error};

/// Users whose refusals get a series of their own; later ones share
/// [`OTHER`].
pub const MAX_USER_SERIES: usize = 64;

/// Namespaces whose concurrency refusals are counted; beyond them, a
/// refusal is still made but not counted per namespace.
pub const MAX_NAMESPACE_SERIES: usize = 1024;

/// The `user` label of the users beyond [`MAX_USER_SERIES`].
pub const OTHER: &str = "(other)";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Some(n)` for a limit of `n`; `None` for unset or 0 (no limit).
fn limit(value: Option<u64>) -> Option<u64> {
    value.filter(|&n| n > 0)
}

fn usize_of(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// A user's limits (ADR 0059). Unset: the default; 0: no limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientLimits {
    /// Calls admitted per second, on average (a token bucket).
    pub requests_per_second: Option<u64>,
    /// Calls admitted at once after a quiet time: the bucket's size.
    /// Unset or 0: `requests_per_second`.
    pub burst: Option<u64>,
    /// Calls of the user running at once, over every namespace.
    pub max_running: Option<u64>,
    /// Query budget: the most `max_results` a read may have.
    pub max_results: Option<u64>,
    /// Query budget: the most `max_visited` a read may have.
    pub max_visited: Option<u64>,
    /// Query budget: the longest timeout a read may have, in milliseconds.
    pub timeout_ms: Option<u64>,
}

impl ClientLimits {
    /// The limits' names, in the order of [`named`](Self::named): the keys
    /// of the stored dict, the CLI and the config.
    pub const KEYS: [&'static str; 6] =
        ["requests_per_second", "burst", "max_running", "max_results", "max_visited", "timeout_ms"];

    /// These limits, with `defaults` where these are unset.
    pub fn over(&self, defaults: &ClientLimits) -> ClientLimits {
        ClientLimits {
            requests_per_second: self.requests_per_second.or(defaults.requests_per_second),
            burst: self.burst.or(defaults.burst),
            max_running: self.max_running.or(defaults.max_running),
            max_results: self.max_results.or(defaults.max_results),
            max_visited: self.max_visited.or(defaults.max_visited),
            timeout_ms: self.timeout_ms.or(defaults.timeout_ms),
        }
    }

    /// Whether nothing is set.
    pub fn is_unset(&self) -> bool {
        *self == ClientLimits::default()
    }

    /// The token bucket: (tokens per second, size), if there is a rate
    /// limit.
    pub fn rate(&self) -> Option<(f64, f64)> {
        let rate = limit(self.requests_per_second)?;
        let burst = limit(self.burst).unwrap_or(rate);
        Some((rate as f64, burst as f64))
    }

    /// The query budget these limits give a read.
    pub fn budget(&self) -> Budget {
        Budget {
            max_results: limit(self.max_results).map(usize_of),
            max_visited: limit(self.max_visited).map(usize_of),
            timeout: limit(self.timeout_ms).map(Duration::from_millis),
        }
    }

    /// The limits as `(name, value)` pairs, set ones only, in a fixed order:
    /// for messages and the CLI.
    pub fn named(&self) -> Vec<(&'static str, u64)> {
        let values = [
            self.requests_per_second,
            self.burst,
            self.max_running,
            self.max_results,
            self.max_visited,
            self.timeout_ms,
        ];
        Self::KEYS.into_iter().zip(values).filter_map(|(n, v)| Some((n, v?))).collect()
    }
}

/// A namespace's limits (ADR 0059). Unset: the default; 0: no limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NamespaceLimits {
    /// Calls on the namespace running at once, of every user together.
    pub max_running: Option<u64>,
    /// The namespace's graph memory above which writes that add are
    /// refused.
    pub max_memory_bytes: Option<u64>,
}

impl NamespaceLimits {
    /// The limits' names, in the order of [`named`](Self::named).
    pub const KEYS: [&'static str; 2] = ["max_running", "max_memory_bytes"];

    /// These limits, with `defaults` where these are unset.
    pub fn over(&self, defaults: &NamespaceLimits) -> NamespaceLimits {
        NamespaceLimits {
            max_running: self.max_running.or(defaults.max_running),
            max_memory_bytes: self.max_memory_bytes.or(defaults.max_memory_bytes),
        }
    }

    pub fn is_unset(&self) -> bool {
        *self == NamespaceLimits::default()
    }

    /// The concurrency limit, if there is one.
    pub fn running(&self) -> Option<u64> {
        limit(self.max_running)
    }

    /// The memory limit in bytes, if there is one.
    pub fn memory(&self) -> Option<u64> {
        limit(self.max_memory_bytes)
    }

    /// The limits as `(name, value)` pairs, set ones only.
    pub fn named(&self) -> Vec<(&'static str, u64)> {
        Self::KEYS
            .into_iter()
            .zip([self.max_running, self.max_memory_bytes])
            .filter_map(|(n, v)| Some((n, v?)))
            .collect()
    }
}

/// A namespace's own limits, by name ([`LimitsReport`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceLimitsInfo {
    pub namespace: String,
    /// Its own values (unset: the default).
    pub limits: NamespaceLimits,
}

/// The limits a server applies (`Accounts::limits`): the defaults, and
/// every namespace's own limits (those the caller has a role on).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LimitsReport {
    /// `[limits.client]`: for users who aren't server admins and have no
    /// value of their own.
    pub client_defaults: ClientLimits,
    /// `[limits.namespace]`.
    pub namespace_defaults: NamespaceLimits,
    /// By namespace name; namespaces without limits of their own are left
    /// out.
    pub namespaces: Vec<NamespaceLimitsInfo>,
}

/// A token bucket: up to `burst` tokens, refilled at `rate` a second.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(burst: f64, now: Instant) -> Self {
        Bucket { tokens: burst, at: now }
    }

    /// The tokens it holds at `now`.
    fn refilled(&self, rate: f64, burst: f64, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        (self.tokens + elapsed * rate).min(burst)
    }
}

/// What a user's calls hold now.
#[derive(Debug, Default)]
struct UserState {
    running: u64,
    bucket: Option<Bucket>,
}

#[derive(Debug, Default)]
struct State {
    users: HashMap<String, UserState>,
    namespaces: HashMap<String, u64>,
}

/// Refusals so far, for the metrics.
#[derive(Debug, Default)]
struct Refusals {
    /// Per user: `[rate, running]`.
    users: BTreeMap<String, [u64; 2]>,
    other: [u64; 2],
    /// Per namespace: concurrency refusals.
    namespaces: BTreeMap<String, u64>,
}

/// Which limit refused a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The user's rate.
    Rate,
    /// The user's running calls.
    Running,
    /// The namespace's running calls.
    NamespaceRunning,
}

/// An admitted call's place in the counts: given back with
/// [`Limiter::release`] when it ends.
#[derive(Debug)]
pub struct Admitted {
    user: String,
    namespace: Option<String>,
}

/// Refusals counted so far ([`Limiter::refusals`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefusalCounts {
    /// `(user, limit, count)`, `limit` being `rate` or `running`; the user
    /// is [`OTHER`] for those beyond [`MAX_USER_SERIES`]. Only nonzero
    /// counts.
    pub users: Vec<(String, &'static str, u64)>,
    /// `(namespace, count)` of concurrency refusals.
    pub namespaces: Vec<(String, u64)>,
}

/// A lookup of a namespace's limits by name, set by the database that owns
/// the namespaces ([`Limiter::set_namespace_limits`]).
pub type NamespaceLookup = Box<dyn Fn(&str) -> NamespaceLimits + Send + Sync>;

/// Admits or refuses calls by their user's and namespace's limits (see the
/// module docs). One per database, inside its request registry.
#[derive(Default)]
pub struct Limiter {
    state: Mutex<State>,
    refusals: Mutex<Refusals>,
    namespaces: Mutex<Option<std::sync::Arc<NamespaceLookup>>>,
}

impl std::fmt::Debug for Limiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Limiter").field("state", &*lock(&self.state)).finish_non_exhaustive()
    }
}

impl Limiter {
    /// Look namespaces' limits up with `lookup` (without it, namespaces
    /// have none).
    pub fn set_namespace_limits(&self, lookup: NamespaceLookup) {
        *lock(&self.namespaces) = Some(std::sync::Arc::new(lookup));
    }

    fn namespace_limits(&self, namespace: &str) -> NamespaceLimits {
        let lookup = lock(&self.namespaces).clone();
        lookup.map_or_else(NamespaceLimits::default, |f| f(namespace))
    }

    /// Admit a call of `user` (whose effective limits are `limits`) on
    /// `namespace`, taking a token from its bucket if `take_token`. O(1)
    /// under one lock. Errors: `resource_exhausted`, naming the limit and
    /// when to retry; nothing is counted against the user then.
    pub fn admit(
        &self,
        user: &str,
        limits: &ClientLimits,
        namespace: Option<&str>,
        take_token: bool,
    ) -> Result<Admitted, Error> {
        let ns_running = namespace.and_then(|ns| self.namespace_limits(ns).running());
        let now = Instant::now();
        let mut state = lock(&self.state);
        let refused = |which: Refused, message: String| (which, Error::new(Code::ResourceExhausted, message));
        let check = (|| {
            let current = state.users.get(user);
            let running = current.map_or(0, |u| u.running);
            if let Some(max) = limit(limits.max_running)
                && running >= max
            {
                return Err(refused(
                    Refused::Running,
                    format!(
                        "user '{}' has {} requests running, its limit (max_running): retry when one of them ends",
                        user, running
                    ),
                ));
            }
            if let (Some(ns), Some(max)) = (namespace, ns_running) {
                let running = state.namespaces.get(ns).copied().unwrap_or(0);
                if running >= max {
                    return Err(refused(
                        Refused::NamespaceRunning,
                        format!(
                            "namespace '{}' has {} requests running, its limit (max_running): retry with backoff",
                            ns, running
                        ),
                    ));
                }
            }
            if take_token && let Some((rate, burst)) = limits.rate() {
                let tokens = current.and_then(|u| u.bucket).map_or(burst, |b| b.refilled(rate, burst, now));
                if tokens < 1.0 {
                    let wait = Duration::from_secs_f64((1.0 - tokens) / rate);
                    return Err(refused(
                        Refused::Rate,
                        format!(
                            "user '{}' is over its rate limit of {} requests per second (burst {}): retry in {} ms",
                            user,
                            rate,
                            burst,
                            wait.as_millis().max(1)
                        ),
                    ));
                }
            }
            Ok(())
        })();
        if let Err((which, e)) = check {
            drop(state);
            self.count(which, user, namespace);
            return Err(e);
        }
        let entry = state.users.entry(user.to_owned()).or_default();
        entry.running += 1;
        match (take_token, limits.rate()) {
            (true, Some((rate, burst))) => {
                let bucket = entry.bucket.unwrap_or_else(|| Bucket::full(burst, now));
                entry.bucket = Some(Bucket { tokens: bucket.refilled(rate, burst, now) - 1.0, at: now });
            }
            // A user whose rate limit was lifted starts full when it has one again
            (_, None) => entry.bucket = None,
            (false, Some(_)) => {}
        }
        if let Some(ns) = namespace {
            *state.namespaces.entry(ns.to_owned()).or_default() += 1;
        }
        Ok(Admitted { user: user.to_owned(), namespace: namespace.map(str::to_owned) })
    }

    /// The call `admitted` ended.
    pub fn release(&self, admitted: &Admitted) {
        let mut state = lock(&self.state);
        if let Some(user) = state.users.get_mut(&admitted.user) {
            user.running = user.running.saturating_sub(1);
            if user.running == 0 && user.bucket.is_none() {
                state.users.remove(&admitted.user);
            }
        }
        if let Some(ns) = &admitted.namespace
            && let Some(n) = state.namespaces.get_mut(ns)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.namespaces.remove(ns);
            }
        }
    }

    /// Calls of `user` running now (admitted, not yet released).
    pub fn running(&self, user: &str) -> u64 {
        lock(&self.state).users.get(user).map_or(0, |u| u.running)
    }

    /// Calls on `namespace` running now.
    pub fn namespace_running(&self, namespace: &str) -> u64 {
        lock(&self.state).namespaces.get(namespace).copied().unwrap_or(0)
    }

    fn count(&self, which: Refused, user: &str, namespace: Option<&str>) {
        let mut refusals = lock(&self.refusals);
        match which {
            Refused::Rate | Refused::Running => {
                let slot = usize::from(which == Refused::Running);
                if let Some(counts) = refusals.users.get_mut(user) {
                    counts[slot] += 1;
                } else if refusals.users.len() < MAX_USER_SERIES {
                    let mut counts = [0; 2];
                    counts[slot] = 1;
                    refusals.users.insert(user.to_owned(), counts);
                } else {
                    refusals.other[slot] += 1;
                }
            }
            Refused::NamespaceRunning => {
                let Some(ns) = namespace else { return };
                if let Some(n) = refusals.namespaces.get_mut(ns) {
                    *n += 1;
                } else if refusals.namespaces.len() < MAX_NAMESPACE_SERIES {
                    refusals.namespaces.insert(ns.to_owned(), 1);
                }
            }
        }
    }

    /// The refusals counted so far.
    pub fn refusals(&self) -> RefusalCounts {
        let refusals = lock(&self.refusals);
        let mut users = Vec::new();
        let others = std::iter::once((OTHER.to_owned(), refusals.other));
        for (user, counts) in refusals.users.iter().map(|(u, c)| (u.clone(), *c)).chain(others) {
            for (name, n) in [("rate", counts[0]), ("running", counts[1])] {
                if n > 0 {
                    users.push((user.clone(), name, n));
                }
            }
        }
        let namespaces = refusals.namespaces.iter().map(|(n, c)| (n.clone(), *c)).collect();
        RefusalCounts { users, namespaces }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(rps: u64, burst: u64) -> ClientLimits {
        ClientLimits { requests_per_second: Some(rps), burst: Some(burst), ..ClientLimits::default() }
    }

    #[test]
    fn own_values_replace_defaults_and_zero_means_none() {
        let defaults = ClientLimits { max_running: Some(4), max_visited: Some(100), ..ClientLimits::default() };
        let own = ClientLimits { max_running: Some(0), requests_per_second: Some(10), ..ClientLimits::default() };
        let effective = own.over(&defaults);
        assert_eq!(effective.max_running, Some(0));
        assert_eq!(effective.max_visited, Some(100));
        assert_eq!(effective.rate(), Some((10.0, 10.0)));
        assert_eq!(effective.budget(), Budget { max_visited: Some(100), ..Budget::default() });
        assert_eq!(ClientLimits { requests_per_second: Some(0), ..own }.rate(), None);
        assert_eq!(effective.named(), vec![("requests_per_second", 10), ("max_running", 0), ("max_visited", 100)]);
        let ns = NamespaceLimits { max_running: Some(0), ..NamespaceLimits::default() };
        let ns = ns.over(&NamespaceLimits { max_running: Some(3), max_memory_bytes: Some(9) });
        assert_eq!((ns.running(), ns.memory()), (None, Some(9)));
    }

    #[test]
    fn the_bucket_admits_its_burst_then_its_rate() {
        let limiter = Limiter::default();
        let limits = rate(20, 3);
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(limiter.admit("ann", &limits, None, true).unwrap());
        }
        let e = limiter.admit("ann", &limits, None, true).unwrap_err();
        assert_eq!(e.code(), Code::ResourceExhausted);
        assert!(e.message().contains("rate limit of 20 requests per second (burst 3): retry in"), "{}", e);
        // Others aren't affected, nor are calls that take no token
        limiter.admit("bob", &limits, None, true).unwrap();
        held.push(limiter.admit("ann", &limits, None, false).unwrap());
        // A token every 50 ms
        std::thread::sleep(Duration::from_millis(60));
        held.push(limiter.admit("ann", &limits, None, true).unwrap());
        assert!(limiter.admit("ann", &limits, None, true).is_err());
        assert_eq!(limiter.running("ann"), 5);
        for a in &held {
            limiter.release(a);
        }
        assert_eq!(limiter.running("ann"), 0);
    }

    #[test]
    fn running_calls_are_limited_per_user_and_per_namespace() {
        let limiter = Limiter::default();
        limiter.set_namespace_limits(Box::new(|ns| NamespaceLimits {
            max_running: (ns == "social").then_some(2),
            ..NamespaceLimits::default()
        }));
        let two = ClientLimits { max_running: Some(2), ..ClientLimits::default() };
        let a = limiter.admit("ann", &two, Some("social"), true).unwrap();
        let b = limiter.admit("ann", &two, None, true).unwrap();
        let e = limiter.admit("ann", &two, Some("other"), true).unwrap_err();
        assert!(e.message().contains("user 'ann' has 2 requests running"), "{}", e);
        let c = limiter.admit("bob", &ClientLimits::default(), Some("social"), true).unwrap();
        let e = limiter.admit("cy", &ClientLimits::default(), Some("social"), true).unwrap_err();
        assert!(e.message().contains("namespace 'social' has 2 requests running"), "{}", e);
        limiter.admit("cy", &ClientLimits::default(), Some("other"), true).unwrap();
        limiter.release(&a);
        limiter.admit("cy", &ClientLimits::default(), Some("social"), true).unwrap();
        assert_eq!(limiter.namespace_running("social"), 2);
        drop((b, c));
        let counts = limiter.refusals();
        assert_eq!(counts.users, vec![("ann".to_owned(), "running", 1)]);
        assert_eq!(counts.namespaces, vec![("social".to_owned(), 1)]);
    }

    #[test]
    fn a_refusal_takes_no_token() {
        let limiter = Limiter::default();
        let limits = ClientLimits { max_running: Some(1), ..rate(1, 2) };
        let first = limiter.admit("ann", &limits, None, true).unwrap();
        // Refused for running: the bucket keeps its second token
        assert!(limiter.admit("ann", &limits, None, true).is_err());
        limiter.release(&first);
        limiter.admit("ann", &limits, None, true).unwrap();
    }

    #[test]
    fn user_series_are_bounded() {
        let limiter = Limiter::default();
        let none = rate(1, 1);
        for i in 0..MAX_USER_SERIES + 10 {
            let user = format!("u{}", i);
            let _held = limiter.admit(&user, &none, None, true).unwrap();
            assert!(limiter.admit(&user, &none, None, true).is_err());
        }
        let counts = limiter.refusals();
        assert_eq!(counts.users.len(), MAX_USER_SERIES + 1);
        assert!(counts.users.contains(&(OTHER.to_owned(), "rate", 10)), "{:?}", counts.users.last());
    }
}
