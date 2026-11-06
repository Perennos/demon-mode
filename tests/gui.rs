//! Ported from tests/test_gui.py.
mod common;

use common::*;
use demon_mode::gui::{self, GuiServer, Options};
use demon_mode::paths::{database_identity, default_database_path};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&body[..line_end]).unwrap().trim(), 16).unwrap();
        body = &body[line_end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn call(port: u16, method: &str, path: &str, body: Option<&[u8]>, headers: &[(&str, &str)]) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("Host")) {
        request.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    if let Some(body) = body {
        stream.write_all(body).unwrap();
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(n, v)| n.eq_ignore_ascii_case("Transfer-Encoding") && v.contains("chunked")) {
        body = dechunk(&body);
    }
    Reply { status, headers, body }
}

struct Running {
    port: u16,
    token: String,
    database_id: Option<String>,
    stop: gui::StopHandle,
    thread: Option<JoinHandle<()>>,
    _temp: Option<TempDir>,
}

impl Running {
    fn start(database: Option<String>, idle: Duration) -> Running {
        let temp = database.is_none().then(TempDir::new);
        let database = database.or_else(|| temp.as_ref().map(|t| t.db("state.sqlite3")));
        let server =
            GuiServer::bind(Options { database, port: 0, exe: EXE.into(), windows_templates: false, idle }).unwrap();
        let (port, token) = (server.port(), server.token().to_string());
        let database_id = server.database_id().map(str::to_string);
        let stop = server.stop_handle();
        let thread = std::thread::spawn(move || server.run());
        Running { port, token, database_id, stop, thread: Some(thread), _temp: temp }
    }

    fn post(&self, path: &str, value: &Value, extra: &[(&str, &str)]) -> Reply {
        let mut headers = vec![("X-Token", self.token.as_str())];
        headers.extend_from_slice(extra);
        call(self.port, "POST", path, Some(value.to_string().as_bytes()), &headers)
    }

    fn join(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                self.thread.take().unwrap().join().unwrap();
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.stop();
        self.join(Duration::from_secs(3));
    }
}

fn running() -> Running {
    Running::start(None, gui::IDLE)
}

#[test]
fn page_and_assets() {
    let server = running();
    let page = call(server.port, "GET", "/", None, &[]);
    assert_eq!(page.status, 200);
    let body = String::from_utf8(page.body.clone()).unwrap();
    assert!(body.contains(&format!(r#"<meta name="x-token" content="{}">"#, server.token)));
    assert!(!body.contains("__TOKEN__"));
    assert!(!body.contains("<script>"));
    assert!(!body.contains(" style="));
    for (path, kind) in [("/app.js", "text/javascript"), ("/app.css", "text/css")] {
        let reply = call(server.port, "GET", path, None, &[]);
        assert_eq!(reply.status, 200);
        assert!(reply.header("Content-Type").unwrap().starts_with(kind));
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in ["mark.svg", "mark-light.svg", "favicon.svg"] {
        let reply = call(server.port, "GET", &format!("/brand/{name}"), None, &[]);
        assert_eq!((reply.status, reply.header("Content-Type")), (200, Some("image/svg+xml")));
        assert_eq!(reply.body, std::fs::read(root.join("brand").join(name)).unwrap(), "{name}");
    }
    for name in ["index.html", "app.css", "app.js"] {
        assert!(root.join("web").join(name).is_file(), "web/{name} is the source of truth");
    }
    assert_eq!(call(server.port, "GET", "/app.js?cache=1", None, &[]).status, 200);
}

#[test]
fn security_headers_on_every_kind_of_response() {
    let server = running();
    for path in ["/", "/nope", "/brand/other.svg", "/brand/../gui.py", "/api/state"] {
        let reply = call(server.port, "GET", path, None, &[]);
        let csp = reply.header("Content-Security-Policy").unwrap();
        assert!(csp.starts_with("default-src 'self'; img-src 'self' data:"));
        for part in ["frame-ancestors 'none'", "form-action 'self'", "base-uri 'none'"] {
            assert!(csp.contains(part));
        }
        assert_eq!(reply.header("X-Frame-Options"), Some("DENY"));
        assert_eq!(reply.header("X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(reply.header("Referrer-Policy"), Some("no-referrer"));
        assert_eq!(reply.header("Cache-Control"), Some("no-store"));
    }
    for path in ["/nope", "/brand/other.svg", "/brand/../gui.py"] {
        assert_eq!(call(server.port, "GET", path, None, &[]).status, 404);
    }
    assert_eq!(call(server.port, "DELETE", "/", None, &[]).status, 501);
}

#[test]
fn state_has_both_providers() {
    let server = running();
    let reply = call(server.port, "GET", "/api/state", None, &[]);
    assert_eq!(reply.status, 200);
    let providers = reply.json()["providers"].clone();
    let names: Vec<&String> = providers.as_object().unwrap().keys().collect();
    assert_eq!(names, ["claude", "codex"]);
    for (name, item) in providers.as_object().unwrap() {
        assert!(item.get("mode").is_some());
        assert_eq!(item["hook"]["client"], name.as_str());
        assert_eq!(item["hook"]["argv"][0], EXE);
    }
}

#[test]
fn post_checks() {
    let server = running();
    let value = json!({"provider": "claude", "weekly": 1, "resets": "x"});
    let body = value.to_string();
    assert_eq!(call(server.port, "POST", "/api/record", Some(body.as_bytes()), &[]).status, 403);
    assert_eq!(call(server.port, "POST", "/api/record", Some(body.as_bytes()), &[("X-Token", "wrong")]).status, 403);
    assert_eq!(call(server.port, "GET", "/", None, &[("Host", "evil.example")]).status, 421);
    let evil_host = [("X-Token", server.token.as_str()), ("Host", "evil.example")];
    assert_eq!(call(server.port, "POST", "/api/record", Some(body.as_bytes()), &evil_host).status, 421);
    assert_eq!(server.post("/api/record", &value, &[("Origin", "http://evil.example")]).status, 403);
    assert_eq!(server.post("/api/record", &value, &[("Origin", "null")]).status, 403);
    let origin = format!("http://localhost:{}", server.port);
    // The origin is accepted, then the reading itself is rejected.
    assert_eq!(server.post("/api/record", &value, &[("Origin", &origin)]).status, 400);
    let host = format!("LOCALHOST:{}", server.port);
    assert_eq!(server.post("/api/record", &value, &[("Host", &host)]).status, 400);
    assert_eq!(call(server.port, "POST", "/api/other", Some(b"{}"), &[("X-Token", &server.token)]).status, 404);
}

#[test]
fn body_limit_and_length_required() {
    let server = running();
    let big = vec![b'x'; 65536 + 1];
    let reply = call(server.port, "POST", "/api/ingest", Some(&big), &[("X-Token", &server.token)]);
    assert_eq!(reply.status, 413);
    let reply = call(server.port, "POST", "/api/ingest", None, &[("X-Token", &server.token)]);
    assert_eq!(reply.status, 411);
}

#[test]
fn record_then_state_reflects_it() {
    let server = running();
    let now = demon_mode::ts::Ts::now();
    let reply = server.post(
        "/api/record",
        &json!({"provider": "codex", "weekly": 40, "resets": (now + 72 * HOUR).iso(), "five_hour": 10,
                "five_hour_resets": (now + 4 * HOUR).iso()}),
        &[],
    );
    assert_eq!(reply.status, 200, "{}", String::from_utf8_lossy(&reply.body));
    assert_eq!(reply.json()["providers"]["codex"]["weekly_used_percent"], 40.0);
    let codex = call(server.port, "GET", "/api/state", None, &[]).json()["providers"]["codex"].clone();
    assert_eq!((codex["weekly_used_percent"].as_f64(), codex["short_used_percent"].as_f64()), (Some(40.0), Some(10.0)));
}

#[test]
fn invalid_record_and_missing_fields() {
    let server = running();
    let reply = server.post("/api/record", &json!({"provider": "claude", "weekly": 150, "resets": "x"}), &[]);
    assert_eq!((reply.status, reply.json()["error"].as_str()), (400, Some("invalid percentage")));
    let reply = server.post("/api/record", &json!({}), &[]);
    assert_eq!(
        (reply.status, reply.json()["error"].as_str()),
        (400, Some("record requires --provider, --weekly, and --resets"))
    );
    let reply = call(server.port, "POST", "/api/record", Some(b"[1]"), &[("X-Token", &server.token)]);
    assert_eq!((reply.status, reply.json()["error"].as_str()), (400, Some("request body must be a JSON object")));
    let reply = server.post("/api/record", &json!({"weekly": 10, "resets": "2026-10-23T12:00:00Z"}), &[]);
    assert_eq!(reply.json()["error"], "unsupported provider or source");
}

#[test]
fn ingest_statusline() {
    let server = running();
    let reset = (demon_mode::ts::Ts::now() + 72 * HOUR).timestamp() as i64;
    let text = json!({"rate_limits": {"seven_day": {"used_percentage": 12.5, "resets_at": reset}}}).to_string();
    let reply = server.post("/api/ingest", &json!({"text": text}), &[]);
    assert_eq!(reply.status, 200, "{}", String::from_utf8_lossy(&reply.body));
    assert_eq!(reply.json()["providers"]["claude"]["weekly_used_percent"], 12.5);
    let reply = server.post("/api/ingest", &json!({"text": "{nope"}), &[]);
    assert_eq!((reply.status, reply.json()["error"].as_str()), (400, Some("input must be valid UTF-8 JSON")));
    assert_eq!(server.post("/api/ingest", &json!({"text": r#"{"rate_limits": {}}"#}), &[]).status, 400);
    assert_eq!(server.post("/api/ingest", &json!({"text": 5}), &[]).status, 400);
    let reply = server.post("/api/ingest", &json!({"text": "{}", "provider": "codex"}), &[]);
    assert_eq!(reply.json()["error"], "Codex app-server import requires the original capture time");
}

#[test]
fn ping() {
    let server = running();
    let reply = call(server.port, "GET", "/api/ping", None, &[]);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), json!({"app": "demon-mode", "database_id": server.database_id}));
    assert!(!String::from_utf8_lossy(&reply.body).contains("state.sqlite3"));
}

#[test]
fn database_identity_normalizes_equivalent_paths_without_exposing_them() {
    let temp = TempDir::new();
    let path = temp.db("ledger.sqlite3");
    let equivalent = temp.path().join(".").join("ledger.sqlite3");
    let identity = database_identity(Some(&path)).unwrap();
    assert_eq!(Some(identity.clone()), database_identity(equivalent.to_str()));
    assert!(!identity.contains(&path));
    assert_eq!(database_identity(Some(":memory:")), None);
}

#[test]
fn default_database_matches_its_explicit_path() {
    let default = default_database_path();
    assert_eq!(database_identity(None), database_identity(default.to_str()));
}

#[test]
fn quit_needs_token_then_stops_server() {
    let mut server = running();
    assert_eq!(call(server.port, "POST", "/api/quit", Some(b"{}"), &[]).status, 403);
    assert!(!server.thread.as_ref().unwrap().is_finished());
    let reply = call(server.port, "POST", "/api/quit", Some(b"{}"), &[("X-Token", &server.token.clone())]);
    assert_eq!(reply.status, 200);
    assert!(server.join(Duration::from_secs(2)));
}

#[test]
fn idle_timeout_stops_server() {
    let mut server = Running::start(None, Duration::from_secs(1));
    assert!(server.join(Duration::from_secs(5)));
}

fn second_instance(database: &str, port: u16, opened: &mut Vec<String>) -> (demon_mode::Result<()>, String) {
    let mut out = Vec::new();
    let mut open = |url: &str| opened.push(url.to_string());
    let options =
        Options { database: Some(database.into()), port, exe: EXE.into(), windows_templates: false, idle: gui::IDLE };
    let result = gui::serve(options, &mut out, Some(&mut open));
    (result, String::from_utf8(out).unwrap())
}

#[test]
fn second_instance_on_same_ledger_exits_zero() {
    let temp = TempDir::new();
    let database = temp.db("s.sqlite3");
    let server = Running::start(Some(database.clone()), gui::IDLE);
    let mut opened = Vec::new();
    let (result, out) = second_instance(&database, server.port, &mut opened);
    assert!(result.is_ok());
    assert_eq!(out, format!("Demon Mode GUI: http://127.0.0.1:{}/\n", server.port));
    assert_eq!(opened, [format!("http://127.0.0.1:{}/", server.port)]);
    let run = invoke(&["--db", &database, "gui", "--no-browser", "--port", &server.port.to_string()]);
    assert_eq!(run.code, 0, "{}", run.err);
}

#[test]
fn second_instance_on_different_ledger_fails_without_opening_or_creating_it() {
    let temp = TempDir::new();
    let server = Running::start(Some(temp.db("existing.sqlite3")), gui::IDLE);
    let requested = temp.db("requested.sqlite3");
    let mut opened = Vec::new();
    let (result, _) = second_instance(&requested, server.port, &mut opened);
    let error = result.unwrap_err().to_string();
    assert!(error.contains("already in use"), "{error}");
    assert!(opened.is_empty());
    assert!(!Path::new(&requested).exists());
    let run = invoke(&["--db", &requested, "gui", "--port", &server.port.to_string()]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("already in use"));
}

#[test]
fn second_instance_reuses_normalized_equivalent_ledger() {
    let temp = TempDir::new();
    let database = temp.path().join("nested").join("state.sqlite3");
    let equivalent = temp.path().join("nested").join(".").join("state.sqlite3");
    let server = Running::start(Some(database.to_string_lossy().into_owned()), gui::IDLE);
    let mut opened = Vec::new();
    let (result, _) = second_instance(equivalent.to_str().unwrap(), server.port, &mut opened);
    assert!(result.is_ok());
}

#[test]
fn port_held_by_another_program_exits_two() {
    let other = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = other.local_addr().unwrap().port().to_string();
    let temp = TempDir::new();
    let run = invoke(&["--db", &temp.db("s.sqlite3"), "gui", "--no-browser", "--port", &port]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("already in use"), "{}", run.err);
}

#[test]
fn gui_subcommand_prints_address() {
    let temp = TempDir::new();
    let mut child = Command::new(demon_binary())
        .args(["--db", &temp.db("s.sqlite3"), "gui", "--no-browser", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let line = line.trim();
    let port = line.strip_prefix("Demon Mode GUI: http://127.0.0.1:").and_then(|rest| rest.strip_suffix('/'));
    assert!(port.is_some_and(|port| port.parse::<u16>().is_ok()), "{line}");
}
