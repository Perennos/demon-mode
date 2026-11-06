//! Ported from tests/test_cli.py and tests/test_manual_flow.py.
mod common;

use common::*;
use demon_mode::governor::{Mode, Provider};
use demon_mode::store::load_policy;
use std::collections::BTreeSet;
use std::process::Command;

#[test]
fn manual_flags_status_json_and_numeric_history() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let run = invoke(&[
        "--db",
        &db,
        "record",
        "--provider",
        "claude",
        "--weekly",
        "0.2",
        "--resets",
        &(NOW + 20 * HOUR).iso(),
        "--five-hour",
        "5",
        "--five-hour-resets",
        &(NOW + 4 * HOUR).iso(),
        "--at",
        &NOW.iso(),
        "--json",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(json(&run.out)["mode"], "DEMON");
    let run = invoke(&["--db", &db, "status", "--provider", "claude", "--json"]);
    assert_eq!((run.code, json(&run.out)["mode"].as_str()), (0, Some("DEMON")));
    let run = invoke(&["--db", &db, "history", "--provider", "claude", "--json"]);
    assert_eq!(run.code, 0);
    let rows = json(&run.out);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let keys: BTreeSet<&str> = rows[0].as_object().unwrap().keys().map(String::as_str).collect();
    let expected = ["observed_at", "weekly_used_percent", "weekly_reset_at", "short_used_percent", "short_reset_at"];
    assert_eq!(keys, expected.into_iter().collect());
    assert!(!run.out.contains(&db));
    let run = invoke(&["--db", &db, "status", "--provider", "claude"]);
    assert_eq!(run.code, 0);
    assert!(run.out.starts_with("DEMON ["));
    assert!(!run.out.contains('\x1b'));
}

#[test]
fn record_aliases_match_the_long_flags() {
    let temp = TempDir::new();
    let (long, short) = (temp.db("long.sqlite3"), temp.db("short.sqlite3"));
    let resets = (NOW + 100 * HOUR).iso();
    let five = (NOW + 4 * HOUR).iso();
    let at = (NOW - 10 * MINUTE).iso();
    let a = invoke(&[
        "--db",
        &long,
        "record",
        "--provider",
        "codex",
        "--weekly",
        "18",
        "--resets",
        &resets,
        "--five-hour",
        "12",
        "--five-hour-resets",
        &five,
        "--at",
        &at,
        "--json",
    ]);
    let b = invoke(&[
        "--db",
        &short,
        "record",
        "--provider",
        "codex",
        "--weekly",
        "18",
        "--resets",
        &resets,
        "--five",
        "12",
        "--five-resets",
        &five,
        "--observed-at",
        &at,
        "--json",
    ]);
    assert_eq!((a.code, b.code), (0, 0));
    assert_eq!(a.out, b.out);
    // Unambiguous prefixes are accepted, as argparse allowed.
    let c = invoke(&["--db", &short, "status", "--prov", "codex", "--js"]);
    assert_eq!(c.code, 0, "{}", c.err);
    assert_eq!(json(&c.out)["weekly_used_percent"], 18.0);
}

#[test]
fn record_validation_errors_exit_two_with_a_plain_message() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let run = invoke(&["--db", &db, "record", "--provider", "claude", "--weekly", "150", "--resets", "x"]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: invalid percentage\n"));
    let run = invoke_with(
        &["--db", &db, "record", "--provider", "claude"],
        Options { stdin_tty: true, ..Options::default() },
    );
    assert_eq!((run.code, run.err.as_str()), (2, "demon: record requires --provider, --weekly, and --resets\n"));
    let run = invoke(&["--db", &db, "record", "--provider", "claude", "--weekly", "-5", "--resets", "x"]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: invalid percentage\n"));
    let ahead = (NOW + 2 * MINUTE).iso();
    let run = invoke(&[
        "--db",
        &db,
        "record",
        "--provider",
        "claude",
        "--weekly",
        "5",
        "--resets",
        &(NOW + HOUR).iso(),
        "--at",
        &ahead,
    ]);
    assert_eq!((run.code, run.err.as_str()), (2, "demon: observation is in the future\n"));
    let usage = invoke(&["status"]);
    assert_eq!(usage.code, 2);
    assert!(usage.err.contains("--provider"));
    assert_eq!(invoke(&["status", "--provider", "gemini"]).code, 2);
}

#[test]
fn hook_native_json_is_bounded_prompt_blind_and_falls_back() {
    let temp = TempDir::new();
    let db = temp.db("bad.sqlite3");
    std::fs::write(&db, b"corrupt").unwrap();
    let prompt = br#"{"hook_event_name": "UserPromptSubmit", "prompt": "SYNTHETIC_SENTINEL_NO_RETENTION"}"#;
    for client in ["claude", "codex"] {
        let run = invoke_stdin(&["--db", &db, "hook", "--client", client], prompt);
        assert_eq!((run.code, run.err.as_str()), (0, ""));
        let payload = json(&run.out);
        let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["hookSpecificOutput"]);
        let native = &payload["hookSpecificOutput"];
        assert_eq!(native["hookEventName"], "UserPromptSubmit");
        assert!(native["additionalContext"].as_str().unwrap().contains("Mode: Lean"));
        assert!(!run.out.contains("SYNTHETIC_SENTINEL"));
        assert!(run.out.len() <= 2048);
        assert_eq!(run.stdin_reads, 0);
        assert_eq!(std::fs::read(&db).unwrap(), b"corrupt");
    }
}

#[test]
fn hook_output_is_compact_json_in_the_native_key_order() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let run = invoke(&["--db", &db, "hook", "--client", "codex"]);
    let head = "{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"Perennos DEMON MODE \\u2014 advisory";
    assert!(run.out.starts_with(head), "{}", run.out);
    assert!(run.out.is_ascii());
    assert!(run.out.ends_with("\"}}\n"));
}

#[test]
fn hook_config_is_print_only_and_preserves_existing_hooks() {
    let temp = TempDir::new();
    let output = Command::new(demon_binary())
        .args(["hook-config", "--client", "claude"])
        .current_dir(temp.path())
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = json(&String::from_utf8(output.stdout).unwrap());
    assert_eq!(value["template_only"], true);
    assert_eq!(value["event"], "UserPromptSubmit");
    assert!(value["merge_instruction"].as_str().unwrap().contains("preserve all other hooks"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0, "hook-config must not create files");
}

#[test]
fn hook_config_points_at_the_running_binary() {
    let run = invoke(&["hook-config", "--client", "claude", "--format", "native"]);
    let handler = &json(&run.out)["hooks"]["UserPromptSubmit"][0]["hooks"][0];
    assert_eq!(handler["command"], EXE);
    assert_eq!(handler["args"], serde_json::json!(["hook", "--client", "claude"]));
    let run = invoke(&["hook-config", "--client", "codex"]);
    let template = json(&run.out);
    assert_eq!(template["command"], format!("{EXE} hook --client codex"));
    assert_eq!(template["argv"][0], EXE);
    assert!(template.get("windows_script").is_none());
    let windows = invoke_with(&["hook-config", "--client", "codex"], Options { windows: true, ..Options::default() });
    let template = json(&windows.out);
    assert!(
        template["command"].as_str().unwrap().starts_with("powershell.exe -NoProfile -NonInteractive -EncodedCommand ")
    );
    assert!(template["windows_script"].as_str().unwrap().starts_with(&format!("& '{EXE}' 'hook'")));
}

#[test]
fn non_tty_no_color_preview_and_tty_preview_boundaries() {
    let run = invoke(&["preview", "--seconds", "3"]);
    assert_eq!(run.code, 0);
    assert!(!run.out.contains('\x1b'));
    let mut slept = Vec::new();
    let run = invoke_with(
        &["preview", "--seconds", "3"],
        Options {
            stdout_tty: true,
            sleep: Box::new(|d| {
                slept.push(d);
                true
            }),
            ..Options::default()
        },
    );
    assert_eq!(run.code, 0);
    assert_eq!(run.out.matches("/3").count(), 3);
    assert!(run.out.contains("\x1b[?25l"));
    assert!(run.out.contains("\x1b[?25h"));
    assert_eq!(slept, vec![std::time::Duration::from_millis(1500); 2]);
    let run = invoke_with(&["preview"], Options { stdout_tty: true, no_color: true, ..Options::default() });
    assert_eq!(run.code, 0);
    assert!(!run.out.contains('\x1b'));
    assert!(!run.out.contains("/3"));
    let run = invoke_with(&["preview", "--plain"], Options { stdout_tty: true, ..Options::default() });
    assert!(!run.out.contains('\x1b'));
    assert_eq!(invoke(&["preview", "--seconds", "4"]).code, 2);
}

#[test]
fn status_uses_mode_palette_only_on_color_tty() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "claude", NOW, 40.0, NOW + 100 * HOUR, Some((96.0, NOW + 4 * HOUR)), NOW);
    let run = invoke_with(
        &["--db", &db, "status", "--provider", "claude"],
        Options { stdout_tty: true, ..Options::default() },
    );
    assert_eq!(run.code, 0);
    assert!(run.out.contains("\x1b[34mAUSTERE\x1b[0m"));
    let run = invoke_with(
        &["--db", &db, "status", "--provider", "claude"],
        Options { stdout_tty: true, no_color: true, ..Options::default() },
    );
    assert_eq!(run.code, 0);
    assert!(!run.out.contains('\x1b'));
}

#[test]
fn preview_interrupt_restores_cursor() {
    let run = invoke_with(&["preview"], Options { stdout_tty: true, sleep: Box::new(|_| false), ..Options::default() });
    assert_eq!(run.code, 130);
    assert!(run.out.contains("\x1b[?25l"));
    assert!(run.out.contains("\x1b[?25h"));
}

#[test]
fn broken_pipe_is_quiet() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let mut stdin = std::io::empty();
    let mut stdout = BrokenPipe;
    let mut stderr = Vec::new();
    let mut sleep = |_| true;
    let mut io = demon_mode::cli::Io {
        stdin: &mut stdin,
        stdin_is_terminal: false,
        stdout: &mut stdout,
        stdout_is_terminal: false,
        stderr: &mut stderr,
        no_color: false,
        now: NOW,
        sleep: &mut sleep,
        exe: Some(EXE.into()),
        windows_templates: false,
        process: false,
    };
    let code = demon_mode::cli::run(["demon", "--db", &db, "status", "--provider", "claude"], &mut io);
    assert_eq!(code, 0);
    assert!(stderr.is_empty());
}

#[test]
fn version_flag_reports_the_crate_version() {
    let run = invoke(&["--version"]);
    assert_eq!((run.code, run.out.trim()), (0, format!("demon {}", demon_mode::VERSION).as_str()));
}

// tests/test_manual_flow.py

#[test]
fn manual_record_evaluates_to_mode_specific_hook_context() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let now = demon_mode::ts::Ts::now();
    let now = demon_mode::ts::Ts::from_micros(now.micros() - now.micros() % SECOND);
    let reading = serde_json::json!({
        "provider": "claude", "observed_at": now.iso(), "weekly_used": 0.2,
        "weekly_reset_at": (now + 20 * HOUR).iso(), "short_used": 5.0, "short_reset_at": (now + 4 * HOUR).iso(),
    });
    let run = invoke_with(
        &["--db", &db, "record", "--json"],
        Options { stdin: reading.to_string().as_bytes(), now, ..Options::default() },
    );
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(json(&run.out)["mode"], "DEMON");
    let run = invoke_at(&["--db", &db, "hook", "--client", "claude"], now);
    let payload = json(&run.out);
    let native = &payload["hookSpecificOutput"];
    let context = native["additionalContext"].as_str().unwrap();
    assert_eq!(load_policy(Some(&db), Provider::Claude, now).mode, Mode::Demon);
    assert_eq!(native["hookEventName"], "UserPromptSubmit");
    assert!(context.contains(Mode::Demon.instruction()));
    assert!(context.contains("never overrides the user's instructions or permissions"));
}

#[test]
fn stdin_json_record_rejects_unknown_or_missing_fields() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    for body in [r#"{"x": 1}"#, r#"{"provider": "claude"}"#, "[]"] {
        let run = invoke_stdin(&["--db", &db, "record"], body.as_bytes());
        assert_eq!((run.code, run.err.as_str()), (2, "demon: manual input has missing or unsupported fields\n"));
    }
    let run = invoke_stdin(&["--db", &db, "record", "--stdin-json"], b"{nope");
    assert_eq!((run.code, run.err.as_str()), (2, "demon: input must be valid UTF-8 JSON\n"));
}

#[test]
fn missing_readings_stay_conservative() {
    let temp = TempDir::new();
    let policy = load_policy(Some(&temp.db("empty.sqlite3")), Provider::Codex, NOW);
    assert_eq!(policy.mode, Mode::Lean);
    assert!(policy.refresh_required);
}

#[test]
fn measured_short_window_hard_brake_caps_mode() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "codex", NOW, 1.0, NOW + 100 * HOUR, Some((96.0, NOW + 4 * HOUR)), NOW);
    let policy = load_policy(Some(&db), Provider::Codex, NOW);
    assert_eq!((policy.mode, policy.reason), (Mode::Austere, "short_window_hard_brake"));
    record(&db, "claude", NOW, 1.0, NOW + 100 * HOUR, Some((94.9, NOW + 4 * HOUR)), NOW);
    assert_ne!(load_policy(Some(&db), Provider::Claude, NOW).reason, "short_window_hard_brake");
}
