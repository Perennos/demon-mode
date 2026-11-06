//! Ported from tests/test_reliability.py: synthetic regressions for real integration boundaries.
mod common;

use common::*;
use demon_mode::governor::{
    Mode, Provider, Val, evaluate_policy, hook_context, parse_percent, parse_timestamp, short_metrics,
};
use demon_mode::sources::{app_server_observation, codex_observation};
use demon_mode::store::{load_history, load_policy};
use demon_mode::ts::Ts;
use serde_json::{Value, json};
use std::time::Instant;

fn event(at: Ts, used: Value, extra: Option<(&str, Value)>) -> Value {
    let mut limits = json!({
        "primary": {"used_percent": 12, "window_minutes": 300, "resets_at": (NOW + 4 * HOUR).timestamp()},
        "secondary": {"used_percent": used, "window_minutes": 10080, "resets_at": (NOW + 100 * HOUR).timestamp()},
    });
    if let Some((key, value)) = extra {
        limits[key] = value;
    }
    json!({"timestamp": at.iso(), "type": "event_msg", "payload": {"type": "token_count", "rate_limits": limits}})
}

fn snapshot() -> Value {
    json!({"limitId": "codex",
        "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": (NOW + 4 * HOUR).timestamp()},
        "secondary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": (NOW + 100 * HOUR).timestamp()}})
}

fn write_lines(path: &std::path::Path, events: &[Value]) {
    let text: String = events.iter().map(|event| format!("{event}\n")).collect();
    std::fs::write(path, text).unwrap();
}

#[test]
fn hook_configuration_preserves_absolute_database_and_native_shape() {
    let temp = TempDir::new();
    let db = temp.db("ledger with spaces.sqlite3");
    let run = invoke(&["--db", &db, "hook-config", "--client", "codex"]);
    let config = json(&run.out);
    assert!(config["argv"].as_array().unwrap().contains(&Value::String(db.clone())));
    let handler = &config["config"]["hooks"]["UserPromptSubmit"][0]["hooks"][0];
    assert_eq!(handler["command"], config["command"]);
    assert_eq!(handler["type"], "command");
    assert!(handler["timeout"].as_i64().unwrap() <= 10);
}

#[test]
fn relative_database_paths_become_absolute_in_templates() {
    let run = invoke(&["--db", "./demo.sqlite3", "hook-config", "--client", "claude", "--format", "native"]);
    let args = &json(&run.out)["hooks"]["UserPromptSubmit"][0]["hooks"][0]["args"];
    let expected = std::env::current_dir().unwrap().join("demo.sqlite3");
    assert_eq!(args[1], expected.to_string_lossy().as_ref());
    let run = invoke(&["--db", ":memory:", "hook-config", "--client", "claude"]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: hook configuration requires a persistent database\n"));
    let run = invoke(&["hook-config", "--client", "claude", "--codex-file", "x.jsonl"]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: --codex-file requires --client codex\n"));
}

#[test]
fn native_config_is_a_config_only_object() {
    let run = invoke(&["hook-config", "--client", "codex", "--format", "native"]);
    assert_eq!(run.code, 0);
    let keys: Vec<String> = json(&run.out).as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["hooks"]);
}

#[test]
fn explicit_log_refresh_happens_before_prompt_blind_hook() {
    let temp = TempDir::new();
    let path = temp.path().join("selected.jsonl");
    write_lines(&path, &[event(NOW, json!(18), None)]);
    let db = temp.db("state.sqlite3");
    let run = invoke_stdin(
        &["--db", &db, "hook", "--client", "codex", "--codex-file", path.to_str().unwrap()],
        br#"{"prompt":"SYNTHETIC_PRIVATE_TEXT"}"#,
    );
    assert_eq!((run.code, run.err.as_str(), run.stdin_reads), (0, "", 0));
    assert_eq!(load_policy(Some(&db), Provider::Codex, NOW).weekly_used, Some(18.0));
    assert!(run.out.contains("18.0% measured"));
    assert!(!run.out.contains("SYNTHETIC_PRIVATE_TEXT"));
}

#[test]
fn failed_hook_refresh_caps_existing_demon_policy() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "codex", NOW, 0.2, NOW + 20 * HOUR, Some((5.0, NOW + 4 * HOUR)), NOW);
    let missing = temp.path().join("missing.jsonl");
    let run = invoke(&["--db", &db, "hook", "--client", "codex", "--codex-file", missing.to_str().unwrap()]);
    assert_eq!((run.code, run.err.as_str()), (0, ""));
    assert!(run.out.contains("Mode: Lean"));
    assert!(run.out.contains("input_refresh_unavailable"));
    assert_eq!(load_history(Some(&db), Provider::Codex, 20, NOW).unwrap().len(), 1);
    let run = invoke(&["hook", "--client", "claude", "--codex-file", missing.to_str().unwrap()]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: --codex-file requires --client codex\n"));
}

#[test]
fn log_reader_uses_newest_measurement_after_old_missing_or_invalid_event() {
    let temp = TempDir::new();
    let path = temp.path().join("selected.jsonl");
    let mut older = event(NOW - HOUR, json!(18), None);
    older["payload"]["rate_limits"] = Value::Null;
    let bad = event(NOW - 30 * MINUTE, json!("unsupported"), None);
    write_lines(&path, &[older, bad, event(NOW, json!(18), None)]);
    let found = codex_observation(path.to_str().unwrap(), NOW).unwrap();
    assert_eq!((found.weekly_used, found.observed_at), (18.0, NOW));
}

#[test]
fn log_reader_rejects_conflicting_latest_events() {
    let temp = TempDir::new();
    let path = temp.path().join("selected.jsonl");
    write_lines(&path, &[event(NOW, json!(18), None), event(NOW, json!(75), None)]);
    assert!(codex_observation(path.to_str().unwrap(), NOW).is_err());
}

#[test]
fn old_malformed_container_or_timestamp_cannot_hide_newer_event() {
    for problem in ["container", "timestamp"] {
        let temp = TempDir::new();
        let mut old = event(NOW - HOUR, json!(18), None);
        if problem == "container" {
            old["payload"]["rate_limits"] = json!([]);
        } else {
            old["timestamp"] = json!("invalid");
        }
        let path = temp.path().join("selected.jsonl");
        write_lines(&path, &[old, event(NOW, json!(18), None)]);
        assert_eq!(codex_observation(path.to_str().unwrap(), NOW).unwrap().weekly_used, 18.0, "{problem}");
    }
}

#[test]
fn newest_malformed_container_or_timestamp_does_not_revive_old_reading() {
    for problem in ["container", "timestamp"] {
        let temp = TempDir::new();
        let mut newest = event(NOW, json!(18), None);
        if problem == "container" {
            newest["payload"]["rate_limits"] = json!([]);
        } else {
            newest["timestamp"] = json!("invalid");
        }
        let path = temp.path().join("selected.jsonl");
        write_lines(&path, &[event(NOW - HOUR, json!(18), None), newest]);
        assert!(codex_observation(path.to_str().unwrap(), NOW).is_err(), "{problem}");
    }
}

#[test]
fn log_reader_does_not_use_other_metered_bucket() {
    let temp = TempDir::new();
    let path = temp.path().join("selected.jsonl");
    write_lines(&path, &[event(NOW, json!(18), Some(("limit_id", json!("codex_other"))))]);
    assert!(codex_observation(path.to_str().unwrap(), NOW).is_err());
}

#[test]
fn app_server_response_and_notification_import_numeric_fields_only() {
    for wrapper in ["response", "notification", "export"] {
        let temp = TempDir::new();
        let mut value = json!({"rateLimits": snapshot(), "rateLimitsByLimitId": {
            "codex": snapshot(), "codex_other": {"limitId": "codex_other"},
        }, "ignored": "SYNTHETIC_PRIVATE_TEXT"});
        if wrapper == "response" {
            value = json!({"id": 6, "result": value});
        } else if wrapper == "notification" {
            value = json!({"method": "account/rateLimits/updated", "params": value});
        }
        let db = temp.db("state.sqlite3");
        let run = invoke_stdin(
            &["--db", &db, "ingest", "codex-app-server", "--stdin", "--at", &NOW.iso()],
            value.to_string().as_bytes(),
        );
        assert_eq!(run.code, 0, "{wrapper}: {}", run.err);
        let policy = load_policy(Some(&db), Provider::Codex, NOW);
        assert_eq!(
            (policy.weekly_used, policy.short_used, policy.source),
            (Some(18.0), Some(12.0), "codex_app_server")
        );
        assert!(!run.out.contains("SYNTHETIC_PRIVATE_TEXT"));
        assert!(!String::from_utf8_lossy(&std::fs::read(&db).unwrap()).contains("SYNTHETIC_PRIVATE_TEXT"));
    }
}

#[test]
fn app_server_import_preserves_original_observation_time() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let observed = NOW - 2 * HOUR;
    let mut value = json!({"result": {"rateLimits": snapshot()}});
    value["result"]["rateLimits"]["primary"]["resetsAt"] = json!((observed + 4 * HOUR).timestamp());
    let run = invoke_stdin(
        &["--db", &db, "ingest", "codex-app-server", "--stdin", "--at", &observed.iso()],
        value.to_string().as_bytes(),
    );
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(load_history(Some(&db), Provider::Codex, 20, NOW).unwrap()[0].observed_at, observed);
    assert!(load_policy(Some(&db), Provider::Codex, NOW).refresh_required);
}

#[test]
fn app_server_requires_original_time_and_one_input() {
    assert_eq!(invoke(&["ingest", "codex-app-server", "--stdin"]).code, 2);
    assert_eq!(invoke(&["ingest", "codex-app-server", "--at", &NOW.iso()]).code, 2);
    assert_eq!(invoke(&["ingest", "codex-app-server", "--stdin", "--file", "x", "--at", &NOW.iso()]).code, 2);
}

#[test]
fn app_server_rejects_ambiguous_or_unsupported_limits_without_mutation() {
    let mut wrong = snapshot();
    wrong["secondary"]["windowDurationMins"] = json!(15);
    let invalid = [
        json!({"rateLimits": wrong}),
        json!({"rateLimitsByLimitId": {"codex_other": snapshot()}}),
        json!({"rateLimits": snapshot(), "rateLimitsByLimitId": {"codex": {}}}),
    ];
    for data in invalid {
        let temp = TempDir::new();
        let db = temp.db("state.sqlite3");
        let run = invoke_stdin(
            &["--db", &db, "ingest", "codex-app-server", "--stdin", "--at", &NOW.iso()],
            data.to_string().as_bytes(),
        );
        assert_eq!(run.code, 2, "{data}");
        assert!(!std::path::Path::new(&db).exists());
    }
}

#[test]
fn hook_context_terminates_and_obeys_tiny_utf8_limits() {
    let policy = evaluate_policy(Provider::Codex, &[], None, NOW, None);
    for limit in [0, 1, 20, 100, 700, 2048] {
        assert!(hook_context(&policy, limit).len() <= limit, "{limit}");
    }
}

#[test]
fn deep_json_returns_quiet_input_error() {
    let deep = format!("{}0{}", "[".repeat(1500), "]".repeat(1500));
    let run = invoke_stdin(&["record", "--stdin-json"], deep.as_bytes());
    assert_eq!((run.code, run.out.as_str()), (2, ""));
    assert!(!run.err.contains("panicked"));
    assert_eq!(run.err, "demon: input must be valid UTF-8 JSON\n");
}

#[test]
fn numeric_overflow_is_a_validation_error() {
    assert!(parse_percent(Some(&Val::Num(1e308 * 10.0))).is_err());
    assert!(parse_timestamp(Some(&Val::Num(1e308 * 10.0))).is_err());
    let temp = TempDir::new();
    let body = json!({"provider": "claude", "observed_at": NOW.iso(), "weekly_used": 1e300,
                      "weekly_reset_at": (NOW + HOUR).iso()});
    let run = invoke_stdin(&["--db", &temp.db("s.sqlite3"), "record"], body.to_string().as_bytes());
    assert_eq!((run.code, run.err.as_str()), (2, "demon: invalid percentage\n"));
}

#[test]
fn plain_status_explains_unknown_readings() {
    let temp = TempDir::new();
    let run = invoke(&["--db", &temp.db("empty.sqlite3"), "status", "--provider", "codex"]);
    assert_eq!(run.code, 0);
    assert_eq!(
        run.out,
        "LEAN [----------] weekly=unknown estimated=unknown headroom=unknown reset=unknown five_hour=unknown \
         reason=short_window_unavailable refresh=required\n"
    );
}

#[test]
fn short_projection_includes_age_since_measurement() {
    let observed = NOW - 30 * MINUTE;
    let reading = demon_mode::governor::normalize_observation(
        &reading("codex", observed, 0.2, NOW + 20 * HOUR, Some((31.0, NOW + 3 * HOUR))),
        demon_mode::governor::Source::Manual,
        NOW,
    )
    .unwrap();
    let short = short_metrics(std::slice::from_ref(&reading), NOW);
    assert_eq!(short.projected_used, Some(100.0));
    assert_eq!(evaluate_policy(Provider::Codex, &[reading], None, NOW, None).reason, "short_window_pressure_brake");
}

#[test]
fn backfilled_decrease_invalidates_headroom_until_new_measurement() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let reset = NOW + 20 * HOUR;
    record(&db, "codex", NOW, 20.0, reset, Some((5.0, NOW + 4 * HOUR)), NOW);
    let policy = record(&db, "codex", NOW - 30 * MINUTE, 99.0, reset, None, NOW);
    assert!(matches!(policy.mode, Mode::Austere | Mode::Lean));
    assert!(policy.refresh_required);
    let policy = load_policy(Some(&db), Provider::Codex, NOW);
    assert!(matches!(policy.mode, Mode::Austere | Mode::Lean));
    assert!(policy.refresh_required);
}

#[test]
fn short_conflict_keeps_hard_brake_across_weekly_reset() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "codex", NOW - 2 * MINUTE, 90.0, NOW - MINUTE, Some((96.0, NOW + 3 * HOUR)), NOW);
    let policy = record(&db, "codex", NOW, 1.0, NOW + 168 * HOUR, Some((1.0, NOW + 3 * HOUR)), NOW);
    assert_eq!((policy.mode, policy.short_used), (Mode::Austere, Some(96.0)));
}

#[test]
fn fractional_timestamps_sort_chronologically_in_history() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let reset = NOW + 100 * HOUR;
    for (at, used) in [(NOW, 20.0), (NOW + 100_000, 21.0)] {
        record(&db, "codex", at, used, reset, None, at);
    }
    let rows = load_history(Some(&db), Provider::Codex, 20, NOW + SECOND).unwrap();
    assert_eq!(rows.iter().map(|item| item.weekly_used).collect::<Vec<_>>(), [21.0, 20.0]);
}

#[test]
fn context_command_prints_pasteable_advice_without_hook_json() {
    let temp = TempDir::new();
    let run = invoke(&["--db", &temp.db("empty.sqlite3"), "context", "--provider", "codex"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("advisory resource policy"));
    assert!(run.out.contains("Mode: Lean"));
    assert!(!run.out.contains("hookSpecificOutput"));
}

#[test]
fn doctor_explains_missing_readings_and_reports_no_host_verification() {
    let temp = TempDir::new();
    let db = temp.db("empty.sqlite3");
    let run = invoke(&["--db", &db, "doctor", "--provider", "codex", "--json"]);
    assert_eq!(run.code, 0, "{}", run.err);
    let result = json(&run.out);
    assert_eq!(result["refresh_required"], true);
    assert_eq!(result["host_integration"], "Untested");
    assert!(!result["next_steps"].as_array().unwrap().is_empty());
    assert_eq!(result["version"], demon_mode::VERSION);
    assert_eq!(result["ledger_present"], false);
    assert_eq!(result["executable"], EXE);
    assert!(result["tempo"].as_str().unwrap().contains("No network check"));
    let run = invoke(&["--db", &db, "doctor", "--provider", "codex"]);
    assert!(run.out.starts_with(&format!("Demon Mode {}; codex\nLEAN [", demon_mode::VERSION)));
    assert!(run.out.contains("Native host delivery: Untested."));
}

#[test]
fn app_server_file_accepts_utf8_bom_and_json_output() {
    let temp = TempDir::new();
    let path = temp.path().join("export.json");
    std::fs::write(&path, format!("\u{feff}{}", json!({"result": {"rateLimits": snapshot()}}))).unwrap();
    let run = invoke(&[
        "--db",
        &temp.db("state.sqlite3"),
        "ingest",
        "codex-app-server",
        "--file",
        path.to_str().unwrap(),
        "--at",
        &NOW.iso(),
        "--json",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(json(&run.out)["weekly_used_percent"], 18.0);
}

#[test]
fn app_server_duplicate_permission_key_is_rejected_without_state_mutation() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let rest = json!({"rateLimits": snapshot()}).to_string();
    let payload = format!(r#"{{"ordinaryUsageAllowed":false,"ordinaryUsageAllowed":true,{}"#, &rest[1..]);
    let run =
        invoke_stdin(&["--db", &db, "ingest", "codex-app-server", "--stdin", "--at", &NOW.iso()], payload.as_bytes());
    assert_eq!((run.code, run.out.as_str()), (2, ""));
    assert!(!std::path::Path::new(&db).exists());
}

#[test]
fn claude_native_hook_uses_shell_free_argument_vector() {
    let temp = TempDir::new();
    let db = temp.db("ledger's $literal.sqlite3");
    let run = invoke(&["--db", &db, "hook-config", "--client", "claude", "--format", "native"]);
    assert_eq!(run.code, 0, "{}", run.err);
    let handler = &json(&run.out)["hooks"]["UserPromptSubmit"][0]["hooks"][0];
    assert_eq!(handler["command"], EXE);
    assert!(handler["args"].as_array().unwrap().contains(&Value::String(db)));
}

#[test]
fn app_server_does_not_infer_permission_from_low_percentages() {
    let cases = [
        ("ordinaryUsageAllowed", json!(false)),
        ("ordinaryUsageAllowed", Value::Null),
        ("ordinaryUsageAllowed", json!("true")),
        ("ordinaryUsageAllowed", json!(1)),
        ("ordinaryUsageAllowed", json!(1.0)),
        ("ordinaryUsageAllowed", json!(0.0)),
        ("spendControlReached", json!(true)),
        ("rateLimitReachedType", json!("workspace_owner_usage_limit_reached")),
    ];
    for (field, value) in cases {
        let mut data = json!({"rateLimits": snapshot()});
        if field == "ordinaryUsageAllowed" {
            data[field] = value.clone();
        } else {
            data["rateLimits"][field] = value.clone();
        }
        assert!(app_server_observation(&data, Some(Val::Time(NOW)), NOW).is_err(), "{field}={value}");
    }
}

#[test]
fn app_server_rejects_minimal_explicit_null_permission() {
    assert!(app_server_observation(&json!({"ordinaryUsageAllowed": null}), Some(Val::Time(NOW)), NOW).is_err());
}

#[test]
fn app_server_allows_missing_or_explicitly_true_usage_permission() {
    for data in [json!({"rateLimits": snapshot()}), json!({"ordinaryUsageAllowed": true, "rateLimits": snapshot()})] {
        assert_eq!(app_server_observation(&data, Some(Val::Time(NOW)), NOW).unwrap().weekly_used, 18.0);
    }
}

#[test]
fn hook_refresh_lock_wait_is_bounded() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    load_policy(Some(&db), Provider::Codex, NOW);
    let path = temp.path().join("selected.jsonl");
    write_lines(&path, &[event(NOW, json!(18), None)]);
    let lock = sql(&db);
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let start = Instant::now();
    let run = invoke(&["--db", &db, "hook", "--client", "codex", "--codex-file", path.to_str().unwrap()]);
    let elapsed = start.elapsed();
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("Mode: Lean"));
    assert!(elapsed.as_secs_f64() < 2.0, "{elapsed:?}");
}

#[test]
fn replayed_claude_statusline_keeps_original_capture_time() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let observed = NOW - 2 * HOUR;
    let data =
        json!({"rate_limits": {"seven_day": {"used_percentage": 18, "resets_at": (NOW + 100 * HOUR).timestamp()}}});
    let run = invoke_stdin(
        &["--db", &db, "ingest", "claude-statusline", "--stdin", "--at", &observed.iso(), "--json"],
        data.to_string().as_bytes(),
    );
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(load_history(Some(&db), Provider::Claude, 20, NOW).unwrap()[0].observed_at, observed);
    assert_eq!(json(&run.out)["refresh_required"], true);
}

#[test]
fn history_limit_is_validated_and_plain_output_is_numeric() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let run = invoke(&["--db", &db, "history", "--provider", "codex", "--limit", "0"]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: history limit must be between 1 and 1000\n"));
    let run = invoke(&["--db", &db, "history", "--provider", "codex"]);
    assert_eq!(run.out, "No local observations.\n");
    record(&db, "codex", NOW, 18.0, NOW + 100 * HOUR, None, NOW);
    let run = invoke(&["--db", &db, "history", "--provider", "codex"]);
    assert_eq!(
        run.out,
        format!(
            "{} weekly=18.0% weekly_reset={} five_hour=unknown five_hour_reset=unknown\n",
            NOW.iso(),
            (NOW + 100 * HOUR).iso()
        )
    );
}
