//! Local transactional storage for observations and governor state.
//!
//! Same SQLite file, schema (`user_version` 2), SQL and transaction boundaries as the Python
//! release's `store.py`, so ledgers written by either version read identically in the other.
//! Timestamps are stored as `isoformat()` text and ordered with the `demon_timestamp` collation.

use crate::governor::{
    self, CONFLICT_DECREASING, CONFLICT_EQUAL_TIME, GovernorState, Mode, Observation, Policy, Provider, REFRESH_AGE,
    Reading, Source, Val, advance_state, evaluate_policy, first_max, is_conflict, normalize_observation,
    weekly_metrics,
};
use crate::paths;
use crate::ts::{self, HOUR, SECOND, Ts};
use crate::{Error, Result};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, params, params_from_iter};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 2;
const RESET_JITTER_SECONDS: i64 = 2;
pub const MAX_OBSERVATIONS: usize = 64;
const ACCEPTED: &str = "schema_marker NOT IN (?, ?)";
const COLUMNS: &str =
    "id, provider, observed_at, weekly_used, weekly_reset_at, short_used, short_reset_at, source, schema_marker";

/// The default wait for a busy ledger; hooks and reads use 0.2 seconds.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// One raw `observations` row, values kept as SQLite returned them.
#[derive(Clone, Debug)]
struct Row {
    id: i64,
    provider: Sql,
    observed_at: Sql,
    weekly_used: Sql,
    weekly_reset_at: Sql,
    short_used: Sql,
    short_reset_at: Sql,
    source: Sql,
    schema_marker: Sql,
}

impl Row {
    fn marker(&self) -> Option<&str> {
        match &self.schema_marker {
            Sql::Text(text) => Some(text),
            _ => None,
        }
    }

    fn accepted(&self) -> bool {
        !self.marker().is_some_and(is_conflict)
    }
}

fn val(value: &Sql) -> Option<Val> {
    match value {
        Sql::Null => None,
        Sql::Integer(number) => Some(Val::Num(*number as f64)),
        Sql::Real(number) => Some(Val::Num(*number)),
        Sql::Text(text) => Some(Val::Text(text.clone())),
        Sql::Blob(_) => Some(Val::Other),
    }
}

fn sql_ts(value: &Sql) -> Result<Ts> {
    governor::parse_timestamp(val(value).as_ref())
}

fn sql_number(value: &Sql) -> Result<f64> {
    match value {
        Sql::Integer(number) => Ok(*number as f64),
        Sql::Real(number) => Ok(*number),
        _ => Err(Error::invalid("invalid stored observation")),
    }
}

fn opt_stamp(value: Option<Ts>) -> Option<String> {
    value.map(Ts::iso)
}

/// An open ledger connection with the `demon_timestamp` collation installed.
pub struct Ledger {
    conn: Connection,
    /// Set when the collation met text that is not a timestamp. The Python collation raised
    /// `ValueError` from the statement that hit it; [`Ledger::check`] reproduces that.
    collation_failed: Arc<AtomicBool>,
}

impl Ledger {
    /// `_connect`: validate the path, create the parent, open, migrate. The default ledger's folder
    /// and file are made private to the user on Unix.
    pub fn open(database: Option<&str>, timeout: Duration) -> Result<Ledger> {
        // Only the default ledger's folder and file are made private (Unix permissions).
        let is_default = database.is_none();
        #[cfg(not(unix))]
        let _ = is_default;
        let target = match database {
            Some(":memory:") => None,
            Some(value) => Some(paths::absolute(&paths::expanduser(value))),
            None => Some(paths::absolute(&paths::default_database_path())),
        };
        if let Some(path) = &target {
            paths::validate_state_path(path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            paths::validate_state_path(path)?;
            #[cfg(unix)]
            if is_default {
                use std::os::unix::fs::PermissionsExt;
                if let Some(parent) = path.parent() {
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
                }
            }
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = match &target {
            Some(path) => Connection::open_with_flags(path, flags)?,
            None => Connection::open_in_memory_with_flags(flags)?,
        };
        conn.busy_timeout(timeout)?;
        let collation_failed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&collation_failed);
        conn.create_collation("demon_timestamp", move |first: &str, second: &str| {
            match (ts::parse_text(first), ts::parse_text(second)) {
                (Ok(left), Ok(right)) => left.cmp(&right),
                _ => {
                    flag.store(true, AtomicOrdering::SeqCst);
                    Ordering::Equal
                }
            }
        })?;
        let ledger = Ledger { conn, collation_failed };
        ledger.migrate()?;
        #[cfg(unix)]
        if let (true, Some(path)) = (is_default, &target) {
            use std::os::unix::fs::PermissionsExt;
            paths::validate_state_path(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(ledger)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::unavailable("state schema is newer than this program"));
        }
        self.conn.execute_batch("PRAGMA foreign_keys = ON")?;
        self.conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))?;
        self.conn.execute_batch("PRAGMA synchronous = FULL")?;
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS observations (
                id INTEGER PRIMARY KEY,
                provider TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                weekly_used REAL NOT NULL,
                weekly_reset_at TEXT NOT NULL,
                short_used REAL,
                short_reset_at TEXT,
                source TEXT NOT NULL,
                schema_marker TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS observations_by_provider_time
                ON observations(provider, observed_at);
            CREATE TABLE IF NOT EXISTS governor_state (
                provider TEXT PRIMARY KEY,
                window_reset_at TEXT,
                base_mode TEXT,
                last_change_at TEXT,
                candidate_mode TEXT,
                candidate_first_at TEXT,
                candidate_last_at TEXT,
                history_uncertain_until TEXT
            );",
        )?;
        let has_uncertain_column = {
            let mut statement = self.conn.prepare("PRAGMA table_info(governor_state)")?;
            let names = statement.query_map([], |row| row.get::<_, String>("name"))?;
            let mut found = false;
            for name in names {
                found |= name? == "history_uncertain_until";
            }
            found
        };
        if version >= SCHEMA_VERSION && has_uncertain_column {
            return Ok(());
        }
        self.exec("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<()> {
            if !has_uncertain_column {
                self.exec("ALTER TABLE governor_state ADD COLUMN history_uncertain_until TEXT")?;
            }
            let legacy: Vec<(Sql, Sql)> = {
                let mut statement = self.conn.prepare(
                    "SELECT provider, COUNT(*) AS row_count, MAX(observed_at COLLATE demon_timestamp) AS latest
                     FROM observations GROUP BY provider HAVING COUNT(*) >= ?",
                )?;
                let rows = statement.query_map([MAX_OBSERVATIONS as i64], |row| Ok((row.get(0)?, row.get(2)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            self.check()?;
            for (provider, latest) in legacy {
                let until = sql_ts(&latest)? + 24 * HOUR;
                self.conn.execute(
                    "INSERT INTO governor_state(provider, history_uncertain_until)
                     VALUES (?, ?)
                     ON CONFLICT(provider) DO UPDATE SET
                       history_uncertain_until=excluded.history_uncertain_until,
                       candidate_mode=NULL, candidate_first_at=NULL, candidate_last_at=NULL",
                    params![provider, until.iso()],
                )?;
            }
            self.exec(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            self.exec("COMMIT")
        })();
        if outcome.is_err() {
            self.rollback();
        }
        outcome
    }

    fn exec(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        !self.conn.is_autocommit()
    }

    fn rollback(&self) {
        if self.in_transaction() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }

    fn check(&self) -> Result<()> {
        if self.collation_failed.load(AtomicOrdering::SeqCst) { Err(Error::invalid(ts::INVALID)) } else { Ok(()) }
    }

    fn one_id(&self, sql: &str, params: impl Params) -> Result<Option<i64>> {
        let id = self.conn.prepare_cached(sql)?.query_row(params, |row| row.get(0)).optional()?;
        self.check()?;
        Ok(id)
    }

    fn scalar(&self, sql: &str, params: impl Params) -> Result<Sql> {
        let value = self.conn.prepare_cached(sql)?.query_row(params, |row| row.get(0))?;
        self.check()?;
        Ok(value)
    }

    fn rows(&self, sql: &str, params: impl Params) -> Result<Vec<Row>> {
        let mut statement = self.conn.prepare_cached(sql)?;
        let mapped = statement.query_map(params, |row| {
            Ok(Row {
                id: row.get(0)?,
                provider: row.get(1)?,
                observed_at: row.get(2)?,
                weekly_used: row.get(3)?,
                weekly_reset_at: row.get(4)?,
                short_used: row.get(5)?,
                short_reset_at: row.get(6)?,
                source: row.get(7)?,
                schema_marker: row.get(8)?,
            })
        })?;
        let rows = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
        self.check()?;
        Ok(rows)
    }

    fn mark_history_uncertain(&self, provider: &str, latest_uncertain_at: Ts) -> Result<()> {
        let mut until = latest_uncertain_at + 24 * HOUR;
        let existing: Option<Sql> = self
            .conn
            .prepare_cached("SELECT history_uncertain_until FROM governor_state WHERE provider = ?")?
            .query_row([provider], |row| row.get(0))
            .optional()?;
        if let Some(value) = existing.filter(|value| *value != Sql::Null) {
            until = until.max(sql_ts(&value)?);
        }
        self.conn.execute(
            "INSERT INTO governor_state(provider, history_uncertain_until)
             VALUES (?, ?)
             ON CONFLICT(provider) DO UPDATE SET
               history_uncertain_until=excluded.history_uncertain_until,
               candidate_mode=NULL, candidate_first_at=NULL, candidate_last_at=NULL",
            params![provider, until.iso()],
        )?;
        Ok(())
    }

    /// `_retained_rows`: keep 24-hour hourly endpoints, mandatory policy anchors, and recent rows.
    fn retained_rows(&self, provider: &str, prune: bool) -> Result<Vec<Row>> {
        let (c1, c2) = (CONFLICT_DECREASING, CONFLICT_EQUAL_TIME);
        let mut ids: BTreeSet<i64> = BTreeSet::new();
        let mut required: BTreeSet<i64> = BTreeSet::new();
        let near_week = "ABS((julianday(weekly_reset_at) - julianday(?)) * 86400.0) <= ?";

        let latest_week = self.one_id(
            &format!(
                "SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} \
                 ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1"
            ),
            params![provider, c1, c2],
        )?;
        if let Some(latest_id) = latest_week {
            ids.insert(latest_id);
            let (weekly_reset, observed_at): (Sql, Sql) = self.conn.query_row(
                "SELECT weekly_reset_at, observed_at FROM observations WHERE id = ?",
                [latest_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let latest_at = sql_ts(&observed_at)?;
            let cutoff = latest_at - 24 * HOUR;
            let first_bin = (cutoff.timestamp() / 3600.0).floor() as i64;
            let last_bin = (latest_at.timestamp() / 3600.0).floor() as i64;
            let first_sql = format!(
                "SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} AND {near_week} \
                 AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) < julianday(?) \
                 AND julianday(observed_at) <= julianday(?) ORDER BY observed_at COLLATE demon_timestamp, id LIMIT 1"
            );
            let last_sql = format!(
                "SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} AND {near_week} \
                 AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) < julianday(?) \
                 AND julianday(observed_at) <= julianday(?) ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1"
            );
            let latest_stamp = latest_at.iso();
            for hour_bin in first_bin..=last_bin {
                let start = Ts::from_micros(hour_bin * HOUR);
                let end = start + HOUR;
                let (start, end) = (start.iso(), end.iso());
                for sql in [&first_sql, &last_sql] {
                    let found = self.one_id(
                        sql,
                        params![provider, c1, c2, weekly_reset, RESET_JITTER_SECONDS, start, end, latest_stamp],
                    )?;
                    if let Some(id) = found {
                        ids.insert(id);
                        required.insert(id);
                    }
                }
            }
            required.insert(latest_id);
            for order in [
                "ORDER BY weekly_used DESC, observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1",
                "ORDER BY julianday(weekly_reset_at), observed_at COLLATE demon_timestamp, id LIMIT 1",
            ] {
                let sql =
                    format!("SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} AND {near_week} {order}");
                if let Some(id) = self.one_id(&sql, params![provider, c1, c2, weekly_reset, RESET_JITTER_SECONDS])? {
                    required.insert(id);
                }
            }
        }

        let latest_short = self.one_id(
            &format!(
                "SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} \
                 AND short_used IS NOT NULL AND short_reset_at IS NOT NULL \
                 ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1"
            ),
            params![provider, c1, c2],
        )?;
        if let Some(short_id) = latest_short {
            ids.insert(short_id);
            required.insert(short_id);
            let short_reset: Sql =
                self.conn
                    .query_row("SELECT short_reset_at FROM observations WHERE id = ?", [short_id], |row| row.get(0))?;
            let short_near = "ABS((julianday(short_reset_at) - julianday(?)) * 86400.0) <= ?";
            for order in [
                "ORDER BY short_used DESC, observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1",
                "ORDER BY julianday(short_reset_at), observed_at COLLATE demon_timestamp, id LIMIT 1",
            ] {
                let sql = format!(
                    "SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} \
                     AND short_used IS NOT NULL AND {short_near} {order}"
                );
                if let Some(id) = self.one_id(&sql, params![provider, c1, c2, short_reset, RESET_JITTER_SECONDS])? {
                    required.insert(id);
                }
            }
        }
        let latest_conflict = self.one_id(
            "SELECT id FROM observations WHERE provider = ? AND schema_marker IN (?, ?) \
             ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1",
            params![provider, c1, c2],
        )?;
        let latest_any = self.one_id(
            "SELECT id FROM observations WHERE provider = ? ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT 1",
            params![provider],
        )?;
        for id in [latest_conflict, latest_any].into_iter().flatten() {
            ids.insert(id);
            required.insert(id);
        }
        ids.extend(required.iter().copied());

        let recent: Vec<i64> = {
            let mut statement = self.conn.prepare_cached(
                "SELECT id FROM observations WHERE provider = ? \
                 ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT ?",
            )?;
            let found = statement.query_map(params![provider, MAX_OBSERVATIONS as i64], |row| row.get(0))?;
            found.collect::<rusqlite::Result<_>>()?
        };
        self.check()?;
        for id in recent {
            if ids.len() >= MAX_OBSERVATIONS {
                break;
            }
            ids.insert(id);
        }

        let ordered: Vec<i64> = ids.iter().copied().collect();
        let placeholders = vec!["?"; ordered.len()].join(",");
        let with_ids = |head: Vec<Sql>, tail: Vec<Sql>| -> Vec<Sql> {
            head.into_iter().chain(ordered.iter().map(|id| Sql::Integer(*id))).chain(tail).collect()
        };
        let selected = if ordered.is_empty() {
            Vec::new()
        } else {
            self.rows(
                &format!(
                    "SELECT {COLUMNS} FROM observations WHERE provider = ? AND id IN ({placeholders}) \
                     ORDER BY observed_at COLLATE demon_timestamp, id"
                ),
                params_from_iter(with_ids(vec![Sql::Text(provider.into())], vec![])),
            )?
        };

        let selected_ids: BTreeSet<i64> = selected.iter().map(|row| row.id).collect();
        let coverage_lost = required.len() > MAX_OBSERVATIONS || selected_ids != ids;
        if let (Some(latest_id), false) = (latest_week, coverage_lost) {
            let (observed_at, weekly_used, weekly_reset): (Sql, Sql, Sql) = self.conn.query_row(
                "SELECT observed_at, weekly_used, weekly_reset_at FROM observations WHERE id = ?",
                [latest_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let latest_at = sql_ts(&observed_at)?;
            let cutoff = latest_at - 24 * HOUR;
            let raw_pair = self.one_id(
                &format!(
                    "SELECT id FROM observations WHERE provider = ? AND {ACCEPTED} \
                     AND ABS((julianday(weekly_reset_at) - julianday(?)) * 86400.0) <= ? \
                     AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) <= julianday(?) - 0.25 \
                     AND weekly_used <= ? ORDER BY observed_at COLLATE demon_timestamp, id LIMIT 1"
                ),
                params![
                    provider,
                    c1,
                    c2,
                    weekly_reset,
                    RESET_JITTER_SECONDS,
                    cutoff.iso(),
                    latest_at.iso(),
                    weekly_used
                ],
            )?;
            if prune
                && raw_pair.is_some()
                && !has_qualifying_recent_pair(&selected)?
                && !has_boundary_pace_bound(&selected)?
            {
                let head = vec![
                    Sql::Text(provider.into()),
                    Sql::Text(c1.into()),
                    Sql::Text(c2.into()),
                    weekly_reset.clone(),
                    Sql::Integer(RESET_JITTER_SECONDS),
                    Sql::Text(cutoff.iso()),
                    Sql::Text(latest_at.iso()),
                ];
                let latest_lost = self.scalar(
                    &format!(
                        "SELECT MAX(observed_at COLLATE demon_timestamp) FROM observations WHERE provider = ? AND {ACCEPTED} \
                         AND ABS((julianday(weekly_reset_at) - julianday(?)) * 86400.0) <= ? \
                         AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) <= julianday(?) \
                         AND id NOT IN ({placeholders})"
                    ),
                    params_from_iter(with_ids(head, vec![])),
                )?;
                let lost_at = if latest_lost == Sql::Null { latest_at } else { sql_ts(&latest_lost)? };
                self.mark_history_uncertain(provider, lost_at)?;
            }
        } else if prune && coverage_lost {
            let latest_at = self.scalar(
                "SELECT MAX(observed_at COLLATE demon_timestamp) FROM observations WHERE provider = ?",
                [provider],
            )?;
            if latest_at != Sql::Null {
                self.mark_history_uncertain(provider, sql_ts(&latest_at)?)?;
            }
        }

        if prune {
            if ordered.is_empty() {
                self.conn.execute("DELETE FROM observations WHERE provider = ?", [provider])?;
            } else {
                self.conn.execute(
                    &format!("DELETE FROM observations WHERE provider = ? AND id NOT IN ({placeholders})"),
                    params_from_iter(with_ids(vec![Sql::Text(provider.into())], vec![])),
                )?;
            }
        }
        Ok(selected)
    }

    fn observations(&self, provider: Provider, now: Ts, prune: bool) -> Result<Vec<Observation>> {
        self.retained_rows(provider.name(), prune)?.iter().map(|row| observation_from_row(row, now)).collect()
    }

    fn state(&self, provider: Provider) -> Result<Option<GovernorState>> {
        let row: Option<[Sql; 7]> = self
            .conn
            .prepare_cached(
                "SELECT window_reset_at, base_mode, last_change_at, candidate_mode, candidate_first_at, \
                 candidate_last_at, history_uncertain_until FROM governor_state WHERE provider = ?",
            )?
            .query_row([provider.name()], |row| {
                Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?])
            })
            .optional()?;
        let Some([window, base, last_change, candidate, first, last, uncertain]) = row else {
            return Ok(None);
        };
        let invalid = || Error::unavailable("invalid stored governor state");
        let mode = |value: &Sql| -> Result<Option<Mode>> {
            match value {
                Sql::Null => Ok(None),
                Sql::Text(text) => Mode::parse(text).map(Some).ok_or_else(invalid),
                _ => Err(invalid()),
            }
        };
        let stamp = |value: &Sql| -> Result<Option<Ts>> {
            match value {
                Sql::Null => Ok(None),
                other => sql_ts(other).map(Some).map_err(|_| invalid()),
            }
        };
        Ok(Some(GovernorState {
            base_mode: mode(&base)?,
            candidate_mode: mode(&candidate)?,
            window_reset_at: stamp(&window)?,
            last_change_at: stamp(&last_change)?,
            candidate_first_at: stamp(&first)?,
            candidate_last_at: stamp(&last)?,
            history_uncertain_until: stamp(&uncertain)?,
        }))
    }

    fn save_state(&self, provider: Provider, state: &GovernorState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO governor_state
               (provider, window_reset_at, base_mode, last_change_at,
                candidate_mode, candidate_first_at, candidate_last_at, history_uncertain_until)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(provider) DO UPDATE SET
               window_reset_at=excluded.window_reset_at,
               base_mode=excluded.base_mode,
               last_change_at=excluded.last_change_at,
               candidate_mode=excluded.candidate_mode,
               candidate_first_at=excluded.candidate_first_at,
               candidate_last_at=excluded.candidate_last_at,
               history_uncertain_until=excluded.history_uncertain_until",
            params![
                provider.name(),
                opt_stamp(state.window_reset_at),
                state.base_mode.map(Mode::name),
                opt_stamp(state.last_change_at),
                state.candidate_mode.map(Mode::name),
                opt_stamp(state.candidate_first_at),
                opt_stamp(state.candidate_last_at),
                opt_stamp(state.history_uncertain_until),
            ],
        )?;
        Ok(())
    }

    fn insert(&self, observation: &Observation) -> Result<()> {
        self.conn.execute(
            "INSERT INTO observations
               (provider, observed_at, weekly_used, weekly_reset_at, short_used,
                short_reset_at, source, schema_marker)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                observation.provider.name(),
                observation.observed_at.iso(),
                observation.weekly_used,
                observation.weekly_reset_at.iso(),
                observation.short_used,
                opt_stamp(observation.short_reset_at),
                observation.source.name(),
                observation.schema_marker,
            ],
        )?;
        Ok(())
    }

    fn record_in_transaction(&self, observation: &Observation, now: Ts) -> Result<Policy> {
        let provider = observation.provider;
        let prior = self.observations(provider, now, false)?;
        if prior.iter().any(|item| same_measurement(item, observation)) {
            let prior = self.observations(provider, now, true)?;
            let result = evaluate_policy(provider, &prior, self.state(provider)?.as_ref(), now, None);
            self.exec("COMMIT")?;
            return Ok(result);
        }

        let mut stored = observation.clone();
        if let Some(marker) = has_conflict(&prior, observation) {
            stored.schema_marker = marker.to_string();
            // A backfill can disprove newer trusted headroom. Invalidate those samples too, so a read
            // cannot hide the conflict merely because its original observation time is older.
            for item in &prior {
                if !is_conflict(&item.schema_marker)
                    && item.observed_at > observation.observed_at
                    && decreases(observation, item)
                {
                    self.conn.execute(
                        "UPDATE observations SET schema_marker=? WHERE provider=? AND observed_at COLLATE demon_timestamp=?",
                        params![marker, item.provider.name(), item.observed_at.iso()],
                    )?;
                    self.check()?;
                }
            }
        }
        self.insert(&stored)?;
        let readings = self.observations(provider, now, true)?;
        let mut state = self.state(provider)?;
        if !is_conflict(&stored.schema_marker) {
            let latest_prior = first_max(&prior, |item| item.observed_at);
            if let (Some(latest), Some(current)) = (latest_prior, &state) {
                if observation.observed_at <= latest.observed_at && current.history_uncertain_until.is_some() {
                    let cleared = current.without_candidate();
                    self.save_state(provider, &cleared)?;
                    state = Some(cleared);
                }
            }
            if latest_prior.is_none_or(|latest| observation.observed_at > latest.observed_at) {
                let metrics = weekly_metrics(&readings, now);
                let uncertain = state.as_ref().and_then(|current| current.history_uncertain_until);
                let fresh_recovery = metrics.available
                    && metrics.age.is_some_and(|age| age <= REFRESH_AGE)
                    && metrics.latest.as_ref() == Some(observation);
                if let (Some(until), Some(current)) = (uncertain, state.clone()) {
                    let new_window = fresh_recovery
                        && current
                            .window_reset_at
                            .is_some_and(|window| observation.weekly_reset_at - window > RESET_JITTER_SECONDS * SECOND);
                    let history_recovered = fresh_recovery && observation.observed_at >= until && metrics.credible_pace;
                    if new_window || history_recovered {
                        let recovered = GovernorState { history_uncertain_until: None, ..current.without_candidate() };
                        self.save_state(provider, &recovered)?;
                        state = Some(recovered);
                    } else {
                        let held = current.without_candidate();
                        self.save_state(provider, &held)?;
                        let result = evaluate_policy(provider, &readings, Some(&held), now, None);
                        self.exec("COMMIT")?;
                        return Ok(result);
                    }
                }
                let recovered = latest_prior.is_some_and(|latest| now - latest.observed_at > REFRESH_AGE);
                let next = advance_state(state.as_ref(), &metrics, now, observation.observed_at, recovered);
                self.save_state(provider, &next)?;
                state = Some(next);
            }
        } else if let Some(current) = &state {
            let cleared = current.without_candidate();
            self.save_state(provider, &cleared)?;
            state = Some(cleared);
        }
        let result = evaluate_policy(provider, &readings, state.as_ref(), now, None);
        self.exec("COMMIT")?;
        Ok(result)
    }

    fn load_in_transaction(&self, provider: Provider, now: Ts) -> Result<Policy> {
        let count_sql = "SELECT COUNT(*) FROM observations WHERE provider = ?";
        let latest_sql = "SELECT MAX(observed_at COLLATE demon_timestamp) FROM observations WHERE provider = ?";
        let mut observations = self.observations(provider, now, false)?;
        let mut state = self.state(provider)?;
        let count: i64 = self.conn.query_row(count_sql, [provider.name()], |row| row.get(0))?;
        let unmarked =
            |state: &Option<GovernorState>| state.as_ref().is_none_or(|s| s.history_uncertain_until.is_none());
        if count > MAX_OBSERVATIONS as i64 && unmarked(&state) {
            let latest = self.scalar(latest_sql, [provider.name()])?;
            self.exec("ROLLBACK")?;
            self.exec("BEGIN IMMEDIATE")?;
            observations = self.observations(provider, now, false)?;
            state = self.state(provider)?;
            let count: i64 = self.conn.query_row(count_sql, [provider.name()], |row| row.get(0))?;
            if count > MAX_OBSERVATIONS as i64 && unmarked(&state) {
                let mut newest = self.scalar(latest_sql, [provider.name()])?;
                if newest == Sql::Null {
                    newest = latest;
                }
                if newest != Sql::Null {
                    self.mark_history_uncertain(provider.name(), sql_ts(&newest)?)?;
                }
                state = self.state(provider)?;
            }
        }
        let result = evaluate_policy(provider, &observations, state.as_ref(), now, None);
        self.exec("COMMIT")?;
        Ok(result)
    }
}

fn row_time(value: &Sql) -> Result<Ts> {
    sql_ts(value)
}

/// The latest accepted row, ties broken by id (the Python `(time, id)` key).
fn latest_accepted(rows: &[Row]) -> Result<Option<(&Row, Ts)>> {
    let mut best: Option<(&Row, Ts)> = None;
    for row in rows.iter().filter(|row| row.accepted()) {
        let at = row_time(&row.observed_at)?;
        if best.is_none_or(|(current, current_at)| (at, row.id) > (current_at, current.id)) {
            best = Some((row, at));
        }
    }
    Ok(best)
}

fn same_week_rows<'a>(rows: &'a [Row], latest: &Row) -> Result<Vec<&'a Row>> {
    let reset = row_time(&latest.weekly_reset_at)?;
    let mut week = Vec::new();
    for row in rows.iter().filter(|row| row.accepted()) {
        if (row_time(&row.weekly_reset_at)? - reset).abs() <= RESET_JITTER_SECONDS * SECOND {
            week.push(row);
        }
    }
    Ok(week)
}

fn has_qualifying_recent_pair(rows: &[Row]) -> Result<bool> {
    let Some((latest, latest_at)) = latest_accepted(rows)? else {
        return Ok(false);
    };
    let latest_used = sql_number(&latest.weekly_used)?;
    for row in same_week_rows(rows, latest)? {
        let gap = latest_at - row_time(&row.observed_at)?;
        if (6 * HOUR..=24 * HOUR).contains(&gap) && latest_used >= sql_number(&row.weekly_used)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_boundary_pace_bound(rows: &[Row]) -> Result<bool> {
    let Some((latest, latest_at)) = latest_accepted(rows)? else {
        return Ok(false);
    };
    let cutoff = latest_at - 24 * HOUR;
    let cutoff_bin = (cutoff.timestamp() / 3600.0).floor() as i64;
    let mut boundary = Vec::new();
    for row in same_week_rows(rows, latest)? {
        let at = row_time(&row.observed_at)?;
        if (at.timestamp() / 3600.0).floor() as i64 == cutoff_bin {
            boundary.push((at, row.id, row));
        }
    }
    boundary.sort_by_key(|(at, id, _)| (*at, *id));
    let (Some(first), Some(last)) = (boundary.first(), boundary.last()) else {
        return Ok(false);
    };
    let hours = latest_at.hours_since(last.0);
    Ok(first.0 < cutoff
        && cutoff <= last.0
        && hours > 0.0
        && sql_number(&latest.weekly_used)? >= sql_number(&first.2.weekly_used)?)
}

fn observation_from_row(row: &Row, now: Ts) -> Result<Observation> {
    let source = match &row.source {
        Sql::Text(name) => Source::parse(name),
        _ => None,
    };
    let marker = row.marker();
    let (Some(source), Some(marker)) = (source, marker) else {
        return Err(Error::invalid("invalid stored observation schema"));
    };
    if marker != source.schema() && !is_conflict(marker) {
        return Err(Error::invalid("invalid stored observation schema"));
    }
    let reading = Reading {
        provider: val(&row.provider),
        observed_at: val(&row.observed_at),
        weekly_used: val(&row.weekly_used),
        weekly_reset_at: val(&row.weekly_reset_at),
        short_used: val(&row.short_used),
        short_reset_at: val(&row.short_reset_at),
    };
    let mut observation = normalize_observation(&reading, source, now)?;
    observation.schema_marker = marker.to_string();
    Ok(observation)
}

/// Compare chronological samples independently in each allowance window.
fn decreases(first: &Observation, second: &Observation) -> bool {
    let jitter = RESET_JITTER_SECONDS * SECOND;
    let weekly =
        (first.weekly_reset_at - second.weekly_reset_at).abs() <= jitter && second.weekly_used < first.weekly_used;
    let short = match (first.short_used, second.short_used, first.short_reset_at, second.short_reset_at) {
        (Some(first_used), Some(second_used), Some(first_reset), Some(second_reset)) => {
            (first_reset - second_reset).abs() <= jitter && second_used < first_used
        }
        _ => false,
    };
    weekly || short
}

fn has_conflict(existing: &[Observation], incoming: &Observation) -> Option<&'static str> {
    let jitter = RESET_JITTER_SECONDS * SECOND;
    let same_window = existing.iter().filter(|item| (item.weekly_reset_at - incoming.weekly_reset_at).abs() <= jitter);
    for item in same_window {
        if item.observed_at == incoming.observed_at && item != incoming {
            return Some(CONFLICT_EQUAL_TIME);
        }
    }
    for item in existing.iter().filter(|item| !is_conflict(&item.schema_marker)) {
        if item.observed_at == incoming.observed_at {
            if decreases(item, incoming) || decreases(incoming, item) {
                return Some(CONFLICT_EQUAL_TIME);
            }
        } else if item.observed_at < incoming.observed_at {
            if decreases(item, incoming) {
                return Some(CONFLICT_DECREASING);
            }
        } else if decreases(incoming, item) {
            return Some(CONFLICT_DECREASING);
        }
    }
    None
}

fn same_measurement(first: &Observation, second: &Observation) -> bool {
    first.provider == second.provider
        && first.observed_at == second.observed_at
        && first.weekly_used == second.weekly_used
        && first.weekly_reset_at == second.weekly_reset_at
        && first.short_used == second.short_used
        && first.short_reset_at == second.short_reset_at
        && first.source == second.source
}

/// Store an accepted reading and its governor transition in one transaction.
pub fn record_observation(
    database: Option<&str>,
    observation: &Observation,
    now: Ts,
    timeout: Duration,
) -> Result<Policy> {
    let ledger = Ledger::open(database, timeout)?;
    ledger.exec("BEGIN IMMEDIATE")?;
    let outcome = ledger.record_in_transaction(observation, now);
    if outcome.is_err() {
        ledger.rollback();
    }
    outcome
}

/// Validate a manual reading and record it.
pub fn record_manual(database: Option<&str>, reading: &Reading, now: Ts) -> Result<Policy> {
    let observation = normalize_observation(reading, Source::Manual, now)?;
    record_observation(database, &observation, now, WRITE_TIMEOUT)
}

/// Read local state; storage failures produce a conservative refresh policy.
pub fn load_policy(database: Option<&str>, provider: Provider, now: Ts) -> Policy {
    let attempt = || -> Result<Policy> {
        let ledger = Ledger::open(database, READ_TIMEOUT)?;
        ledger.exec("BEGIN")?;
        let outcome = ledger.load_in_transaction(provider, now);
        if outcome.is_err() {
            ledger.rollback();
        }
        outcome
    };
    attempt().unwrap_or_else(|_| evaluate_policy(provider, &[], None, now, Some("state_unavailable")))
}

/// Like [`load_policy`], but never creates a ledger that does not exist yet. Used by the read-only
/// MCP tools, where answering a question must not leave a file behind.
pub fn peek_policy(database: Option<&str>, provider: Provider, now: Ts) -> (Policy, bool) {
    let path = match database {
        Some(":memory:") => return (load_policy(database, provider, now), true),
        Some(value) => paths::absolute(&paths::expanduser(value)),
        None => paths::absolute(&paths::default_database_path()),
    };
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (evaluate_policy(provider, &[], None, now, None), false)
        }
        _ => (load_policy(database, provider, now), true),
    }
}

/// Newest observations first, at most `min(limit, 64)`.
pub fn load_history(database: Option<&str>, provider: Provider, limit: i64, now: Ts) -> Result<Vec<Observation>> {
    if !(1..=1000).contains(&limit) {
        return Err(Error::invalid("history limit must be between 1 and 1000"));
    }
    let ledger = Ledger::open(database, READ_TIMEOUT)?;
    ledger.exec("BEGIN")?;
    let outcome = (|| -> Result<Vec<Observation>> {
        let rows = ledger.rows(
            &format!(
                "SELECT {COLUMNS} FROM observations WHERE provider = ? \
                 ORDER BY observed_at COLLATE demon_timestamp DESC, id DESC LIMIT ?"
            ),
            params![provider.name(), limit.min(MAX_OBSERVATIONS as i64)],
        )?;
        let values = rows.iter().map(|row| observation_from_row(row, now)).collect::<Result<Vec<_>>>()?;
        ledger.exec("COMMIT")?;
        Ok(values)
    })();
    if outcome.is_err() {
        ledger.rollback();
    }
    outcome
}
