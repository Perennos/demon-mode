//! The installed-binary workflow from the former scripts/smoke_installed.py, run against the real
//! `demon` binary from a clean folder outside the checkout.
mod common;

use common::*;
use demon_mode::ts::Ts;
use serde_json::{Value, json};
use std::process::{Command, Output};

fn run_in(dir: &std::path::Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).current_dir(dir).output().expect("run");
    check(output, program)
}

fn check(output: Output, what: &str) -> String {
    assert!(output.status.success(), "{what} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn installed_binary_workflow() {
    let temp = TempDir::new();
    let root = temp.path();
    let demon = demon_binary();
    assert!(run_in(root, demon, &["--help"]).contains("hook-config"));
    assert!(run_in(root, demon, &["--version"]).contains(demon_mode::VERSION));
    let now = Ts::now();
    // Shell-sensitive characters prove argument preservation through every hook form.
    let db = temp.db("ledger's $literal & percent%.sqlite3");
    let with_db = |args: &[&str]| -> String {
        let mut full = vec!["--db", db.as_str()];
        full.extend_from_slice(args);
        run_in(root, demon, &full)
    };
    let resets = (now + 100 * HOUR).iso();
    let policy: Value = serde_json::from_str(&with_db(&[
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
        &(now + 4 * HOUR).iso(),
        "--at",
        &now.iso(),
        "--json",
    ]))
    .unwrap();
    assert_eq!(policy["weekly_used_percent"], 18.0);
    let history: Value = serde_json::from_str(&with_db(&["history", "--provider", "codex", "--json"])).unwrap();
    assert_eq!(history[0]["short_used_percent"], 12.0);
    assert!(with_db(&["context", "--provider", "codex"]).contains("Mode:"));
    let doctor: Value = serde_json::from_str(&with_db(&["doctor", "--provider", "codex", "--json"])).unwrap();
    assert_eq!(doctor["source"], "manual");
    for client in ["codex", "claude"] {
        if client == "claude" {
            with_db(&["record", "--provider", client, "--weekly", "18", "--resets", &resets, "--at", &now.iso()]);
        }
        let config: Value =
            serde_json::from_str(&with_db(&["hook-config", "--client", client, "--format", "native"])).unwrap();
        let handler = &config["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        let output = if let Some(args) = handler["args"].as_array() {
            let args: Vec<&str> = args.iter().map(|arg| arg.as_str().unwrap()).collect();
            assert_eq!(handler["command"], std::fs::canonicalize(demon).unwrap().to_string_lossy().as_ref());
            run_in(root, handler["command"].as_str().unwrap(), &args)
        } else {
            let command = handler["command"].as_str().unwrap();
            #[cfg(windows)]
            let output = Command::new("cmd").args(["/C", command]).current_dir(root).output().unwrap();
            #[cfg(not(windows))]
            let output = Command::new("sh").args(["-c", command]).current_dir(root).output().unwrap();
            check(output, command)
        };
        assert!(output.len() <= 2048);
        let context = json(&output)["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
        assert!(context.contains("18.0% measured"), "{client}: {context}");
    }
    let export = root.join("limits.json");
    let body = json!({"id": 6, "result": {"rateLimits": {"limitId": "codex",
        "secondary": {"usedPercent": 19, "windowDurationMins": 10080, "resetsAt": (now + 100 * HOUR).timestamp()}}}});
    std::fs::write(&export, format!("\u{feff}{body}")).unwrap();
    let imported: Value = serde_json::from_str(&with_db(&[
        "ingest",
        "codex-app-server",
        "--file",
        export.to_str().unwrap(),
        "--at",
        &(now + 1000).iso(),
        "--json",
    ]))
    .unwrap();
    assert_eq!(
        (imported["source"].as_str(), imported["weekly_used_percent"].as_f64()),
        (Some("codex_app_server"), Some(19.0))
    );
}
