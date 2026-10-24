"""Local CLI, optional bounded readers, and native UserPromptSubmit output."""

from __future__ import annotations

import argparse
import base64
from dataclasses import replace
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shlex
import sqlite3
import sys
import time
from typing import Optional, Sequence

from .governor import (
    PROVIDERS,
    Policy,
    evaluate_policy,
    hook_context,
)
from .store import load_history, load_policy, record_manual, record_observation
from .sources import (
    InputUnavailable,
    _MAX_CODEX_TAIL_BYTES,  # retained for existing local integrations
    _codex_observation,
    _observation_from_statusline,
    _read_json,
    app_server_observation,
)
from . import __version__

_MAX_HOOK_BYTES = 2048
_MAX_STATUSLINE_BYTES = 65536
_MODE_COLORS = {"Austere": "34", "Lean": "36", "Normal": "32", "Rich": "33", "DEMON": "31"}


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="demon")
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
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

    context = commands.add_parser("context", help="print pasteable advisory text for hosts without hooks")
    context.add_argument("--provider", choices=sorted(PROVIDERS), required=True)
    doctor = commands.add_parser("doctor", help="explain local readiness and any required refresh")
    doctor.add_argument("--provider", choices=sorted(PROVIDERS), required=True)
    doctor.add_argument("--json", action="store_true")

    ingest = commands.add_parser("ingest", help="opt-in read of one supported local input")
    sources = ingest.add_subparsers(dest="source_command", required=True)
    claude = sources.add_parser("claude-statusline")
    claude.add_argument("--stdin", action="store_true", required=True)
    claude.add_argument("--at", help="original capture time when replaying saved statusline JSON")
    codex = sources.add_parser("codex")
    codex.add_argument("--file", required=True, help="one explicitly selected JSONL file")
    app_server = sources.add_parser("codex-app-server", help="import an exported account/rateLimits response")
    exported = app_server.add_mutually_exclusive_group(required=True)
    exported.add_argument("--stdin", action="store_true")
    exported.add_argument("--file", help="one explicitly selected JSON export")
    app_server.add_argument("--at", required=True, help="original observation time, including UTC offset")
    for source in (claude, codex, app_server):
        source.add_argument("--json", action="store_true")

    hook = commands.add_parser("hook", help="emit native UserPromptSubmit additionalContext JSON")
    hook.add_argument("--client", choices=sorted(PROVIDERS), required=True)
    hook.add_argument("--codex-file", help="opt-in refresh from this explicit Codex JSONL file before output")
    hook_config = commands.add_parser("hook-config", help="print a command template; never edits settings")
    hook_config.add_argument("--client", choices=sorted(PROVIDERS), required=True)
    hook_config.add_argument("--codex-file", help="include explicit Codex JSONL refresh in the command")
    hook_config.add_argument("--format", choices=("template", "native"), default="template")

    preview = commands.add_parser("preview", help="run a bounded three-frame terminal preview")
    preview.add_argument("--seconds", type=int, choices=(1, 2, 3), default=3)
    preview.add_argument("--plain", action="store_true")
    return parser


def _now() -> datetime:
    return datetime.now(timezone.utc)


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
    refresh = "required" if policy.refresh_required else "not_required"
    return (f"{label} [{gauge}] weekly={used} estimated={estimate} headroom={ratio} reset={reset} "
            f"five_hour={_number(policy.short_used)} reason={policy.reason} refresh={refresh}")


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


def _hook_config(client: str, database: Optional[str] = None, codex_file: Optional[str] = None) -> dict:
    command = [sys.executable, "-m", "demon_mode"]
    if database is not None:
        if database == ":memory:":
            raise ValueError("hook configuration requires a persistent database")
        command += ["--db", str(Path(database).expanduser().absolute())]
    command += ["hook", "--client", client]
    if codex_file is not None:
        if client != "codex":
            raise ValueError("--codex-file requires --client codex")
        command += ["--codex-file", str(Path(codex_file).expanduser().absolute())]
    windows_script = None
    if os.name == "nt":
        # Codex can invoke commands through PowerShell, cmd, or a chosen shell.
        # Keep the outer command shell-neutral and quote every payload argument
        # as a PowerShell literal, preserving apostrophes, $, %, &, and spaces.
        windows_script = "& " + " ".join("'" + arg.replace("'", "''") + "'" for arg in command)
        windows_script += "; exit $LASTEXITCODE"
        encoded = base64.b64encode(windows_script.encode("utf-16le")).decode("ascii")
        quoted = "powershell.exe -NoProfile -NonInteractive -EncodedCommand " + encoded
    else:
        quoted = shlex.join(command)
    handler = {"type": "command", "command": quoted, "timeout": 10}
    if client == "claude":
        handler = {"type": "command", "command": command[0], "args": command[1:], "timeout": 10}
    result = {
        "template_only": True,
        "client": client,
        "event": "UserPromptSubmit",
        "command": quoted,
        "argv": command,
        "config": {"hooks": {"UserPromptSubmit": [{"hooks": [
            handler,
        ]}]}},
        "merge_instruction": "Add this command to the existing hook list; preserve all other hooks.",
    }
    if windows_script is not None:
        result["windows_script"] = windows_script
    return result


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
    if args.command == "context":
        policy = load_policy(database, args.provider, now=current)
        sys.stdout.write(hook_context(policy, current) + "\n")
        return 0
    if args.command == "doctor":
        policy = load_policy(database, args.provider, now=current)
        if policy.reason == "state_unavailable":
            steps = ["Check database permissions, locks, and schema compatibility; preserve the existing database."]
        elif policy.refresh_required:
            steps = ["Import a supported provider reading or record measured usage with its original observation time."]
        else:
            steps = ["Review hook-config output, merge it additively, and verify delivery in the native client."]
        report = {
            "version": __version__, "python": sys.version.split()[0],
            "provider": args.provider, "mode": policy.mode, "source": policy.source,
            "reason": policy.reason, "refresh_required": policy.refresh_required,
            "host_integration": "Untested", "next_steps": steps,
        }
        if args.json:
            _write_json(report)
        else:
            sys.stdout.write(f"Demon Mode {__version__}; Python {report['python']}; {args.provider}\n")
            sys.stdout.write(_status_line(policy, color=False) + "\n")
            sys.stdout.write("Native host delivery: Untested.\n" + "\n".join(steps) + "\n")
        return 0
    if args.command == "ingest":
        if args.source_command == "claude-statusline":
            payload = _read_json(sys.stdin, _MAX_STATUSLINE_BYTES)
            observation = _observation_from_statusline(payload, current, observed_at=args.at)
        elif args.source_command == "codex":
            observation = _codex_observation(args.file, current)
        else:
            if args.stdin:
                payload = _read_json(sys.stdin, _MAX_STATUSLINE_BYTES)
            else:
                path = Path(args.file).expanduser()
                if ((os.name == "nt" and str(path).startswith("\\\\"))
                        or path.is_symlink() or not path.is_file()):
                    raise InputUnavailable("Codex export must be one explicitly selected regular file")
                with path.open("rb") as stream:
                    payload = _read_json(stream, _MAX_STATUSLINE_BYTES)
            observation = app_server_observation(payload, args.at, current)
        policy = record_observation(database, observation, now=current)
        _show_policy(policy, current, as_json=args.json, plain=True)
        return 0
    if args.command == "hook-config":
        config = _hook_config(args.client, database, args.codex_file)
        _write_json(config["config"] if args.format == "native" else config)
        return 0
    if args.command == "preview":
        return _preview(args.seconds, plain=args.plain)
    raise ValueError("unsupported command")


def main(argv: Optional[Sequence[str]] = None) -> int:
    try:
        parsed = _parser().parse_args(argv)
        if parsed.command == "hook":
            if parsed.codex_file and parsed.client != "codex":
                raise ValueError("--codex-file requires --client codex")
            current = _now()
            refresh_failed = False
            if parsed.codex_file:
                try:
                    observation = _codex_observation(parsed.codex_file, current)
                    record_observation(parsed.db, observation, now=current, timeout=0.2)
                except Exception:
                    refresh_failed = True
            try:
                policy = load_policy(parsed.db, parsed.client, now=current)
            except Exception:
                policy = evaluate_policy(parsed.client, [], None, now=current, store_issue="state_unavailable")
            if refresh_failed:
                policy = replace(
                    policy, mode=policy.mode if policy.mode in ("Austere", "Lean") else "Lean",
                    reason="input_refresh_unavailable", freshness="input_unavailable", refresh_required=True,
                )
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
