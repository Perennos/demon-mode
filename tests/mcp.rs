//! The MCP server (suite contract section E) and the `tempo` snippets.
mod common;

use common::*;
use demon_mode::mcp::{dispatch, handle_line, serve, tempo_policy, tools};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn call_tool(db: &str, name: &str, arguments: Value) -> Value {
    dispatch(&request(json!(1), "tools/call", json!({"name": name, "arguments": arguments})), Some(db), NOW).unwrap()
}

#[test]
fn initialize_negotiates_known_versions_and_reports_tools_capability() {
    for version in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let reply = dispatch(&request(json!(1), "initialize", json!({"protocolVersion": version})), None, NOW).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], version);
    }
    let reply =
        dispatch(&request(json!("a"), "initialize", json!({"protocolVersion": "1999-01-01"})), None, NOW).unwrap();
    let result = &reply["result"];
    assert_eq!(reply["id"], "a");
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["capabilities"], json!({"tools": {"listChanged": false}}));
    assert_eq!(result["serverInfo"]["name"], "demon-mode");
    assert_eq!(result["serverInfo"]["version"], demon_mode::VERSION);
    let bad = dispatch(&request(json!(2), "initialize", json!([])), None, NOW).unwrap();
    assert_eq!(bad["error"]["code"], -32602);
}

#[test]
fn notifications_get_no_reply_and_errors_use_standard_codes() {
    assert_eq!(dispatch(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), None, NOW), None);
    assert_eq!(dispatch(&json!({"jsonrpc": "2.0", "method": "tools/call", "params": {}}), None, NOW), None);
    assert_eq!(dispatch(&request(json!(1), "ping", Value::Null), None, NOW).unwrap()["result"], json!({}));
    assert_eq!(dispatch(&request(json!(1), "resources/list", json!({})), None, NOW).unwrap()["error"]["code"], -32601);
    assert_eq!(handle_line(b"{not json", None, NOW).unwrap()["error"]["code"], -32700);
    assert_eq!(handle_line(b"[1]", None, NOW).unwrap()["error"]["code"], -32600);
    assert_eq!(
        dispatch(&json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}), None, NOW).unwrap()["error"]["code"],
        -32600
    );
    assert_eq!(
        dispatch(&json!({"jsonrpc": "2.0", "id": true, "method": "ping"}), None, NOW).unwrap()["error"]["code"],
        -32600
    );
    let unknown = dispatch(&request(json!(3), "tools/call", json!({"name": "demon_delete"})), None, NOW).unwrap();
    assert_eq!(unknown["error"]["code"], -32602);
    assert_eq!(dispatch(&request(json!(4), "tools/call", json!("x")), None, NOW).unwrap()["error"]["code"], -32602);
}

#[test]
fn tool_list_matches_the_suite_contract() {
    let reply = dispatch(&request(json!(1), "tools/list", json!({})), None, NOW).unwrap();
    let listed = reply["result"]["tools"].as_array().unwrap().clone();
    assert_eq!(Value::Array(listed.clone()), tools());
    let names: Vec<&str> = listed.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["demon_status", "demon_context"]);
    for tool in &listed {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        let hints = &tool["annotations"];
        assert_eq!((hints["readOnlyHint"].as_bool(), hints["destructiveHint"].as_bool()), (Some(true), Some(false)));
        assert_eq!((hints["idempotentHint"].as_bool(), hints["openWorldHint"].as_bool()), (Some(true), Some(false)));
    }
}

#[test]
fn status_tool_reports_both_providers_without_creating_a_ledger() {
    let temp = TempDir::new();
    let db = temp.db("absent.sqlite3");
    let reply = call_tool(&db, "demon_status", json!({}));
    let result = &reply["result"];
    assert_eq!(result["isError"], false);
    let structured = &result["structuredContent"];
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(), *structured);
    assert_eq!(structured["ledger_present"], false);
    assert_eq!(structured["providers"]["claude"]["mode"], "Lean");
    assert_eq!(structured["providers"]["codex"]["gauge"], "----------");
    assert!(!std::path::Path::new(&db).exists(), "a read-only tool must not create the ledger");
}

#[test]
fn status_and_context_tools_read_the_same_policy_as_the_cli() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "codex", NOW, 42.0, NOW + 100 * HOUR, Some((10.0, NOW + 4 * HOUR)), NOW);
    let reply = call_tool(&db, "demon_status", json!({"provider": "codex"}));
    let codex = reply["result"]["structuredContent"]["providers"].clone();
    assert_eq!(codex.as_object().unwrap().len(), 1);
    let status = invoke_at(&["--db", &db, "status", "--provider", "codex", "--json"], NOW);
    let mut cli = json(&status.out);
    let item = codex["codex"].clone();
    for key in ["gauge", "status_line", "weekly_reading_age_seconds"] {
        cli[key] = item[key].clone();
    }
    assert_eq!(item, cli);
    assert_eq!(item["gauge"], "####------");
    assert_eq!(item["weekly_reading_age_seconds"], 0);
    let line = invoke_at(&["--db", &db, "status", "--provider", "codex"], NOW).out;
    assert_eq!(item["status_line"].as_str().unwrap(), line.trim_end());

    let reply = call_tool(&db, "demon_context", json!({"provider": "codex"}));
    let context = invoke_at(&["--db", &db, "context", "--provider", "codex"], NOW).out;
    assert_eq!(reply["result"]["structuredContent"]["context"].as_str().unwrap(), context.trim_end());
    assert_eq!(reply["result"]["structuredContent"]["mode"], "Lean");
}

#[test]
fn bad_tool_arguments_are_tool_errors() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    for (name, arguments) in [
        ("demon_context", json!({})),
        ("demon_context", json!({"provider": "gemini"})),
        ("demon_status", json!({"provider": 1})),
        ("demon_status", json!({"extra": true})),
        ("demon_status", json!([])),
    ] {
        let reply = call_tool(&db, name, arguments.clone());
        assert_eq!(reply["result"]["isError"], true, "{name} {arguments}");
        assert!(reply["result"]["structuredContent"]["error"].is_string());
    }
}

#[test]
fn serve_reads_lines_skips_notifications_and_rejects_oversize_messages() {
    let big = format!("{{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\",\"pad\":\"{}\"}}", "x".repeat(1 << 20));
    let input = format!(
        "{}\n{}\n\n{}\n{}\r\n",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        big,
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
    );
    let mut output = Vec::new();
    serve(None, &mut input.as_bytes(), &mut output, &|| NOW).unwrap();
    let replies: Vec<Value> = String::from_utf8(output).unwrap().lines().map(json).collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert_eq!(replies[2], json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
}

#[test]
fn binary_speaks_mcp_over_stdio() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let now = demon_mode::ts::Ts::now();
    record(&db, "claude", now - 10 * MINUTE, 10.0, now + 100 * HOUR, None, now);
    let mut child = Command::new(demon_binary())
        .args(["mcp", "--db", &db])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |message: Value| -> Value {
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        json(&line)
    };
    let init =
        ask(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}}));
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    let listed = ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 2);
    let status = ask(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                            "params": {"name": "demon_status", "arguments": {"provider": "claude"}}}));
    let claude = &status["result"]["structuredContent"]["providers"]["claude"];
    assert_eq!(claude["weekly_used_percent"], 10.0);
    assert_eq!(status["result"]["structuredContent"]["ledger_present"], true);
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}

#[test]
fn tempo_policy_fragment_fits_tempo_validation_rules() {
    let run = invoke(&["tempo", "policy"]);
    assert_eq!(run.code, 0);
    let policy = json(&run.out);
    assert_eq!(policy, tempo_policy());
    assert_eq!(policy["version"], 1);
    assert!(policy.get("callers").is_none(), "the owner adds callers; no tokens are generated");
    let profile = &policy["profiles"]["demon-mode"];
    assert_eq!(profile["max_calls_per_request"], 2);
    let allowed = [
        "type",
        "properties",
        "required",
        "additionalProperties",
        "enum",
        "maxLength",
        "minLength",
        "minimum",
        "maximum",
        "items",
        "maxItems",
        "description",
    ];
    fn keys_ok(schema: &Value, allowed: &[&str]) -> bool {
        schema.as_object().unwrap().iter().all(|(key, value)| {
            allowed.contains(&key.as_str())
                && (key != "properties" || value.as_object().unwrap().values().all(|sub| keys_ok(sub, allowed)))
        })
    }
    for (name, tool) in profile["tools"].as_object().unwrap() {
        assert!(name.starts_with("demon_"));
        assert_eq!((tool["risk"].as_str(), tool["locality"].as_str()), (Some("observe"), Some("local")));
        assert!((1..=400).contains(&tool["description"].as_str().unwrap().len()));
        assert_eq!(tool["parameters"]["additionalProperties"], false);
        assert!(keys_ok(&tool["parameters"], &allowed), "{name}");
    }
}

#[test]
fn mcp_config_uses_absolute_paths_and_no_secrets() {
    let run = invoke(&["--db", "./ledger.sqlite3", "tempo", "mcp-config"]);
    assert_eq!(run.code, 0, "{}", run.err);
    let entry = &json(&run.out)["mcpServers"]["demon-mode"];
    assert_eq!(entry["command"], EXE);
    let expected = std::env::current_dir().unwrap().join("ledger.sqlite3");
    assert_eq!(entry["args"], json!(["mcp", "--db", expected.to_string_lossy()]));
    let default = invoke(&["tempo", "mcp-config"]);
    let args = &json(&default.out)["mcpServers"]["demon-mode"]["args"];
    assert!(std::path::Path::new(args[2].as_str().unwrap()).is_absolute());
    assert!(!default.out.to_lowercase().contains("token"));
    assert_eq!(invoke(&["--db", ":memory:", "tempo", "mcp-config"]).code, 2);
}
