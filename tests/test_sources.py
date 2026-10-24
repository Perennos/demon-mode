import io
from datetime import timedelta
import json
from pathlib import Path
import tempfile
import unittest

import demon_mode.__main__ as cli
from demon_mode.governor import MODE_RANK, SOURCE_SCHEMAS
from demon_mode.store import load_history, load_policy, record_manual


from support import NOW, invoke


class SourcesTests(unittest.TestCase):
    def test_claude_statusline_ingest_uses_only_whitelisted_fields(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
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
            db = Path(temp).resolve() / "state.sqlite3"
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
            db = Path(temp).resolve() / "state.sqlite3"
            path = Path(temp).resolve() / "one-session.jsonl"
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
                    db = Path(temp).resolve() / f"{label}.sqlite3"
                    path = Path(temp).resolve() / f"{label}.jsonl"
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
            db = Path(temp).resolve() / "state.sqlite3"
            path = Path(temp).resolve() / "tail.jsonl"
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

            unknown = Path(temp).resolve() / "unknown.jsonl"
            unknown.write_text(json.dumps({"type": "event_msg", "payload": {"type": "token_info", "info": {"total_token_usage": 100}}}) + "\n", encoding="utf-8")
            before = len(load_history(db, "codex", now=NOW))
            code, _, err = invoke(["--db", str(db), "ingest", "codex", "--file", str(unknown)])
            self.assertEqual(code, 2)
            self.assertIn("Codex rate-limit", err)
            self.assertEqual(len(load_history(db, "codex", now=NOW)), before)

            ambiguous = Path(temp).resolve() / "ambiguous.jsonl"
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



if __name__ == "__main__":
    unittest.main()
