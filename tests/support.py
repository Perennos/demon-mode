import io
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
import sqlite3
from unittest.mock import patch

import demon_mode.__main__ as cli
from demon_mode.governor import normalize_observation


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
