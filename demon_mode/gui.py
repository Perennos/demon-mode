"""Local page for DEMON MODE: a thin shell over the code the CLI already runs."""

from __future__ import annotations

import argparse
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib import resources
import io
import json
import secrets
import sqlite3
from typing import Optional
from urllib.parse import urlsplit
import webbrowser

from .__main__ import (
    _MAX_STATUSLINE_BYTES,
    _hook_config,
    _manual_record,
    _now,
    _observation_from_statusline,
    _read_json,
)
from .governor import PROVIDERS
from .store import load_policy, record_observation

_MAX_BODY_BYTES = 65536
_SECURITY_HEADERS = (
    ("Content-Security-Policy", "default-src 'self'; img-src 'self' data:"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
)
_ASSETS = {
    "/": ("web/index.html", "text/html; charset=utf-8"),
    "/app.css": ("web/app.css", "text/css; charset=utf-8"),
    "/app.js": ("web/app.js", "text/javascript; charset=utf-8"),
    **{
        f"/brand/{name}.svg": (f"brand/{name}.svg", "image/svg+xml")
        for name in ("mark", "mark-light", "favicon")
    },
}


def _state(database: Optional[str]) -> dict:
    """Same calls as `status --json`, once per provider, plus that client's hook template."""
    current = _now()
    return {
        "providers": {
            name: {**load_policy(database, name, now=current).as_dict(current), "hook": _hook_config(name)}
            for name in sorted(PROVIDERS)
        }
    }


def _record(database: Optional[str], body: dict) -> None:
    args = argparse.Namespace(
        stdin_json=False,
        # "" rather than None, so _manual_record can never fall back to reading this process's stdin
        provider=body.get("provider") or "",
        weekly=body.get("weekly"),
        resets=body.get("resets"),
        five_hour=body.get("five_hour"),
        five_resets=body.get("five_hour_resets"),
        observed_at=body.get("at"),
    )
    _manual_record(database, args, _now())


def _ingest(database: Optional[str], body: dict) -> None:
    text = body.get("text")
    if not isinstance(text, str):
        raise ValueError("text must be a string")
    current = _now()
    payload = _read_json(io.BytesIO(text.encode("utf-8")), _MAX_STATUSLINE_BYTES)
    record_observation(database, _observation_from_statusline(payload, current), now=current)


_ACTIONS = {"/api/record": _record, "/api/ingest": _ingest}


class _Handler(BaseHTTPRequestHandler):
    timeout = 10

    def log_message(self, *args) -> None:
        pass  # pythonw has no stderr to write to

    def end_headers(self) -> None:
        for name, value in _SECURITY_HEADERS:
            self.send_header(name, value)
        super().end_headers()

    def parse_request(self) -> bool:
        if not super().parse_request():
            return False
        if self.headers.get("Host", "").lower() not in self.server.hosts:
            self.send_error(HTTPStatus.MISDIRECTED_REQUEST)
            return False
        return True

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _json(self, status: int, value: object) -> None:
        self._send(status, json.dumps(value, sort_keys=True).encode("utf-8"), "application/json")

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        if path == "/api/state":
            return self._json(HTTPStatus.OK, _state(self.server.database))
        asset = _ASSETS.get(path)
        if asset is None:
            return self.send_error(HTTPStatus.NOT_FOUND)
        body = (resources.files(__package__) / asset[0]).read_bytes()
        if path == "/":
            body = body.replace(b"__TOKEN__", self.server.token.encode("ascii"))
        self._send(HTTPStatus.OK, body, asset[1])

    def do_POST(self) -> None:
        action = _ACTIONS.get(urlsplit(self.path).path)
        if action is None:
            return self.send_error(HTTPStatus.NOT_FOUND)
        origin = self.headers.get("Origin")
        supplied = self.headers.get("X-Token", "").encode("utf-8")
        if (origin is not None and origin not in self.server.origins) or not secrets.compare_digest(
            supplied, self.server.token.encode("ascii")
        ):
            return self._json(HTTPStatus.FORBIDDEN, {"error": "forbidden"})
        try:
            length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            length = -1
        if length < 0:
            return self._json(HTTPStatus.LENGTH_REQUIRED, {"error": "Content-Length is required"})
        if length > _MAX_BODY_BYTES:
            return self._json(HTTPStatus.REQUEST_ENTITY_TOO_LARGE, {"error": "request body is too large"})
        try:
            body = json.loads(self.rfile.read(length))
        except (ValueError, RecursionError):
            body = None
        if not isinstance(body, dict):
            return self._json(HTTPStatus.BAD_REQUEST, {"error": "request body must be a JSON object"})
        try:
            action(self.server.database, body)
        except (ValueError, TypeError) as exc:
            return self._json(HTTPStatus.BAD_REQUEST, {"error": str(exc)})
        except (OSError, sqlite3.Error):
            return self._json(HTTPStatus.INTERNAL_SERVER_ERROR, {"error": "local input or state is unavailable"})
        self._json(HTTPStatus.OK, _state(self.server.database))


class GuiServer(ThreadingHTTPServer):
    allow_reuse_address = False  # on Windows SO_REUSEADDR would let another process share the port

    def __init__(self, database: Optional[str], port: int = 0) -> None:
        super().__init__(("127.0.0.1", port), _Handler)
        self.database = database
        self.token = secrets.token_urlsafe(32)
        names = ("127.0.0.1", "localhost")
        self.hosts = {*names, *(f"{name}:{self.server_port}" for name in names)}
        self.origins = {f"http://{name}:{self.server_port}" for name in names}


def serve(database: Optional[str], port: int, open_browser: bool) -> int:
    try:
        server = GuiServer(database, port)
    except OSError as exc:
        raise ValueError(f"cannot listen on 127.0.0.1:{port}: {exc.strerror}") from exc
    url = f"http://127.0.0.1:{server.server_port}/"
    print(f"Demon Mode GUI: {url}", flush=True)
    if open_browser:
        webbrowser.open(url)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0
