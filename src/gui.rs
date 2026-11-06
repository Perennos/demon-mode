//! The local page: a thin shell over the code the CLI already runs.
//!
//! Loopback only, embedded assets, Host and Origin checks, a per-run token for every state-changing
//! request, bounded bodies, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, a strict
//! CSP, a 30-minute idle shutdown, and single-instance reuse bound to the selected ledger.

use crate::cli::{ManualArgs, record_from_args, truthy};
use crate::governor::{Provider, Val};
use crate::hook;
use crate::paths;
use crate::pyjson;
use crate::sources;
use crate::store::{self, WRITE_TIMEOUT};
use crate::ts::Ts;
use crate::{Error, Result, term};
use serde_json::{Map, Value, json};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response};

pub const DEFAULT_PORT: u16 = 47823;
pub const IDLE: Duration = Duration::from_secs(30 * 60);
pub const MAX_BODY_BYTES: usize = 65536;

const CSP: &str =
    "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; form-action 'self'; base-uri 'none'";
const SECURITY_HEADERS: [(&str, &str); 5] = [
    ("Content-Security-Policy", CSP),
    ("X-Frame-Options", "DENY"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
];

const INDEX_HTML: &str = include_str!("../web/index.html");
const ASSETS: [(&str, &[u8], &str); 5] = [
    ("/app.css", include_bytes!("../web/app.css"), "text/css; charset=utf-8"),
    ("/app.js", include_bytes!("../web/app.js"), "text/javascript; charset=utf-8"),
    ("/brand/mark.svg", include_bytes!("../brand/mark.svg"), "image/svg+xml"),
    ("/brand/mark-light.svg", include_bytes!("../brand/mark-light.svg"), "image/svg+xml"),
    ("/brand/favicon.svg", include_bytes!("../brand/favicon.svg"), "image/svg+xml"),
];

/// How to run one GUI server.
#[derive(Clone, Debug)]
pub struct Options {
    pub database: Option<String>,
    pub port: u16,
    pub exe: String,
    pub windows_templates: bool,
    pub idle: Duration,
}

struct Context {
    database: Option<String>,
    database_id: Option<String>,
    token: String,
    hosts: [String; 4],
    origins: [String; 2],
    exe: String,
    windows_templates: bool,
    last_request: Mutex<Instant>,
    stop: AtomicBool,
}

pub struct GuiServer {
    server: tiny_http::Server,
    context: Arc<Context>,
    port: u16,
    idle: Duration,
}

/// Stops a running [`GuiServer`] from another thread.
#[derive(Clone)]
pub struct StopHandle(Arc<Context>);

impl StopHandle {
    pub fn stop(&self) {
        self.0.stop.store(true, Ordering::SeqCst);
    }
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(hook::base64_url(&bytes))
}

impl GuiServer {
    /// Bind 127.0.0.1 only. The ledger is not opened until a request needs it.
    pub fn bind(options: Options) -> io::Result<GuiServer> {
        let listener = TcpListener::bind(("127.0.0.1", options.port))?;
        let port = listener.local_addr()?.port();
        let server =
            tiny_http::Server::from_listener(listener, None).map_err(|error| io::Error::other(error.to_string()))?;
        let token = random_token()?;
        let context = Context {
            database_id: paths::database_identity(options.database.as_deref()),
            database: options.database,
            token,
            hosts: ["127.0.0.1".into(), "localhost".into(), format!("127.0.0.1:{port}"), format!("localhost:{port}")],
            origins: [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")],
            exe: options.exe,
            windows_templates: options.windows_templates,
            last_request: Mutex::new(Instant::now()),
            stop: AtomicBool::new(false),
        };
        Ok(GuiServer { server, context: Arc::new(context), port, idle: options.idle })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.context.token
    }

    pub fn database_id(&self) -> Option<&str> {
        self.context.database_id.as_deref()
    }

    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(Arc::clone(&self.context))
    }

    /// Serve until Quit, the idle timeout, Ctrl+C, or a [`StopHandle`]. Each request gets a thread.
    pub fn run(self) {
        loop {
            if self.context.stop.load(Ordering::SeqCst) || term::interrupted() {
                break;
            }
            let idle_for = self.context.last_request.lock().map(|last| last.elapsed()).unwrap_or_default();
            if idle_for > self.idle {
                break;
            }
            let wait = Duration::from_millis(200).min(self.idle.saturating_sub(idle_for) + Duration::from_millis(1));
            match self.server.recv_timeout(wait) {
                Ok(Some(request)) => {
                    if let Ok(mut last) = self.context.last_request.lock() {
                        *last = Instant::now();
                    }
                    let context = Arc::clone(&self.context);
                    std::thread::spawn(move || handle(&context, request));
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request.headers().iter().find(|header| header.field.equiv(name)).map(|header| header.value.as_str())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
}

impl Reply {
    fn json(status: u16, value: &Value) -> Reply {
        Reply { status, body: pyjson::compact(value).into_bytes(), content_type: "application/json" }
    }

    fn error(status: u16, text: &str) -> Reply {
        Reply { status, body: format!("{status} {text}\n").into_bytes(), content_type: "text/plain; charset=utf-8" }
    }

    fn message(status: u16, message: &str) -> Reply {
        Reply::json(status, &json!({"error": message}))
    }
}

fn handle(context: &Context, mut request: Request) {
    let (reply, quit) = route(context, &mut request);
    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    let mut headers = vec![("Content-Type", reply.content_type)];
    headers.extend(SECURITY_HEADERS);
    for (name, value) in headers {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
    if quit {
        context.stop.store(true, Ordering::SeqCst);
    }
}

fn route(context: &Context, request: &mut Request) -> (Reply, bool) {
    let host = header(request, "Host").unwrap_or("").to_lowercase();
    if !context.hosts.contains(&host) {
        return (Reply::error(421, "Misdirected Request"), false);
    }
    let target = request.url();
    let path = target.split(['?', '#']).next().unwrap_or("").to_string();
    match request.method() {
        Method::Get => (get(context, &path), false),
        Method::Post => post(context, request, &path),
        _ => (Reply::error(501, "Unsupported method"), false),
    }
}

fn get(context: &Context, path: &str) -> Reply {
    match path {
        "/api/ping" => Reply::json(200, &json!({"app": "demon-mode", "database_id": context.database_id})),
        "/api/state" => match state(context) {
            Ok(value) => Reply::json(200, &value),
            Err(error) => failure(&error),
        },
        "/" => Reply {
            status: 200,
            body: INDEX_HTML.replace("__TOKEN__", &context.token).into_bytes(),
            content_type: "text/html; charset=utf-8",
        },
        _ => match ASSETS.iter().find(|(name, _, _)| *name == path) {
            Some((_, body, kind)) => Reply { status: 200, body: body.to_vec(), content_type: kind },
            None => Reply::error(404, "Not Found"),
        },
    }
}

fn failure(error: &Error) -> Reply {
    match error {
        Error::Invalid(message) => Reply::message(400, message),
        Error::Unavailable(_) => Reply::message(500, "local input or state is unavailable"),
    }
}

fn post(context: &Context, request: &mut Request, path: &str) -> (Reply, bool) {
    type Action = fn(Option<&str>, &Map<String, Value>) -> Result<()>;
    let action: Option<Action> = match path {
        "/api/record" => Some(record),
        "/api/ingest" => Some(ingest),
        "/api/quit" => None,
        _ => return (Reply::error(404, "Not Found"), false),
    };
    let origin_ok = header(request, "Origin").is_none_or(|origin| context.origins.iter().any(|ok| ok == origin));
    let supplied = header(request, "X-Token").unwrap_or("");
    if !origin_ok || !constant_time_eq(supplied.as_bytes(), context.token.as_bytes()) {
        return (Reply::message(403, "forbidden"), false);
    }
    let Some(action) = action else {
        return (Reply::json(200, &json!({"ok": true})), true);
    };
    let length = header(request, "Content-Length").and_then(|value| value.trim().parse::<i64>().ok()).unwrap_or(-1);
    if length < 0 {
        return (Reply::message(411, "Content-Length is required"), false);
    }
    if length as u64 > MAX_BODY_BYTES as u64 {
        return (Reply::message(413, "request body is too large"), false);
    }
    let mut raw = Vec::new();
    if request.as_reader().take(length as u64).read_to_end(&mut raw).is_err() {
        return (Reply::message(400, "request body must be a JSON object"), false);
    }
    let text = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(text) else {
        return (Reply::message(400, "request body must be a JSON object"), false);
    };
    if let Err(error) = action(context.database.as_deref(), &body) {
        return (failure(&error), false);
    }
    match state(context) {
        Ok(value) => (Reply::json(200, &value), false),
        Err(error) => (failure(&error), false),
    }
}

/// Same calls as `status --json`, once per provider, plus that client's hook template.
fn state(context: &Context) -> Result<Value> {
    let now = Ts::now();
    let database = context.database.as_deref();
    let mut providers = Map::new();
    for provider in Provider::ALL {
        let mut item = store::load_policy(database, provider, now).as_json(now);
        let template = hook::hook_config(&context.exe, provider, database, None, context.windows_templates)?;
        if let Value::Object(map) = &mut item {
            map.insert("hook".into(), template);
        }
        providers.insert(provider.name().into(), item);
    }
    Ok(json!({"providers": providers}))
}

fn record(database: Option<&str>, body: &Map<String, Value>) -> Result<()> {
    let get = |key: &str| body.get(key).and_then(Val::from_json);
    // "" rather than None, so a missing provider is reported as unsupported, never read from stdin.
    let provider = match body.get("provider") {
        Some(value) if truthy(value) => Val::from_json(value),
        _ => Some(Val::text("")),
    };
    let args = ManualArgs {
        provider,
        weekly: get("weekly"),
        resets: get("resets"),
        five_hour: get("five_hour"),
        five_resets: get("five_hour_resets"),
        observed_at: body.get("at").filter(|value| truthy(value)).and_then(Val::from_json),
    };
    record_from_args(database, args, Ts::now()).map(|_| ())
}

fn ingest(database: Option<&str>, body: &Map<String, Value>) -> Result<()> {
    let text = body.get("text").and_then(Value::as_str).ok_or_else(|| Error::invalid("text must be a string"))?;
    let provider = match body.get("provider") {
        None => Some("claude"),
        Some(value) => value.as_str(),
    };
    let now = Ts::now();
    let payload = sources::parse_json_bytes(text.as_bytes(), sources::MAX_STATUSLINE_BYTES)?;
    let observation = match provider {
        Some("claude") => sources::statusline_observation(&payload, now, None)?,
        Some("codex") => {
            let at = body
                .get("at")
                .and_then(Value::as_str)
                .filter(|at| !at.is_empty())
                .ok_or_else(|| Error::invalid("Codex app-server import requires the original capture time"))?;
            sources::app_server_observation(&payload, Some(Val::text(at)), now)?
        }
        _ => return Err(Error::invalid("provider must be claude or codex")),
    };
    store::record_observation(database, &observation, now, WRITE_TIMEOUT).map(|_| ())
}

/// Ask whatever answers on the port whether it is a Demon Mode page for the same ledger.
fn is_demon_mode(port: u16, database: Option<&str>) -> bool {
    let Some(expected) = paths::database_identity(database) else {
        return false;
    };
    let Ok(body) = ping(port) else {
        return false;
    };
    let body = &body[..body.len().min(1024)];
    let Ok(Value::Object(payload)) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    payload.get("app").and_then(Value::as_str) == Some("demon-mode")
        && payload
            .get("database_id")
            .and_then(Value::as_str)
            .is_some_and(|id| constant_time_eq(id.as_bytes(), expected.as_bytes()))
}

fn ping(port: u16) -> io::Result<Vec<u8>> {
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET /api/ping HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.take(65536).read_to_end(&mut response)?;
    let split =
        response.windows(4).position(|window| window == b"\r\n\r\n").ok_or_else(|| io::Error::other("no headers"))?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(io::Error::other("not ok"));
    }
    Ok(response[split + 4..].to_vec())
}

/// Open the default browser; failures are ignored, the address is printed anyway.
pub fn open_browser(url: &str) {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").creation_flags(0x0800_0000);
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = Command::new("xdg-open");
    let _ = command.arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

fn strerror(error: &io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(index) => text[..index].to_string(),
        None => text,
    }
}

/// `demon gui`: serve, or hand over to a page already serving the same ledger on that port.
pub fn serve(options: Options, out: &mut dyn Write, mut open: Option<&mut dyn FnMut(&str)>) -> Result<()> {
    let port = options.port;
    let database = options.database.clone();
    let server = match GuiServer::bind(options) {
        Ok(server) => server,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let url = format!("http://127.0.0.1:{port}/");
            if is_demon_mode(port, database.as_deref()) {
                let _ = writeln!(out, "Demon Mode GUI: {url}");
                let _ = out.flush();
                if let Some(open) = open.as_mut() {
                    open(&url);
                }
                return Ok(());
            }
            return Err(Error::invalid(format!(
                "port {port} is already in use by another program; pick another with --port"
            )));
        }
        Err(error) => return Err(Error::invalid(format!("cannot listen on 127.0.0.1:{port}: {}", strerror(&error)))),
    };
    let url = format!("http://127.0.0.1:{}/", server.port());
    let _ = writeln!(out, "Demon Mode GUI: {url}");
    let _ = out.flush();
    if let Some(open) = open.as_mut() {
        open(&url);
    }
    server.run();
    Ok(())
}
