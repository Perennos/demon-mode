"""Bounded readers for explicit local inputs; no network or credential access."""

from datetime import datetime
import json
import os
from pathlib import Path

from .governor import SOURCE_SCHEMAS, normalize_observation, parse_timestamp

_MAX_CODEX_TAIL_BYTES = 1024 * 1024
_MAX_CODEX_LINE_BYTES = 65536
_WINDOWS_MINUTES = {300: "short", 10080: "weekly"}


class InputUnavailable(ValueError):
    """A source did not contain one unambiguous, supported numeric observation."""


def _read_json(stream, maximum: int) -> object:
    source = getattr(stream, "buffer", stream)
    raw = source.read(maximum + 1)
    if isinstance(raw, str):
        raw = raw.encode("utf-8")
    if len(raw) > maximum:
        raise ValueError("input exceeds the configured size limit")
    try:
        return json.loads(raw.decode("utf-8-sig"))
    except (UnicodeDecodeError, ValueError, RecursionError) as exc:
        raise ValueError("input must be valid UTF-8 JSON") from exc


def _observation_from_statusline(value: object, current: datetime, *, observed_at: object = None):
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
        observed_at=current if observed_at is None else observed_at,
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
    unorderable_at = -1
    for index, line in enumerate(data.splitlines()):
        if not line or len(line) > _MAX_CODEX_LINE_BYTES:
            continue
        try:
            event = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, ValueError, RecursionError):
            continue
        if not isinstance(event, dict) or event.get("type") != "event_msg":
            continue
        payload = event.get("payload")
        if not isinstance(payload, dict) or payload.get("type") not in ("token_info", "token_count"):
            continue
        rate_limits = payload.get("rate_limits")
        # Token-only events are not new allowance measurements.
        if rate_limits is None:
            continue
        if isinstance(rate_limits, dict) and rate_limits.get("limit_id") not in (None, "codex"):
            continue
        try:
            observed = parse_timestamp(event.get("timestamp"))
        except ValueError:
            unorderable_at = index
            continue
        candidates.append((observed, rate_limits, index))
    if not candidates:
        raise InputUnavailable("no supported Codex rate-limit event was found")
    newest = max(item[0] for item in candidates)
    latest_position = max(item[2] for item in candidates if item[0] == newest)
    if unorderable_at > latest_position:
        raise InputUnavailable("Codex latest rate-limit timestamp is invalid")
    observations = []
    # Validate the latest measurement, not obsolete data from earlier in a session.
    # A malformed latest measurement must fail rather than revive old headroom.
    for observed, rate_limits, _ in candidates:
        if observed != newest:
            continue
        if not isinstance(rate_limits, dict):
            raise InputUnavailable("Codex rate-limit container is missing or ambiguous")
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
        observations.append(
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
            )
        )
    if any(item != observations[0] for item in observations[1:]):
        raise InputUnavailable("Codex newest rate-limit readings conflict")
    return observations[0]


def app_server_observation(value: object, observed_at: object, now: datetime):
    """Accept a captured response, notification, or its unwrapped rate-limit data.

    Exports have no observation timestamp: the caller must supply the original
    capture time. Only the shared Codex bucket is supported by this ledger.
    """
    if not isinstance(value, dict) or "error" in value:
        raise ValueError("Codex app-server rate-limit export is unavailable")
    if "result" in value:
        value = value["result"]
    elif "method" in value:
        if value["method"] != "account/rateLimits/updated":
            raise ValueError("unsupported Codex app-server notification")
        value = value.get("params")
    if not isinstance(value, dict):
        raise ValueError("Codex app-server rate-limit export is unavailable")
    if "ordinaryUsageAllowed" in value and value["ordinaryUsageAllowed"] is not True:
        raise ValueError("Codex ordinary included usage is not confirmed available; refresh provider status")
    if "rateLimitsByLimitId" in value and value["rateLimitsByLimitId"] is not None:
        buckets = value["rateLimitsByLimitId"]
        if not isinstance(buckets, dict) or "codex" not in buckets:
            raise ValueError("Codex shared rate-limit bucket is unavailable")
        limits = buckets["codex"]
    else:
        limits = value.get("rateLimits")
    if not isinstance(limits, dict) or limits.get("limitId") not in (None, "codex"):
        raise ValueError("Codex shared rate-limit bucket is unavailable")
    if limits.get("spendControlReached") is True or limits.get("rateLimitReachedType") is not None:
        raise ValueError("Codex reports a reached usage or spend limit; refresh provider status")
    matches = {300: [], 10080: []}
    for name in ("primary", "secondary"):
        bucket = limits.get(name)
        if bucket is None:
            continue
        if not isinstance(bucket, dict):
            raise ValueError("Codex app-server rate-limit bucket is incomplete")
        minutes = bucket.get("windowDurationMins")
        if isinstance(minutes, bool) or not isinstance(minutes, int) or minutes not in matches:
            raise ValueError("unsupported Codex app-server quota window")
        if "usedPercent" not in bucket or "resetsAt" not in bucket:
            raise ValueError("Codex app-server rate-limit bucket is incomplete")
        matches[minutes].append(bucket)
    if len(matches[10080]) != 1 or len(matches[300]) > 1:
        raise ValueError("Codex weekly bucket is missing or rate-limit buckets are ambiguous")
    weekly = matches[10080][0]
    short = matches[300][0] if matches[300] else None
    return normalize_observation(
        provider="codex", observed_at=observed_at,
        weekly_used=weekly["usedPercent"], weekly_reset_at=weekly["resetsAt"],
        short_used=short["usedPercent"] if short else None,
        short_reset_at=short["resetsAt"] if short else None,
        source="codex_app_server", schema_marker=SOURCE_SCHEMAS["codex_app_server"], now=now,
    )
