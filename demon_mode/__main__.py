"""Local CLI, optional bounded readers, and native UserPromptSubmit output."""

from __future__ import annotations

import argparse
from dataclasses import replace
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shlex
import sqlite3
import subprocess
import sys
import time
from typing import Optional, Sequence

from .governor import (
    PROVIDERS,
    SOURCE_SCHEMAS,
    Policy,
    evaluate_policy,
    hook_context,
    normalize_observation,
    parse_timestamp,
)
from .store import load_history, load_policy, record_manual, record_observation

_MAX_HOOK_BYTES = 2048
_MAX_STATUSLINE_BYTES = 65536
_MAX_CODEX_TAIL_BYTES = 1024 * 1024
_MAX_CODEX_LINE_BYTES = 65536
_WINDOWS_MINUTES = {300: "short", 10080: "weekly"}
_MODE_COLORS = {"Austere": "34", "Lean": "36", "Normal": "32", "Rich": "33", "DEMON": "31"}


class InputUnavailable(ValueError):
    """A source did not contain one unambiguous, supported numeric observation."""


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="demon")
    parser.add_argument("--db", help="local SQLite path; defaults to this user's data directory")
    commands = parser.add_subparsers(dest="command", required=True)

    record = commands.add_parser("record", help="record one manual weekly and optional five-hour reading")
    record.add_argument("--provider", choices=sorted(PROVIDERS))
    record.add_argument("--weekly", type=float)
    record.add_argument("--resets")
    record.add_argument("--five-hour", "--five", dest="five_hour", type=float)
    record.add_argument("--five-hour-resets", "--five-resets", dest="five_resets")
    record.add_argument("--at", "--observed-at", dest="observed_at")
    record.add_argument("--stdin-json", action="store_true", help="read one bounded manual JSON object from stdin")
    record.add_argument("--json", action="store_true")
    record.add_argument("--plain", action="store_true")

    status = commands.add_parser("status", help="show current local policy")
    status.add_argument("--provider", choices=sorted(PROVIDERS), required=True)
    status.add_argument("--json", action="store_true")
    status.add_argument("--plain", action="store_true")

    history = commands.add_parser("history", help="show numeric observations only")
    history.add_argument("--provider", choices=sorted(PROVIDERS), required=True)
    history.add_argument("--limit", type=int, default=20)
    history.add_argument("--json", action="store_true")

    ingest = commands.add_parser("ingest", help="opt-in read of one supported local input")
    sources = ingest.add_subparsers(dest="source_command", required=True)
    claude = sources.add_parser("claude-statusline")
    claude.add_argument("--stdin", action="store_true", required=True)
    codex = sources.add_parser("codex")
    codex.add_argument("--file", required=True, help="one explicitly selected JSONL file")

    hook = commands.add_parser("hook", help="emit native UserPromptSubmit additionalContext JSON")
    hook.add_argument("--client", choices=sorted(PROVIDERS), required=True)
    hook_config = commands.add_parser("hook-config", help="print a command template; never edits settings")
    hook_config.add_argument("--client", choices=sorted(PROVIDERS), required=True)

    preview = commands.add_parser("preview", help="run a bounded three-frame terminal preview")
    preview.add_argument("--seconds", type=int, choices=(1, 2, 3), default=3)
    preview.add_argument("--plain", action="store_true")
    return parser


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _read_json(stream, maximum: int) -> object:
    source = getattr(stream, "buffer", stream)
    raw = source.read(maximum + 1)
    if isinstance(raw, str):
        raw = raw.encode("utf-8")
    if len(raw) > maximum:
        raise ValueError("input exceeds the configured size limit")
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("input must be valid UTF-8 JSON") from exc


def _manual_record(database: Optional[str], args, current: datetime) -> Policy:
    json_input = args.stdin_json or (
        args.provider is None
        and args.weekly is None
        and args.resets is None
        and not getattr(sys.stdin, "isatty", lambda: True)()
    )
    if json_input:
        value = _read_json(sys.stdin, 4096)
        fields = {"provider", "observed_at", "weekly_used", "weekly_reset_at", "short_used", "short_reset_at"}
        required = {"provider", "observed_at", "weekly_used", "weekly_reset_at"}
        if not isinstance(value, dict) or not required.issubset(value) or not set(value).issubset(fields):
            raise ValueError("manual input has missing or unsupported fields")
        return record_manual(database, **value, now=current)

    if args.provider is None or args.weekly is None or args.resets is None:
        raise ValueError("record requires --provider, --weekly, and --resets")
    return record_manual(
        database,
        provider=args.provider,
        observed_at=args.observed_at or current,
        weekly_used=args.weekly,
        weekly_reset_at=args.resets,
        short_used=args.five_hour,
        short_reset_at=args.five_resets,
        now=current,
    )


def _status_line(policy: Policy, *, color: bool) -> str:
    if policy.weekly_used is None:
        used = "unknown"
        gauge = "----------"
    else:
        used = f"{policy.weekly_used:.1f}%"
        filled = max(0, min(10, int(policy.weekly_used / 10.0)))
        gauge = "#" * filled + "-" * (10 - filled)
    estimate = "unknown" if policy.estimated_weekly_used is None else f"{policy.estimated_weekly_used:.1f}%"
    ratio = "unknown" if policy.ratio is None else f"{policy.ratio:.2f}x"
    reset = policy.weekly_reset_at.isoformat().replace("+00:00", "Z") if policy.weekly_reset_at else "unknown"
    label = policy.mode.upper()
    if color:
        label = f"\x1b[{_MODE_COLORS[policy.mode]}m{label}\x1b[0m"
    return f"{label} [{gauge}] weekly={used} estimated={estimate} headroom={ratio} reset={reset}"


def _write_json(value: object) -> None:
    rendered = json.dumps(value, ensure_ascii=True, sort_keys=True, indent=2)
    sys.stdout.write(rendered + "\n")


def _show_policy(policy: Policy, current: datetime, *, as_json: bool, plain: bool = False) -> None:
    if as_json:
        _write_json(policy.as_dict(current))
        return
    tty = bool(getattr(sys.stdout, "isatty", lambda: False)())
    color = tty and not plain and "NO_COLOR" not in os.environ
    sys.stdout.write(_status_line(policy, color=color) + "\n")


def _history_rows(database: Optional[str], provider: str, limit: int, current: datetime) -> list[dict]:
    observations = load_history(database, provider, limit=limit, now=current)
    return [
        {
            "observed_at": item.observed_at.isoformat().replace("+00:00", "Z"),
            "weekly_used_percent": item.weekly_used,
            "weekly_reset_at": item.weekly_reset_at.isoformat().replace("+00:00", "Z"),
            "short_used_percent": item.short_used,
            "short_reset_at": (
                item.short_reset_at.isoformat().replace("+00:00", "Z") if item.short_reset_at else None
            ),
        }
        for item in observations
    ]


def _plain_history(rows: list[dict]) -> str:
    return "\n".join(
        f"{row['observed_at']} weekly={row['weekly_used_percent']:.1f}% "
        f"weekly_reset={row['weekly_reset_at']} five_hour={_number(row['short_used_percent'])} "
        f"five_hour_reset={row['short_reset_at'] or 'unknown'}"
        for row in rows
    ) or "No local observations."


def _number(value: object) -> str:
    return "unknown" if value is None else f"{float(value):.1f}%"


def _observation_from_statusline(value: object, current: datetime):
    if not isinstance(value, dict) or not isinstance(value.get("rate_limits"), dict):
        raise InputUnavailable("Claude statusline rate limits are unavailable")
    limits = value["rate_limits"]
    weekly = limits.get("seven_day")
    short = limits.get("five_hour")
    if not isinstance(weekly, dict) or "used_percentage" not in weekly or "resets_at" not in weekly:
        raise InputUnavailable("Claude weekly rate limit is unavailable")
    short_used = short_reset = None
    if short is not None:
        if not isinstance(short, dict) or "used_percentage" not in short or "resets_at" not in short:
            raise InputUnavailable("Claude five-hour rate limit is incomplete")
        short_used, short_reset = short["used_percentage"], short["resets_at"]
    return normalize_observation(
        provider="claude",
        observed_at=current,
        weekly_used=weekly["used_percentage"],
        weekly_reset_at=weekly["resets_at"],
        short_used=short_used,
        short_reset_at=short_reset,
        source="claude_statusline",
        schema_marker=SOURCE_SCHEMAS["claude_statusline"],
        now=current,
    )


def _codex_observation(path: str, current: datetime):
    source_path = Path(path).expanduser()
    if (os.name == "nt" and str(source_path).startswith("\\\\")) or source_path.is_symlink() or not source_path.is_file():
        raise InputUnavailable("Codex input must be one explicitly selected regular file")
    with source_path.open("rb") as stream:
        size = source_path.stat().st_size
        offset = max(0, size - _MAX_CODEX_TAIL_BYTES)
        stream.seek(offset)
        data = stream.read(_MAX_CODEX_TAIL_BYTES)
    if offset:
        boundary = data.find(b"\n")
        if boundary < 0:
            raise InputUnavailable("Codex tail has no complete records")
        data = data[boundary + 1 :]
    if data and not data.endswith(b"\n"):
        boundary = data.rfind(b"\n")
        data = data[: boundary + 1] if boundary >= 0 else b""

    candidates = []
    for line in data.splitlines():
        if not line or len(line) > _MAX_CODEX_LINE_BYTES:
            continue
        try:
            event = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if not isinstance(event, dict) or event.get("type") != "event_msg":
            continue
        payload = event.get("payload")
        if not isinstance(payload, dict) or payload.get("type") != "token_info":
            continue
        rate_limits = payload.get("rate_limits")
        if not isinstance(rate_limits, dict):
            raise InputUnavailable("Codex rate-limit container is missing or ambiguous")
        observed = parse_timestamp(event.get("timestamp"))
        matches = {"weekly": [], "short": []}
        for bucket in rate_limits.values():
            if not isinstance(bucket, dict) or "window_minutes" not in bucket:
                continue
            minutes = bucket["window_minutes"]
            if isinstance(minutes, bool) or not isinstance(minutes, int) or minutes not in _WINDOWS_MINUTES:
                continue
            if "used_percent" not in bucket or "resets_at" not in bucket:
                raise InputUnavailable("Codex rate-limit bucket is incomplete")
            matches[_WINDOWS_MINUTES[minutes]].append(bucket)
        if len(matches["weekly"]) != 1 or len(matches["short"]) > 1:
            raise InputUnavailable("Codex weekly bucket is missing or rate-limit buckets are ambiguous")
        weekly = matches["weekly"][0]
        short = matches["short"][0] if matches["short"] else None
        candidates.append(
            (
                observed,
                normalize_observation(
                    provider="codex",
                    observed_at=observed,
                    weekly_used=weekly["used_percent"],
                    weekly_reset_at=weekly["resets_at"],
                    short_used=short["used_percent"] if short else None,
                    short_reset_at=short["resets_at"] if short else None,
                    source="codex_token_info",
                    schema_marker=SOURCE_SCHEMAS["codex_token_info"],
                    now=current,
                ),
            )
        )
    if not candidates:
        raise InputUnavailable("no supported Codex token_info rate-limit event was found")
    return max(candidates, key=lambda item: item[0])[1]


def _hook_payload(policy: Policy, current: datetime) -> dict:
    return {
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": hook_context(policy, current, limit=1500),
        }
    }


def _hook_json(policy: Policy, current: datetime) -> str:
    rendered = json.dumps(_hook_payload(policy, current), ensure_ascii=True, separators=(",", ":"))
    if len(rendered.encode("utf-8")) + 1 <= _MAX_HOOK_BYTES:
        return rendered
    safe = replace(
        policy,
        mode="Lean",
        source="unavailable",
        freshness="context_limit",
        reason="context_limit_refresh_required",
        refresh_required=True,
    )
    fallback = {
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": hook_context(safe, current, limit=700),
        }
    }
    rendered = json.dumps(fallback, ensure_ascii=True, separators=(",", ":"))
    if len(rendered.encode("utf-8")) + 1 > _MAX_HOOK_BYTES:
        fallback["hookSpecificOutput"]["additionalContext"] = (
            "Perennos DEMON MODE advisory. Mode: Lean. Refresh readings before any upgrade. "
            "This advisory never changes permissions or authorizes extra work."
        )
        rendered = json.dumps(fallback, ensure_ascii=True, separators=(",", ":"))
    return rendered


def _hook_config(client: str) -> dict:
    command = [sys.executable, "-m", "demon_mode", "hook", "--client", client]
    quoted = subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)
    return {
        "template_only": True,
        "client": client,
        "event": "UserPromptSubmit",
        "command": quoted,
        "merge_instruction": "Add this command to the existing hook list; preserve all other hooks.",
    }


def _preview(seconds: int, *, plain: bool = False) -> int:
    tty = bool(getattr(sys.stdout, "isatty", lambda: False)())
    if not tty or plain or "NO_COLOR" in os.environ:
        sys.stdout.write("DEMON preview [###-------] (three-frame preview unavailable in static output)\n")
        return 0
    frames = ("DEMON [#---------] 1/3", "DEMON [#####-----] 2/3", "DEMON [##########] 3/3")
    colors = ("34", "33", "31")
    sys.stdout.write("\x1b[?25l")
    try:
        for index, frame in enumerate(frames):
            sys.stdout.write(f"\r\x1b[{colors[index]}m{frame}\x1b[0m")
            sys.stdout.flush()
            if index < len(frames) - 1:
                time.sleep(seconds / 2.0)
    except KeyboardInterrupt:
        return 130
    finally:
        sys.stdout.write("\x1b[?25h\n")
        sys.stdout.flush()
    return 0


def _command(args) -> int:
    current = _now()
    database = args.db

    if args.command == "record":
        policy = _manual_record(database, args, current)
        _show_policy(policy, current, as_json=args.json, plain=args.plain)
        return 0
    if args.command == "status":
        policy = load_policy(database, args.provider, now=current)
        _show_policy(policy, current, as_json=args.json, plain=args.plain)
        return 0
    if args.command == "history":
        rows = _history_rows(database, args.provider, args.limit, current)
        if args.json:
            _write_json(rows)
        else:
            sys.stdout.write(_plain_history(rows) + "\n")
        return 0
    if args.command == "ingest":
        if args.source_command == "claude-statusline":
            payload = _read_json(sys.stdin, _MAX_STATUSLINE_BYTES)
            observation = _observation_from_statusline(payload, current)
        else:
            observation = _codex_observation(args.file, current)
        policy = record_observation(database, observation, now=current)
        _show_policy(policy, current, as_json=False, plain=True)
        return 0
    if args.command == "hook-config":
        _write_json(_hook_config(args.client))
        return 0
    if args.command == "preview":
        return _preview(args.seconds, plain=args.plain)
    raise ValueError("unsupported command")


def main(argv: Optional[Sequence[str]] = None) -> int:
    try:
        parsed = _parser().parse_args(argv)
        if parsed.command == "hook":
            current = _now()
            try:
                policy = load_policy(parsed.db, parsed.client, now=current)
            except Exception:
                policy = evaluate_policy(parsed.client, [], None, now=current, store_issue="state_unavailable")
            sys.stdout.write(_hook_json(policy, current) + "\n")
            return 0
        return _command(parsed)
    except BrokenPipeError:
        return 0
    except (ValueError, TypeError) as exc:
        print(f"demon: {exc}", file=sys.stderr)
        return 2
    except (OSError, sqlite3.Error):
        print("demon: local input or state is unavailable", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
