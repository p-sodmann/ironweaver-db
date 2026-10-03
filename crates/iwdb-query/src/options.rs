//! Read options and limits: what a caller asks for ([`QueryOptions`]) and
//! what the server allows ([`LimitConfig`]: defaults and hard caps).

use std::time::Duration;

use iwdb_storage::HistoryId;

use crate::{Cursor, Error};

/// Limits a caller asks for. `None`: the server's default
/// ([`LimitConfig::default_limits`]). A value above the server's cap is
/// lowered to the cap (as page sizes are in most APIs); 0 is invalid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// Most results in the answer: the page size of a paginated read, the
    /// size of the answer of the others (see each operation).
    pub max_results: Option<usize>,
    /// Most nodes a read may visit: enter during a search, or check as an
    /// index or scan candidate or against a pattern's node variable.
    pub max_visited: Option<usize>,
    /// Most edges a read may examine (the core's `Budget::max_edges`).
    pub max_edges: Option<usize>,
}

/// Options of every read of the [`Database`](crate::Database) trait.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryOptions {
    /// Read-your-writes: wait until the namespace has applied this seq.
    pub min_seq: Option<u64>,
    /// The history `min_seq` belongs to; a seq of another history fails
    /// with `invalid_argument` instead of reading (ADR 0016).
    pub history: Option<HistoryId>,
    /// How long the request may take, from the call (queueing and the
    /// `min_seq` wait included). `None`: the server's default; above the
    /// server's maximum: the maximum. After it the read fails with
    /// `timeout`.
    pub timeout: Option<Duration>,
    pub limits: Limits,
    /// When a read reaches `max_visited` or `max_edges` (and, where the
    /// operation says so, `max_results`): `false` (the default) fails with
    /// `budget_exceeded`; `true` answers with what was found and
    /// [`Answer::truncated`](crate::Answer::truncated) set. A truncated
    /// answer has no cursor.
    pub partial: bool,
    /// Continue a paginated read: the [`Answer::next`](crate::Answer::next)
    /// of the previous page, with the same request.
    pub cursor: Option<Cursor>,
}

impl QueryOptions {
    /// Options that wait for `seq` (read-your-writes).
    pub fn min_seq(seq: u64) -> Self {
        QueryOptions { min_seq: Some(seq), ..QueryOptions::default() }
    }

    /// These options with `limits`.
    pub fn with_limits(self, max_results: Option<usize>, max_visited: Option<usize>, max_edges: Option<usize>) -> Self {
        QueryOptions { limits: Limits { max_results, max_visited, max_edges }, ..self }
    }
}

/// Resolved limits: what a read may actually do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub max_results: usize,
    pub max_visited: usize,
    pub max_edges: usize,
}

/// The server's limits: defaults for requests that set none, and hard caps
/// no request can raise. Every read is bounded by them (design rule 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitConfig {
    pub default_limits: Bounds,
    pub max_limits: Bounds,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
}

impl LimitConfig {
    /// 1 000 results, 100 000 nodes visited, 1 000 000 edges examined,
    /// 30 s (the store's `DEFAULT_TIMEOUT`).
    pub const DEFAULT_LIMITS: Bounds = Bounds { max_results: 1_000, max_visited: 100_000, max_edges: 1_000_000 };
    /// 100 000 results, 10 000 000 nodes, 100 000 000 edges, 5 minutes.
    pub const MAX_LIMITS: Bounds = Bounds { max_results: 100_000, max_visited: 10_000_000, max_edges: 100_000_000 };

    /// Check that every limit is at least 1 and no default is above its cap.
    pub fn check(&self) -> Result<(), Error> {
        let (d, m) = (&self.default_limits, &self.max_limits);
        let pairs = [
            ("max_results", d.max_results, m.max_results),
            ("max_visited", d.max_visited, m.max_visited),
            ("max_edges", d.max_edges, m.max_edges),
        ];
        for (name, default, max) in pairs {
            if default == 0 || default > max {
                return Err(Error::invalid(format!(
                    "the default {} ({}) must be between 1 and the maximum ({})",
                    name, default, max
                )));
            }
        }
        if self.default_timeout.is_zero() || self.default_timeout > self.max_timeout {
            return Err(Error::invalid("the default timeout must be positive and at most the maximum timeout"));
        }
        Ok(())
    }

    /// The limits and the timeout a read with `options` runs under.
    /// Errors: `invalid_argument` for a limit of 0.
    pub fn resolve(&self, options: &QueryOptions) -> Result<(Bounds, Duration), Error> {
        let one = |name: &str, asked: Option<usize>, default: usize, max: usize| match asked {
            Some(0) => Err(Error::invalid(format!("{} must be at least 1", name))),
            Some(n) => Ok(n.min(max)),
            None => Ok(default),
        };
        let (d, m, l) = (&self.default_limits, &self.max_limits, &options.limits);
        let bounds = Bounds {
            max_results: one("max_results", l.max_results, d.max_results, m.max_results)?,
            max_visited: one("max_visited", l.max_visited, d.max_visited, m.max_visited)?,
            max_edges: one("max_edges", l.max_edges, d.max_edges, m.max_edges)?,
        };
        let timeout = options.timeout.unwrap_or(self.default_timeout).min(self.max_timeout);
        Ok((bounds, timeout))
    }
}

impl Default for LimitConfig {
    fn default() -> Self {
        LimitConfig {
            default_limits: Self::DEFAULT_LIMITS,
            max_limits: Self::MAX_LIMITS,
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Code;

    #[test]
    fn defaults_apply_and_caps_lower_what_is_asked() {
        let config = LimitConfig::default();
        config.check().expect("valid");
        let (bounds, timeout) = config.resolve(&QueryOptions::default()).expect("resolve");
        assert_eq!(bounds, LimitConfig::DEFAULT_LIMITS);
        assert_eq!(timeout, Duration::from_secs(30));
        let asked = QueryOptions { timeout: Some(Duration::MAX), ..QueryOptions::default() }.with_limits(
            Some(usize::MAX),
            Some(7),
            None,
        );
        let (bounds, timeout) = config.resolve(&asked).expect("resolve");
        assert_eq!(bounds.max_results, LimitConfig::MAX_LIMITS.max_results);
        assert_eq!(bounds.max_visited, 7);
        assert_eq!(timeout, Duration::from_secs(300));
        let zero = QueryOptions::default().with_limits(None, None, Some(0));
        assert_eq!(config.resolve(&zero).map_err(|e| e.code()), Err(Code::InvalidArgument));
    }

    #[test]
    fn a_default_above_its_cap_is_refused() {
        let mut config = LimitConfig::default();
        config.default_limits.max_edges = config.max_limits.max_edges + 1;
        assert!(config.check().is_err());
        let config = LimitConfig { default_timeout: Duration::ZERO, ..LimitConfig::default() };
        assert!(config.check().is_err());
    }
}
