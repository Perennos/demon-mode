//! Ported from tests/test_sources.py.
mod common;

use common::*;
use demon_mode::governor::{Mode, Provider, Source};
use demon_mode::sources::MAX_CODEX_TAIL_BYTES;
use demon_mode::store::{load_history, load_policy};
use serde_json::json;

fn unix(at: demon_mode::ts::Ts) -> f64 {
    at.timestamp()
}

#[test]
fn claude_statusline_ingest_uses_only_whitelisted_fields() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let data = json!({
        "rate_limits": {
            "seven_day": {"used_percentage": 30.0, "resets_at": unix(NOW + 100 * HOUR)},
            "five_hour": {"used_percentage": 12.0, "resets_at": unix(NOW + 3 * HOUR)},
        },
        "transcript": "SYNTHETIC_SENTINEL_NO_RETENTION",
    });
    let run = invoke_stdin(&["--db", &db, "ingest", "claude-statusline", "--stdin"], data.to_string().as_bytes());
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(!run.out.contains("SYNTHETIC_SENTINEL"));
    let policy = load_policy(Some(&db), Provider::Claude, NOW);
    assert_eq!(policy.source, "claude_statusline");
    assert_eq!((policy.weekly_used, policy.short_used), (Some(30.0), Some(12.0)));
    assert!(!String::from_utf8_lossy(&std::fs::read(&db).unwrap()).contains("SYNTHETIC_SENTINEL"));
}

#[test]
fn missing_claude_fields_do_not_refresh_or_erase_good_data() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "claude", NOW, 20.0, NOW + 100 * HOUR, None, NOW);
    let run = invoke_stdin(
        &["--db", &db, "ingest", "claude-statusline", "--stdin"],
        br#"{"rate_limits": {"five_hour": null}}"#,
    );
    assert_eq!(run.code, 2);
    assert!(run.err.contains("weekly rate limit is unavailable"));
    assert_eq!(load_policy(Some(&db), Provider::Claude, NOW).weekly_used, Some(20.0));
    assert_eq!(load_history(Some(&db), Provider::Claude, 20, NOW).unwrap().len(), 1);
}

#[test]
fn claude_statusline_requires_the_stdin_flag() {
    assert_eq!(invoke(&["ingest", "claude-statusline"]).code, 2);
}

#[test]
fn codex_explicit_jsonl_uses_window_minutes_not_bucket_position() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let path = temp.path().join("one-session.jsonl");
    let event = json!({
        "timestamp": NOW.iso(), "type": "event_msg",
        "payload": {"type": "token_info", "rate_limits": {
            "primary": {"used_percent": 44, "window_minutes": 300, "resets_at": unix(NOW + 4 * HOUR)},
            "secondary": {"used_percent": 18, "window_minutes": 10080, "resets_at": unix(NOW + 100 * HOUR)},
        }},
    });
    std::fs::write(&path, format!("{event}\n")).unwrap();
    let path_text = path.to_string_lossy().into_owned();
    let run = invoke(&["--db", &db, "ingest", "codex", "--file", &path_text]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(!run.out.contains(&path_text));
    let policy = load_policy(Some(&db), Provider::Codex, NOW);
    assert_eq!(policy.source, "codex_token_info");
    assert_eq!((policy.weekly_used, policy.short_used), (Some(18.0), Some(44.0)));
    assert!(!String::from_utf8_lossy(&std::fs::read(&db).unwrap()).contains(&path_text));
}

#[test]
fn codex_token_info_and_token_count_event_names() {
    let observed = NOW - 2 * HOUR;
    let weekly_reset = observed + 100 * HOUR;
    let temp = TempDir::new();
    for (label, event_name, duplicate_weekly) in [
        ("token_info", "token_info", false),
        ("token_count", "token_count", false),
        ("unsupported", "synthetic_other_event", false),
        ("duplicate_weekly", "token_count", true),
    ] {
        let db = temp.db(&format!("{label}.sqlite3"));
        let path = temp.path().join(format!("{label}.jsonl"));
        let mut rate_limits = json!({
            "primary": {"used_percent": 18.0, "window_minutes": 10080, "resets_at": unix(weekly_reset)},
            "credits": {"used_percent": null, "window_minutes": null, "resets_at": null},
        });
        if duplicate_weekly {
            rate_limits["secondary"] =
                json!({"used_percent": 20.0, "window_minutes": 10080, "resets_at": unix(weekly_reset)});
        }
        let event = json!({"timestamp": observed.iso(), "type": "event_msg", "payload": {"type": event_name, "rate_limits": rate_limits}});
        std::fs::write(&path, format!("{event}\n")).unwrap();
        let path_text = path.to_string_lossy().into_owned();
        let run = invoke(&["--db", &db, "ingest", "codex", "--file", &path_text]);
        assert!(!(run.out.clone() + &run.err).contains(&path_text), "{label}");
        if label.starts_with("token_") {
            assert_eq!(run.code, 0, "{label}: {}", run.err);
            let policy = load_policy(Some(&db), Provider::Codex, NOW);
            let history = load_history(Some(&db), Provider::Codex, 20, NOW).unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(policy.source, "codex_token_info");
            assert_eq!(history[0].source, Source::CodexTokenInfo);
            assert_eq!(history[0].schema_marker, Source::CodexTokenInfo.schema());
            assert_eq!(history[0].weekly_used, 18.0);
            assert_eq!(history[0].observed_at, observed);
            assert_eq!((history[0].short_used, history[0].short_reset_at, policy.short_used), (None, None, None));
            assert!(policy.mode.rank() <= Mode::Normal.rank());
        } else {
            assert_eq!(run.code, 2, "{label}");
            if label == "unsupported" {
                assert!(run.err.contains("no supported Codex rate-limit event was found"));
            } else {
                assert!(run.err.contains("ambiguous"));
            }
            assert!(load_history(Some(&db), Provider::Codex, 20, NOW).unwrap().is_empty());
        }
    }
}

#[test]
fn codex_reader_skips_partial_and_oversize_lines_and_refuses_unknown_shape() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let path = temp.path().join("tail.jsonl");
    let event = json!({
        "timestamp": NOW.iso(), "type": "event_msg",
        "payload": {"type": "token_info", "rate_limits": {
            "bucket-a": {"used_percent": 18, "window_minutes": 10080, "resets_at": unix(NOW + 100 * HOUR)},
        }},
    });
    let mut data = vec![b'p'; MAX_CODEX_TAIL_BYTES as usize + 100];
    data.push(b'\n');
    data.extend(vec![b'x'; 65537]);
    data.push(b'\n');
    data.extend(format!("{event}\n").bytes());
    data.extend(event.to_string().bytes());
    std::fs::write(&path, data).unwrap();
    let run = invoke(&["--db", &db, "ingest", "codex", "--file", path.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(load_policy(Some(&db), Provider::Codex, NOW).weekly_used, Some(18.0));

    let unknown = temp.path().join("unknown.jsonl");
    let shape = json!({"type": "event_msg", "payload": {"type": "token_info", "info": {"total_token_usage": 100}}});
    std::fs::write(&unknown, format!("{shape}\n")).unwrap();
    let before = load_history(Some(&db), Provider::Codex, 20, NOW).unwrap().len();
    let run = invoke(&["--db", &db, "ingest", "codex", "--file", unknown.to_str().unwrap()]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("Codex rate-limit"));
    assert_eq!(load_history(Some(&db), Provider::Codex, 20, NOW).unwrap().len(), before);

    let ambiguous = temp.path().join("ambiguous.jsonl");
    let event = json!({
        "timestamp": NOW.iso(), "type": "event_msg",
        "payload": {"type": "token_info", "rate_limits": {
            "first": {"used_percent": 18, "window_minutes": 10080, "resets_at": unix(NOW + 100 * HOUR)},
            "second": {"used_percent": 20, "window_minutes": 10080, "resets_at": unix(NOW + 100 * HOUR)},
        }},
    });
    std::fs::write(&ambiguous, format!("{event}\n")).unwrap();
    let run = invoke(&["--db", &db, "ingest", "codex", "--file", ambiguous.to_str().unwrap()]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("ambiguous"));
    assert_eq!(load_history(Some(&db), Provider::Codex, 20, NOW).unwrap().len(), before);
}

#[test]
fn codex_file_must_be_a_regular_file_not_a_link_or_folder() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let run = invoke(&["--db", &db, "ingest", "codex", "--file", temp.path().to_str().unwrap()]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: Codex input must be one explicitly selected regular file\n"));
    #[cfg(unix)]
    {
        let real = temp.path().join("real.jsonl");
        std::fs::write(&real, b"{}\n").unwrap();
        let link = temp.path().join("link.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let run = invoke(&["--db", &db, "ingest", "codex", "--file", link.to_str().unwrap()]);
        assert_eq!(run.code, 2);
        assert!(run.err.contains("explicitly selected regular file"));
    }
    assert!(!std::path::Path::new(&db).exists());
}
