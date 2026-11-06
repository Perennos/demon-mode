//! Bounded readers for explicit local inputs. No network, no credentials, no directory scanning.
//!
//! Only numeric quota values, source identifiers and timestamps leave these functions; prompts,
//! transcripts, account fields and file paths are never retained.

use crate::governor::{Observation, Reading, Source, Val, normalize_observation, parse_timestamp};
use crate::paths;
use crate::pyjson;
use crate::ts::Ts;
use crate::{Error, Result};
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub const MAX_CODEX_TAIL_BYTES: u64 = 1024 * 1024;
pub const MAX_CODEX_LINE_BYTES: usize = 65536;
pub const MAX_STATUSLINE_BYTES: usize = 65536;
pub const MAX_MANUAL_BYTES: usize = 4096;

const BAD_JSON: &str = "input must be valid UTF-8 JSON";

/// Read at most `maximum` bytes of UTF-8 JSON (an optional BOM is allowed); duplicate keys fail.
pub fn read_json(reader: &mut dyn Read, maximum: usize) -> Result<Value> {
    let mut raw = Vec::new();
    reader.take(maximum as u64 + 1).read_to_end(&mut raw)?;
    parse_json_bytes(&raw, maximum)
}

pub fn parse_json_bytes(raw: &[u8], maximum: usize) -> Result<Value> {
    if raw.len() > maximum {
        return Err(Error::invalid("input exceeds the configured size limit"));
    }
    let text = std::str::from_utf8(raw).map_err(|_| Error::invalid(BAD_JSON))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    pyjson::parse_strict(text).map_err(|_| Error::invalid(BAD_JSON))
}

fn field(map: &Map<String, Value>, key: &str) -> Option<Val> {
    map.get(key).and_then(Val::from_json)
}

fn is_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) if number.is_i64() || number.is_u64() => number.as_i64().or(Some(i64::MAX)),
        _ => None,
    }
}

/// Claude Code status-line JSON: `rate_limits.seven_day` and optional `rate_limits.five_hour`.
pub fn statusline_observation(value: &Value, now: Ts, observed_at: Option<&str>) -> Result<Observation> {
    let limits = value
        .as_object()
        .and_then(|object| object.get("rate_limits"))
        .and_then(Value::as_object)
        .ok_or_else(|| Error::invalid("Claude statusline rate limits are unavailable"))?;
    let weekly = limits
        .get("seven_day")
        .and_then(Value::as_object)
        .filter(|weekly| weekly.contains_key("used_percentage") && weekly.contains_key("resets_at"))
        .ok_or_else(|| Error::invalid("Claude weekly rate limit is unavailable"))?;
    let (mut short_used, mut short_reset) = (None, None);
    match limits.get("five_hour") {
        None | Some(Value::Null) => {}
        Some(Value::Object(short)) if short.contains_key("used_percentage") && short.contains_key("resets_at") => {
            short_used = field(short, "used_percentage");
            short_reset = field(short, "resets_at");
        }
        Some(_) => return Err(Error::invalid("Claude five-hour rate limit is incomplete")),
    }
    let reading = Reading {
        provider: Some(Val::text("claude")),
        observed_at: Some(observed_at.map_or(Val::Time(now), Val::text)),
        weekly_used: field(weekly, "used_percentage"),
        weekly_reset_at: field(weekly, "resets_at"),
        short_used,
        short_reset_at: short_reset,
    };
    normalize_observation(&reading, Source::ClaudeStatusline, now)
}

/// One explicitly selected regular file (no links, no Windows UNC paths); `what` names it in errors.
pub fn open_selected_file(path: &str, what: &str) -> Result<File> {
    let selected = paths::expanduser(path);
    let refused = || Error::invalid(format!("{what} must be one explicitly selected regular file"));
    if cfg!(windows) && selected.to_string_lossy().starts_with(r"\\") {
        return Err(refused());
    }
    let is_link = std::fs::symlink_metadata(&selected).is_ok_and(|info| info.file_type().is_symlink());
    let is_file = std::fs::metadata(&selected).is_ok_and(|info| info.is_file());
    if is_link || !is_file {
        return Err(refused());
    }
    Ok(File::open(&selected)?)
}

/// `bytes.splitlines()`: break at `\n`, `\r\n` and `\r`.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let (mut start, mut index) = (0, 0);
    while index < data.len() {
        match data[index] {
            b'\n' => {
                lines.push(&data[start..index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                lines.push(&data[start..index]);
                index += 1;
                if data.get(index) == Some(&b'\n') {
                    index += 1;
                }
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

const SHORT_MINUTES: i64 = 300;
const WEEKLY_MINUTES: i64 = 10080;

/// The newest Codex rate-limit event near the end of one explicitly selected JSONL session file.
pub fn codex_observation(path: &str, now: Ts) -> Result<Observation> {
    let mut file = open_selected_file(path, "Codex input")?;
    let size = file.metadata()?.len();
    let offset = size.saturating_sub(MAX_CODEX_TAIL_BYTES);
    file.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.take(MAX_CODEX_TAIL_BYTES).read_to_end(&mut data)?;
    let mut data = data.as_slice();
    if offset > 0 {
        let boundary = data
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| Error::invalid("Codex tail has no complete records"))?;
        data = &data[boundary + 1..];
    }
    if !data.is_empty() && !data.ends_with(b"\n") {
        data = match data.iter().rposition(|byte| *byte == b'\n') {
            Some(boundary) => &data[..boundary + 1],
            None => &[],
        };
    }

    let mut candidates: Vec<(Ts, Value, usize)> = Vec::new();
    let mut unorderable_at: Option<usize> = None;
    for (index, line) in split_lines(data).into_iter().enumerate() {
        if line.is_empty() || line.len() > MAX_CODEX_LINE_BYTES {
            continue;
        }
        let Some(event) = std::str::from_utf8(line).ok().and_then(|text| serde_json::from_str::<Value>(text).ok())
        else {
            continue;
        };
        let Some(event) = event.as_object() else { continue };
        if event.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = event.get("payload").and_then(Value::as_object) else { continue };
        if !matches!(payload.get("type").and_then(Value::as_str), Some("token_info" | "token_count")) {
            continue;
        }
        // Token-only events are not new allowance measurements.
        let rate_limits = match payload.get("rate_limits") {
            None | Some(Value::Null) => continue,
            Some(limits) => limits,
        };
        if let Some(limits) = rate_limits.as_object() {
            match limits.get("limit_id") {
                None | Some(Value::Null) => {}
                Some(Value::String(id)) if id == "codex" => {}
                Some(_) => continue,
            }
        }
        match parse_timestamp(event.get("timestamp").and_then(Val::from_json).as_ref()) {
            Ok(observed) => candidates.push((observed, rate_limits.clone(), index)),
            Err(_) => unorderable_at = Some(index),
        }
    }
    let Some(newest) = candidates.iter().map(|(observed, _, _)| *observed).max() else {
        return Err(Error::invalid("no supported Codex rate-limit event was found"));
    };
    let latest_position = candidates.iter().filter(|(observed, _, _)| *observed == newest).map(|(_, _, i)| *i).max();
    if unorderable_at.is_some() && unorderable_at > latest_position {
        return Err(Error::invalid("Codex latest rate-limit timestamp is invalid"));
    }

    // Validate the latest measurement, not obsolete data from earlier in a session.
    // A malformed latest measurement must fail rather than revive old headroom.
    let mut observations = Vec::new();
    for (observed, rate_limits, _) in candidates.iter().filter(|(observed, _, _)| *observed == newest) {
        let limits = rate_limits
            .as_object()
            .ok_or_else(|| Error::invalid("Codex rate-limit container is missing or ambiguous"))?;
        let (mut weekly, mut short) = (Vec::new(), Vec::new());
        for bucket in limits.values() {
            let Some(bucket) = bucket.as_object() else { continue };
            let Some(minutes) = bucket.get("window_minutes").and_then(is_int) else { continue };
            if minutes != SHORT_MINUTES && minutes != WEEKLY_MINUTES {
                continue;
            }
            if !bucket.contains_key("used_percent") || !bucket.contains_key("resets_at") {
                return Err(Error::invalid("Codex rate-limit bucket is incomplete"));
            }
            if minutes == WEEKLY_MINUTES { weekly.push(bucket) } else { short.push(bucket) }
        }
        if weekly.len() != 1 || short.len() > 1 {
            return Err(Error::invalid("Codex weekly bucket is missing or rate-limit buckets are ambiguous"));
        }
        let reading = Reading {
            provider: Some(Val::text("codex")),
            observed_at: Some(Val::Time(*observed)),
            weekly_used: field(weekly[0], "used_percent"),
            weekly_reset_at: field(weekly[0], "resets_at"),
            short_used: short.first().and_then(|bucket| field(bucket, "used_percent")),
            short_reset_at: short.first().and_then(|bucket| field(bucket, "resets_at")),
        };
        observations.push(normalize_observation(&reading, Source::CodexTokenInfo, now)?);
    }
    if observations.iter().skip(1).any(|item| *item != observations[0]) {
        return Err(Error::invalid("Codex newest rate-limit readings conflict"));
    }
    Ok(observations.swap_remove(0))
}

/// A captured `account/rateLimits/read` response, an `account/rateLimits/updated` notification, or
/// the unwrapped payload. Exports carry no observation time, so the caller supplies the original one.
pub fn app_server_observation(value: &Value, observed_at: Option<Val>, now: Ts) -> Result<Observation> {
    let unavailable = || Error::invalid("Codex app-server rate-limit export is unavailable");
    let outer = value.as_object().filter(|object| !object.contains_key("error")).ok_or_else(unavailable)?;
    let inner = if let Some(result) = outer.get("result") {
        result
    } else if let Some(method) = outer.get("method") {
        if method.as_str() != Some("account/rateLimits/updated") {
            return Err(Error::invalid("unsupported Codex app-server notification"));
        }
        outer.get("params").unwrap_or(&Value::Null)
    } else {
        value
    };
    let inner = inner.as_object().ok_or_else(unavailable)?;
    if let Some(allowed) = inner.get("ordinaryUsageAllowed") {
        if *allowed != Value::Bool(true) {
            return Err(Error::invalid(
                "Codex ordinary included usage is not confirmed available; refresh provider status",
            ));
        }
    }
    let shared = || Error::invalid("Codex shared rate-limit bucket is unavailable");
    let limits = match inner.get("rateLimitsByLimitId") {
        Some(buckets) if !buckets.is_null() => {
            buckets.as_object().and_then(|buckets| buckets.get("codex")).ok_or_else(shared)?
        }
        _ => inner.get("rateLimits").unwrap_or(&Value::Null),
    };
    let limits = limits.as_object().ok_or_else(shared)?;
    match limits.get("limitId") {
        None | Some(Value::Null) => {}
        Some(Value::String(id)) if id == "codex" => {}
        Some(_) => return Err(shared()),
    }
    let spend_reached = limits.get("spendControlReached") == Some(&Value::Bool(true));
    let limit_reached = limits.get("rateLimitReachedType").is_some_and(|kind| !kind.is_null());
    if spend_reached || limit_reached {
        return Err(Error::invalid("Codex reports a reached usage or spend limit; refresh provider status"));
    }
    let incomplete = || Error::invalid("Codex app-server rate-limit bucket is incomplete");
    let (mut weekly, mut short) = (Vec::new(), Vec::new());
    for name in ["primary", "secondary"] {
        let bucket = match limits.get(name) {
            None | Some(Value::Null) => continue,
            Some(bucket) => bucket.as_object().ok_or_else(incomplete)?,
        };
        let minutes = bucket.get("windowDurationMins").and_then(is_int);
        if minutes != Some(SHORT_MINUTES) && minutes != Some(WEEKLY_MINUTES) {
            return Err(Error::invalid("unsupported Codex app-server quota window"));
        }
        if !bucket.contains_key("usedPercent") || !bucket.contains_key("resetsAt") {
            return Err(incomplete());
        }
        if minutes == Some(WEEKLY_MINUTES) { weekly.push(bucket) } else { short.push(bucket) }
    }
    if weekly.len() != 1 || short.len() > 1 {
        return Err(Error::invalid("Codex weekly bucket is missing or rate-limit buckets are ambiguous"));
    }
    let reading = Reading {
        provider: Some(Val::text("codex")),
        observed_at,
        weekly_used: field(weekly[0], "usedPercent"),
        weekly_reset_at: field(weekly[0], "resetsAt"),
        short_used: short.first().and_then(|bucket| field(bucket, "usedPercent")),
        short_reset_at: short.first().and_then(|bucket| field(bucket, "resetsAt")),
    };
    normalize_observation(&reading, Source::CodexAppServer, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_matches_python_bytes_splitlines() {
        assert_eq!(split_lines(b""), Vec::<&[u8]>::new());
        assert_eq!(split_lines(b"\n"), vec![b"" as &[u8]]);
        assert_eq!(split_lines(b"a\r\nb\rc\n\nd"), vec![b"a" as &[u8], b"b", b"c", b"", b"d"]);
    }
}
