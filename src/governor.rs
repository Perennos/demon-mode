//! Pure policy math. This module performs no I/O.
//!
//! A line-for-line port of the Python `governor.py`. Ties in `max`/`min` keep the first item, as
//! Python does, and every float operation happens in the same order so results are bit-identical.

use crate::pyjson::opt_float;
use crate::ts::{self, HOUR, MINUTE, SECOND, Ts};
use crate::{Error, Result};
use serde_json::{Map, Value};

pub const WEEK: i64 = 168 * HOUR;
pub const SHORT_WINDOW: i64 = 5 * HOUR;
pub const RESET_JITTER: i64 = 2 * SECOND;
pub const RESET_TOLERANCE: i64 = 2 * MINUTE;
pub const FUTURE_TOLERANCE: i64 = 60 * SECOND;
pub const MAX_AGE: i64 = 6 * HOUR;
pub const REFRESH_AGE: i64 = HOUR;

pub const CONFLICT_DECREASING: &str = "conflict-decreasing-v1";
pub const CONFLICT_EQUAL_TIME: &str = "conflict-equal-time-v1";

pub fn is_conflict(marker: &str) -> bool {
    marker == CONFLICT_DECREASING || marker == CONFLICT_EQUAL_TIME
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    /// Sorted by name, the order the Python release iterated `sorted(PROVIDERS)`.
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    pub fn parse(name: &str) -> Option<Provider> {
        match name {
            "claude" => Some(Provider::Claude),
            "codex" => Some(Provider::Codex),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Manual,
    ClaudeStatusline,
    CodexTokenInfo,
    CodexAppServer,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Manual => "manual",
            Source::ClaudeStatusline => "claude_statusline",
            Source::CodexTokenInfo => "codex_token_info",
            Source::CodexAppServer => "codex_app_server",
        }
    }

    pub fn schema(self) -> &'static str {
        match self {
            Source::Manual => "manual-v1",
            Source::ClaudeStatusline => "claude-statusline-ratelimits-v1",
            Source::CodexTokenInfo => "codex-event-msg-token-info-v1",
            Source::CodexAppServer => "codex-app-server-ratelimits-v1",
        }
    }

    pub fn parse(name: &str) -> Option<Source> {
        [Source::Manual, Source::ClaudeStatusline, Source::CodexTokenInfo, Source::CodexAppServer]
            .into_iter()
            .find(|source| source.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    Austere,
    Lean,
    Normal,
    Rich,
    Demon,
}

pub const MODES: [Mode; 5] = [Mode::Austere, Mode::Lean, Mode::Normal, Mode::Rich, Mode::Demon];

impl Mode {
    pub fn rank(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Austere => "Austere",
            Mode::Lean => "Lean",
            Mode::Normal => "Normal",
            Mode::Rich => "Rich",
            Mode::Demon => "DEMON",
        }
    }

    pub fn parse(name: &str) -> Option<Mode> {
        MODES.into_iter().find(|mode| mode.name() == name)
    }

    /// ANSI colour used by `status` on a colour terminal.
    pub fn color(self) -> &'static str {
        match self {
            Mode::Austere => "34",
            Mode::Lean => "36",
            Mode::Normal => "32",
            Mode::Rich => "33",
            Mode::Demon => "31",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Mode::Austere => {
                "One bounded work unit at a time; use no optional helpers. Reuse evidence and minimize exploratory \
                 branches while retaining required correctness, security, and accessibility checks."
            }
            Mode::Lean => {
                "Work directly; use one focused review when needed and at most one helper that is already permitted. \
                 Skip speculative polishing."
            }
            Mode::Normal => {
                "Use your normal working policy for this authorized task. Do not add work beyond its scope."
            }
            Mode::Rich => {
                "For this authorized task, use up to two independent helpers only if already supported and permitted, \
                 and add one independent review for consequential output. Request only effort already allowed; change \
                 no client setting."
            }
            Mode::Demon => {
                "With fresh headroom and a reset within 24 hours, use up to three independent helpers only if already \
                 authorized, explore more deeply, and add one review pass to high-value work already authorized. Stop \
                 at target pressure or stale readings; do not invent tasks or start background work."
            }
        }
    }

    fn upgrade_threshold(self) -> f64 {
        match self {
            Mode::Lean => 0.70,
            Mode::Normal => 0.95,
            Mode::Rich => 1.25,
            Mode::Demon => 1.40,
            Mode::Austere => 0.0,
        }
    }
}

fn cap(mode: Mode, ceiling: Mode) -> Mode {
    if mode.rank() <= ceiling.rank() { mode } else { ceiling }
}

fn lower(mode: Mode) -> Mode {
    MODES[mode.rank().saturating_sub(1)]
}

/// A loosely typed input value, standing in for the Python objects the release accepted.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Bool(bool),
    Num(f64),
    Text(String),
    Time(Ts),
    Other,
}

impl Val {
    /// JSON `null` becomes `None`, like Python's `None`.
    pub fn from_json(value: &Value) -> Option<Val> {
        match value {
            Value::Null => None,
            Value::Bool(flag) => Some(Val::Bool(*flag)),
            Value::Number(number) => Some(number.as_f64().map_or(Val::Other, Val::Num)),
            Value::String(text) => Some(Val::Text(text.clone())),
            Value::Array(_) | Value::Object(_) => Some(Val::Other),
        }
    }

    pub fn text(value: &str) -> Val {
        Val::Text(value.to_string())
    }
}

/// Aware ISO-8601 text or finite Unix seconds; local (naive) time is refused.
pub fn parse_timestamp(value: Option<&Val>) -> Result<Ts> {
    match value {
        Some(Val::Time(ts)) => Ok(*ts),
        Some(Val::Num(number)) => ts::from_unix_seconds(*number).map_err(Error::invalid),
        Some(Val::Text(text)) => ts::parse_text(text).map_err(Error::invalid),
        _ => Err(Error::invalid(ts::INVALID)),
    }
}

pub fn parse_percent(value: Option<&Val>) -> Result<f64> {
    match value {
        Some(Val::Num(number)) if number.is_finite() && (0.0..=100.0).contains(number) => Ok(*number),
        _ => Err(Error::invalid("invalid percentage")),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub provider: Provider,
    pub observed_at: Ts,
    pub weekly_used: f64,
    pub weekly_reset_at: Ts,
    pub short_used: Option<f64>,
    pub short_reset_at: Option<Ts>,
    pub source: Source,
    pub schema_marker: String,
}

/// The raw fields of one reading before validation; `None` is Python's `None`.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub provider: Option<Val>,
    pub observed_at: Option<Val>,
    pub weekly_used: Option<Val>,
    pub weekly_reset_at: Option<Val>,
    pub short_used: Option<Val>,
    pub short_reset_at: Option<Val>,
}

pub fn normalize_observation(reading: &Reading, source: Source, now: Ts) -> Result<Observation> {
    let provider = match &reading.provider {
        Some(Val::Text(name)) => Provider::parse(name),
        _ => None,
    }
    .ok_or_else(|| Error::invalid("unsupported provider or source"))?;
    let observed = parse_timestamp(reading.observed_at.as_ref())?;
    let weekly = parse_percent(reading.weekly_used.as_ref())?;
    let weekly_reset = parse_timestamp(reading.weekly_reset_at.as_ref())?;
    if observed > now + FUTURE_TOLERANCE {
        return Err(Error::invalid("observation is in the future"));
    }
    if !(observed < weekly_reset && weekly_reset <= observed + WEEK + RESET_TOLERANCE) {
        return Err(Error::invalid("invalid weekly reset"));
    }
    if reading.short_used.is_none() != reading.short_reset_at.is_none() {
        return Err(Error::invalid("short-window values must be paired"));
    }
    let short = reading.short_used.as_ref().map(|value| parse_percent(Some(value))).transpose()?;
    let short_reset = reading.short_reset_at.as_ref().map(|value| parse_timestamp(Some(value))).transpose()?;
    if let Some(reset) = short_reset {
        if !(observed < reset && reset <= observed + SHORT_WINDOW + RESET_TOLERANCE) {
            return Err(Error::invalid("invalid short reset"));
        }
    }
    Ok(Observation {
        provider,
        observed_at: observed,
        weekly_used: weekly,
        weekly_reset_at: weekly_reset,
        short_used: short,
        short_reset_at: short_reset,
        source,
        schema_marker: source.schema().to_string(),
    })
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GovernorState {
    pub window_reset_at: Option<Ts>,
    pub base_mode: Option<Mode>,
    pub last_change_at: Option<Ts>,
    pub candidate_mode: Option<Mode>,
    pub candidate_first_at: Option<Ts>,
    pub candidate_last_at: Option<Ts>,
    pub history_uncertain_until: Option<Ts>,
}

impl GovernorState {
    fn fresh(window: Ts, mode: Mode, now: Ts) -> GovernorState {
        GovernorState {
            window_reset_at: Some(window),
            base_mode: Some(mode),
            last_change_at: Some(now),
            ..Default::default()
        }
    }

    pub fn without_candidate(&self) -> GovernorState {
        GovernorState { candidate_mode: None, candidate_first_at: None, candidate_last_at: None, ..self.clone() }
    }
}

#[derive(Clone, Debug)]
pub struct WeeklyMetrics {
    pub available: bool,
    pub reason: &'static str,
    pub latest: Option<Observation>,
    pub deadline: Option<Ts>,
    pub window_start: Option<Ts>,
    pub pace_per_hour: Option<f64>,
    pub estimated_used: Option<f64>,
    pub ratio: Option<f64>,
    pub target: Option<f64>,
    pub credible_pace: bool,
    /// Age of the latest reading in microseconds.
    pub age: Option<i64>,
    pub raw_mode: Mode,
    pub recent_pace_upper_bound_per_hour: Option<f64>,
}

impl WeeklyMetrics {
    pub fn unavailable(reason: &'static str) -> WeeklyMetrics {
        WeeklyMetrics {
            available: false,
            reason,
            latest: None,
            deadline: None,
            window_start: None,
            pace_per_hour: None,
            estimated_used: None,
            ratio: None,
            target: None,
            credible_pace: false,
            age: None,
            raw_mode: Mode::Lean,
            recent_pace_upper_bound_per_hour: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShortMetrics {
    pub available: bool,
    pub reason: &'static str,
    pub latest: Option<Observation>,
    pub deadline: Option<Ts>,
    pub projected_used: Option<f64>,
    pub age: Option<i64>,
}

/// The first item with the largest key (Python `max` semantics).
pub fn first_max<'a, T, K: PartialOrd>(items: impl IntoIterator<Item = &'a T>, key: impl Fn(&T) -> K) -> Option<&'a T> {
    let mut best: Option<(&'a T, K)> = None;
    for item in items {
        let value = key(item);
        if best.as_ref().is_none_or(|(_, current)| value > *current) {
            best = Some((item, value));
        }
    }
    best.map(|(item, _)| item)
}

/// Python `max(a, b)`: `a` unless `b` is strictly greater.
fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// Python `min(a, b)`: `a` unless `b` is strictly smaller.
fn py_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

fn same_window(first: Ts, second: Ts) -> bool {
    (first - second).abs() <= RESET_JITTER
}

fn hour_bin(at: Ts) -> i64 {
    (at.timestamp() / 3600.0).floor() as i64
}

pub fn weekly_metrics(readings: &[Observation], now: Ts) -> WeeklyMetrics {
    let items: Vec<&Observation> = readings.iter().filter(|item| !is_conflict(&item.schema_marker)).collect();
    let Some(newest) = first_max(items.iter().copied(), |item| item.observed_at) else {
        return WeeklyMetrics::unavailable("weekly_observation_unavailable");
    };
    let window: Vec<&Observation> =
        items.iter().copied().filter(|item| same_window(item.weekly_reset_at, newest.weekly_reset_at)).collect();
    let deadline = window.iter().map(|item| item.weekly_reset_at).min().expect("window holds newest");
    let latest = first_max(window.iter().copied(), |item| item.observed_at).expect("window holds newest").clone();
    let start = deadline - WEEK;
    let age = (now - latest.observed_at).max(0);
    let mut partial = WeeklyMetrics {
        latest: Some(latest.clone()),
        deadline: Some(deadline),
        window_start: Some(start),
        age: Some(age),
        ..WeeklyMetrics::unavailable("weekly_window_expired")
    };
    if now >= deadline {
        return partial;
    }
    if age > MAX_AGE {
        partial.reason = "weekly_observation_stale";
        return partial;
    }

    let mut ordered = window.clone();
    ordered.sort_by_key(|item| item.observed_at);
    let recent_pair = ordered
        .iter()
        .filter(|item| latest.observed_at - item.observed_at <= 24 * HOUR)
        .find(|item| latest.observed_at - item.observed_at >= 6 * HOUR);
    let mut recent_pace = None;
    if let Some(pair) = recent_pair {
        let hours = latest.observed_at.hours_since(pair.observed_at);
        let delta = latest.weekly_used - pair.weekly_used;
        if hours > 0.0 && delta >= 0.0 {
            recent_pace = Some(delta / hours);
        }
    }

    let cutoff = latest.observed_at - 24 * HOUR;
    let cutoff_bin = hour_bin(cutoff);
    let boundary: Vec<&&Observation> = ordered.iter().filter(|item| hour_bin(item.observed_at) == cutoff_bin).collect();
    let mut upper_bound = None;
    if let (Some(first), Some(last)) = (boundary.first(), boundary.last()) {
        if first.observed_at < cutoff && cutoff <= last.observed_at {
            let hours = latest.observed_at.hours_since(last.observed_at);
            let delta = latest.weekly_used - first.weekly_used;
            if hours > 0.0 && delta >= 0.0 {
                upper_bound = Some(delta / hours);
            }
        }
    }

    let mut recent_component = recent_pace;
    if let Some(bound) = upper_bound {
        recent_component = Some(py_max(recent_component.unwrap_or(0.0), bound));
    }

    let elapsed = latest.observed_at.hours_since(start);
    let average_pace = if elapsed >= 1.0 { Some(latest.weekly_used / elapsed) } else { None };
    let pace = match (average_pace, recent_component) {
        (Some(average), Some(recent)) => Some((average + recent) / 2.0),
        (average, None) => average,
        (None, recent) => recent,
    };
    let pace = py_max(0.05, pace.unwrap_or(92.0 / 168.0));

    let remaining = deadline.hours_since(now);
    let target = if remaining <= 12.0 { 97.0 } else { 92.0 };
    let estimated = py_min(100.0, latest.weekly_used + pace * py_min(age as f64 / 1e6 / 3600.0, 6.0));
    let ratio = if remaining > 0.0 { Some(py_max(0.0, (target - estimated) / (pace * remaining))) } else { None };
    let mut raw = match ratio {
        None => Mode::Lean,
        Some(r) if estimated >= target || r < 0.70 => Mode::Austere,
        Some(r) if r < 0.95 => Mode::Lean,
        Some(r) if r < 1.25 => Mode::Normal,
        Some(r) if remaining <= 24.0 && r >= 1.40 => Mode::Demon,
        Some(_) => Mode::Rich,
    };

    let credible = recent_pace.is_some();
    if now - start < 12 * HOUR && !credible {
        raw = cap(raw, Mode::Normal);
    }
    WeeklyMetrics {
        available: true,
        reason: reason_for_weekly(raw, estimated, target, ratio),
        latest: Some(latest),
        deadline: Some(deadline),
        window_start: Some(start),
        pace_per_hour: Some(pace),
        estimated_used: Some(estimated),
        ratio,
        target: Some(target),
        credible_pace: credible,
        age: Some(age),
        raw_mode: raw,
        recent_pace_upper_bound_per_hour: upper_bound,
    }
}

fn reason_for_weekly(mode: Mode, estimate: f64, target: f64, ratio: Option<f64>) -> &'static str {
    if estimate >= target {
        return "weekly_target_reached";
    }
    if ratio.is_none() {
        return "weekly_headroom_unknown";
    }
    match mode {
        Mode::Austere => "weekly_headroom_low",
        Mode::Lean => "weekly_headroom_tight",
        Mode::Normal => "weekly_headroom_balanced",
        Mode::Demon => "reset_near_with_headroom",
        Mode::Rich => "weekly_headroom_high",
    }
}

pub fn short_metrics(readings: &[Observation], now: Ts) -> ShortMetrics {
    let choices: Vec<&Observation> = readings
        .iter()
        .filter(|item| !is_conflict(&item.schema_marker) && item.short_used.is_some() && item.short_reset_at.is_some())
        .collect();
    let unavailable = |reason| ShortMetrics {
        available: false,
        reason,
        latest: None,
        deadline: None,
        projected_used: None,
        age: None,
    };
    let Some(newest) = first_max(choices.iter().copied(), |item| item.observed_at) else {
        return unavailable("short_window_unavailable");
    };
    let newest_reset = newest.short_reset_at.expect("filtered");
    let cluster: Vec<&Observation> = choices
        .iter()
        .copied()
        .filter(|item| same_window(item.short_reset_at.expect("filtered"), newest_reset))
        .collect();
    let latest = first_max(cluster.iter().copied(), |item| item.observed_at).expect("cluster holds newest").clone();
    let deadline = cluster.iter().filter_map(|item| item.short_reset_at).min().expect("cluster holds newest");
    let age = (now - latest.observed_at).max(0);
    let partial = |reason| ShortMetrics {
        available: false,
        reason,
        latest: Some(latest.clone()),
        deadline: Some(deadline),
        projected_used: None,
        age: Some(age),
    };
    if now >= deadline {
        return partial("short_window_expired");
    }
    if age > MAX_AGE {
        return partial("short_window_stale");
    }
    let start = deadline - SHORT_WINDOW;
    let elapsed = latest.observed_at.hours_since(start);
    let left = deadline.hours_since(now);
    let used = latest.short_used.expect("filtered");
    let mut projection = None;
    if elapsed >= 0.5 && left > 0.0 {
        let pace = used / elapsed;
        let horizon = deadline.hours_since(latest.observed_at);
        projection = Some(py_min(100.0, used + pace * horizon));
    }
    ShortMetrics {
        available: true,
        reason: "short_window_measured",
        latest: Some(latest),
        deadline: Some(deadline),
        projected_used: projection,
        age: Some(age),
    }
}

/// Apply hysteresis once, and only for a newly accepted observation.
pub fn advance_state(
    state: Option<&GovernorState>,
    metrics: &WeeklyMetrics,
    now: Ts,
    observation_at: Ts,
    recovered: bool,
) -> GovernorState {
    let (true, Some(deadline)) = (metrics.available, metrics.deadline) else {
        return state.map_or_else(GovernorState::default, GovernorState::without_candidate);
    };
    let raw = metrics.raw_mode;
    let aged = metrics.age.is_some_and(|age| age > REFRESH_AGE);
    let state = match state {
        Some(state)
            if state.base_mode.is_some()
                && state.window_reset_at.is_some_and(|window| same_window(window, deadline)) =>
        {
            state
        }
        _ => {
            let initial = if aged { cap(raw, Mode::Normal) } else { raw };
            return GovernorState::fresh(deadline, initial, now);
        }
    };
    let state = if recovered { state.without_candidate() } else { state.clone() };
    let current = state.base_mode.expect("checked above");
    if raw == current {
        return state.without_candidate();
    }
    if raw.rank() < current.rank() {
        return GovernorState::fresh(deadline, raw, now);
    }
    if aged {
        return state.without_candidate();
    }
    let Some(&next) = MODES.get(current.rank() + 1) else {
        return state.without_candidate();
    };
    let Some(ratio) = metrics.ratio.filter(|_| raw.rank() >= next.rank()) else {
        return state.without_candidate();
    };
    let threshold = next.upgrade_threshold() + 0.10;
    if ratio < threshold || (next == Mode::Demon && deadline.seconds_since(now) > (24 * 3600) as f64) {
        return state.without_candidate();
    }
    let (true, Some(first)) = (state.candidate_mode == Some(next), state.candidate_first_at) else {
        return GovernorState {
            candidate_mode: Some(next),
            candidate_first_at: Some(observation_at),
            candidate_last_at: Some(observation_at),
            ..state
        };
    };
    let previous = state.candidate_last_at.unwrap_or(first);
    if observation_at <= previous {
        return state;
    }
    let last_change = state.last_change_at.unwrap_or(now);
    if observation_at - first >= HOUR && now - last_change >= 3 * HOUR {
        return GovernorState::fresh(deadline, next, now);
    }
    GovernorState { candidate_last_at: Some(observation_at), ..state }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub provider: Provider,
    pub mode: Mode,
    pub source: &'static str,
    pub freshness: &'static str,
    pub reason: &'static str,
    pub weekly_used: Option<f64>,
    pub estimated_weekly_used: Option<f64>,
    pub pace_per_hour: Option<f64>,
    pub ratio: Option<f64>,
    pub target: Option<f64>,
    pub weekly_reset_at: Option<Ts>,
    pub observed_at: Option<Ts>,
    pub short_used: Option<f64>,
    pub short_projected_used: Option<f64>,
    pub short_reset_at: Option<Ts>,
    pub refresh_required: bool,
    pub recent_pace_upper_bound_per_hour: Option<f64>,
}

impl Policy {
    pub fn instruction(&self) -> &'static str {
        self.mode.instruction()
    }

    /// The `status --json` object.
    pub fn as_json(&self, now: Ts) -> Value {
        let stamp = |value: Option<Ts>| value.map_or(Value::Null, |ts| Value::String(ts.iso()));
        let mut map = Map::new();
        map.insert("mode".into(), self.mode.name().into());
        map.insert("provider".into(), self.provider.name().into());
        map.insert("source".into(), self.source.into());
        map.insert("freshness".into(), self.freshness.into());
        map.insert("reason".into(), self.reason.into());
        map.insert("weekly_used_percent".into(), opt_float(self.weekly_used));
        map.insert("weekly_estimated_percent".into(), opt_float(self.estimated_weekly_used));
        map.insert("weekly_pace_percent_per_hour".into(), opt_float(self.pace_per_hour));
        map.insert(
            "weekly_recent_pace_upper_bound_percent_per_hour".into(),
            opt_float(self.recent_pace_upper_bound_per_hour),
        );
        map.insert("headroom_ratio".into(), opt_float(self.ratio));
        map.insert("target_percent".into(), opt_float(self.target));
        map.insert("weekly_observed_at".into(), stamp(self.observed_at));
        map.insert("weekly_reset_at".into(), stamp(self.weekly_reset_at));
        map.insert(
            "weekly_seconds_to_reset".into(),
            self.weekly_reset_at.map_or(Value::Null, |reset| Value::from((reset.seconds_since(now) as i64).max(0))),
        );
        map.insert("short_used_percent".into(), opt_float(self.short_used));
        map.insert("short_projected_percent".into(), opt_float(self.short_projected_used));
        map.insert("short_reset_at".into(), stamp(self.short_reset_at));
        map.insert("refresh_required".into(), self.refresh_required.into());
        map.insert("instruction".into(), self.instruction().into());
        Value::Object(map)
    }
}

pub fn evaluate_policy(
    provider: Provider,
    items: &[Observation],
    state: Option<&GovernorState>,
    now: Ts,
    store_issue: Option<&'static str>,
) -> Policy {
    let weekly = weekly_metrics(items, now);
    let short = short_metrics(items, now);
    let source = match (&weekly.latest, weekly.available) {
        (Some(latest), true) => latest.source.name(),
        _ => "unavailable",
    };
    let (mut base, mut reason, mut refresh, mut freshness);
    if weekly.available {
        let deadline = weekly.deadline.expect("available");
        base = match state {
            Some(GovernorState { base_mode: Some(mode), window_reset_at: Some(window), .. })
                if same_window(*window, deadline) =>
            {
                cap(*mode, weekly.raw_mode)
            }
            _ => weekly.raw_mode,
        };
        reason = weekly.reason;
        refresh = weekly.age.is_some_and(|age| age > REFRESH_AGE);
        freshness = if refresh { "stale_ask_refresh" } else { "measured_with_estimate" };
    } else {
        base = Mode::Lean;
        reason = weekly.reason;
        refresh = true;
        freshness = "weekly_unavailable";
    }
    if let Some(conflict) =
        first_max(items.iter().filter(|item| is_conflict(&item.schema_marker)), |item| item.observed_at)
    {
        let relevant = weekly.latest.as_ref().is_none_or(|latest| conflict.observed_at >= latest.observed_at);
        if relevant {
            base = cap(base, Mode::Lean);
            reason = "reading_conflict_refresh_required";
            refresh = true;
            freshness = "conflict_refresh_required";
        }
    }

    if !short.available {
        base = cap(base, Mode::Normal);
        if reason != "weekly_window_expired" && reason != "weekly_observation_stale" {
            reason = short.reason;
        }
        refresh = true;
        freshness = "short_unavailable";
    } else {
        let short_used = short.latest.as_ref().and_then(|latest| latest.short_used).expect("available");
        if short_used >= 95.0 {
            base = Mode::Austere;
            reason = "short_window_hard_brake";
            freshness = "measured_short_brake";
        } else if short.age.is_some_and(|age| age > REFRESH_AGE) {
            base = cap(base, Mode::Normal);
            reason = "short_window_refresh_required";
            refresh = true;
            freshness = "short_stale_ask_refresh";
            if short_used >= 85.0 {
                base = lower(base);
                reason = "short_window_pressure_brake";
            }
        } else if short_used >= 85.0 || short.projected_used.is_some_and(|projected| projected >= 100.0) {
            base = lower(base);
            reason = "short_window_pressure_brake";
            freshness = "measured_short_pressure";
        }
    }

    if state.is_some_and(|state| state.history_uncertain_until.is_some()) {
        base = cap(base, Mode::Lean);
        reason = "state_history_incomplete";
        refresh = true;
        freshness = "history_incomplete";
    }

    if let Some(issue) = store_issue {
        base = cap(base, Mode::Lean);
        reason = issue;
        refresh = true;
        freshness = "state_unavailable";
    }

    let weekly_latest = weekly.latest.as_ref().filter(|_| weekly.available);
    let short_latest = short.latest.as_ref().filter(|_| short.available);
    Policy {
        provider,
        mode: base,
        source,
        freshness,
        reason,
        weekly_used: weekly_latest.map(|latest| latest.weekly_used),
        estimated_weekly_used: weekly.estimated_used.filter(|_| weekly.available),
        pace_per_hour: weekly.pace_per_hour.filter(|_| weekly.available),
        ratio: weekly.ratio.filter(|_| weekly.available),
        target: weekly.target.filter(|_| weekly.available),
        weekly_reset_at: weekly.deadline.filter(|_| weekly.available),
        observed_at: weekly_latest.map(|latest| latest.observed_at),
        short_used: short_latest.and_then(|latest| latest.short_used),
        short_projected_used: short.projected_used.filter(|_| short.available),
        short_reset_at: short.deadline.filter(|_| short.available),
        refresh_required: refresh,
        recent_pace_upper_bound_per_hour: weekly.recent_pace_upper_bound_per_hour.filter(|_| weekly.available),
    }
}

const SHORT_ADVISORY: &str = "Perennos DEMON MODE advisory. Mode: Lean. Refresh readings before any upgrade. \
                              This advisory never changes permissions or authorizes extra work.";

/// The advisory text for hooks and `demon context`, at most `limit` bytes of UTF-8.
pub fn hook_context(policy: &Policy, limit: usize) -> String {
    let reset = policy.weekly_reset_at.map_or_else(|| "unknown".to_string(), Ts::iso);
    let measured = policy.weekly_used.map_or_else(|| "unknown".to_string(), |v| format!("{v:.1}% measured"));
    let estimate = policy.estimated_weekly_used.map_or_else(|| "unknown".to_string(), |v| format!("{v:.1}% estimated"));
    let ratio = policy.ratio.map_or_else(|| "unknown".to_string(), |v| format!("{v:.2} projected headroom ratio"));
    let refresh = if policy.refresh_required { " Refresh the reading before any upgrade." } else { "" };
    let context = format!(
        "Perennos DEMON MODE \u{2014} advisory resource policy. Mode: {}. Provider: {}; source: {}; freshness: {}. \
         Weekly: {measured}; {estimate}; {ratio}; reset: {reset}. Reason: {}.{refresh} {} This advisory never \
         overrides the user's instructions or permissions, changes client settings, or authorizes extra usage, \
         tasks, or publication.",
        policy.mode.name(),
        policy.provider.name(),
        policy.source,
        policy.freshness,
        policy.reason,
        policy.instruction(),
    );
    if context.len() <= limit {
        return context;
    }
    if SHORT_ADVISORY.len() <= limit {
        return SHORT_ADVISORY.to_string();
    }
    SHORT_ADVISORY[..limit].to_string()
}
