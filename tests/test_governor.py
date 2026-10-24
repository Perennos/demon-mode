from datetime import timedelta
from pathlib import Path
import tempfile
import unittest

from demon_mode.governor import MODE_POLICY, GovernorState, WeeklyMetrics, advance_state, evaluate_policy, parse_timestamp, weekly_metrics
from demon_mode.store import record_manual


from support import NOW, database_connection, observation


class GovernorTests(unittest.TestCase):
    def test_five_mode_instructions_are_distinct(self):
        self.assertEqual(len(MODE_POLICY), 5)
        self.assertEqual(len(set(MODE_POLICY.values())), 5)


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
            db = Path(temp).resolve() / "state.sqlite3"
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


    def test_new_reset_window_replaces_transition_candidate(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
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



if __name__ == "__main__":
    unittest.main()
