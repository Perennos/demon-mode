from datetime import timedelta
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import demon_mode.__main__ as cli
from demon_mode.store import record_manual


from support import NOW, TTY, PromptInput, invoke


class CliTests(unittest.TestCase):
    def test_manual_flags_status_json_and_numeric_history(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "state.sqlite3"
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


    def test_hook_native_json_is_bounded_prompt_blind_and_falls_back(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp).resolve() / "bad.sqlite3"
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
            before = list(Path(temp).resolve().iterdir())
            code, out, _ = invoke(["hook-config", "--client", "claude"])
            value = json.loads(out)
            self.assertEqual(code, 0)
            self.assertTrue(value["template_only"])
            self.assertEqual(value["event"], "UserPromptSubmit")
            self.assertIn("preserve all other hooks", value["merge_instruction"])
            self.assertEqual(before, list(Path(temp).resolve().iterdir()))


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
            db = Path(temp).resolve() / "state.sqlite3"
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
            db = Path(temp).resolve() / "state.sqlite3"
            code, _, err = invoke(
                ["--db", str(db), "status", "--provider", "claude"], stdout=Broken()
            )
        self.assertEqual(code, 0)
        self.assertEqual(err, "")



if __name__ == "__main__":
    unittest.main()
