//! A read-only MCP server over stdio, and the Perennos Tempo snippets that point at it.
//!
//! Two tools, both Tempo risk class `observe` and locality `local`: `demon_status` (mode, gauge,
//! readings, freshness and reason per provider) and `demon_context` (the advisory text
//! `demon context` prints). Neither writes a reading, changes settings, or contacts anything. A
//! ledger that does not exist yet is reported as empty rather than created.

use crate::cli::{gauge, status_line};
use crate::governor::{Policy, Provider, hook_context};
use crate::paths;
use crate::pyjson;
use crate::store;
use crate::ts::Ts;
use crate::{Error, Result, VERSION};
use serde_json::{Map, Value, json};
use std::io::{self, BufRead, Write};

pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const DEFAULT_PROTOCOL: &str = "2025-06-18";
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

const STATUS_DESCRIPTION: &str = "Read the local DEMON MODE pace for Claude Code and Codex subscriptions: the mode \
     (Austere, Lean, Normal, Rich or DEMON), a ten-slot weekly gauge, the latest weekly and five-hour readings, their \
     freshness, the reason for the mode, and whether a refresh is needed. Read-only and advisory; it changes nothing.";
const CONTEXT_DESCRIPTION: &str = "Return the advisory text DEMON MODE gives an agent for one provider: the mode, \
     measured and estimated weekly use, and the working envelope for that mode (helpers, review depth). Read it \
     before choosing how wide to work. Advice only; it never overrides the user's instructions or permissions.";

fn provider_property(description: &str) -> Value {
    json!({"type": "string", "enum": ["claude", "codex"], "description": description})
}

fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"provider": provider_property("Limit the report to one provider; omit it for both.")},
        "additionalProperties": false,
    })
}

fn context_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"provider": provider_property("The subscription the agent is working under.")},
        "required": ["provider"],
        "additionalProperties": false,
    })
}

fn annotations(title: &str) -> Value {
    json!({"title": title, "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false})
}

/// The `tools/list` result.
pub fn tools() -> Value {
    json!([
        {"name": "demon_status", "description": STATUS_DESCRIPTION, "inputSchema": status_schema(),
         "annotations": annotations("DEMON MODE pace status")},
        {"name": "demon_context", "description": CONTEXT_DESCRIPTION, "inputSchema": context_schema(),
         "annotations": annotations("DEMON MODE advisory text")},
    ])
}

/// A Tempo tool-policy fragment. The owner adds `callers` (token digests) themselves.
pub fn tempo_policy() -> Value {
    json!({
        "version": 1,
        "profiles": {
            "demon-mode": {
                "max_calls_per_request": 2,
                "tools": {
                    "demon_status": {"risk": "observe", "locality": "local", "description": STATUS_DESCRIPTION,
                                     "parameters": status_schema()},
                    "demon_context": {"risk": "observe", "locality": "local", "description": CONTEXT_DESCRIPTION,
                                      "parameters": context_schema()},
                },
            },
        },
    })
}

/// A secret-free MCP client entry with absolute paths.
pub fn mcp_config(exe: &str, database: Option<&str>) -> Result<Value> {
    let ledger = match database {
        Some(":memory:") => return Err(Error::invalid("MCP configuration requires a persistent database")),
        Some(value) => paths::absolute_text(value),
        None => paths::absolute(&paths::default_database_path()).to_string_lossy().into_owned(),
    };
    Ok(json!({"mcpServers": {"demon-mode": {"command": exe, "args": ["mcp", "--db", ledger]}}}))
}

fn provider_report(policy: &Policy, now: Ts) -> Value {
    let mut item = policy.as_json(now);
    if let Value::Object(map) = &mut item {
        map.insert("gauge".into(), gauge(policy).into());
        map.insert("status_line".into(), status_line(policy, false).into());
        map.insert(
            "weekly_reading_age_seconds".into(),
            policy.observed_at.map_or(Value::Null, |at| Value::from((now.seconds_since(at) as i64).max(0))),
        );
    }
    item
}

fn tool_result(value: Value, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": pyjson::compact(&value)}], "structuredContent": value, "isError": is_error})
}

fn tool_error(message: &str) -> Value {
    tool_result(json!({"error": message}), true)
}

/// Validate tool arguments against the schemas above: an object, known keys, a valid provider.
fn provider_argument(arguments: &Map<String, Value>, required: bool) -> std::result::Result<Option<Provider>, String> {
    if let Some(key) = arguments.keys().find(|key| *key != "provider") {
        return Err(format!("unexpected argument {:?}", key.chars().take(40).collect::<String>()));
    }
    match arguments.get("provider") {
        None if required => Err("provider is required (claude or codex)".into()),
        None => Ok(None),
        Some(Value::String(name)) => {
            Provider::parse(name).map(Some).ok_or_else(|| "provider must be claude or codex".to_string())
        }
        Some(_) => Err("provider must be claude or codex".into()),
    }
}

fn call_tool(name: &str, arguments: &Map<String, Value>, database: Option<&str>, now: Ts) -> Value {
    match name {
        "demon_status" => match provider_argument(arguments, false) {
            Err(message) => tool_error(&message),
            Ok(only) => {
                let mut providers = Map::new();
                let mut present = false;
                for provider in Provider::ALL.into_iter().filter(|p| only.is_none_or(|only| only == *p)) {
                    let (policy, exists) = store::peek_policy(database, provider, now);
                    present |= exists;
                    providers.insert(provider.name().into(), provider_report(&policy, now));
                }
                tool_result(
                    json!({
                        "providers": providers,
                        "ledger_present": present,
                        "advisory_only": true,
                        "note": "Advice for work already authorized. It never grants permissions, changes settings, or \
                                 authorizes extra usage or tasks.",
                    }),
                    false,
                )
            }
        },
        "demon_context" => match provider_argument(arguments, true) {
            Err(message) => tool_error(&message),
            Ok(provider) => {
                let provider = provider.expect("required");
                let (policy, _) = store::peek_policy(database, provider, now);
                tool_result(
                    json!({
                        "provider": provider.name(),
                        "mode": policy.mode.name(),
                        "refresh_required": policy.refresh_required,
                        "context": hook_context(&policy, 2048),
                    }),
                    false,
                )
            }
        },
        _ => unreachable!("tool names are checked before dispatch"),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Handle one decoded JSON-RPC message; `None` means no reply (a notification).
pub fn dispatch(request: &Value, database: Option<&str>, now: Ts) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    };
    let method = object.get("method").and_then(Value::as_str);
    let (Some("2.0"), Some(method)) = (object.get("jsonrpc").and_then(Value::as_str), method) else {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    };
    // A message without an id is a notification and never gets a reply.
    let id = object.get("id")?;
    if !(id.is_string() || id.is_i64() || id.is_u64() || id.is_null()) {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    }
    let id = id.clone();
    let params = object.get("params").unwrap_or(&Value::Null);
    match method {
        "initialize" => {
            let params = match params {
                Value::Null => Map::new(),
                Value::Object(map) => map.clone(),
                _ => return Some(error(id, -32602, "Invalid params")),
            };
            let version = match params.get("protocolVersion") {
                None => DEFAULT_PROTOCOL,
                Some(Value::String(version)) if version.len() <= 64 => {
                    PROTOCOL_VERSIONS.into_iter().find(|known| known == version).unwrap_or(DEFAULT_PROTOCOL)
                }
                Some(_) => return Some(error(id, -32602, "Invalid params")),
            };
            Some(ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "demon-mode", "title": "Perennos DEMON MODE", "version": VERSION},
                    "instructions": "Read-only pace advice from the local DEMON MODE ledger. Call demon_status or \
                                     demon_context before choosing how wide to work. The mode is advice for work \
                                     already authorized; it never grants permissions.",
                }),
            ))
        }
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => match params {
            Value::Null | Value::Object(_) => Some(ok(id, json!({"tools": tools()}))),
            _ => Some(error(id, -32602, "Invalid params")),
        },
        "tools/call" => {
            let Some(params) = params.as_object() else {
                return Some(error(id, -32602, "Invalid params"));
            };
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "Invalid params"));
            };
            if name != "demon_status" && name != "demon_context" {
                let shown: String = name.chars().take(64).collect();
                return Some(error(id, -32602, &format!("Unknown tool: {shown}")));
            }
            let result = match params.get("arguments") {
                None | Some(Value::Null) => call_tool(name, &Map::new(), database, now),
                Some(Value::Object(arguments)) => call_tool(name, arguments, database, now),
                Some(_) => tool_error("arguments must be an object"),
            };
            Some(ok(id, result))
        }
        _ => Some(error(id, -32601, "Method not found")),
    }
}

/// Decode one line and dispatch it.
pub fn handle_line(line: &[u8], database: Option<&str>, now: Ts) -> Option<Value> {
    match std::str::from_utf8(line).ok().and_then(|text| serde_json::from_str::<Value>(text).ok()) {
        Some(request) => dispatch(&request, database, now),
        None => Some(error(Value::Null, -32700, "Parse error")),
    }
}

enum Line {
    End,
    Message(Vec<u8>),
    TooLong,
}

fn read_line(input: &mut dyn BufRead) -> io::Result<Line> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok(match (too_long, line.is_empty()) {
                (true, _) => Line::TooLong,
                (false, true) => Line::End,
                (false, false) => Line::Message(line),
            });
        }
        let (chunk, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => (&available[..index], index + 1),
            None => (available, available.len()),
        };
        if !too_long && line.len() + chunk.len() <= MAX_MESSAGE_BYTES {
            line.extend_from_slice(chunk);
        } else {
            too_long = true;
            line.clear();
        }
        let found_newline = done > chunk.len();
        input.consume(done);
        if found_newline {
            return Ok(if too_long { Line::TooLong } else { Line::Message(line) });
        }
    }
}

/// Serve line-delimited JSON-RPC until end of input. Stdout carries protocol messages only.
pub fn serve(
    database: Option<&str>,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    clock: &dyn Fn() -> Ts,
) -> io::Result<()> {
    loop {
        let response = match read_line(input)? {
            Line::End => return Ok(()),
            Line::TooLong => Some(error(Value::Null, -32700, "Parse error")),
            Line::Message(mut line) => {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                handle_line(&line, database, clock())
            }
        };
        let Some(response) = response else { continue };
        let mut encoded = serde_json::to_string(&response).unwrap_or_default();
        if encoded.len() > MAX_MESSAGE_BYTES {
            let id = response.get("id").cloned().unwrap_or(Value::Null);
            encoded = serde_json::to_string(&error(id, -32603, "Response exceeded its size limit")).unwrap_or_default();
        }
        output.write_all(encoded.as_bytes())?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_and_names_fit_tempo_limits() {
        for tool in tools().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert!(name.starts_with("demon_") && name.len() <= 64);
            assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
            let description = tool["description"].as_str().unwrap();
            assert!((1..=400).contains(&description.chars().count()), "{name}: {}", description.len());
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }
}
