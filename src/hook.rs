//! Native `UserPromptSubmit` hook output and the print-only `hook-config` template.

use crate::governor::{Mode, Policy, Provider, hook_context};
use crate::paths;
use crate::pyjson;
use crate::{Error, Result};
use serde_json::{Map, Value, json};

pub const MAX_HOOK_BYTES: usize = 2048;

fn render(context: &str) -> String {
    format!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":{}}}}}",
        pyjson::quote(context)
    )
}

/// Compact hook JSON that always fits the 2048-byte hook budget, newline included.
pub fn hook_json(policy: &Policy) -> String {
    let rendered = render(&hook_context(policy, 1500));
    if rendered.len() < MAX_HOOK_BYTES {
        return rendered;
    }
    let safe = Policy {
        mode: Mode::Lean,
        source: "unavailable",
        freshness: "context_limit",
        reason: "context_limit_refresh_required",
        refresh_required: true,
        ..policy.clone()
    };
    let rendered = render(&hook_context(&safe, 700));
    if rendered.len() < MAX_HOOK_BYTES {
        return rendered;
    }
    render(
        "Perennos DEMON MODE advisory. Mode: Lean. Refresh readings before any upgrade. \
         This advisory never changes permissions or authorizes extra work.",
    )
}

/// `shlex.quote`.
pub fn shell_quote(argument: &str) -> String {
    if argument.is_empty() {
        return "''".into();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if argument.chars().all(safe) {
        return argument.into();
    }
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64_with(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(alphabet[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64(bytes: &[u8]) -> String {
    base64_with(bytes, BASE64, true)
}

/// `base64.urlsafe_b64encode(...).rstrip(b"=")`, as `secrets.token_urlsafe` builds tokens.
pub fn base64_url(bytes: &[u8]) -> String {
    base64_with(bytes, BASE64_URL, false)
}

/// The `hook` command line for one client, starting with the binary's absolute path.
pub fn hook_argv(exe: &str, client: Provider, database: Option<&str>, codex_file: Option<&str>) -> Result<Vec<String>> {
    let mut command = vec![exe.to_string()];
    if let Some(database) = database {
        if database == ":memory:" {
            return Err(Error::invalid("hook configuration requires a persistent database"));
        }
        command.extend(["--db".into(), paths::absolute_text(database)]);
    }
    command.extend(["hook".into(), "--client".into(), client.name().into()]);
    if let Some(codex_file) = codex_file {
        if client != Provider::Codex {
            return Err(Error::invalid("--codex-file requires --client codex"));
        }
        command.extend(["--codex-file".into(), paths::absolute_text(codex_file)]);
    }
    Ok(command)
}

/// The full `hook-config` template. `windows` selects the PowerShell form used on Windows.
pub fn hook_config(
    exe: &str,
    client: Provider,
    database: Option<&str>,
    codex_file: Option<&str>,
    windows: bool,
) -> Result<Value> {
    let command = hook_argv(exe, client, database, codex_file)?;
    let mut windows_script = None;
    let quoted = if windows {
        // Codex can invoke commands through PowerShell, cmd, or a chosen shell. Keep the outer command
        // shell-neutral and pass every argument as a PowerShell literal, preserving ', $, %, & and spaces.
        let mut script = String::from("& ");
        script.push_str(
            &command.iter().map(|arg| format!("'{}'", arg.replace('\'', "''"))).collect::<Vec<_>>().join(" "),
        );
        script.push_str("; exit $LASTEXITCODE");
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64(&utf16);
        windows_script = Some(script);
        format!("powershell.exe -NoProfile -NonInteractive -EncodedCommand {encoded}")
    } else {
        command.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ")
    };
    let handler = match client {
        Provider::Claude => json!({"type": "command", "command": command[0], "args": command[1..], "timeout": 10}),
        Provider::Codex => json!({"type": "command", "command": quoted, "timeout": 10}),
    };
    let mut result = Map::new();
    result.insert("template_only".into(), true.into());
    result.insert("client".into(), client.name().into());
    result.insert("event".into(), "UserPromptSubmit".into());
    result.insert("command".into(), quoted.into());
    result.insert("argv".into(), command.into());
    result.insert("config".into(), json!({"hooks": {"UserPromptSubmit": [{"hooks": [handler]}]}}));
    result.insert(
        "merge_instruction".into(),
        "Add this command to the existing hook list; preserve all other hooks.".into(),
    );
    if let Some(script) = windows_script {
        result.insert("windows_script".into(), script.into());
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_matches_python_shlex() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/usr/bin/demon"), "/usr/bin/demon");
        assert_eq!(shell_quote("ledger's $x"), "'ledger'\"'\"'s $x'");
        assert_eq!(shell_quote("a b"), "'a b'");
    }

    #[test]
    fn base64_variants() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64_url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_url(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn windows_template_encodes_literal_powershell_arguments() {
        let database = "it's $x & 100%.sqlite3";
        let config = hook_config(r"C:\Users\A B\demon.exe", Provider::Codex, Some(database), None, true).unwrap();
        let script = config["windows_script"].as_str().unwrap();
        let absolute = paths::absolute_text(database).replace('\'', "''");
        assert_eq!(
            script,
            format!(r"& 'C:\Users\A B\demon.exe' '--db' '{absolute}' 'hook' '--client' 'codex'; exit $LASTEXITCODE")
        );
        let command = config["command"].as_str().unwrap();
        let encoded = command.strip_prefix("powershell.exe -NoProfile -NonInteractive -EncodedCommand ").unwrap();
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(encoded, base64(&utf16));
        let handler = &config["config"]["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(handler["command"], config["command"]);
        let claude = hook_config(r"C:\demon.exe", Provider::Claude, None, None, true).unwrap();
        let handler = &claude["config"]["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(handler["command"], r"C:\demon.exe");
        assert_eq!(handler["args"], json!(["hook", "--client", "claude"]));
    }
}
