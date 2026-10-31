"""Synthetic regressions for real integration boundaries; no credentials or prompts."""

import io
import json
import sqlite3
import sys
import time
from pathlib import Path
import tempfile
import unittest

from support import NOW, PromptInput, invoke
from datetime import timedelta
import demon_mode.__main__ as cli
from demon_mode.governor import evaluate_policy, hook_context, parse_percent, parse_timestamp
from demon_mode.store import load_history, load_policy, record_manual


def event(at=NOW, used=18, **extra):
    return {
        "timestamp": at.isoformat(), "type": "event_msg",
        "payload": {"type": "token_count", "rate_limits": {
            "primary": {"used_percent": 12, "window_minutes": 300,
                        "resets_at": (NOW + timedelta(hours=4)).timestamp()},
            "secondary": {"used_percent": used, "window_minutes": 10080,
                          "resets_at": (NOW + timedelta(hours=100)).timestamp()},
            **extra,
        }},
    }


def snapshot():
    return {"limitId": "codex", "primary": {
        "usedPercent": 12, "windowDurationMins": 300,
        "resetsAt": (NOW + timedelta(hours=4)).timestamp(),
    }, "secondary": {
        "usedPercent": 18, "windowDurationMins": 10080,
        "resetsAt": (NOW + timedelta(hours=100)).timestamp(),
    }}


class IntegrationReliabilityTests(unittest.TestCase):
    def test_hook_configuration_preserves_absolute_database_and_native_shape(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "ledger with spaces.sqlite3"
            _, out, _ = invoke(["--db", str(db), "hook-config", "--client", "codex"])
            config = json.loads(out)
            self.assertIn(str(db), config["argv"])
            handler = config["config"]["hooks"]["UserPromptSubmit"][0]["hooks"][0]
            self.assertEqual(handler["command"], config["command"])
            self.assertEqual(handler["type"], "command")
            self.assertLessEqual(handler["timeout"], 10)

    def test_native_config_is_a_config_only_object(self):
        code, out, _ = invoke(["hook-config", "--client", "codex", "--format", "native"])
        self.assertEqual(code, 0)
        self.assertEqual(set(json.loads(out)), {"hooks"})

    def test_explicit_log_refresh_happens_before_prompt_blind_hook(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "selected.jsonl"
            path.write_text(json.dumps(event()) + "\n", encoding="utf-8")
            db = Path(temp).resolve() / "state.sqlite3"
            prompt = PromptInput('{"prompt":"SYNTHETIC_PRIVATE_TEXT"}')
            code, out, err = invoke(["--db", str(db), "hook", "--client", "codex",
                                     "--codex-file", str(path)], stdin=prompt)
            self.assertEqual(code, 0)
            self.assertEqual(err, "")
            self.assertEqual(prompt.read_calls, 0)
            self.assertEqual(load_policy(db, "codex", now=NOW).weekly_used, 18)
            self.assertIn("18.0% measured", out)
            self.assertNotIn("SYNTHETIC_PRIVATE_TEXT", out)

    def test_failed_hook_refresh_caps_existing_demon_policy(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            record_manual(db, provider="codex", observed_at=NOW, weekly_used=0.2,
                          weekly_reset_at=NOW + timedelta(hours=20), short_used=5,
                          short_reset_at=NOW + timedelta(hours=4), now=NOW)
            code, out, err = invoke(["--db", str(db), "hook", "--client", "codex",
                                     "--codex-file", str(Path(temp).resolve() / "missing.jsonl")])
            self.assertEqual(code, 0)
            self.assertEqual(err, "")
            self.assertIn("Mode: Lean", out)
            self.assertIn("input_refresh_unavailable", out)
            self.assertEqual(len(load_history(db, "codex", now=NOW)), 1)

    def test_log_reader_uses_newest_measurement_after_old_missing_or_invalid_event(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "selected.jsonl"
            older = event(NOW - timedelta(hours=1))
            older["payload"]["rate_limits"] = None
            bad = event(NOW - timedelta(minutes=30), used="unsupported")
            path.write_text("\n".join(map(json.dumps, [older, bad, event()])) + "\n", encoding="utf-8")
            observation = cli._codex_observation(str(path), NOW)
            self.assertEqual(observation.weekly_used, 18)
            self.assertEqual(observation.observed_at, NOW)

    def test_log_reader_rejects_conflicting_latest_events(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "selected.jsonl"
            path.write_text("\n".join(map(json.dumps, [event(used=18), event(used=75)])) + "\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                cli._codex_observation(str(path), NOW)

    def test_old_malformed_container_or_timestamp_cannot_hide_newer_event(self):
        for problem in ("container", "timestamp"):
            with self.subTest(problem=problem), tempfile.TemporaryDirectory() as temp:
                old = event(NOW - timedelta(hours=1))
                if problem == "container":
                    old["payload"]["rate_limits"] = []
                else:
                    old["timestamp"] = "invalid"
                path = Path(temp).resolve() / "selected.jsonl"
                path.write_text("\n".join(map(json.dumps, [old, event()])) + "\n", encoding="utf-8")
                self.assertEqual(cli._codex_observation(str(path), NOW).weekly_used, 18)

    def test_newest_malformed_container_or_timestamp_does_not_revive_old_reading(self):
        for problem in ("container", "timestamp"):
            with self.subTest(problem=problem), tempfile.TemporaryDirectory() as temp:
                newest = event()
                if problem == "container":
                    newest["payload"]["rate_limits"] = []
                else:
                    newest["timestamp"] = "invalid"
                path = Path(temp).resolve() / "selected.jsonl"
                path.write_text("\n".join(map(json.dumps, [event(NOW - timedelta(hours=1)), newest])) + "\n", encoding="utf-8")
                with self.assertRaises(ValueError):
                    cli._codex_observation(str(path), NOW)

    def test_log_reader_does_not_use_other_metered_bucket(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "selected.jsonl"
            path.write_text(json.dumps(event(limit_id="codex_other")) + "\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                cli._codex_observation(str(path), NOW)

    def test_app_server_response_and_notification_import_numeric_fields_only(self):
        for wrapper in ("response", "notification", "export"):
            with self.subTest(wrapper=wrapper), tempfile.TemporaryDirectory() as temp:
                value = {"rateLimits": snapshot(), "rateLimitsByLimitId": {
                    "codex": snapshot(), "codex_other": {"limitId": "codex_other"},
                }, "ignored": "SYNTHETIC_PRIVATE_TEXT"}
                if wrapper == "response":
                    value = {"id": 6, "result": value}
                elif wrapper == "notification":
                    value = {"method": "account/rateLimits/updated", "params": value}
                db = Path(temp).resolve() / "state.sqlite3"
                code, out, err = invoke([
                    "--db", str(db), "ingest", "codex-app-server", "--stdin",
                    "--at", NOW.isoformat(),
                ], stdin=io.StringIO(json.dumps(value)))
                self.assertEqual(code, 0, err)
                policy = load_policy(db, "codex", now=NOW)
                self.assertEqual(policy.weekly_used, 18)
                self.assertEqual(policy.short_used, 12)
                self.assertEqual(policy.source, "codex_app_server")
                self.assertNotIn("SYNTHETIC_PRIVATE_TEXT", out)
                self.assertNotIn(b"SYNTHETIC_PRIVATE_TEXT", db.read_bytes())

    def test_app_server_import_preserves_original_observation_time(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            observed = NOW - timedelta(hours=2)
            value = {"result": {"rateLimits": snapshot()}}
            value["result"]["rateLimits"]["primary"]["resetsAt"] = (observed + timedelta(hours=4)).timestamp()
            code, _, err = invoke(["--db", str(db), "ingest", "codex-app-server", "--stdin",
                                   "--at", observed.isoformat()], stdin=io.StringIO(json.dumps(value)))
            self.assertEqual(code, 0, err)
            self.assertEqual(load_history(db, "codex", now=NOW)[0].observed_at, observed)
            self.assertTrue(load_policy(db, "codex", now=NOW).refresh_required)

    def test_app_server_rejects_ambiguous_or_unsupported_limits_without_mutation(self):
        invalid = []
        wrong = snapshot()
        wrong["secondary"]["windowDurationMins"] = 15
        invalid.append({"rateLimits": wrong})
        invalid.append({"rateLimitsByLimitId": {"codex_other": snapshot()}})
        invalid.append({"rateLimits": snapshot(), "rateLimitsByLimitId": {"codex": {}}})
        for data in invalid:
            with self.subTest(data=data), tempfile.TemporaryDirectory() as temp:
                db = Path(temp).resolve() / "state.sqlite3"
                code, out, _ = invoke(["--db", str(db), "ingest", "codex-app-server", "--stdin",
                                       "--at", NOW.isoformat()], stdin=io.StringIO(json.dumps(data)))
                self.assertEqual(code, 2)
                self.assertFalse(db.exists())

    def test_hook_context_terminates_and_obeys_tiny_utf8_limits(self):
        policy = evaluate_policy("codex", [], None, now=NOW)
        for limit in (0, 1, 20, 100, 700, 2048):
            with self.subTest(limit=limit):
                result = hook_context(policy, NOW, limit=limit)
                self.assertLessEqual(len(result.encode("utf-8")), limit)

    def test_deep_json_returns_quiet_input_error(self):
        code, out, err = invoke(["record", "--stdin-json"], stdin=io.StringIO("[" * 1500 + "0" + "]" * 1500))
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertNotIn("Traceback", err)

    def test_numeric_overflow_is_a_validation_error(self):
        for parser in (parse_percent, parse_timestamp):
            with self.subTest(parser=parser.__name__):
                with self.assertRaises(ValueError):
                    parser(10 ** 1000)

    def test_plain_status_explains_unknown_readings(self):
        with tempfile.TemporaryDirectory() as temp:
            code, out, _ = invoke(["--db", str(Path(temp).resolve() / "empty.sqlite3"), "status", "--provider", "codex"])
            self.assertEqual(code, 0)
            self.assertIn("reason=", out)
            self.assertIn("refresh=required", out)

    def test_short_projection_includes_age_since_measurement(self):
        from demon_mode.governor import normalize_observation, short_metrics
        observed = NOW - timedelta(minutes=30)
        reading = normalize_observation(
            provider="codex", observed_at=observed, weekly_used=0.2,
            weekly_reset_at=NOW + timedelta(hours=20), short_used=31,
            short_reset_at=NOW + timedelta(hours=3), now=NOW,
        )
        short = short_metrics([reading], NOW)
        self.assertEqual(short.projected_used, 100)
        self.assertEqual(evaluate_policy("codex", [reading], None, now=NOW).reason,
                         "short_window_pressure_brake")

    def test_backfilled_decrease_invalidates_headroom_until_new_measurement(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            reset = NOW + timedelta(hours=20)
            record_manual(db, provider="codex", observed_at=NOW, weekly_used=20,
                          weekly_reset_at=reset, short_used=5,
                          short_reset_at=NOW + timedelta(hours=4), now=NOW)
            policy = record_manual(db, provider="codex", observed_at=NOW - timedelta(minutes=30),
                                   weekly_used=99, weekly_reset_at=reset, now=NOW)
            self.assertIn(policy.mode, ("Austere", "Lean"))
            self.assertTrue(policy.refresh_required)
            policy = load_policy(db, "codex", now=NOW)
            self.assertIn(policy.mode, ("Austere", "Lean"))
            self.assertTrue(policy.refresh_required)

    def test_short_conflict_keeps_hard_brake_across_weekly_reset(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            record_manual(db, provider="codex", observed_at=NOW - timedelta(minutes=2),
                          weekly_used=90, weekly_reset_at=NOW - timedelta(minutes=1),
                          short_used=96, short_reset_at=NOW + timedelta(hours=3), now=NOW)
            policy = record_manual(db, provider="codex", observed_at=NOW,
                                   weekly_used=1, weekly_reset_at=NOW + timedelta(days=7),
                                   short_used=1, short_reset_at=NOW + timedelta(hours=3), now=NOW)
            self.assertEqual(policy.mode, "Austere")
            self.assertEqual(policy.short_used, 96)

    def test_fractional_timestamps_sort_chronologically_in_history(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            reset = NOW + timedelta(hours=100)
            for at, used in ((NOW, 20), (NOW + timedelta(milliseconds=100), 21)):
                record_manual(db, provider="codex", observed_at=at, weekly_used=used,
                              weekly_reset_at=reset, now=at)
            rows = load_history(db, "codex", now=NOW + timedelta(seconds=1))
            self.assertEqual([item.weekly_used for item in rows], [21, 20])

    def test_context_command_prints_pasteable_advice_without_hook_json(self):
        with tempfile.TemporaryDirectory() as temp:
            code, out, err = invoke(["--db", str(Path(temp).resolve() / "empty.sqlite3"),
                                     "context", "--provider", "codex"])
            self.assertEqual(code, 0, err)
            self.assertIn("advisory resource policy", out)
            self.assertIn("Mode: Lean", out)
            self.assertNotIn("hookSpecificOutput", out)

    def test_doctor_explains_missing_readings_and_reports_no_host_verification(self):
        with tempfile.TemporaryDirectory() as temp:
            code, out, err = invoke(["--db", str(Path(temp).resolve() / "empty.sqlite3"),
                                     "doctor", "--provider", "codex", "--json"])
            self.assertEqual(code, 0, err)
            result = json.loads(out)
            self.assertTrue(result["refresh_required"])
            self.assertEqual(result["host_integration"], "Untested")
            self.assertTrue(result["next_steps"])
            self.assertEqual(result["version"], "0.1.1")

    def test_app_server_file_accepts_utf8_bom_and_json_output(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "export.json"
            path.write_text(json.dumps({"result": {"rateLimits": snapshot()}}), encoding="utf-8-sig")
            code, out, err = invoke(["--db", str(Path(temp).resolve() / "state.sqlite3"), "ingest",
                                     "codex-app-server", "--file", str(path), "--at", NOW.isoformat(), "--json"])
            self.assertEqual(code, 0, err)
            self.assertEqual(json.loads(out)["weekly_used_percent"], 18)

    def test_claude_native_hook_uses_shell_free_argument_vector(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "ledger's $literal.sqlite3"
            code, out, err = invoke(["--db", str(db), "hook-config", "--client", "claude", "--format", "native"])
            self.assertEqual(code, 0, err)
            handler = json.loads(out)["hooks"]["UserPromptSubmit"][0]["hooks"][0]
            self.assertEqual(handler["command"], sys.executable)
            self.assertIn(str(db), handler["args"])

    def test_app_server_does_not_infer_permission_from_low_percentages(self):
        for field, value in (("ordinaryUsageAllowed", False), ("ordinaryUsageAllowed", None),
                             ("ordinaryUsageAllowed", "true"), ("ordinaryUsageAllowed", 1),
                             ("spendControlReached", True),
                             ("rateLimitReachedType", "workspace_owner_usage_limit_reached")):
            with self.subTest(field=field):
                data = {"rateLimits": snapshot()}
                target = data if field == "ordinaryUsageAllowed" else data["rateLimits"]
                target[field] = value
                from demon_mode.sources import app_server_observation
                with self.assertRaises(ValueError):
                    app_server_observation(data, NOW, NOW)

    def test_app_server_allows_missing_or_explicitly_true_usage_permission(self):
        from demon_mode.sources import app_server_observation
        for data in ({"rateLimits": snapshot()},
                     {"ordinaryUsageAllowed": True, "rateLimits": snapshot()}):
            with self.subTest(data=data):
                result = app_server_observation(data, NOW, NOW)
                self.assertEqual(result.weekly_used, 18)

    def test_hook_refresh_lock_wait_is_bounded(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            load_policy(db, "codex", now=NOW)
            path = Path(temp).resolve() / "selected.jsonl"
            path.write_text(json.dumps(event()) + "\n", encoding="utf-8")
            lock = sqlite3.connect(db)
            try:
                lock.execute("BEGIN EXCLUSIVE")
                start = time.monotonic()
                code, out, err = invoke(["--db", str(db), "hook", "--client", "codex", "--codex-file", str(path)])
                elapsed = time.monotonic() - start
            finally:
                lock.rollback()
                lock.close()
            self.assertEqual(code, 0, err)
            self.assertIn("Mode: Lean", out)
            self.assertLess(elapsed, 2.0)

    def test_replayed_claude_statusline_keeps_original_capture_time(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
            observed = NOW - timedelta(hours=2)
            data = {"rate_limits": {"seven_day": {
                "used_percentage": 18, "resets_at": (NOW + timedelta(hours=100)).timestamp(),
            }}}
            code, out, err = invoke(["--db", str(db), "ingest", "claude-statusline", "--stdin",
                                     "--at", observed.isoformat(), "--json"], stdin=io.StringIO(json.dumps(data)))
            self.assertEqual(code, 0, err)
            self.assertEqual(load_history(db, "claude", now=NOW)[0].observed_at, observed)
            self.assertTrue(json.loads(out)["refresh_required"])


if __name__ == "__main__":
    unittest.main()
