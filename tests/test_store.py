from dataclasses import replace
from datetime import timedelta
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest

from demon_mode.governor import CONFLICT_SCHEMAS, MODE_RANK, evaluate_policy, normalize_observation, parse_timestamp, weekly_metrics
from demon_mode.store import load_history, load_policy, record_manual


from support import NOW, database_connection, observation, invoke


class StoreTests(unittest.TestCase):
    def test_status_reads_do_not_promote_or_mutate_candidate_state(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            record_manual(
                db, provider="claude", observed_at=NOW, weekly_used=0.2,
                weekly_reset_at=NOW + timedelta(hours=20), short_used=2.0,
                short_reset_at=NOW + timedelta(hours=4), now=NOW,
            )
            with database_connection(db) as connection:
                connection.execute(
                    "UPDATE governor_state SET base_mode='Normal', last_change_at=?, candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
                    ((NOW - timedelta(hours=2)).isoformat(), NOW.isoformat(), (NOW + timedelta(hours=1)).isoformat()),
                )
            with database_connection(db) as connection:
                before = connection.execute(
                    "SELECT base_mode, candidate_mode, candidate_first_at, candidate_last_at FROM governor_state"
                ).fetchone()
            code, _, _ = invoke(["--db", str(db), "status", "--provider", "claude", "--json"], now=NOW + timedelta(hours=3))
            self.assertEqual(code, 0)
            with database_connection(db) as connection:
                after = connection.execute(
                    "SELECT base_mode, candidate_mode, candidate_first_at, candidate_last_at FROM governor_state"
                ).fetchone()
            self.assertEqual(before, after)


    def test_decrease_conflict_preserves_peak_and_clears_candidate(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            record_manual(
                db, provider="claude", observed_at=NOW, weekly_used=20.0,
                weekly_reset_at=NOW + timedelta(hours=100), short_used=1.0,
                short_reset_at=NOW + timedelta(hours=4), now=NOW,
            )
            with database_connection(db) as connection:
                connection.execute(
                    "UPDATE governor_state SET candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
                    (NOW.isoformat(), NOW.isoformat()),
                )
            later = NOW + timedelta(hours=1)
            policy = record_manual(
                db, provider="claude", observed_at=later, weekly_used=19.0,
                weekly_reset_at=NOW + timedelta(hours=100), short_used=1.0,
                short_reset_at=NOW + timedelta(hours=4), now=later,
            )
            self.assertEqual(policy.weekly_used, 20.0)
            self.assertEqual(policy.reason, "reading_conflict_refresh_required")
            with database_connection(db) as connection:
                row = connection.execute(
                    "SELECT candidate_mode, candidate_first_at, candidate_last_at FROM governor_state WHERE provider='claude'"
                ).fetchone()
                markers = [r[0] for r in connection.execute("SELECT schema_marker FROM observations")]
            self.assertEqual(row, (None, None, None))
            self.assertIn("conflict-decreasing-v1", markers)


    def test_repeated_decreases_compare_with_independent_window_peaks(self):
        cases = (
            ("weekly", (20.0, 19.0, 19.5, 20.1), (10.0, 10.0, 10.0, 10.0)),
            ("short", (20.0, 20.1, 20.2, 20.3), (20.0, 19.0, 19.5, 20.1)),
        )
        for provider_case, weekly, short in cases:
            with self.subTest(bucket=provider_case), tempfile.TemporaryDirectory() as temp:
                db = Path(temp).resolve() / "state.sqlite3"
                for index, (week_used, short_used) in enumerate(zip(weekly, short)):
                    at = NOW + timedelta(hours=index)
                    if index == 1:
                        with database_connection(db) as connection:
                            connection.execute(
                                "UPDATE governor_state SET candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
                                (NOW.isoformat(), NOW.isoformat()),
                            )
                    record_manual(
                        db, provider="claude", observed_at=at, weekly_used=week_used,
                        weekly_reset_at=NOW + timedelta(hours=100), short_used=short_used,
                        short_reset_at=NOW + timedelta(hours=4), now=at,
                    )
                    if index in (1, 2):
                        history = load_history(db, "claude", now=at)
                        self.assertIn("conflict-decreasing-v1", {item.schema_marker for item in history})
                        with database_connection(db) as connection:
                            candidate = connection.execute(
                                "SELECT candidate_mode FROM governor_state WHERE provider='claude'"
                            ).fetchone()[0]
                        self.assertIsNone(candidate)
                history = load_history(db, "claude", now=NOW + timedelta(hours=3))
                self.assertNotIn(history[0].schema_marker, CONFLICT_SCHEMAS)
                self.assertEqual(history[0].weekly_used, weekly[-1])
                self.assertEqual(history[0].short_used, short[-1])


    def test_equal_timestamp_disagreement_is_a_conflict(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            record_manual(
                db, provider="claude", observed_at=NOW, weekly_used=20.0,
                weekly_reset_at=NOW + timedelta(hours=100), short_used=1.0,
                short_reset_at=NOW + timedelta(hours=4), now=NOW,
            )
            policy = record_manual(
                db, provider="claude", observed_at=NOW, weekly_used=21.0,
                weekly_reset_at=NOW + timedelta(hours=100), short_used=1.0,
                short_reset_at=NOW + timedelta(hours=4), now=NOW,
            )
            self.assertEqual(policy.reason, "reading_conflict_refresh_required")
            history = load_history(db, "claude", now=NOW)
            self.assertEqual(len(history), 2)
            self.assertIn(history[0].schema_marker, CONFLICT_SCHEMAS)


    def test_out_of_order_input_does_not_replace_newest_or_advance_state(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            reset = NOW + timedelta(hours=100)
            record_manual(db, provider="codex", observed_at=NOW, weekly_used=20, weekly_reset_at=reset, now=NOW)
            newest = NOW + timedelta(hours=2)
            record_manual(db, provider="codex", observed_at=newest, weekly_used=30, weekly_reset_at=reset, now=newest)
            with database_connection(db) as connection:
                before = connection.execute("SELECT base_mode, candidate_mode, candidate_last_at FROM governor_state").fetchone()
            record_manual(db, provider="codex", observed_at=NOW + timedelta(hours=1), weekly_used=25, weekly_reset_at=reset, now=newest)
            policy = load_policy(db, "codex", now=newest)
            with database_connection(db) as connection:
                after = connection.execute("SELECT base_mode, candidate_mode, candidate_last_at FROM governor_state").fetchone()
            self.assertEqual(policy.weekly_used, 30)
            self.assertEqual(before, after)
            self.assertEqual(len(load_history(db, "codex", now=newest)), 3)


    def test_provider_histories_and_policy_state_are_separate(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            for provider, amount in (("claude", 12.0), ("codex", 78.0)):
                record_manual(db, provider=provider, observed_at=NOW, weekly_used=amount,
                              weekly_reset_at=NOW + timedelta(hours=100), now=NOW)
            self.assertEqual(load_policy(db, "claude", now=NOW).weekly_used, 12.0)
            self.assertEqual(load_policy(db, "codex", now=NOW).weekly_used, 78.0)
            self.assertEqual(len(load_history(db, "claude", now=NOW)), 1)
            self.assertEqual(len(load_history(db, "codex", now=NOW)), 1)


    def test_observation_history_is_bounded_and_keeps_window_peak(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            reset = NOW + timedelta(hours=100)
            for index in range(70):
                at = NOW - timedelta(minutes=5 * (69 - index))
                record_manual(
                    db, provider="claude", observed_at=at,
                    weekly_used=20.0 if index == 0 else 19.0,
                    weekly_reset_at=reset, now=at,
                )
            with database_connection(db) as connection:
                count = connection.execute(
                    "SELECT COUNT(*) FROM observations WHERE provider='claude'"
                ).fetchone()[0]
            self.assertLessEqual(count, 64)
            history = load_history(db, "claude", limit=1000, now=NOW)
            self.assertLessEqual(len(history), 64)
            policy = load_policy(db, "claude", now=NOW)
            self.assertEqual(policy.weekly_used, 20.0)
            self.assertIn(history[0].schema_marker, CONFLICT_SCHEMAS)
            self.assertEqual(policy.mode, "Lean")
            self.assertTrue(policy.refresh_required)
            with database_connection(db) as connection:
                connection.execute(
                    """INSERT INTO observations
                       (provider, observed_at, weekly_used, weekly_reset_at, short_used,
                        short_reset_at, source, schema_marker)
                       SELECT provider, observed_at, weekly_used, weekly_reset_at, short_used,
                              short_reset_at, source, schema_marker
                       FROM observations WHERE provider='claude' LIMIT 1"""
                )
            incomplete = load_policy(db, "claude", now=NOW)
            self.assertEqual(incomplete.mode, "Lean")
            self.assertEqual(incomplete.reason, "state_history_incomplete")
            self.assertTrue(incomplete.refresh_required)

            pace_db = Path(temp).resolve() / "pace.sqlite3"
            for index in range(70):
                at = NOW - timedelta(minutes=10 * (69 - index))
                record_manual(
                    pace_db, provider="codex", observed_at=at,
                    weekly_used=20.0 + index * 0.1,
                    weekly_reset_at=reset, now=at,
                )
            pace = load_policy(pace_db, "codex", now=NOW).pace_per_hour
            expected_average = 26.9 / 68.0
            expected_recent = 6.9 / 11.5
            self.assertAlmostEqual(pace, (expected_average + expected_recent) / 2.0)


    def test_dense_rollover_preserves_bounds_and_recovers_uncertain_history(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "dense.sqlite3"
            start = NOW
            week_reset = start + timedelta(hours=160)
            raw = []

            def check_invariants(at):
                with database_connection(db) as connection:
                    count = connection.execute(
                        "SELECT COUNT(*) FROM observations WHERE provider='claude'"
                    ).fetchone()[0]
                self.assertLessEqual(count, 64)
                stored = load_history(db, "claude", limit=1000, now=at)
                self.assertLessEqual(len(stored), 64)
                accepted_raw = [item for item in raw if item.schema_marker not in CONFLICT_SCHEMAS]
                accepted_stored = [item for item in stored if item.schema_marker not in CONFLICT_SCHEMAS]
                if accepted_raw:
                    latest_raw = max(accepted_raw, key=lambda item: item.observed_at)
                    raw_week = [
                        item for item in accepted_raw
                        if abs((item.weekly_reset_at - latest_raw.weekly_reset_at).total_seconds()) <= 2
                    ]
                    stored_week = [
                        item for item in accepted_stored
                        if abs((item.weekly_reset_at - latest_raw.weekly_reset_at).total_seconds()) <= 2
                    ]
                    self.assertIn(latest_raw, accepted_stored)
                    self.assertEqual(
                        max(item.weekly_used for item in raw_week),
                        max(item.weekly_used for item in stored_week),
                    )
                    self.assertEqual(
                        min(item.weekly_reset_at for item in raw_week),
                        min(item.weekly_reset_at for item in stored_week),
                    )
                    raw_short = [item for item in raw_week if item.short_reset_at is not None]
                    stored_short = [item for item in accepted_stored if item.short_reset_at is not None]
                    if raw_short:
                        latest_short = max(raw_short, key=lambda item: item.observed_at)
                        raw_short_window = [
                            item for item in raw_short
                            if abs((item.short_reset_at - latest_short.short_reset_at).total_seconds()) <= 2
                        ]
                        stored_short_window = [
                            item for item in stored_short
                            if abs((item.short_reset_at - latest_short.short_reset_at).total_seconds()) <= 2
                        ]
                        self.assertEqual(
                            max(item.short_used for item in raw_short_window),
                            max(item.short_used for item in stored_short_window),
                        )
                        self.assertEqual(
                            min(item.short_reset_at for item in raw_short_window),
                            min(item.short_reset_at for item in stored_short_window),
                        )
                    raw_conflicts = [item for item in raw if item.schema_marker in CONFLICT_SCHEMAS]
                    stored_conflicts = [item for item in stored if item.schema_marker in CONFLICT_SCHEMAS]
                    if raw_conflicts:
                        self.assertEqual(
                            max(item.observed_at for item in raw_conflicts),
                            max(item.observed_at for item in stored_conflicts),
                        )
                    reference = weekly_metrics(raw, at)
                    bounded = weekly_metrics(stored, at)
                    if reference.credible_pace:
                        self.assertTrue(
                            bounded.credible_pace
                            or bounded.recent_pace_upper_bound_per_hour is not None
                            or load_policy(db, "claude", now=at).reason == "state_history_incomplete"
                        )

            def add(minute, now=None):
                at = start + timedelta(minutes=minute)
                current = now or at
                if minute == 0:
                    reset = week_reset + timedelta(seconds=2)
                elif minute == 120:
                    reset = week_reset
                else:
                    reset = week_reset + timedelta(seconds=1)
                short_reset = start + timedelta(minutes=(minute // 300 + 1) * 300)
                weekly_used = 3.2 + minute * 0.0066666666667
                short_used = 30.0 + (minute % 300) * 0.02
                item = observation(
                    at=at, used=weekly_used, reset=reset, short=short_used,
                    short_reset=short_reset, now=current,
                )
                record_manual(
                    db, provider="claude", observed_at=at, weekly_used=weekly_used,
                    weekly_reset_at=reset, short_used=short_used, short_reset_at=short_reset, now=current,
                )
                raw.append(item)
                check_invariants(current)
                if minute == 240:
                    conflict = observation(
                        at=at, used=weekly_used + 0.2, reset=reset,
                        short=short_used + 0.2, short_reset=short_reset, now=at,
                    )
                    record_manual(
                        db, provider="claude", observed_at=at, weekly_used=weekly_used + 0.2,
                        weekly_reset_at=reset, short_used=short_used + 0.2,
                        short_reset_at=short_reset, now=at,
                    )
                    raw.append(replace(conflict, schema_marker="conflict-equal-time-v1"))
                    check_invariants(at)

            timeline = [0]
            timeline.extend(range(60, 120))
            timeline.extend(range(120, 24 * 60 + 1, 60))
            timeline.extend(range(24 * 60 + 20, 25 * 60 + 31))
            for minute in timeline:
                add(minute)

            rollover = start + timedelta(hours=25)
            with database_connection(db) as connection:
                expired_old = connection.execute(
                    "SELECT 1 FROM observations WHERE provider='claude' AND observed_at=?",
                    (start.isoformat().replace("+00:00", "Z"),),
                ).fetchone()
            self.assertIsNone(expired_old)
            latest = start + timedelta(hours=25, minutes=30)
            bounded_policy = load_policy(db, "claude", now=latest)
            reference_policy = evaluate_policy("claude", raw, None, now=latest)
            self.assertIsNotNone(bounded_policy.recent_pace_upper_bound_per_hour)
            self.assertGreaterEqual(bounded_policy.pace_per_hour, reference_policy.pace_per_hour)
            self.assertLessEqual(bounded_policy.ratio, reference_policy.ratio)
            self.assertLessEqual(MODE_RANK[bounded_policy.mode], MODE_RANK[reference_policy.mode])
            self.assertEqual(
                bounded_policy.as_dict(latest)["weekly_recent_pace_upper_bound_percent_per_hour"],
                bounded_policy.recent_pace_upper_bound_per_hour,
            )
            with database_connection(db) as connection:
                state = connection.execute(
                    "SELECT history_uncertain_until FROM governor_state WHERE provider='claude'"
                ).fetchone()
            self.assertIsNone(state[0])

            for minute in range(25 * 60 + 31, 26 * 60 + 2):
                add(minute)
            latest = start + timedelta(hours=26, minutes=1)
            fresh = load_policy(db, "claude", now=latest)
            self.assertNotEqual(fresh.reason, "state_history_incomplete")
            self.assertGreater(MODE_RANK[fresh.mode], MODE_RANK["Lean"])

            uncertain_until = latest + timedelta(hours=24)
            until_stamp = uncertain_until.isoformat().replace("+00:00", "Z")
            with database_connection(db) as connection:
                connection.execute(
                    "UPDATE governor_state SET history_uncertain_until=?, candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
                    (until_stamp, latest.isoformat(), latest.isoformat()),
                )
            held = load_policy(db, "claude", now=latest)
            self.assertEqual(held.reason, "state_history_incomplete")
            self.assertLessEqual(MODE_RANK[held.mode], MODE_RANK["Lean"])
            for hour in range(1, 24):
                add(int((latest - start).total_seconds() // 60) + hour * 60)
                with database_connection(db) as connection:
                    state = connection.execute(
                        "SELECT history_uncertain_until, candidate_mode FROM governor_state WHERE provider='claude'"
                    ).fetchone()
                self.assertEqual(state[0], until_stamp)
                self.assertIsNone(state[1])
            held_after_deadline = load_policy(db, "claude", now=uncertain_until + timedelta(minutes=1))
            self.assertEqual(held_after_deadline.reason, "state_history_incomplete")
            stale_at = uncertain_until + timedelta(minutes=1)
            stale_now = stale_at + timedelta(hours=2)
            add(int((stale_at - start).total_seconds() // 60), now=stale_now)
            with database_connection(db) as connection:
                state = connection.execute(
                    "SELECT history_uncertain_until, candidate_mode, candidate_first_at, candidate_last_at FROM governor_state WHERE provider='claude'"
                ).fetchone()
            stale_latest = load_history(db, "claude", limit=1, now=stale_now)[0]
            stale_policy = load_policy(db, "claude", now=stale_now)
            self.assertEqual(
                (
                    stale_latest.observed_at,
                    stale_now - stale_latest.observed_at,
                    *state,
                    stale_policy.reason,
                    MODE_RANK[stale_policy.mode] <= MODE_RANK["Lean"],
                ),
                (
                    stale_at,
                    timedelta(hours=2),
                    until_stamp,
                    None,
                    None,
                    None,
                    "state_history_incomplete",
                    True,
                ),
            )
            recovered_at = stale_now + timedelta(minutes=1)
            add(int((recovered_at - start).total_seconds() // 60), now=recovered_at)
            with database_connection(db) as connection:
                state = connection.execute(
                    "SELECT history_uncertain_until FROM governor_state WHERE provider='claude'"
                ).fetchone()
            self.assertIsNone(state[0])
            recovered_policy = load_policy(db, "claude", now=recovered_at)
            self.assertNotEqual(recovered_policy.reason, "state_history_incomplete")
            self.assertTrue(weekly_metrics(load_history(db, "claude", limit=1000, now=recovered_at), recovered_at).credible_pace)

            pristine_db = Path(temp).resolve() / "pristine.sqlite3"
            pristine_at = NOW
            pristine_reset = pristine_at + timedelta(hours=120)
            pristine = observation(
                at=pristine_at, used=20.0, reset=pristine_reset, now=pristine_at,
            )
            record_manual(
                pristine_db, provider="codex", observed_at=pristine_at, weekly_used=20.0,
                weekly_reset_at=pristine_reset, now=pristine_at,
            )
            pristine_metrics = weekly_metrics([pristine], pristine_at)
            self.assertFalse(pristine_metrics.credible_pace)
            self.assertIsNone(pristine_metrics.recent_pace_upper_bound_per_hour)
            self.assertAlmostEqual(pristine_metrics.pace_per_hour, 20.0 / 48.0)
            with database_connection(pristine_db) as connection:
                pristine_state = connection.execute(
                    "SELECT history_uncertain_until FROM governor_state WHERE provider='codex'"
                ).fetchone()
            self.assertIsNone(pristine_state[0])

            legacy_db = Path(temp).resolve() / "legacy-v1.sqlite3"
            legacy_start = NOW - timedelta(minutes=63)
            legacy_reset = NOW + timedelta(hours=166)
            connection = sqlite3.connect(legacy_db)
            connection.executescript(
                """CREATE TABLE observations (
                       id INTEGER PRIMARY KEY, provider TEXT NOT NULL, observed_at TEXT NOT NULL,
                       weekly_used REAL NOT NULL, weekly_reset_at TEXT NOT NULL, short_used REAL,
                       short_reset_at TEXT, source TEXT NOT NULL, schema_marker TEXT NOT NULL
                   );
                   CREATE INDEX observations_by_provider_time ON observations(provider, observed_at);
                   CREATE TABLE governor_state (
                       provider TEXT PRIMARY KEY, window_reset_at TEXT, base_mode TEXT,
                       last_change_at TEXT, candidate_mode TEXT, candidate_first_at TEXT,
                       candidate_last_at TEXT
                   );
                   PRAGMA user_version = 1;"""
            )
            connection.executemany(
                """INSERT INTO observations
                   (provider, observed_at, weekly_used, weekly_reset_at, short_used,
                    short_reset_at, source, schema_marker)
                   VALUES ('codex', ?, ?, ?, NULL, NULL, 'manual', 'manual-v1')""",
                [
                    (
                        (legacy_start + timedelta(minutes=index)).isoformat().replace("+00:00", "Z"),
                        10.0 + index / 100.0,
                        legacy_reset.isoformat().replace("+00:00", "Z"),
                    )
                    for index in range(64)
                ],
            )
            connection.commit()
            connection.close()
            legacy_policy = load_policy(legacy_db, "codex", now=NOW)
            self.assertEqual(legacy_policy.reason, "state_history_incomplete")
            with database_connection(legacy_db) as connection:
                version = connection.execute("PRAGMA user_version").fetchone()[0]
                legacy_state = connection.execute(
                    "SELECT history_uncertain_until FROM governor_state WHERE provider='codex'"
                ).fetchone()[0]
            self.assertEqual(version, 2)
            self.assertEqual(parse_timestamp(legacy_state), NOW + timedelta(hours=24))


    def test_corrupt_and_locked_state_fall_back_without_erasure(self):
        with tempfile.TemporaryDirectory() as temp:
            corrupt = Path(temp).resolve() / "corrupt.sqlite3"
            corrupt.write_bytes(b"not a sqlite database")
            policy = load_policy(corrupt, "claude", now=NOW)
            self.assertEqual(policy.mode, "Lean")
            self.assertEqual(policy.reason, "state_unavailable")
            self.assertEqual(corrupt.read_bytes(), b"not a sqlite database")

            locked = Path(temp).resolve() / "locked.sqlite3"
            load_policy(locked, "claude", now=NOW)
            connection = sqlite3.connect(locked, timeout=0.1)
            try:
                connection.execute("BEGIN EXCLUSIVE")
                fallback = load_policy(locked, "claude", now=NOW)
                self.assertEqual(fallback.mode, "Lean")
                self.assertEqual(fallback.reason, "state_unavailable")
            finally:
                connection.rollback()
                connection.close()


    def test_linked_state_paths_are_rejected_and_newer_schema_is_unchanged(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            newer = root / "newer.sqlite3"
            connection = sqlite3.connect(newer)
            connection.execute("PRAGMA user_version = 99")
            connection.execute("CREATE TABLE marker (value TEXT)")
            connection.execute("INSERT INTO marker VALUES ('keep')")
            connection.commit()
            connection.close()
            before = newer.read_bytes()
            fallback = load_policy(newer, "claude", now=NOW)
            self.assertEqual((fallback.mode, fallback.reason), ("Lean", "state_unavailable"))
            self.assertEqual(newer.read_bytes(), before)

            nonregular = root / "directory.sqlite3"
            nonregular.mkdir()
            directory_fallback = load_policy(nonregular, "claude", now=NOW)
            self.assertEqual((directory_fallback.mode, directory_fallback.reason), ("Lean", "state_unavailable"))
            parent_file = root / "not-a-directory"
            parent_file.write_bytes(b"preserve")
            parent_fallback = load_policy(parent_file / "state.sqlite3", "claude", now=NOW)
            self.assertEqual((parent_fallback.mode, parent_fallback.reason), ("Lean", "state_unavailable"))
            self.assertEqual(parent_file.read_bytes(), b"preserve")

            target = root / "target.sqlite3"
            target.write_bytes(before)
            linked_file = root / "linked.sqlite3"
            try:
                os.symlink(target, linked_file)
            except (OSError, NotImplementedError) as exc:
                self.skipTest(f"native symlink creation unavailable: {type(exc).__name__}")
            linked_fallback = load_policy(linked_file, "claude", now=NOW)
            self.assertEqual((linked_fallback.mode, linked_fallback.reason), ("Lean", "state_unavailable"))
            self.assertEqual(target.read_bytes(), before)

            real_parent = root / "real-parent"
            real_parent.mkdir()
            linked_parent = root / "linked-parent"
            try:
                os.symlink(real_parent, linked_parent, target_is_directory=True)
            except (OSError, NotImplementedError) as exc:
                self.skipTest(f"native directory symlink creation unavailable: {type(exc).__name__}")
            parent_fallback = load_policy(linked_parent / "child.sqlite3", "claude", now=NOW)
            self.assertEqual((parent_fallback.mode, parent_fallback.reason), ("Lean", "state_unavailable"))
            self.assertFalse((real_parent / "child.sqlite3").exists())


    def test_invalid_manual_schema_values_are_rejected(self):
        bad = (
            {"weekly_used": True},
            {"weekly_used": float("nan")},
            {"weekly_used": float("inf")},
            {"observed_at": "2026-10-23T12:00:00"},
            {"weekly_reset_at": NOW - timedelta(seconds=1)},
        )
        for change in bad:
            with self.subTest(change=change):
                values = {
                    "provider": "claude", "observed_at": NOW, "weekly_used": 10.0,
                    "weekly_reset_at": NOW + timedelta(hours=100), "now": NOW,
                }
                values.update(change)
                with self.assertRaises(ValueError):
                    normalize_observation(**values)



if __name__ == "__main__":
    unittest.main()
