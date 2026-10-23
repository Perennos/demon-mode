"""Local transactional storage for manual DEMON MODE observations."""

from __future__ import annotations

from dataclasses import replace
from datetime import datetime, timedelta, timezone
import math
import os
from pathlib import Path
import sqlite3
import stat
from typing import Optional

from .governor import (
    CONFLICT_SCHEMAS,
    MODE_RANK,
    PROVIDERS,
    SOURCE_SCHEMAS,
    GovernorState,
    Observation,
    Policy,
    REFRESH_AGE,
    advance_state,
    evaluate_policy,
    normalize_observation,
    parse_timestamp,
    weekly_metrics,
)

_SCHEMA_VERSION = 2
_RESET_JITTER_SECONDS = 2
_MAX_OBSERVATIONS = 64


def default_database_path() -> Path:
    """Return the per-user local state path without contacting any service."""
    if os.name == "nt":
        base = Path(os.environ.get("LOCALAPPDATA") or Path.home() / "AppData" / "Local")
    else:
        base = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share")
    return base / "Perennos" / "demon-mode" / "state.sqlite3"


def _validate_state_path(path: Path) -> None:
    """Reject linked or non-regular state paths before SQLite can follow them."""
    path = path.absolute()
    reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    parent = Path(path.anchor)
    for part in path.parts[1:-1]:
        parent /= part
        try:
            info = parent.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & reparse_flag:
            raise sqlite3.OperationalError("state parent must not be a link")
        if not stat.S_ISDIR(info.st_mode):
            raise sqlite3.OperationalError("state parent must be a directory")
    try:
        info = path.lstat()
    except FileNotFoundError:
        return
    if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & reparse_flag:
        raise sqlite3.OperationalError("state file must not be a link")
    if not stat.S_ISREG(info.st_mode):
        raise sqlite3.OperationalError("state file must be regular")


def _connect(database: os.PathLike[str] | str | None, *, timeout: float = 5.0) -> sqlite3.Connection:
    is_default = database is None
    value = os.fspath(database) if database is not None else os.fspath(default_database_path())
    if value != ":memory:":
        path = Path(value).expanduser().absolute()
        _validate_state_path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        _validate_state_path(path)
        if is_default and os.name != "nt":
            path.parent.chmod(0o700)
        value = os.fspath(path)
    connection = sqlite3.connect(value, timeout=timeout, isolation_level=None)
    connection.row_factory = sqlite3.Row
    try:
        version = connection.execute("PRAGMA user_version").fetchone()[0]
        if version > _SCHEMA_VERSION:
            raise sqlite3.DatabaseError("state schema is newer than this program")
        connection.execute("PRAGMA foreign_keys = ON")
        connection.execute("PRAGMA journal_mode = DELETE")
        connection.execute("PRAGMA synchronous = FULL")
        connection.executescript(
            """
            CREATE TABLE IF NOT EXISTS observations (
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
            );
            """
        )
        columns = {
            row["name"] for row in connection.execute("PRAGMA table_info(governor_state)")
        }
        coverage_unestablished = version < _SCHEMA_VERSION or "history_uncertain_until" not in columns
        if coverage_unestablished:
            connection.execute("BEGIN IMMEDIATE")
            try:
                if "history_uncertain_until" not in columns:
                    connection.execute(
                        "ALTER TABLE governor_state ADD COLUMN history_uncertain_until TEXT"
                    )
                legacy = connection.execute(
                    """SELECT provider, COUNT(*) AS row_count, MAX(observed_at) AS latest
                       FROM observations GROUP BY provider HAVING COUNT(*) >= ?""",
                    (_MAX_OBSERVATIONS,),
                ).fetchall()
                for row in legacy:
                    uncertain_until = _stamp(
                        parse_timestamp(row["latest"]) + timedelta(hours=24)
                    )
                    connection.execute(
                        """INSERT INTO governor_state(provider, history_uncertain_until)
                           VALUES (?, ?)
                           ON CONFLICT(provider) DO UPDATE SET
                             history_uncertain_until=excluded.history_uncertain_until,
                             candidate_mode=NULL, candidate_first_at=NULL, candidate_last_at=NULL""",
                        (row["provider"], uncertain_until),
                    )
                connection.execute(f"PRAGMA user_version = {_SCHEMA_VERSION}")
                connection.execute("COMMIT")
            except Exception:
                if connection.in_transaction:
                    connection.execute("ROLLBACK")
                raise
        if is_default and os.name != "nt" and value != ":memory:":
            _validate_state_path(Path(value))
            Path(value).chmod(0o600)
    except Exception:
        connection.close()
        raise
    return connection


def _stamp(value: Optional[datetime]) -> Optional[str]:
    return value.isoformat().replace("+00:00", "Z") if value is not None else None


def _mark_history_uncertain(
    connection: sqlite3.Connection, provider: str, latest_uncertain_at: datetime
) -> None:
    until = parse_timestamp(latest_uncertain_at) + timedelta(hours=24)
    row = connection.execute(
        "SELECT history_uncertain_until FROM governor_state WHERE provider = ?", (provider,)
    ).fetchone()
    if row is not None and row[0] is not None:
        until = max(until, parse_timestamp(row[0]))
    connection.execute(
        """INSERT INTO governor_state(provider, history_uncertain_until)
           VALUES (?, ?)
           ON CONFLICT(provider) DO UPDATE SET
             history_uncertain_until=excluded.history_uncertain_until,
             candidate_mode=NULL, candidate_first_at=NULL, candidate_last_at=NULL""",
        (provider, _stamp(until)),
    )


def _row_time(row: sqlite3.Row, column: str = "observed_at") -> datetime:
    return parse_timestamp(row[column])


def _has_qualifying_recent_pair(rows: list[sqlite3.Row]) -> bool:
    accepted = [row for row in rows if row["schema_marker"] not in CONFLICT_SCHEMAS]
    if not accepted:
        return False
    latest = max(accepted, key=lambda row: (_row_time(row), row["id"]))
    latest_at = _row_time(latest)
    week = [
        row for row in accepted
        if abs((_row_time(row, "weekly_reset_at") - _row_time(latest, "weekly_reset_at")).total_seconds())
        <= _RESET_JITTER_SECONDS
    ]
    return any(
        timedelta(hours=6) <= latest_at - _row_time(row) <= timedelta(hours=24)
        and latest["weekly_used"] >= row["weekly_used"]
        for row in week
    )


def _has_boundary_pace_bound(rows: list[sqlite3.Row]) -> bool:
    accepted = [row for row in rows if row["schema_marker"] not in CONFLICT_SCHEMAS]
    if not accepted:
        return False
    latest = max(accepted, key=lambda row: (_row_time(row), row["id"]))
    latest_at = _row_time(latest)
    cutoff = latest_at - timedelta(hours=24)
    cutoff_bin = math.floor(cutoff.timestamp() / 3600.0)
    week = [
        row for row in accepted
        if abs((_row_time(row, "weekly_reset_at") - _row_time(latest, "weekly_reset_at")).total_seconds())
        <= _RESET_JITTER_SECONDS
    ]
    boundary = [
        row for row in week
        if math.floor(_row_time(row).timestamp() / 3600.0) == cutoff_bin
    ]
    if not boundary:
        return False
    boundary.sort(key=lambda row: (_row_time(row), row["id"]))
    first, last = boundary[0], boundary[-1]
    hours = (latest_at - _row_time(last)).total_seconds() / 3600.0
    return (
        _row_time(first) < cutoff <= _row_time(last)
        and hours > 0
        and latest["weekly_used"] >= first["weekly_used"]
    )


def _observation_from_row(row: sqlite3.Row, now: datetime) -> Observation:
    source = row["source"]
    marker = row["schema_marker"]
    expected = SOURCE_SCHEMAS.get(source)
    if expected is None or marker not in (expected, *CONFLICT_SCHEMAS):
        raise ValueError("invalid stored observation schema")
    observation = normalize_observation(
        provider=row["provider"],
        observed_at=row["observed_at"],
        weekly_used=row["weekly_used"],
        weekly_reset_at=row["weekly_reset_at"],
        short_used=row["short_used"],
        short_reset_at=row["short_reset_at"],
        source=source,
        schema_marker=expected,
        now=now,
    )
    return replace(observation, schema_marker=marker)


def _retained_rows(
    connection: sqlite3.Connection,
    provider: str,
    *,
    prune: bool = False,
) -> list[sqlite3.Row]:
    """Keep 24-hour hourly endpoints, mandatory policy anchors, and recent rows."""
    ids: set[int] = set()
    required_ids: set[int] = set()
    conflict_values = tuple(CONFLICT_SCHEMAS)
    accepted = "schema_marker NOT IN (?, ?)"

    def add_one(sql: str, params: tuple[object, ...]) -> Optional[sqlite3.Row]:
        row = connection.execute(sql, params).fetchone()
        if row is not None:
            ids.add(row[0])
        return row

    latest_week = add_one(
        f"SELECT id FROM observations WHERE provider = ? AND {accepted} ORDER BY observed_at DESC, id DESC LIMIT 1",
        (provider, *conflict_values),
    )
    if latest_week is not None:
        reset = connection.execute(
            "SELECT weekly_reset_at, observed_at FROM observations WHERE id = ?", (latest_week[0],)
        ).fetchone()
        weekly_reset, observed_at = reset
        near_week = "ABS((julianday(weekly_reset_at) - julianday(?)) * 86400.0) <= ?"
        latest_at = parse_timestamp(observed_at)
        cutoff = latest_at - timedelta(hours=24)
        first_bin = math.floor(cutoff.timestamp() / 3600.0)
        last_bin = math.floor(latest_at.timestamp() / 3600.0)
        for hour_bin in range(first_bin, last_bin + 1):
            start = datetime.fromtimestamp(hour_bin * 3600, timezone.utc)
            end = start + timedelta(hours=1)
            params = (
                provider, *conflict_values, weekly_reset, _RESET_JITTER_SECONDS,
                _stamp(start), _stamp(end), _stamp(latest_at),
            )
            first = add_one(
                f"SELECT id FROM observations WHERE provider = ? AND {accepted} AND {near_week} "
                "AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) < julianday(?) "
                "AND julianday(observed_at) <= julianday(?) ORDER BY observed_at, id LIMIT 1",
                params,
            )
            last = add_one(
                f"SELECT id FROM observations WHERE provider = ? AND {accepted} AND {near_week} "
                "AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) < julianday(?) "
                "AND julianday(observed_at) <= julianday(?) ORDER BY observed_at DESC, id DESC LIMIT 1",
                params,
            )
            if first is not None:
                required_ids.add(first[0])
            if last is not None:
                required_ids.add(last[0])
        required_ids.add(latest_week[0])
        peak = connection.execute(
            f"SELECT id FROM observations WHERE provider = ? AND {accepted} AND {near_week} "
            "ORDER BY weekly_used DESC, observed_at DESC, id DESC LIMIT 1",
            (provider, *conflict_values, weekly_reset, _RESET_JITTER_SECONDS),
        ).fetchone()
        earliest_deadline = connection.execute(
            f"SELECT id FROM observations WHERE provider = ? AND {accepted} AND {near_week} "
            "ORDER BY julianday(weekly_reset_at), observed_at, id LIMIT 1",
            (provider, *conflict_values, weekly_reset, _RESET_JITTER_SECONDS),
        ).fetchone()
        if peak is not None:
            required_ids.add(peak[0])
        if earliest_deadline is not None:
            required_ids.add(earliest_deadline[0])

    latest_short = add_one(
        f"SELECT id FROM observations WHERE provider = ? AND {accepted} "
        "AND short_used IS NOT NULL AND short_reset_at IS NOT NULL "
        "ORDER BY observed_at DESC, id DESC LIMIT 1",
        (provider, *conflict_values),
    )
    if latest_short is not None:
        required_ids.add(latest_short[0])
        short_reset = connection.execute(
            "SELECT short_reset_at FROM observations WHERE id = ?", (latest_short[0],)
        ).fetchone()[0]
        short_near = (
            "ABS((julianday(short_reset_at) - julianday(?)) * 86400.0) <= ?"
        )
        short_peak = connection.execute(
            f"SELECT id FROM observations WHERE provider = ? AND {accepted} "
            f"AND short_used IS NOT NULL AND {short_near} "
            "ORDER BY short_used DESC, observed_at DESC, id DESC LIMIT 1",
            (provider, *conflict_values, short_reset, _RESET_JITTER_SECONDS),
        ).fetchone()
        short_deadline = connection.execute(
            f"SELECT id FROM observations WHERE provider = ? AND {accepted} "
            f"AND short_used IS NOT NULL AND {short_near} "
            "ORDER BY julianday(short_reset_at), observed_at, id LIMIT 1",
            (provider, *conflict_values, short_reset, _RESET_JITTER_SECONDS),
        ).fetchone()
        if short_peak is not None:
            required_ids.add(short_peak[0])
        if short_deadline is not None:
            required_ids.add(short_deadline[0])
    latest_conflict = add_one(
        "SELECT id FROM observations WHERE provider = ? AND schema_marker IN (?, ?) "
        "ORDER BY observed_at DESC, id DESC LIMIT 1",
        (provider, *conflict_values),
    )
    if latest_conflict is not None:
        required_ids.add(latest_conflict[0])
    latest_any = add_one(
        "SELECT id FROM observations WHERE provider = ? ORDER BY observed_at DESC, id DESC LIMIT 1",
        (provider,),
    )
    if latest_any is not None:
        required_ids.add(latest_any[0])
    ids.update(required_ids)

    recent = connection.execute(
        "SELECT id FROM observations WHERE provider = ? ORDER BY observed_at DESC, id DESC LIMIT ?",
        (provider, _MAX_OBSERVATIONS),
    )
    for row in recent:
        if len(ids) >= _MAX_OBSERVATIONS:
            break
        ids.add(row[0])

    ordered_ids = tuple(sorted(ids))
    selected_rows: list[sqlite3.Row] = []
    if ordered_ids:
        placeholders = ",".join("?" for _ in ordered_ids)
        selected_rows = connection.execute(
            f"SELECT * FROM observations WHERE provider = ? AND id IN ({placeholders}) ORDER BY observed_at, id",
            (provider, *ordered_ids),
        ).fetchall()

    coverage_lost = len(required_ids) > _MAX_OBSERVATIONS or {
        row["id"] for row in selected_rows
    } != set(ordered_ids)
    if latest_week is not None and not coverage_lost:
        latest_observed = connection.execute(
            "SELECT observed_at, weekly_used, weekly_reset_at FROM observations WHERE id = ?",
            (latest_week[0],),
        ).fetchone()
        latest_at = parse_timestamp(latest_observed["observed_at"])
        cutoff = latest_at - timedelta(hours=24)
        raw_pair = connection.execute(
            f"SELECT id FROM observations WHERE provider = ? AND {accepted} "
            "AND ABS((julianday(weekly_reset_at) - julianday(?)) * 86400.0) <= ? "
            "AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) <= julianday(?) - 0.25 "
            "AND weekly_used <= ? ORDER BY observed_at, id LIMIT 1",
            (
                provider, *conflict_values, latest_observed["weekly_reset_at"],
                _RESET_JITTER_SECONDS, _stamp(cutoff), _stamp(latest_at), latest_observed["weekly_used"],
            ),
        ).fetchone()
        if (
            prune
            and raw_pair is not None
            and not _has_qualifying_recent_pair(selected_rows)
            and not _has_boundary_pace_bound(selected_rows)
        ):
            placeholders = ",".join("?" for _ in ordered_ids)
            latest_lost = connection.execute(
                f"SELECT MAX(observed_at) FROM observations WHERE provider = ? AND {accepted} "
                "AND ABS((julianday(weekly_reset_at) - julianday(?)) * 86400.0) <= ? "
                "AND julianday(observed_at) >= julianday(?) AND julianday(observed_at) <= julianday(?) "
                f"AND id NOT IN ({placeholders})",
                (
                    provider, *conflict_values, latest_observed["weekly_reset_at"],
                    _RESET_JITTER_SECONDS, _stamp(cutoff), _stamp(latest_at), *ordered_ids,
                ),
            ).fetchone()[0]
            _mark_history_uncertain(
                connection, provider,
                parse_timestamp(latest_lost) if latest_lost is not None else latest_at,
            )
    elif prune and coverage_lost:
        latest_at = connection.execute(
            "SELECT MAX(observed_at) FROM observations WHERE provider = ?", (provider,)
        ).fetchone()[0]
        if latest_at is not None:
            _mark_history_uncertain(connection, provider, parse_timestamp(latest_at))

    if prune:
        if ordered_ids:
            connection.execute(
                f"DELETE FROM observations WHERE provider = ? AND id NOT IN ({placeholders})",
                (provider, *ordered_ids),
            )
        else:
            connection.execute("DELETE FROM observations WHERE provider = ?", (provider,))
    return selected_rows


def _observations(
    connection: sqlite3.Connection,
    provider: str,
    now: Optional[datetime] = None,
    *,
    prune: bool = False,
) -> list[Observation]:
    current = parse_timestamp(now or datetime.now(timezone.utc))
    rows = _retained_rows(connection, provider, prune=prune)
    return [_observation_from_row(row, current) for row in rows]


def _state(connection: sqlite3.Connection, provider: str) -> Optional[GovernorState]:
    row = connection.execute(
        "SELECT * FROM governor_state WHERE provider = ?", (provider,)
    ).fetchone()
    if row is None:
        return None
    try:
        if row["base_mode"] not in (None, *MODE_RANK) or row["candidate_mode"] not in (None, *MODE_RANK):
            raise ValueError("invalid stored mode")
        return GovernorState(
            window_reset_at=(
                None if row["window_reset_at"] is None else parse_timestamp(row["window_reset_at"])
            ),
            base_mode=row["base_mode"],
            last_change_at=(
                None if row["last_change_at"] is None else parse_timestamp(row["last_change_at"])
            ),
            candidate_mode=row["candidate_mode"],
            candidate_first_at=(
                None if row["candidate_first_at"] is None else parse_timestamp(row["candidate_first_at"])
            ),
            candidate_last_at=(
                None if row["candidate_last_at"] is None else parse_timestamp(row["candidate_last_at"])
            ),
            history_uncertain_until=(
                None if row["history_uncertain_until"] is None
                else parse_timestamp(row["history_uncertain_until"])
            ),
        )
    except ValueError as exc:
        raise sqlite3.DatabaseError("invalid stored governor state") from exc


def _save_state(connection: sqlite3.Connection, provider: str, state: GovernorState) -> None:
    connection.execute(
        """INSERT INTO governor_state
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
             history_uncertain_until=excluded.history_uncertain_until""",
        (
            provider,
            _stamp(state.window_reset_at),
            state.base_mode,
            _stamp(state.last_change_at),
            state.candidate_mode,
            _stamp(state.candidate_first_at),
            _stamp(state.candidate_last_at),
            _stamp(state.history_uncertain_until),
        ),
    )


def _has_conflict(existing: list[Observation], incoming: Observation) -> Optional[str]:
    same_window = [
        item
        for item in existing
        if abs((item.weekly_reset_at - incoming.weekly_reset_at).total_seconds())
        <= _RESET_JITTER_SECONDS
    ]
    for item in same_window:
        if item.observed_at == incoming.observed_at and item != incoming:
            return "conflict-equal-time-v1"
    accepted = [item for item in same_window if item.schema_marker not in CONFLICT_SCHEMAS]
    earlier = [item for item in accepted if item.observed_at < incoming.observed_at]
    if not earlier:
        return None
    weekly_peak = max(item.weekly_used for item in earlier)
    weekly_decreased = incoming.weekly_used < weekly_peak
    short_prior = [
        item
        for item in earlier
        if item.short_used is not None
        and item.short_reset_at is not None
        and incoming.short_used is not None
        and incoming.short_reset_at is not None
        and abs((incoming.short_reset_at - item.short_reset_at).total_seconds())
        <= _RESET_JITTER_SECONDS
    ]
    short_peak = max((item.short_used for item in short_prior), default=None)
    short_decreased = short_peak is not None and incoming.short_used < short_peak
    return "conflict-decreasing-v1" if weekly_decreased or short_decreased else None


def _same_measurement(first: Observation, second: Observation) -> bool:
    return (
        first.provider == second.provider
        and first.observed_at == second.observed_at
        and first.weekly_used == second.weekly_used
        and first.weekly_reset_at == second.weekly_reset_at
        and first.short_used == second.short_used
        and first.short_reset_at == second.short_reset_at
        and first.source == second.source
    )


def _insert(connection: sqlite3.Connection, observation: Observation) -> None:
    connection.execute(
        """INSERT INTO observations
           (provider, observed_at, weekly_used, weekly_reset_at, short_used,
            short_reset_at, source, schema_marker)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
        (
            observation.provider,
            _stamp(observation.observed_at),
            observation.weekly_used,
            _stamp(observation.weekly_reset_at),
            observation.short_used,
            _stamp(observation.short_reset_at),
            observation.source,
            observation.schema_marker,
        ),
    )


def record_manual(
    database: os.PathLike[str] | str | None,
    *,
    provider: str,
    observed_at: object,
    weekly_used: object,
    weekly_reset_at: object,
    short_used: object = None,
    short_reset_at: object = None,
    now: Optional[datetime] = None,
) -> Policy:
    current = parse_timestamp(now or datetime.now().astimezone())
    observation = normalize_observation(
        provider=provider,
        observed_at=observed_at,
        weekly_used=weekly_used,
        weekly_reset_at=weekly_reset_at,
        short_used=short_used,
        short_reset_at=short_reset_at,
        source="manual",
        schema_marker=SOURCE_SCHEMAS["manual"],
        now=current,
    )
    return record_observation(database, observation, now=current)


def record_observation(
    database: os.PathLike[str] | str | None,
    observation: Observation,
    *,
    now: datetime,
) -> Policy:
    """Store an accepted reading and its governor transition in one transaction."""
    if observation.provider not in PROVIDERS:
        raise ValueError("unsupported provider")
    current = parse_timestamp(now)
    connection = _connect(database)
    try:
        connection.execute("BEGIN IMMEDIATE")
        prior = _observations(connection, observation.provider, current)
        if any(_same_measurement(item, observation) for item in prior):
            prior = _observations(connection, observation.provider, current, prune=True)
            result = evaluate_policy(
                observation.provider, prior, _state(connection, observation.provider), now=current
            )
            connection.execute("COMMIT")
            return result

        conflict = _has_conflict(prior, observation)
        stored = observation
        if conflict:
            stored = replace(observation, schema_marker=conflict)
        _insert(connection, stored)
        readings = _observations(connection, observation.provider, current, prune=True)
        state = _state(connection, observation.provider)
        if stored.schema_marker not in CONFLICT_SCHEMAS:
            latest_prior = max(prior, key=lambda item: item.observed_at, default=None)
            if (
                latest_prior is not None
                and observation.observed_at <= latest_prior.observed_at
                and state is not None
                and state.history_uncertain_until is not None
            ):
                state = replace(
                    state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None
                )
                _save_state(connection, observation.provider, state)
            if latest_prior is None or observation.observed_at > latest_prior.observed_at:
                metrics = weekly_metrics(readings, current)
                uncertain = state.history_uncertain_until if state else None
                fresh_recovery = bool(
                    metrics.available
                    and metrics.age is not None
                    and metrics.age <= REFRESH_AGE
                    and metrics.latest is not None
                    and metrics.latest == observation
                )
                new_window = bool(
                    fresh_recovery
                    and uncertain is not None
                    and state is not None
                    and state.window_reset_at is not None
                    and observation.weekly_reset_at - state.window_reset_at
                    > timedelta(seconds=_RESET_JITTER_SECONDS)
                )
                history_recovered = bool(
                    fresh_recovery
                    and uncertain is not None
                    and observation.observed_at >= uncertain
                    and metrics.credible_pace
                )
                if uncertain is not None and (new_window or history_recovered):
                    state = replace(
                        state,
                        history_uncertain_until=None,
                        candidate_mode=None,
                        candidate_first_at=None,
                        candidate_last_at=None,
                    )
                    _save_state(connection, observation.provider, state)
                elif uncertain is not None:
                    state = replace(
                        state,
                        candidate_mode=None,
                        candidate_first_at=None,
                        candidate_last_at=None,
                    )
                    _save_state(connection, observation.provider, state)
                    result = evaluate_policy(observation.provider, readings, state, now=current)
                    connection.execute("COMMIT")
                    return result
                recovered = bool(
                    latest_prior and current - latest_prior.observed_at > REFRESH_AGE
                )
                next_state = advance_state(
                    state,
                    metrics,
                    now=current,
                    observation_at=observation.observed_at,
                    recovered=recovered,
                )
                _save_state(connection, observation.provider, next_state)
                state = next_state
        elif state is not None:
            state = replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)
            _save_state(connection, observation.provider, state)
        result = evaluate_policy(observation.provider, readings, state, now=current)
        connection.execute("COMMIT")
        return result
    except Exception:
        if connection.in_transaction:
            connection.execute("ROLLBACK")
        raise
    finally:
        connection.close()


def load_policy(
    database: os.PathLike[str] | str | None,
    provider: str,
    *,
    now: Optional[datetime] = None,
) -> Policy:
    """Read local state; storage failures produce a conservative refresh policy."""
    if provider not in PROVIDERS:
        raise ValueError("unsupported provider")
    current = parse_timestamp(now or datetime.now().astimezone())
    connection: Optional[sqlite3.Connection] = None
    try:
        connection = _connect(database, timeout=0.2)
        connection.execute("BEGIN")
        observations = _observations(connection, provider, current)
        state = _state(connection, provider)
        count = connection.execute(
            "SELECT COUNT(*) FROM observations WHERE provider = ?", (provider,)
        ).fetchone()[0]
        if count > _MAX_OBSERVATIONS and (state is None or state.history_uncertain_until is None):
            latest = connection.execute(
                "SELECT MAX(observed_at) FROM observations WHERE provider = ?", (provider,)
            ).fetchone()[0]
            connection.execute("ROLLBACK")
            connection.execute("BEGIN IMMEDIATE")
            observations = _observations(connection, provider, current)
            state = _state(connection, provider)
            count = connection.execute(
                "SELECT COUNT(*) FROM observations WHERE provider = ?", (provider,)
            ).fetchone()[0]
            if count > _MAX_OBSERVATIONS and (state is None or state.history_uncertain_until is None):
                latest = connection.execute(
                    "SELECT MAX(observed_at) FROM observations WHERE provider = ?", (provider,)
                ).fetchone()[0] or latest
                if latest is not None:
                    _mark_history_uncertain(connection, provider, parse_timestamp(latest))
                state = _state(connection, provider)
        result = evaluate_policy(provider, observations, state, now=current)
        connection.execute("COMMIT")
        return result
    except (OSError, sqlite3.Error, ValueError, RuntimeError):
        if connection is not None and connection.in_transaction:
            connection.execute("ROLLBACK")
        return evaluate_policy(provider, [], None, now=current, store_issue="state_unavailable")
    finally:
        if connection is not None:
            connection.close()


def load_history(
    database: os.PathLike[str] | str | None,
    provider: str,
    *,
    limit: int = 20,
    now: Optional[datetime] = None,
) -> list[Observation]:
    if provider not in PROVIDERS:
        raise ValueError("unsupported provider")
    if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= 1000:
        raise ValueError("history limit must be between 1 and 1000")
    current = parse_timestamp(now or datetime.now(timezone.utc))
    connection = _connect(database, timeout=0.2)
    try:
        connection.execute("BEGIN")
        rows = connection.execute(
            "SELECT * FROM observations WHERE provider = ? ORDER BY observed_at DESC, id DESC LIMIT ?",
            (provider, min(limit, _MAX_OBSERVATIONS)),
        ).fetchall()
        values = [_observation_from_row(row, current) for row in rows]
        connection.execute("COMMIT")
        return values
    except Exception:
        if connection.in_transaction:
            connection.execute("ROLLBACK")
        raise
    finally:
        connection.close()
