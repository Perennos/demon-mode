"""Pure policy math for DEMON MODE; this module never performs I/O."""

from __future__ import annotations

from dataclasses import dataclass, replace
from datetime import datetime, timedelta, timezone
import math
from numbers import Real
from typing import Iterable, Optional

UTC = timezone.utc
WEEK = timedelta(hours=168)
SHORT_WINDOW = timedelta(hours=5)
RESET_JITTER = timedelta(seconds=2)
RESET_TOLERANCE = timedelta(minutes=2)
FUTURE_TOLERANCE = timedelta(seconds=60)
MAX_AGE = timedelta(hours=6)
REFRESH_AGE = timedelta(hours=1)

PROVIDERS = frozenset(("claude", "codex"))
SOURCES = frozenset(("manual", "claude_statusline", "codex_token_info", "codex_app_server"))
SOURCE_SCHEMAS = {
    "manual": "manual-v1",
    "claude_statusline": "claude-statusline-ratelimits-v1",
    "codex_token_info": "codex-event-msg-token-info-v1",
    "codex_app_server": "codex-app-server-ratelimits-v1",
}
CONFLICT_SCHEMAS = frozenset(("conflict-decreasing-v1", "conflict-equal-time-v1"))

MODES = ("Austere", "Lean", "Normal", "Rich", "DEMON")
MODE_RANK = {name: index for index, name in enumerate(MODES)}
UPGRADE_THRESHOLDS = {"Lean": 0.70, "Normal": 0.95, "Rich": 1.25, "DEMON": 1.40}
MODE_POLICY = {
    "Austere": (
        "One bounded work unit at a time; use no optional helpers. Reuse evidence and "
        "minimize exploratory branches while retaining required correctness, security, and accessibility checks."
    ),
    "Lean": (
        "Work directly; use one focused review when needed and at most one helper that is already permitted. "
        "Skip speculative polishing."
    ),
    "Normal": "Use your normal working policy for this authorized task. Do not add work beyond its scope.",
    "Rich": (
        "For this authorized task, use up to two independent helpers only if already supported and permitted, "
        "and add one independent review for consequential output. Request only effort already allowed; change no client setting."
    ),
    "DEMON": (
        "With fresh headroom and a reset within 24 hours, use up to three independent helpers only if already "
        "authorized, explore more deeply, and add one review pass to high-value work already authorized. Stop at "
        "target pressure or stale readings; do not invent tasks or start background work."
    ),
}


@dataclass(frozen=True)
class Observation:
    provider: str
    observed_at: datetime
    weekly_used: float
    weekly_reset_at: datetime
    short_used: Optional[float]
    short_reset_at: Optional[datetime]
    source: str
    schema_marker: str


@dataclass(frozen=True)
class GovernorState:
    window_reset_at: Optional[datetime] = None
    base_mode: Optional[str] = None
    last_change_at: Optional[datetime] = None
    candidate_mode: Optional[str] = None
    candidate_first_at: Optional[datetime] = None
    candidate_last_at: Optional[datetime] = None
    history_uncertain_until: Optional[datetime] = None


@dataclass(frozen=True)
class WeeklyMetrics:
    available: bool
    reason: str
    latest: Optional[Observation] = None
    deadline: Optional[datetime] = None
    window_start: Optional[datetime] = None
    pace_per_hour: Optional[float] = None
    estimated_used: Optional[float] = None
    ratio: Optional[float] = None
    target: Optional[float] = None
    credible_pace: bool = False
    age: Optional[timedelta] = None
    raw_mode: str = "Lean"
    recent_pace_upper_bound_per_hour: Optional[float] = None


@dataclass(frozen=True)
class ShortMetrics:
    available: bool
    reason: str
    latest: Optional[Observation] = None
    deadline: Optional[datetime] = None
    projected_used: Optional[float] = None
    age: Optional[timedelta] = None


@dataclass(frozen=True)
class Policy:
    provider: str
    mode: str
    source: str
    freshness: str
    reason: str
    weekly_used: Optional[float]
    estimated_weekly_used: Optional[float]
    pace_per_hour: Optional[float]
    ratio: Optional[float]
    target: Optional[float]
    weekly_reset_at: Optional[datetime]
    observed_at: Optional[datetime]
    short_used: Optional[float]
    short_projected_used: Optional[float]
    short_reset_at: Optional[datetime]
    refresh_required: bool
    recent_pace_upper_bound_per_hour: Optional[float] = None

    @property
    def instruction(self) -> str:
        return MODE_POLICY[self.mode]

    def as_dict(self, now: datetime) -> dict:
        def stamp(value: Optional[datetime]) -> Optional[str]:
            return value.isoformat().replace("+00:00", "Z") if value else None

        return {
            "mode": self.mode,
            "provider": self.provider,
            "source": self.source,
            "freshness": self.freshness,
            "reason": self.reason,
            "weekly_used_percent": self.weekly_used,
            "weekly_estimated_percent": self.estimated_weekly_used,
            "weekly_pace_percent_per_hour": self.pace_per_hour,
            "weekly_recent_pace_upper_bound_percent_per_hour": self.recent_pace_upper_bound_per_hour,
            "headroom_ratio": self.ratio,
            "target_percent": self.target,
            "weekly_observed_at": stamp(self.observed_at),
            "weekly_reset_at": stamp(self.weekly_reset_at),
            "weekly_seconds_to_reset": max(0, int((self.weekly_reset_at - now).total_seconds()))
            if self.weekly_reset_at
            else None,
            "short_used_percent": self.short_used,
            "short_projected_percent": self.short_projected_used,
            "short_reset_at": stamp(self.short_reset_at),
            "refresh_required": self.refresh_required,
            "instruction": self.instruction,
        }


def parse_timestamp(value: object) -> datetime:
    """Parse an aware ISO-8601 timestamp or finite Unix seconds; reject local time."""
    if isinstance(value, bool):
        raise ValueError("invalid timestamp")
    if isinstance(value, datetime):
        result = value
    elif isinstance(value, Real):
        try:
            number = float(value)
        except OverflowError as exc:
            raise ValueError("invalid timestamp") from exc
        if not math.isfinite(number) or abs(number) > 253402300799:
            raise ValueError("invalid timestamp")
        try:
            result = datetime.fromtimestamp(number, UTC)
        except (OverflowError, OSError, ValueError) as exc:
            raise ValueError("invalid timestamp") from exc
    elif isinstance(value, str) and len(value) <= 128:
        text = value.strip()
        if text.endswith("Z"):
            text = text[:-1] + "+00:00"
        try:
            result = datetime.fromisoformat(text)
        except ValueError as exc:
            raise ValueError("invalid timestamp") from exc
    else:
        raise ValueError("invalid timestamp")
    if result.tzinfo is None or result.utcoffset() is None:
        raise ValueError("timestamp must include a UTC offset")
    try:
        return result.astimezone(UTC)
    except (OverflowError, ValueError) as exc:
        raise ValueError("invalid timestamp") from exc


def parse_percent(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, Real):
        raise ValueError("invalid percentage")
    try:
        number = float(value)
    except OverflowError as exc:
        raise ValueError("invalid percentage") from exc
    if not math.isfinite(number) or number < 0.0 or number > 100.0:
        raise ValueError("invalid percentage")
    return number


def normalize_observation(
    *,
    provider: str,
    observed_at: object,
    weekly_used: object,
    weekly_reset_at: object,
    short_used: object = None,
    short_reset_at: object = None,
    source: str = "manual",
    schema_marker: Optional[str] = None,
    now: Optional[datetime] = None,
) -> Observation:
    if provider not in PROVIDERS or source not in SOURCES:
        raise ValueError("unsupported provider or source")
    expected = SOURCE_SCHEMAS[source]
    marker = schema_marker or expected
    if marker != expected:
        raise ValueError("unsupported source schema")
    current = parse_timestamp(now or datetime.now(UTC))
    observed = parse_timestamp(observed_at)
    weekly = parse_percent(weekly_used)
    weekly_reset = parse_timestamp(weekly_reset_at)
    if observed > current + FUTURE_TOLERANCE:
        raise ValueError("observation is in the future")
    if not observed < weekly_reset <= observed + WEEK + RESET_TOLERANCE:
        raise ValueError("invalid weekly reset")
    if (short_used is None) != (short_reset_at is None):
        raise ValueError("short-window values must be paired")
    short = None if short_used is None else parse_percent(short_used)
    short_reset = None if short_reset_at is None else parse_timestamp(short_reset_at)
    if short_reset is not None and not observed < short_reset <= observed + SHORT_WINDOW + RESET_TOLERANCE:
        raise ValueError("invalid short reset")
    return Observation(provider, observed, weekly, weekly_reset, short, short_reset, source, marker)


def _valid_reading(item: Observation) -> bool:
    return item.schema_marker not in CONFLICT_SCHEMAS


def _same_window(first: datetime, second: datetime) -> bool:
    return abs(first - second) <= RESET_JITTER


def _rank(mode: str) -> int:
    return MODE_RANK[mode]


def _cap(mode: str, ceiling: str) -> str:
    return mode if _rank(mode) <= _rank(ceiling) else ceiling


def _lower(mode: str) -> str:
    return MODES[max(0, _rank(mode) - 1)]


def weekly_metrics(readings: Iterable[Observation], now: datetime) -> WeeklyMetrics:
    now = parse_timestamp(now)
    items = [item for item in readings if _valid_reading(item)]
    if not items:
        return WeeklyMetrics(False, "weekly_observation_unavailable")
    newest = max(items, key=lambda item: item.observed_at)
    window = [item for item in items if _same_window(item.weekly_reset_at, newest.weekly_reset_at)]
    deadline = min(item.weekly_reset_at for item in window)
    latest = max(window, key=lambda item: item.observed_at)
    start = deadline - WEEK
    age = max(timedelta(0), now - latest.observed_at)
    if now >= deadline:
        return WeeklyMetrics(False, "weekly_window_expired", latest, deadline, start, age=age)
    if age > MAX_AGE:
        return WeeklyMetrics(False, "weekly_observation_stale", latest, deadline, start, age=age)

    ordered = sorted(window, key=lambda item: item.observed_at)
    recent = [item for item in ordered if latest.observed_at - item.observed_at <= timedelta(hours=24)]
    recent_pair = next(
        (item for item in recent if latest.observed_at - item.observed_at >= timedelta(hours=6)),
        None,
    )
    recent_pace = None
    if recent_pair is not None:
        hours = (latest.observed_at - recent_pair.observed_at).total_seconds() / 3600.0
        delta = latest.weekly_used - recent_pair.weekly_used
        if hours > 0 and delta >= 0:
            recent_pace = delta / hours

    cutoff = latest.observed_at - timedelta(hours=24)
    cutoff_bin = math.floor(cutoff.timestamp() / 3600.0)
    boundary_bin = [
        item for item in ordered if math.floor(item.observed_at.timestamp() / 3600.0) == cutoff_bin
    ]
    recent_pace_upper_bound = None
    if boundary_bin:
        first_boundary = boundary_bin[0]
        last_boundary = boundary_bin[-1]
        if first_boundary.observed_at < cutoff <= last_boundary.observed_at:
            hours = (latest.observed_at - last_boundary.observed_at).total_seconds() / 3600.0
            delta = latest.weekly_used - first_boundary.weekly_used
            if hours > 0 and delta >= 0:
                recent_pace_upper_bound = delta / hours

    recent_component = recent_pace
    if recent_pace_upper_bound is not None:
        recent_component = max(recent_component or 0.0, recent_pace_upper_bound)

    elapsed = (latest.observed_at - start).total_seconds() / 3600.0
    average_pace = latest.weekly_used / elapsed if elapsed >= 1.0 else None
    if average_pace is not None and recent_component is not None:
        pace = (average_pace + recent_component) / 2.0
    else:
        pace = recent_component if recent_component is not None else average_pace
    if pace is None:
        pace = 92.0 / 168.0
    pace = max(0.05, pace)

    remaining = (deadline - now).total_seconds() / 3600.0
    target = 97.0 if remaining <= 12.0 else 92.0
    estimated = min(100.0, latest.weekly_used + pace * min(age.total_seconds() / 3600.0, 6.0))
    ratio = max(0.0, (target - estimated) / (pace * remaining)) if remaining > 0 else None
    if ratio is None:
        raw = "Lean"
    elif estimated >= target or ratio < 0.70:
        raw = "Austere"
    elif ratio < 0.95:
        raw = "Lean"
    elif ratio < 1.25:
        raw = "Normal"
    elif remaining <= 24.0 and ratio >= 1.40:
        raw = "DEMON"
    else:
        raw = "Rich"

    credible = recent_pace is not None
    if now - start < timedelta(hours=12) and not credible:
        raw = _cap(raw, "Normal")
    reason = _reason_for_weekly(raw, estimated, target, ratio)
    return WeeklyMetrics(
        True, reason, latest, deadline, start, pace, estimated, ratio, target, credible, age, raw,
        recent_pace_upper_bound_per_hour=recent_pace_upper_bound,
    )


def _reason_for_weekly(mode: str, estimate: float, target: float, ratio: Optional[float]) -> str:
    if estimate >= target:
        return "weekly_target_reached"
    if ratio is None:
        return "weekly_headroom_unknown"
    if mode == "Austere":
        return "weekly_headroom_low"
    if mode == "Lean":
        return "weekly_headroom_tight"
    if mode == "Normal":
        return "weekly_headroom_balanced"
    if mode == "DEMON":
        return "reset_near_with_headroom"
    return "weekly_headroom_high"


def short_metrics(readings: Iterable[Observation], now: datetime) -> ShortMetrics:
    now = parse_timestamp(now)
    choices = [
        item
        for item in readings
        if _valid_reading(item) and item.short_used is not None and item.short_reset_at is not None
    ]
    if not choices:
        return ShortMetrics(False, "short_window_unavailable")
    newest = max(choices, key=lambda item: item.observed_at)
    cluster = [item for item in choices if _same_window(item.short_reset_at, newest.short_reset_at)]
    latest = max(cluster, key=lambda item: item.observed_at)
    deadline = min(item.short_reset_at for item in cluster if item.short_reset_at is not None)
    age = max(timedelta(0), now - latest.observed_at)
    if now >= deadline:
        return ShortMetrics(False, "short_window_expired", latest, deadline, age=age)
    if age > MAX_AGE:
        return ShortMetrics(False, "short_window_stale", latest, deadline, age=age)
    start = deadline - SHORT_WINDOW
    elapsed = (latest.observed_at - start).total_seconds() / 3600.0
    left = (deadline - now).total_seconds() / 3600.0
    projection = None
    if elapsed >= 0.5 and left > 0:
        pace = latest.short_used / elapsed
        horizon = (deadline - latest.observed_at).total_seconds() / 3600.0
        projection = min(100.0, latest.short_used + pace * horizon)
    return ShortMetrics(True, "short_window_measured", latest, deadline, projection, age)


def advance_state(
    state: Optional[GovernorState],
    metrics: WeeklyMetrics,
    *,
    now: datetime,
    observation_at: datetime,
    recovered: bool = False,
) -> GovernorState:
    """Apply hysteresis once, and only for a newly accepted observation."""
    if not metrics.available or metrics.deadline is None:
        if state is None:
            return GovernorState()
        return replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)
    now = parse_timestamp(now)
    observed = parse_timestamp(observation_at)
    raw = metrics.raw_mode
    if (
        state is None
        or state.base_mode is None
        or state.window_reset_at is None
        or not _same_window(state.window_reset_at, metrics.deadline)
    ):
        initial = _cap(raw, "Normal") if metrics.age and metrics.age > REFRESH_AGE else raw
        return GovernorState(metrics.deadline, initial, now)

    if recovered:
        state = replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)
    current = state.base_mode
    if raw == current:
        return replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)
    if _rank(raw) < _rank(current):
        return GovernorState(metrics.deadline, raw, now)

    if metrics.age and metrics.age > REFRESH_AGE:
        return replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)

    next_mode = MODES[_rank(current) + 1]
    if _rank(raw) < _rank(next_mode) or metrics.ratio is None:
        return replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)
    threshold = UPGRADE_THRESHOLDS[next_mode] + 0.10
    if metrics.ratio < threshold or (next_mode == "DEMON" and (metrics.deadline - now).total_seconds() > 24 * 3600):
        return replace(state, candidate_mode=None, candidate_first_at=None, candidate_last_at=None)

    if state.candidate_mode != next_mode or state.candidate_first_at is None:
        return replace(state, candidate_mode=next_mode, candidate_first_at=observed, candidate_last_at=observed)
    previous_candidate = state.candidate_last_at or state.candidate_first_at
    if observed <= previous_candidate:
        return state
    candidate_last = observed
    first = state.candidate_first_at
    last_change = state.last_change_at or now
    if candidate_last - first >= timedelta(hours=1) and now - last_change >= timedelta(hours=3):
        return GovernorState(metrics.deadline, next_mode, now)
    return replace(state, candidate_last_at=candidate_last)


def evaluate_policy(
    provider: str,
    readings: Iterable[Observation],
    state: Optional[GovernorState],
    *,
    now: datetime,
    store_issue: Optional[str] = None,
) -> Policy:
    now = parse_timestamp(now)
    items = list(readings)
    conflicts = [item for item in items if item.schema_marker in CONFLICT_SCHEMAS]
    weekly = weekly_metrics(items, now)
    short = short_metrics(items, now)
    source = weekly.latest.source if weekly.available and weekly.latest else "unavailable"
    if weekly.available:
        if state and state.base_mode and state.window_reset_at and _same_window(state.window_reset_at, weekly.deadline):
            base = _cap(state.base_mode, weekly.raw_mode)
        else:
            base = weekly.raw_mode
        reason = weekly.reason
        refresh = bool(weekly.age and weekly.age > REFRESH_AGE)
        freshness = "stale_ask_refresh" if refresh else "measured_with_estimate"
    else:
        base = "Lean"
        reason = weekly.reason
        refresh = True
        freshness = "weekly_unavailable"
    if conflicts:
        latest_conflict = max(conflicts, key=lambda item: item.observed_at)
        relevant = not weekly.latest or latest_conflict.observed_at >= weekly.latest.observed_at
        if relevant:
            base = _cap(base, "Lean")
            reason = "reading_conflict_refresh_required"
            refresh = True
            freshness = "conflict_refresh_required"

    if not short.available:
        base = _cap(base, "Normal")
        if reason not in ("weekly_window_expired", "weekly_observation_stale"):
            reason = short.reason
        refresh = True
        freshness = "short_unavailable"
    else:
        assert short.latest is not None
        short_used = short.latest.short_used
        if short_used is not None and short_used >= 95.0:
            base = "Austere"
            reason = "short_window_hard_brake"
            freshness = "measured_short_brake"
        elif short.age and short.age > REFRESH_AGE:
            base = _cap(base, "Normal")
            reason = "short_window_refresh_required"
            refresh = True
            freshness = "short_stale_ask_refresh"
            if short_used is not None and short_used >= 85.0:
                base = _lower(base)
                reason = "short_window_pressure_brake"
        elif short_used is not None and (short_used >= 85.0 or (short.projected_used is not None and short.projected_used >= 100.0)):
            base = _lower(base)
            reason = "short_window_pressure_brake"
            freshness = "measured_short_pressure"

    if state and state.history_uncertain_until is not None:
        base = _cap(base, "Lean")
        reason = "state_history_incomplete"
        refresh = True
        freshness = "history_incomplete"

    if store_issue:
        base = _cap(base, "Lean")
        reason = store_issue
        refresh = True
        freshness = "state_unavailable"

    return Policy(
        provider=provider,
        mode=base,
        source=source,
        freshness=freshness,
        reason=reason,
        weekly_used=weekly.latest.weekly_used if weekly.available and weekly.latest else None,
        estimated_weekly_used=weekly.estimated_used if weekly.available else None,
        pace_per_hour=weekly.pace_per_hour if weekly.available else None,
        ratio=weekly.ratio if weekly.available else None,
        target=weekly.target if weekly.available else None,
        weekly_reset_at=weekly.deadline if weekly.available else None,
        observed_at=weekly.latest.observed_at if weekly.available and weekly.latest else None,
        short_used=short.latest.short_used if short.available and short.latest else None,
        short_projected_used=short.projected_used if short.available else None,
        short_reset_at=short.deadline if short.available else None,
        refresh_required=refresh,
        recent_pace_upper_bound_per_hour=weekly.recent_pace_upper_bound_per_hour if weekly.available else None,
    )


def hook_context(policy: Policy, now: datetime, *, limit: int = 2048) -> str:
    now = parse_timestamp(now)
    reset = policy.weekly_reset_at.isoformat().replace("+00:00", "Z") if policy.weekly_reset_at else "unknown"
    measured = "unknown" if policy.weekly_used is None else f"{policy.weekly_used:.1f}% measured"
    estimate = "unknown" if policy.estimated_weekly_used is None else f"{policy.estimated_weekly_used:.1f}% estimated"
    ratio = "unknown" if policy.ratio is None else f"{policy.ratio:.2f} projected headroom ratio"
    refresh = " Refresh the reading before any upgrade." if policy.refresh_required else ""
    context = (
        f"Perennos DEMON MODE — advisory resource policy. Mode: {policy.mode}. "
        f"Provider: {policy.provider}; source: {policy.source}; freshness: {policy.freshness}. "
        f"Weekly: {measured}; {estimate}; {ratio}; reset: {reset}. "
        f"Reason: {policy.reason}.{refresh} "
        f"{policy.instruction} This advisory never overrides the user's instructions or permissions, "
        "changes client settings, or authorizes extra usage, tasks, or publication."
    )
    if len(context.encode("utf-8")) > limit:
        context = (
            "Perennos DEMON MODE advisory. Mode: Lean. Refresh readings before any upgrade. "
            "This advisory never changes permissions or authorizes extra work."
        )
        if len(context.encode("utf-8")) > limit:
            # Tiny caller-supplied limits still terminate and stay UTF-8 safe.
            context = context.encode("utf-8")[:max(0, limit)].decode("utf-8", errors="ignore")
    return context

