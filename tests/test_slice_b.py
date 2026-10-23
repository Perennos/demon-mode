import io
from contextlib import contextmanager
from dataclasses import replace
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import demon_mode.__main__ as cli
from demon_mode.governor import (
    CONFLICT_SCHEMAS,
    MODE_RANK,
    MODE_POLICY,
    SOURCE_SCHEMAS,
    GovernorState,
    WeeklyMetrics,
    advance_state,
    evaluate_policy,
    normalize_observation,
    parse_timestamp,
    weekly_metrics,
)
from demon_mode.store import load_history, load_policy, record_manual


UTC = timezone.utc
NOW = datetime(2026, 10, 23, 12, 0, tzinfo=UTC)


class TTY(io.StringIO):
    def isatty(self):
        return True


class PromptInput(io.StringIO):
    def __init__(self, value):
        super().__init__(value)
        self.read_calls = 0

    def read(self, *args, **kwargs):
        self.read_calls += 1
        return super().read(*args, **kwargs)


@contextmanager
def database_connection(path):
    connection = sqlite3.connect(path)
    try:
        yield connection
        connection.commit()
    except Exception:
        connection.rollback()
        raise
    finally:
        connection.close()


def observation(
    *, provider="claude", at=NOW, used=10.0, reset=None, short=None, short_reset=None, now=None
):
    reset = reset or at + timedelta(hours=100)
    return normalize_observation(
        provider=provider,
        observed_at=at,
        weekly_used=used,
        weekly_reset_at=reset,
        short_used=short,
        short_reset_at=short_reset,
        source="manual",
        now=now or max(NOW, at),
    )


def invoke(argv, *, stdin=None, stdout=None, stderr=None, now=NOW):
    output = stdout or io.StringIO()
    errors = stderr or io.StringIO()
    with patch("sys.stdin", stdin or io.StringIO("")), patch("sys.stdout", output), patch(
        "sys.stderr", errors
    ), patch.object(cli, "_now", return_value=now):
        code = cli.main(argv)
    return code, output.getvalue(), errors.getvalue()


class SliceBTests(unittest.TestCase):
    def test_five_mode_instructions_are_distinct(self):
        self.assertEqual(len(MODE_POLICY), 5)
        self.assertEqual(len(set(MODE_POLICY.values())), 5)

    def test_manual_flags_status_json_and_numeric_history(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            code, out, _ = invoke(
                [
                    "--db", str(db), "record", "--provider", "claude", "--weekly", "0.2",
                    "--resets", (NOW + timedelta(hours=20)).isoformat(), "--five-hour", "5",
                    "--five-hour-resets", (NOW + timedelta(hours=4)).isoformat(), "--at", NOW.isoformat(), "--json",
                ]
            )
            self.assertEqual(code, 0)
            self.assertEqual(json.loads(out)["mode"], "DEMON")
            code, out, _ = invoke(["--db", str(db), "status", "--provider", "claude", "--json"])
            self.assertEqual(code, 0)
            self.assertEqual(json.loads(out)["mode"], "DEMON")
            code, out, _ = invoke(["--db", str(db), "history", "--provider", "claude", "--json"])
            self.assertEqual(code, 0)
            rows = json.loads(out)
            self.assertEqual(len(rows), 1)
            self.assertEqual(set(rows[0]), {
                "observed_at", "weekly_used_percent", "weekly_reset_at", "short_used_percent", "short_reset_at"
            })
            self.assertNotIn(str(db), out)
            code, out, _ = invoke(["--db", str(db), "status", "--provider", "claude"])
            self.assertEqual(code, 0)
            self.assertTrue(out.startswith("DEMON ["))
            self.assertNotIn("\x1b[", out)

    def test_threshold_bands_first_twelve_hours_and_last_twelve_hour_target(self):
        cases = ((55.0, "Austere"), (42.0, "Lean"), (36.0, "Normal"), (30.0, "Rich"))
        for used, expected in cases:
            with self.subTest(used=used):
                metrics = weekly_metrics([observation(used=used)], NOW)
                self.assertEqual(metrics.raw_mode, expected)
        early_at = NOW
        early_reset = early_at + timedelta(hours=164)
        early = weekly_metrics(
            [observation(at=early_at, used=1.0, reset=early_reset)], early_at
        )
        self.assertEqual(early.raw_mode, "Normal")
        self.assertFalse(early.credible_pace)
        last_12 = weekly_metrics(
            [observation(used=1.0, reset=NOW + timedelta(hours=12))], NOW
        )
        outside_12 = weekly_metrics(
            [observation(used=1.0, reset=NOW + timedelta(hours=13))], NOW
        )
        self.assertEqual(last_12.target, 97.0)
        self.assertEqual(outside_12.target, 92.0)

    def test_blended_pace_and_delayed_read_estimate(self):
        deadline = NOW + timedelta(hours=68)
        first = observation(at=NOW - timedelta(hours=6), used=14.0, reset=deadline)
        latest = observation(at=NOW, used=20.0, reset=deadline)
        metrics = weekly_metrics([first, latest], NOW)
        self.assertAlmostEqual(metrics.pace_per_hour, 0.6)
        delayed = observation(
            at=NOW - timedelta(hours=2), used=20.0,
            reset=NOW + timedelta(hours=98), short=5.0,
            short_reset=NOW + timedelta(hours=2), now=NOW,
        )
        policy = evaluate_policy("claude", [delayed], None, now=NOW)
        self.assertAlmostEqual(policy.estimated_weekly_used, 20.0 + (20.0 / 68.0) * 2.0)
        self.assertTrue(policy.refresh_required)

    def test_short_hard_brake_is_final_even_when_weekly_is_expired(self):
        stale_week = observation(
            at=NOW - timedelta(hours=2), used=90.0,
            reset=NOW - timedelta(hours=1), short=95.0,
            short_reset=NOW + timedelta(hours=2), now=NOW,
        )
        policy = evaluate_policy("codex", [stale_week], None, now=NOW)
        self.assertEqual(policy.mode, "Austere")
        self.assertEqual(policy.reason, "short_window_hard_brake")

    def test_short_projection_at_84_percent_brakes_one_level(self):
        item = observation(
            used=0.2, reset=NOW + timedelta(hours=20), short=84.0,
            short_reset=NOW + timedelta(hours=4),
        )
        policy = evaluate_policy("claude", [item], None, now=NOW)
        self.assertEqual(policy.short_projected_used, 100.0)
        self.assertEqual(policy.mode, "Rich")
        self.assertEqual(policy.reason, "short_window_pressure_brake")

    def test_measured_short_usage_at_85_brakes_without_projection_ceiling(self):
        item = observation(
            used=0.2, reset=NOW + timedelta(hours=20), short=85.0,
            short_reset=NOW + timedelta(minutes=12),
        )
        policy = evaluate_policy("claude", [item], None, now=NOW)
        self.assertLess(policy.short_projected_used, 100.0)
        self.assertEqual(policy.mode, "Rich")
        self.assertEqual(policy.reason, "short_window_pressure_brake")

    def test_stale_but_active_85_percent_short_reading_deducts_from_lean(self):
        item = observation(
            at=NOW - timedelta(hours=2), used=42.0, reset=NOW + timedelta(hours=100),
            short=85.0, short_reset=NOW + timedelta(hours=3), now=NOW,
        )
        self.assertEqual(weekly_metrics([item], NOW).raw_mode, "Lean")
        policy = evaluate_policy("claude", [item], None, now=NOW)
        self.assertEqual(policy.mode, "Austere")
        self.assertEqual(policy.reason, "short_window_pressure_brake")
        self.assertTrue(policy.refresh_required)
        self.assertEqual(policy.freshness, "short_stale_ask_refresh")

    def test_missing_and_expired_windows_are_conservative(self):
        missing = evaluate_policy("claude", [], None, now=NOW)
        self.assertEqual(missing.mode, "Lean")
        expired = observation(
            at=NOW - timedelta(hours=6), used=40.0,
            reset=NOW - timedelta(hours=1), short=40.0,
            short_reset=NOW - timedelta(hours=2), now=NOW,
        )
        policy = evaluate_policy("claude", [expired], None, now=NOW)
        self.assertEqual(policy.mode, "Lean")
        self.assertTrue(policy.refresh_required)

    def test_upward_hysteresis_needs_two_distinct_observations_and_cooldown(self):
        deadline = NOW + timedelta(hours=100)
        state = GovernorState(window_reset_at=deadline, base_mode="Normal", last_change_at=NOW - timedelta(hours=2))
        metrics = WeeklyMetrics(True, "weekly_headroom_high", deadline=deadline, ratio=1.5, raw_mode="Rich")
        first = advance_state(state, metrics, now=NOW, observation_at=NOW)
        self.assertEqual(first.base_mode, "Normal")
        self.assertEqual(first.candidate_mode, "Rich")
        replay = advance_state(first, metrics, now=NOW + timedelta(hours=1), observation_at=NOW)
        self.assertEqual(replay.base_mode, "Normal")
        second_at = NOW + timedelta(hours=1)
        promoted = advance_state(first, metrics, now=second_at, observation_at=second_at)
        self.assertEqual(promoted.base_mode, "Rich")

    def test_aged_import_cannot_confirm_upgrade_but_can_downgrade(self):
        deadline = NOW + timedelta(hours=100)
        stale_high = WeeklyMetrics(
            True, "weekly_headroom_high", deadline=deadline, ratio=1.5,
            raw_mode="Rich", age=timedelta(hours=2),
        )
        candidate = GovernorState(
            window_reset_at=deadline, base_mode="Normal",
            last_change_at=NOW - timedelta(hours=4), candidate_mode="Rich",
            candidate_first_at=NOW - timedelta(hours=2),
            candidate_last_at=NOW - timedelta(hours=2),
        )
        held = advance_state(candidate, stale_high, now=NOW, observation_at=NOW - timedelta(hours=2))
        self.assertEqual(held.base_mode, "Normal")
        self.assertIsNone(held.candidate_mode)

        stale_low = WeeklyMetrics(
            True, "weekly_headroom_tight", deadline=deadline, ratio=0.5,
            raw_mode="Lean", age=timedelta(hours=2),
        )
        lowered = advance_state(
            GovernorState(
                window_reset_at=deadline, base_mode="Rich",
                last_change_at=NOW - timedelta(hours=4),
            ),
            stale_low, now=NOW, observation_at=NOW - timedelta(hours=2),
        )
        self.assertEqual(lowered.base_mode, "Lean")

    def test_stale_gap_clears_candidate_before_fresh_recovery(self):
        deadline = NOW + timedelta(hours=100)
        state = GovernorState(
            window_reset_at=deadline, base_mode="Normal",
            last_change_at=NOW - timedelta(hours=4), candidate_mode="Rich",
            candidate_first_at=NOW - timedelta(hours=2),
            candidate_last_at=NOW - timedelta(hours=2),
        )
        unavailable = advance_state(
            state, WeeklyMetrics(False, "weekly_observation_stale"),
            now=NOW, observation_at=NOW,
        )
        self.assertIsNone(unavailable.candidate_mode)
        fresh = WeeklyMetrics(
            True, "weekly_headroom_high", deadline=deadline, ratio=1.5,
            raw_mode="Rich", age=timedelta(0),
        )
        recovered = advance_state(
            state, fresh, now=NOW, observation_at=NOW, recovered=True,
        )
        self.assertEqual(recovered.base_mode, "Normal")
        self.assertEqual(recovered.candidate_mode, "Rich")
        self.assertEqual(recovered.candidate_first_at, NOW)

        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            prior_at = NOW - timedelta(hours=3)
            record_manual(
                db, provider="claude", observed_at=prior_at, weekly_used=20.0,
                weekly_reset_at=deadline, now=prior_at,
            )
            with database_connection(db) as connection:
                connection.execute(
                    "UPDATE governor_state SET base_mode='Normal', last_change_at=?, candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
                    ((NOW - timedelta(hours=5)).isoformat(), (NOW - timedelta(hours=2)).isoformat(), (NOW - timedelta(hours=2)).isoformat()),
                )
            record_manual(
                db, provider="claude", observed_at=NOW, weekly_used=21.0,
                weekly_reset_at=deadline, now=NOW,
            )
            with database_connection(db) as connection:
                row = connection.execute(
                    "SELECT base_mode, candidate_mode, candidate_first_at FROM governor_state WHERE provider='claude'"
                ).fetchone()
            self.assertEqual(row[:2], ("Normal", "Rich"))
            self.assertEqual(parse_timestamp(row[2]), NOW)

    def test_status_reads_do_not_promote_or_mutate_candidate_state(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
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
            db = Path(temp) / "state.sqlite3"
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
                db = Path(temp) / "state.sqlite3"
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
            db = Path(temp) / "state.sqlite3"
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
            db = Path(temp) / "state.sqlite3"
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

    def test_new_reset_window_replaces_transition_candidate(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            old_reset = NOW + timedelta(hours=100)
            record_manual(db, provider="claude", observed_at=NOW, weekly_used=20, weekly_reset_at=old_reset, now=NOW)
            with database_connection(db) as connection:
                connection.execute(
                    "UPDATE governor_state SET candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
                    (NOW.isoformat(), NOW.isoformat()),
                )
            later = NOW + timedelta(hours=1)
            new_reset = later + timedelta(hours=168)
            record_manual(db, provider="claude", observed_at=later, weekly_used=1, weekly_reset_at=new_reset, now=later)
            with database_connection(db) as connection:
                row = connection.execute(
                    "SELECT window_reset_at, candidate_mode, candidate_first_at FROM governor_state WHERE provider='claude'"
                ).fetchone()
            self.assertEqual(parse_timestamp(row[0]), new_reset)
            self.assertEqual(row[1:], (None, None))

    def test_provider_histories_and_policy_state_are_separate(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            for provider, amount in (("claude", 12.0), ("codex", 78.0)):
                record_manual(db, provider=provider, observed_at=NOW, weekly_used=amount,
                              weekly_reset_at=NOW + timedelta(hours=100), now=NOW)
            self.assertEqual(load_policy(db, "claude", now=NOW).weekly_used, 12.0)
            self.assertEqual(load_policy(db, "codex", now=NOW).weekly_used, 78.0)
            self.assertEqual(len(load_history(db, "claude", now=NOW)), 1)
            self.assertEqual(len(load_history(db, "codex", now=NOW)), 1)

    def test_observation_history_is_bounded_and_keeps_window_peak(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
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

            pace_db = Path(temp) / "pace.sqlite3"
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
            db = Path(temp) / "dense.sqlite3"
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

            pristine_db = Path(temp) / "pristine.sqlite3"
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

            legacy_db = Path(temp) / "legacy-v1.sqlite3"
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
            corrupt = Path(temp) / "corrupt.sqlite3"
            corrupt.write_bytes(b"not a sqlite database")
            policy = load_policy(corrupt, "claude", now=NOW)
            self.assertEqual(policy.mode, "Lean")
            self.assertEqual(policy.reason, "state_unavailable")
            self.assertEqual(corrupt.read_bytes(), b"not a sqlite database")

            locked = Path(temp) / "locked.sqlite3"
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
            root = Path(temp)
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

    def test_claude_statusline_ingest_uses_only_whitelisted_fields(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            data = {
                "rate_limits": {
                    "seven_day": {"used_percentage": 30.0, "resets_at": (NOW + timedelta(hours=100)).timestamp()},
                    "five_hour": {"used_percentage": 12.0, "resets_at": (NOW + timedelta(hours=3)).timestamp()},
                },
                "transcript": "SYNTHETIC_SENTINEL_NO_RETENTION",
            }
            code, out, _ = invoke(
                ["--db", str(db), "ingest", "claude-statusline", "--stdin"],
                stdin=io.StringIO(json.dumps(data)),
            )
            self.assertEqual(code, 0)
            self.assertNotIn("SYNTHETIC_SENTINEL", out)
            policy = load_policy(db, "claude", now=NOW)
            self.assertEqual(policy.source, "claude_statusline")
            self.assertEqual(policy.weekly_used, 30.0)
            self.assertEqual(policy.short_used, 12.0)
            self.assertNotIn(b"SYNTHETIC_SENTINEL", db.read_bytes())

    def test_missing_claude_fields_do_not_refresh_or_erase_good_data(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            record_manual(db, provider="claude", observed_at=NOW, weekly_used=20,
                          weekly_reset_at=NOW + timedelta(hours=100), now=NOW)
            code, _, err = invoke(
                ["--db", str(db), "ingest", "claude-statusline", "--stdin"],
                stdin=io.StringIO(json.dumps({"rate_limits": {"five_hour": None}})),
            )
            self.assertEqual(code, 2)
            self.assertIn("weekly rate limit is unavailable", err)
            self.assertEqual(load_policy(db, "claude", now=NOW).weekly_used, 20.0)
            self.assertEqual(len(load_history(db, "claude", now=NOW)), 1)

    def test_codex_explicit_jsonl_uses_window_minutes_not_bucket_position(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            path = Path(temp) / "one-session.jsonl"
            event = {
                "timestamp": NOW.isoformat(), "type": "event_msg",
                "payload": {
                    "type": "token_info",
                    "rate_limits": {
                        "primary": {"used_percent": 44, "window_minutes": 300,
                                    "resets_at": (NOW + timedelta(hours=4)).timestamp()},
                        "secondary": {"used_percent": 18, "window_minutes": 10080,
                                      "resets_at": (NOW + timedelta(hours=100)).timestamp()},
                    },
                },
            }
            path.write_text(json.dumps(event) + "\n", encoding="utf-8")
            code, out, _ = invoke(["--db", str(db), "ingest", "codex", "--file", str(path)])
            self.assertEqual(code, 0)
            self.assertNotIn(str(path), out)
            policy = load_policy(db, "codex", now=NOW)
            self.assertEqual(policy.source, "codex_token_info")
            self.assertEqual(policy.weekly_used, 18.0)
            self.assertEqual(policy.short_used, 44.0)
            self.assertNotIn(str(path).encode(), db.read_bytes())

    def test_codex_token_info_and_token_count_event_names(self):
        observed = NOW - timedelta(hours=2)
        weekly_reset = observed + timedelta(hours=100)
        cases = (
            ("token_info", "token_info", False),
            ("token_count", "token_count", False),
            ("unsupported", "synthetic_other_event", False),
            ("duplicate_weekly", "token_count", True),
        )
        with tempfile.TemporaryDirectory() as temp:
            for label, event_name, duplicate_weekly in cases:
                with self.subTest(case=label):
                    db = Path(temp) / f"{label}.sqlite3"
                    path = Path(temp) / f"{label}.jsonl"
                    rate_limits = {
                        "primary": {
                            "used_percent": 18.0,
                            "window_minutes": 10080,
                            "resets_at": weekly_reset.timestamp(),
                        },
                        "credits": {
                            "used_percent": None,
                            "window_minutes": None,
                            "resets_at": None,
                        },
                    }
                    if duplicate_weekly:
                        rate_limits["secondary"] = {
                            "used_percent": 20.0,
                            "window_minutes": 10080,
                            "resets_at": weekly_reset.timestamp(),
                        }
                    event = {
                        "timestamp": observed.isoformat(),
                        "type": "event_msg",
                        "payload": {"type": event_name, "rate_limits": rate_limits},
                    }
                    path.write_text(json.dumps(event) + "\n", encoding="utf-8")
                    code, out, err = invoke(
                        ["--db", str(db), "ingest", "codex", "--file", str(path)]
                    )
                    self.assertNotIn(str(path), out + err)

                    if label in ("token_info", "token_count"):
                        self.assertEqual(code, 0)
                        policy = load_policy(db, "codex", now=NOW)
                        history = load_history(db, "codex", now=NOW)
                        self.assertEqual(len(history), 1)
                        self.assertEqual(policy.source, "codex_token_info")
                        self.assertEqual(history[0].source, "codex_token_info")
                        self.assertEqual(
                            history[0].schema_marker, SOURCE_SCHEMAS["codex_token_info"]
                        )
                        self.assertEqual(history[0].weekly_used, 18.0)
                        self.assertEqual(history[0].observed_at, observed)
                        self.assertIsNone(history[0].short_used)
                        self.assertIsNone(history[0].short_reset_at)
                        self.assertIsNone(policy.short_used)
                        self.assertLessEqual(MODE_RANK[policy.mode], MODE_RANK["Normal"])
                        self.assertNotIn(str(path).encode(), db.read_bytes())
                    else:
                        self.assertEqual(code, 2)
                        if label == "unsupported":
                            self.assertIn("no supported Codex rate-limit event was found", err)
                        else:
                            self.assertIn("ambiguous", err)
                        self.assertEqual(load_history(db, "codex", now=NOW), [])

    def test_codex_reader_skips_partial_and_oversize_lines_and_refuses_unknown_shape(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            path = Path(temp) / "tail.jsonl"
            event = {
                "timestamp": NOW.isoformat(), "type": "event_msg",
                "payload": {"type": "token_info", "rate_limits": {
                    "bucket-a": {"used_percent": 18, "window_minutes": 10080,
                                 "resets_at": (NOW + timedelta(hours=100)).timestamp()}
                }},
            }
            valid = (json.dumps(event) + "\n").encode()
            partial = json.dumps(event).encode()
            path.write_bytes(
                b"p" * (cli._MAX_CODEX_TAIL_BYTES + 100)
                + b"\n"
                + b"x" * 65537
                + b"\n"
                + valid
                + partial
            )
            code, _, _ = invoke(["--db", str(db), "ingest", "codex", "--file", str(path)])
            self.assertEqual(code, 0)
            self.assertEqual(load_policy(db, "codex", now=NOW).weekly_used, 18.0)

            unknown = Path(temp) / "unknown.jsonl"
            unknown.write_text(json.dumps({"type": "event_msg", "payload": {"type": "token_info", "info": {"total_token_usage": 100}}}) + "\n", encoding="utf-8")
            before = len(load_history(db, "codex", now=NOW))
            code, _, err = invoke(["--db", str(db), "ingest", "codex", "--file", str(unknown)])
            self.assertEqual(code, 2)
            self.assertIn("Codex rate-limit", err)
            self.assertEqual(len(load_history(db, "codex", now=NOW)), before)

            ambiguous = Path(temp) / "ambiguous.jsonl"
            ambiguous_event = {
                "timestamp": NOW.isoformat(), "type": "event_msg",
                "payload": {"type": "token_info", "rate_limits": {
                    "first": {"used_percent": 18, "window_minutes": 10080,
                              "resets_at": (NOW + timedelta(hours=100)).timestamp()},
                    "second": {"used_percent": 20, "window_minutes": 10080,
                               "resets_at": (NOW + timedelta(hours=100)).timestamp()},
                }},
            }
            ambiguous.write_text(json.dumps(ambiguous_event) + "\n", encoding="utf-8")
            code, _, err = invoke(["--db", str(db), "ingest", "codex", "--file", str(ambiguous)])
            self.assertEqual(code, 2)
            self.assertIn("ambiguous", err)
            self.assertEqual(len(load_history(db, "codex", now=NOW)), before)

    def test_hook_native_json_is_bounded_prompt_blind_and_falls_back(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "bad.sqlite3"
            db.write_bytes(b"corrupt")
            prompt = PromptInput(json.dumps({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "SYNTHETIC_SENTINEL_NO_RETENTION",
            }))
            for client in ("claude", "codex"):
                code, out, err = invoke(
                    ["--db", str(db), "hook", "--client", client], stdin=prompt
                )
                payload = json.loads(out)
                self.assertEqual(code, 0)
                self.assertEqual(err, "")
                self.assertEqual(set(payload), {"hookSpecificOutput"})
                native = payload["hookSpecificOutput"]
                self.assertEqual(native["hookEventName"], "UserPromptSubmit")
                self.assertIn("Mode: Lean", native["additionalContext"])
                self.assertNotIn("SYNTHETIC_SENTINEL", out)
                self.assertLessEqual(len(out.encode("utf-8")), 2048)
                self.assertEqual(prompt.read_calls, 0)
                self.assertNotIn(b"SYNTHETIC_SENTINEL", db.read_bytes())

    def test_hook_config_is_print_only_and_preserves_existing_hooks(self):
        with tempfile.TemporaryDirectory() as temp:
            before = list(Path(temp).iterdir())
            code, out, _ = invoke(["hook-config", "--client", "claude"])
            value = json.loads(out)
            self.assertEqual(code, 0)
            self.assertTrue(value["template_only"])
            self.assertEqual(value["event"], "UserPromptSubmit")
            self.assertIn("preserve all other hooks", value["merge_instruction"])
            self.assertEqual(before, list(Path(temp).iterdir()))

    def test_non_tty_no_color_preview_and_tty_preview_boundaries(self):
        code, plain, _ = invoke(["preview", "--seconds", "3"])
        self.assertEqual(code, 0)
        self.assertNotIn("\x1b[", plain)
        tty = TTY()
        with patch.dict(os.environ, {}, clear=True), patch.object(cli.time, "sleep", return_value=None):
            code, animated, _ = invoke(["preview", "--seconds", "3"], stdout=tty)
        self.assertEqual(code, 0)
        self.assertEqual(animated.count("/3"), 3)
        self.assertIn("\x1b[?25l", animated)
        self.assertIn("\x1b[?25h", animated)
        color_tty = TTY()
        with patch.dict(os.environ, {"NO_COLOR": ""}, clear=True):
            code, static, _ = invoke(["preview"], stdout=color_tty)
        self.assertEqual(code, 0)
        self.assertNotIn("\x1b[", static)
        self.assertNotIn("/3", static)

    def test_status_uses_mode_palette_only_on_color_tty(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            record_manual(
                db, provider="claude", observed_at=NOW, weekly_used=40.0,
                weekly_reset_at=NOW + timedelta(hours=100), short_used=96.0,
                short_reset_at=NOW + timedelta(hours=4), now=NOW,
            )
            tty = TTY()
            with patch.dict(os.environ, {}, clear=True):
                code, colored, _ = invoke(
                    ["--db", str(db), "status", "--provider", "claude"], stdout=tty
                )
            self.assertEqual(code, 0)
            self.assertIn("\x1b[34mAUSTERE\x1b[0m", colored)
            no_color_tty = TTY()
            with patch.dict(os.environ, {"NO_COLOR": ""}, clear=True):
                code, plain, _ = invoke(
                    ["--db", str(db), "status", "--provider", "claude"], stdout=no_color_tty
                )
            self.assertEqual(code, 0)
            self.assertNotIn("\x1b[", plain)

    def test_preview_interrupt_restores_cursor(self):
        tty = TTY()
        with patch.dict(os.environ, {}, clear=True), patch.object(cli.time, "sleep", side_effect=KeyboardInterrupt):
            code, output, _ = invoke(["preview"], stdout=tty)
        self.assertEqual(code, 130)
        self.assertIn("\x1b[?25l", output)
        self.assertIn("\x1b[?25h", output)

    def test_broken_pipe_is_quiet(self):
        class Broken:
            def getvalue(self):
                return ""

            def write(self, value):
                raise BrokenPipeError

            def isatty(self):
                return False

        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / "state.sqlite3"
            code, _, err = invoke(
                ["--db", str(db), "status", "--provider", "claude"], stdout=Broken()
            )
        self.assertEqual(code, 0)
        self.assertEqual(err, "")


if __name__ == "__main__":
    unittest.main()
