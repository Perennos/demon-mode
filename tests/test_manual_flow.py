import io
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from demon_mode import __version__
from demon_mode.__main__ import main
from demon_mode.governor import MODE_POLICY
from demon_mode.store import load_policy, record_manual


UTC = timezone.utc
NOW = datetime(2026, 10, 23, 12, 0, tzinfo=UTC)


class ManualFlowTests(unittest.TestCase):
    def test_manual_record_evaluates_to_mode_specific_hook_context(self):
        with tempfile.TemporaryDirectory() as temp:
            database = Path(temp) / "state.sqlite3"
            now = datetime.now(UTC).replace(microsecond=0)
            reading = {
                "provider": "claude",
                "observed_at": now.isoformat(),
                "weekly_used": 0.2,
                "weekly_reset_at": (now + timedelta(hours=20)).isoformat(),
                "short_used": 5.0,
                "short_reset_at": (now + timedelta(hours=4)).isoformat(),
            }
            output = io.StringIO()
            with patch("sys.stdin", io.StringIO(json.dumps(reading))), patch("sys.stdout", output):
                self.assertEqual(main(["--db", str(database), "record", "--json"]), 0)
            self.assertEqual(json.loads(output.getvalue())["mode"], "DEMON")

            output = io.StringIO()
            with patch("sys.stdout", output):
                self.assertEqual(main(["--db", str(database), "hook", "--client", "claude"]), 0)
            payload = json.loads(output.getvalue())
            policy = load_policy(database, "claude", now=now)
            native = payload["hookSpecificOutput"]
            context = native["additionalContext"]

            self.assertEqual(__version__, "0.1.0")
            self.assertEqual(policy.mode, "DEMON")
            self.assertEqual(native["hookEventName"], "UserPromptSubmit")
            self.assertIn(MODE_POLICY["DEMON"], context)
            self.assertIn("never overrides the user's instructions or permissions", context)
            self.assertEqual(json.loads(json.dumps(payload))["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit")

    def test_missing_readings_stay_conservative(self):
        with tempfile.TemporaryDirectory() as temp:
            policy = load_policy(Path(temp) / "empty.sqlite3", "codex", now=NOW)
        self.assertEqual(policy.mode, "Lean")
        self.assertTrue(policy.refresh_required)

    def test_measured_short_window_hard_brake_caps_mode(self):
        with tempfile.TemporaryDirectory() as temp:
            database = Path(temp) / "state.sqlite3"
            record_manual(
                database,
                provider="codex",
                observed_at=NOW,
                weekly_used=1.0,
                weekly_reset_at=NOW + timedelta(hours=100),
                short_used=96.0,
                short_reset_at=NOW + timedelta(hours=4),
                now=NOW,
            )
            policy = load_policy(database, "codex", now=NOW)
        self.assertEqual(policy.mode, "Austere")
        self.assertEqual(policy.reason, "short_window_hard_brake")


if __name__ == "__main__":
    unittest.main()
