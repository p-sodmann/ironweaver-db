//! [`PostgresSource`]: a Postgres table as the source of a projection
//! (ADR 0032, feature `postgres`).
//!
//! The table has a position column, a `bigint` (or `integer`) that
//! increases with every event, typically a `bigserial` primary key. A read
//! is
//!
//! ```sql
//! SELECT <columns> FROM <table> WHERE <position> > $1 ORDER BY <position> LIMIT $2
//! ```
//!
//! and each row is an event whose fields are its columns:
//!
//! | Postgres | value |
//! |---|---|
//! | `smallint`, `integer`, `bigint` | `Int` |
//! | `real`, `double precision` | `Float` |
//! | `text`, `varchar`, `char`, `name` | `String` |
//! | `boolean` | `Bool` |
//! | `json`, `jsonb` | nested values (objects as dicts, integers as `Int`) |
//! | `bytea` | `Bytes` |
//! | `timestamp`, `timestamptz` | `DateTime` (microseconds; UTC for `timestamptz`) |
//! | `NULL` | `None` |
//!
//! Other types (`numeric`, `date`, `uuid`, arrays, ...) fail the read with
//! an error that names the column: select them cast (`amount::float8`,
//! `id::text`) in a view.
//!
//! **Holes.** Positions are taken to be dense. A transaction may take an
//! id from the sequence and commit after a later one has: then a read sees
//! a hole that fills later, and a mark past it would skip the event for
//! good. So when the rows skip a position, the source returns the rows
//! before the hole and waits for it, for up to
//! [`gap_timeout`](PostgresConfig::gap_timeout); after that it takes the
//! hole for a rolled back transaction and goes on. A table whose
//! positions are sparse by design sets the timeout to zero. A first read
//! (after 0) starts at the lowest row, wherever it is.
//!
//! The connection is made on the first read and again after an error; TLS
//! comes with step 15 (the URL's `sslmode` must allow none).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ironweaver_core::{DateTime, Value};
use postgres::types::Type;
use postgres::{Client, NoTls, Row};

use super::{Source, SourceError, SourceEvent};

/// Where a [`PostgresSource`] reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostgresConfig {
    /// A connection string (`postgresql://user:pass@host:5432/db`, or
    /// `host=... user=...`).
    pub url: String,
    /// The table or view, optionally with its schema (`events`,
    /// `public.events`). Quoted as identifiers.
    pub table: String,
    /// The position column.
    pub position: String,
    /// The columns to read (the position among them or not); `None`: all.
    pub columns: Option<Vec<String>>,
    /// How long a hole in the positions is waited for (5 s by default; 0:
    /// never).
    pub gap_timeout: Duration,
}

impl PostgresConfig {
    /// A config with every column and the default gap timeout.
    pub fn new(url: impl Into<String>, table: impl Into<String>, position: impl Into<String>) -> Self {
        PostgresConfig {
            url: url.into(),
            table: table.into(),
            position: position.into(),
            columns: None,
            gap_timeout: Duration::from_secs(5),
        }
    }
}

/// A Postgres table as a [`Source`] (see the module docs).
pub struct PostgresSource {
    config: PostgresConfig,
    query: String,
    client: Option<Client>,
    gaps: Gaps,
}

impl std::fmt::Debug for PostgresSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the URL: it may hold a password
        f.debug_struct("PostgresSource").field("table", &self.config.table).finish_non_exhaustive()
    }
}

impl PostgresSource {
    /// A source for `config`. Doesn't connect yet; fails if a name is
    /// empty.
    pub fn new(config: PostgresConfig) -> Result<Self, SourceError> {
        let table = config.table.split('.').map(quote).collect::<Result<Vec<_>, _>>()?.join(".");
        let position = quote(&config.position)?;
        let columns = match &config.columns {
            None => "*".to_owned(),
            Some(columns) => {
                // The position is always read: it is the event's
                let mut names = vec![position.clone()];
                for c in columns.iter().filter(|c| **c != config.position) {
                    names.push(quote(c)?);
                }
                names.join(", ")
            }
        };
        let query = format!("SELECT {} FROM {} WHERE {} > $1 ORDER BY {} LIMIT $2", columns, table, position, position);
        Ok(PostgresSource { config, query, client: None, gaps: Gaps::default() })
    }

    /// The query a read runs.
    pub fn query(&self) -> &str {
        &self.query
    }

    fn rows(&mut self, after: u64, limit: usize) -> Result<Vec<Row>, SourceError> {
        let client = match &mut self.client {
            Some(client) => client,
            None => {
                let client = Client::connect(&self.config.url, NoTls)
                    .map_err(|e| SourceError::new(format!("can't connect to Postgres: {}", e)))?;
                self.client.insert(client)
            }
        };
        let after = i64::try_from(after).map_err(|_| SourceError::new(format!("position {} is too large", after)))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        match client.query(self.query.as_str(), &[&after, &limit]) {
            Ok(rows) => Ok(rows),
            Err(e) => {
                // Connect again next time
                self.client = None;
                Err(SourceError::new(format!("reading {} failed: {}", self.config.table, e)))
            }
        }
    }
}

impl Source for PostgresSource {
    fn read(&mut self, after: u64, limit: usize) -> Result<Vec<SourceEvent>, SourceError> {
        let rows = self.rows(after, limit)?;
        let events = rows.iter().map(|row| event(row, &self.config.position)).collect::<Result<Vec<_>, _>>()?;
        Ok(self.gaps.cut(after, events, Instant::now(), self.config.gap_timeout))
    }
}

fn quote(name: &str) -> Result<String, SourceError> {
    if name.is_empty() {
        return Err(SourceError::new("a table or column name is empty"));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn event(row: &Row, position_column: &str) -> Result<SourceEvent, SourceError> {
    let mut fields = ironweaver_core::Attrs::with_capacity(row.len());
    let mut position = None;
    for (i, column) in row.columns().iter().enumerate() {
        let value = cell(row, i, column.type_())
            .map_err(|e| SourceError::new(format!("column '{}' ({}): {}", column.name(), column.type_(), e)))?;
        if column.name() == position_column {
            position = match value {
                Value::Int(p) if p > 0 => Some(p as u64),
                _ => return Err(SourceError::new(format!("position {:?} is not an integer above 0", value))),
            };
        }
        fields.insert(column.name().to_owned(), value);
    }
    let position = position.ok_or_else(|| SourceError::new(format!("no column '{}' was read", position_column)))?;
    Ok(SourceEvent { position, fields })
}

type CellError = Box<dyn std::error::Error + Sync + Send>;

fn cell(row: &Row, i: usize, ty: &Type) -> Result<Value, CellError> {
    fn get<'a, T: postgres::types::FromSql<'a>>(row: &'a Row, i: usize) -> Result<Option<T>, CellError> {
        Ok(row.try_get::<_, Option<T>>(i)?)
    }
    let value = match *ty {
        Type::INT2 => get::<i16>(row, i)?.map(|v| Value::Int(v.into())),
        Type::INT4 => get::<i32>(row, i)?.map(|v| Value::Int(v.into())),
        Type::INT8 => get::<i64>(row, i)?.map(Value::Int),
        Type::FLOAT4 => get::<f32>(row, i)?.map(|v| Value::Float(v.into())),
        Type::FLOAT8 => get::<f64>(row, i)?.map(Value::Float),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => get::<String>(row, i)?.map(Value::String),
        Type::BOOL => get::<bool>(row, i)?.map(Value::Bool),
        Type::BYTEA => get::<Vec<u8>>(row, i)?.map(Value::Bytes),
        Type::JSON | Type::JSONB => get::<serde_json::Value>(row, i)?.map(json),
        Type::TIMESTAMP => get::<SystemTime>(row, i)?.map(|t| datetime(t, None)).transpose()?,
        Type::TIMESTAMPTZ => get::<SystemTime>(row, i)?.map(|t| datetime(t, Some(0))).transpose()?,
        _ => return Err("this type isn't read; select it cast (to text, float8, ...)".into()),
    };
    Ok(value.unwrap_or(Value::None))
}

fn datetime(time: SystemTime, offset: Option<i32>) -> Result<Value, CellError> {
    let micros = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_micros())?,
        Err(before) => -i64::try_from(before.duration().as_micros())?,
    };
    Ok(Value::DateTime(DateTime { micros, offset }))
}

/// JSON as values: objects as dicts, integers that fit as `Int`, other
/// numbers as `Float`.
fn json(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::None,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(items) => Value::List(items.into_iter().map(json).collect()),
        serde_json::Value::Object(entries) => Value::Dict(entries.into_iter().map(|(k, v)| (k, json(v))).collect()),
    }
}

/// The hole being waited for: the missing position and since when.
#[derive(Debug, Default)]
struct Gaps {
    waiting: Option<(u64, Instant)>,
}

impl Gaps {
    /// The events up to the first hole that is still waited for. `after`
    /// 0 accepts any first position.
    fn cut(&mut self, after: u64, events: Vec<SourceEvent>, now: Instant, timeout: Duration) -> Vec<SourceEvent> {
        if timeout.is_zero() {
            self.waiting = None;
            return events;
        }
        let mut expected = (after > 0).then_some(after + 1);
        let mut keep = events.len();
        for (i, event) in events.iter().enumerate() {
            if let Some(missing) = expected.filter(|e| event.position > *e) {
                match self.waiting {
                    Some((hole, since)) if hole == missing && now.duration_since(since) >= timeout => {
                        log::warn!("position {} didn't appear within {:?}: taken as rolled back", missing, timeout);
                        self.waiting = None;
                    }
                    Some((hole, _)) if hole == missing => {
                        keep = i;
                        break;
                    }
                    _ => {
                        self.waiting = Some((missing, now));
                        keep = i;
                        break;
                    }
                }
            }
            expected = Some(event.position + 1);
        }
        if keep == events.len() && self.waiting.is_some_and(|(hole, _)| expected.is_some_and(|e| hole < e)) {
            // The hole filled
            self.waiting = None;
        }
        let mut events = events;
        events.truncate(keep);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(positions: &[u64]) -> Vec<SourceEvent> {
        positions.iter().map(|&position| SourceEvent { position, fields: Default::default() }).collect()
    }

    fn positions(events: &[SourceEvent]) -> Vec<u64> {
        events.iter().map(|e| e.position).collect()
    }

    #[test]
    fn holes_are_waited_for_then_skipped() {
        let timeout = Duration::from_secs(5);
        let t0 = Instant::now();
        let mut gaps = Gaps::default();
        // The first read starts anywhere; dense rows pass
        assert_eq!(positions(&gaps.cut(0, events(&[3, 4, 5]), t0, timeout)), [3, 4, 5]);
        // A hole at 7: the rows before it
        assert_eq!(positions(&gaps.cut(5, events(&[6, 8, 9]), t0, timeout)), [6]);
        // Still missing, before the timeout: nothing
        assert!(gaps.cut(6, events(&[8, 9]), t0 + Duration::from_secs(4), timeout).is_empty());
        // After it: skipped, and a later hole starts its own wait
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(positions(&gaps.cut(6, events(&[8, 9, 11]), t1, timeout)), [8, 9]);
        assert!(gaps.cut(9, events(&[11]), t1 + Duration::from_secs(1), timeout).is_empty());
        // The hole fills: everything, and the wait is over
        assert_eq!(positions(&gaps.cut(9, events(&[10, 11]), t1 + Duration::from_secs(2), timeout)), [10, 11]);
        assert!(gaps.waiting.is_none());
        // Without a timeout, holes don't matter
        assert_eq!(positions(&gaps.cut(11, events(&[20, 30]), t1, Duration::ZERO)), [20, 30]);
    }

    #[test]
    fn names_are_quoted() {
        let mut config = PostgresConfig::new("host=x", "app.ev\"ents", "id");
        let source = PostgresSource::new(config.clone()).unwrap();
        assert_eq!(source.query(), r#"SELECT * FROM "app"."ev""ents" WHERE "id" > $1 ORDER BY "id" LIMIT $2"#);
        config.columns = Some(vec!["kind".into(), "id".into()]);
        let source = PostgresSource::new(config.clone()).unwrap();
        assert!(source.query().starts_with(r#"SELECT "id", "kind" FROM"#), "{}", source.query());
        config.position = String::new();
        assert!(PostgresSource::new(config).is_err());
    }

    #[test]
    fn json_becomes_values() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a": [1, 2.5, "x", null, true], "b": {"c": -3}}"#).unwrap();
        let Value::Dict(d) = json(v) else { panic!("a dict") };
        assert_eq!(
            d.get("a"),
            Some(&Value::List(vec![
                Value::Int(1),
                Value::Float(2.5),
                Value::String("x".into()),
                Value::None,
                Value::Bool(true)
            ]))
        );
        assert!(matches!(d.get("b"), Some(Value::Dict(inner)) if inner.get("c") == Some(&Value::Int(-3))));
    }
}
