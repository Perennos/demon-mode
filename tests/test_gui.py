from datetime import datetime, timedelta, timezone
import http.client
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import unittest

from demon_mode.gui import GuiServer

ROOT = Path(__file__).resolve().parents[1]
UTC = timezone.utc


class GuiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.server = GuiServer(str(Path(cls.temp.name) / "state.sqlite3"), 0)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.port = cls.server.server_port
        cls.token = re.search(rb'name="x-token" content="([^"]+)"', cls.call("GET", "/")[2]).group(1).decode()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.temp.cleanup()

    @classmethod
    def call(cls, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", cls.port, timeout=10)
        try:
            connection.request(method, path, body=body, headers=headers or {})
            response = connection.getresponse()
            return response.status, response, response.read()
        finally:
            connection.close()

    def post(self, path, value, **headers):
        headers = {"X-Token": self.token, **headers}
        status, _, data = self.call("POST", path, json.dumps(value), headers)
        return status, json.loads(data)

    def test_page_and_assets(self):
        status, response, body = self.call("GET", "/")
        self.assertEqual(status, 200)
        self.assertIn(b'<meta name="x-token" content="', body)
        self.assertNotIn(b"__TOKEN__", body)
        self.assertNotIn(b"<script>", body)
        self.assertNotIn(b" style=", body)
        for path, kind in (("/app.js", "text/javascript"), ("/app.css", "text/css")):
            status, response, _ = self.call("GET", path)
            self.assertEqual(status, 200)
            self.assertTrue(response.getheader("Content-Type").startswith(kind))
        status, response, body = self.call("GET", "/brand/mark.svg")
        self.assertEqual((status, response.getheader("Content-Type")), (200, "image/svg+xml"))
        self.assertEqual(body, (ROOT / "brand" / "mark.svg").read_bytes())

    def test_security_headers_on_every_kind_of_response(self):
        for method, path in (("GET", "/"), ("GET", "/nope"), ("GET", "/brand/other.svg"), ("GET", "/brand/../gui.py")):
            _, response, _ = self.call(method, path)
            self.assertEqual(response.getheader("Content-Security-Policy"), "default-src 'self'; img-src 'self' data:")
            self.assertEqual(response.getheader("X-Content-Type-Options"), "nosniff")
            self.assertEqual(response.getheader("Referrer-Policy"), "no-referrer")
            self.assertEqual(response.getheader("Cache-Control"), "no-store")
        self.assertEqual(self.call("GET", "/nope")[0], 404)
        self.assertEqual(self.call("GET", "/brand/other.svg")[0], 404)
        self.assertEqual(self.call("GET", "/brand/../gui.py")[0], 404)

    def test_state_has_both_providers(self):
        status, _, body = self.call("GET", "/api/state")
        providers = json.loads(body)["providers"]
        self.assertEqual(status, 200)
        self.assertEqual(sorted(providers), ["claude", "codex"])
        for name, item in providers.items():
            self.assertIn("mode", item)
            self.assertEqual(item["hook"]["client"], name)

    def test_post_checks(self):
        value = {"provider": "claude", "weekly": 1, "resets": "x"}
        status, _, _ = self.call("POST", "/api/record", json.dumps(value))
        self.assertEqual(status, 403)
        status, _, _ = self.call("POST", "/api/record", json.dumps(value), {"X-Token": "wrong"})
        self.assertEqual(status, 403)
        status, _, _ = self.call("GET", "/", headers={"Host": "evil.example"})
        self.assertEqual(status, 421)
        status, _, _ = self.call("POST", "/api/record", json.dumps(value), {"X-Token": self.token, "Host": "evil.example"})
        self.assertEqual(status, 421)
        self.assertEqual(self.post("/api/record", value, Origin="http://evil.example")[0], 403)
        self.assertEqual(self.post("/api/record", value, Origin="null")[0], 403)
        status, _ = self.post("/api/record", {"provider": "claude", "weekly": 1, "resets": "x"}, Origin=f"http://localhost:{self.port}")
        self.assertEqual(status, 400)  # origin accepted, then the reading itself is rejected

    def test_body_limit(self):
        status, _, _ = self.call("POST", "/api/ingest", b"x" * (65536 + 1), {"X-Token": self.token})
        self.assertEqual(status, 413)

    def test_record_then_state_reflects_it(self):
        now = datetime.now(UTC)
        status, data = self.post("/api/record", {
            "provider": "codex",
            "weekly": 40,
            "resets": (now + timedelta(hours=72)).isoformat(),
            "five_hour": 10,
            "five_hour_resets": (now + timedelta(hours=4)).isoformat(),
        })
        self.assertEqual(status, 200)
        self.assertEqual(data["providers"]["codex"]["weekly_used_percent"], 40)
        _, _, body = self.call("GET", "/api/state")
        codex = json.loads(body)["providers"]["codex"]
        self.assertEqual((codex["weekly_used_percent"], codex["short_used_percent"]), (40, 10))

    def test_invalid_record_and_missing_fields(self):
        status, data = self.post("/api/record", {"provider": "claude", "weekly": 150, "resets": "x"})
        self.assertEqual((status, data["error"]), (400, "invalid percentage"))
        status, data = self.post("/api/record", {})
        self.assertEqual((status, data["error"]), (400, "record requires --provider, --weekly, and --resets"))
        self.assertEqual(self.call("POST", "/api/record", b"[1]", {"X-Token": self.token})[0], 400)

    def test_ingest_statusline(self):
        reset = int((datetime.now(UTC) + timedelta(days=3)).timestamp())
        text = json.dumps({"rate_limits": {"seven_day": {"used_percentage": 12.5, "resets_at": reset}}})
        status, data = self.post("/api/ingest", {"text": text})
        self.assertEqual(status, 200)
        self.assertEqual(data["providers"]["claude"]["weekly_used_percent"], 12.5)
        status, data = self.post("/api/ingest", {"text": "{nope"})
        self.assertEqual((status, data["error"]), (400, "input must be valid UTF-8 JSON"))
        self.assertEqual(self.post("/api/ingest", {"text": '{"rate_limits": {}}'})[0], 400)
        self.assertEqual(self.post("/api/ingest", {"text": 5})[0], 400)


class PackagingTests(unittest.TestCase):
    def test_package_brand_copies_are_byte_identical(self):
        for name in ("mark.svg", "mark-light.svg", "favicon.svg"):
            self.assertEqual((ROOT / "demon_mode" / "brand" / name).read_bytes(), (ROOT / "brand" / name).read_bytes())

    def test_gui_subcommand_prints_address(self):
        with tempfile.TemporaryDirectory() as temp:
            process = subprocess.Popen(
                [sys.executable, "-m", "demon_mode", "--db", str(Path(temp) / "s.sqlite3"), "gui", "--no-browser", "--port", "0"],
                cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            )
            try:
                line = process.stdout.readline().strip()
            finally:
                process.terminate()
                process.communicate(timeout=10)
        self.assertRegex(line, r"^Demon Mode GUI: http://127\.0\.0\.1:\d+/$")


if __name__ == "__main__":
    unittest.main()
