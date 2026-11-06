//! The `demon` command line: the same subcommands, flags, aliases, output and exit codes as the
//! Python release, plus `mcp` and `tempo` for Perennos Tempo and other MCP hosts.

use crate::governor::{Mode, Policy, Provider, Val, hook_context};
use crate::hook;
use crate::paths;
use crate::pyjson;
use crate::sources;
use crate::store::{self, READ_TIMEOUT, WRITE_TIMEOUT};
use crate::ts::Ts;
use crate::{Error, VERSION, gui, mcp, term};
use clap::{ArgGroup, Args, Parser, Subcommand};
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::time::Duration;

const PROVIDERS: [&str; 2] = ["claude", "codex"];

#[derive(Parser, Debug)]
#[command(
    name = "demon",
    version,
    about = "Perennos DEMON MODE: a local, advisory pace governor for Claude Code and Codex subscriptions.",
    infer_long_args = true,
    disable_help_subcommand = true
)]
struct Cli {
    /// local SQLite path; defaults to this user's data directory
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// record one manual weekly and optional five-hour reading
    Record(RecordArgs),
    /// show current local policy
    Status(StatusArgs),
    /// show numeric observations only
    History(HistoryArgs),
    /// print pasteable advisory text for hosts without hooks
    Context(ProviderArgs),
    /// explain local readiness and any required refresh
    Doctor(DoctorArgs),
    /// opt-in read of one supported local input
    Ingest {
        #[command(subcommand)]
        source: IngestSource,
    },
    /// emit native UserPromptSubmit additionalContext JSON
    Hook(HookArgs),
    /// print a command template; never edits settings
    HookConfig(HookConfigArgs),
    /// run a bounded three-frame terminal preview
    Preview(PreviewArgs),
    /// serve a local page on 127.0.0.1 and open it in a browser
    Gui(GuiArgs),
    /// serve read-only pace tools to an MCP host (Perennos Tempo or any other) over stdio
    Mcp,
    /// print Perennos Tempo integration snippets; never contacts Tempo
    Tempo {
        #[command(subcommand)]
        action: TempoAction,
    },
}

#[derive(Args, Debug)]
struct RecordArgs {
    #[arg(long, value_parser = PROVIDERS)]
    provider: Option<String>,
    /// weekly percent used, 0 to 100
    #[arg(long, value_name = "PERCENT", allow_negative_numbers = true)]
    weekly: Option<f64>,
    /// weekly reset time, ISO 8601 with a UTC offset (a trailing Z works)
    #[arg(long, value_name = "TIME")]
    resets: Option<String>,
    /// five-hour percent used; give it with --five-hour-resets
    #[arg(long = "five-hour", value_name = "PERCENT", visible_alias = "five", allow_negative_numbers = true)]
    five_hour: Option<f64>,
    /// five-hour reset time
    #[arg(long = "five-hour-resets", value_name = "TIME", visible_alias = "five-resets")]
    five_resets: Option<String>,
    /// when the reading was taken (default: now)
    #[arg(long = "at", value_name = "TIME", visible_alias = "observed-at")]
    observed_at: Option<String>,
    /// read one bounded manual JSON object from stdin
    #[arg(long)]
    stdin_json: bool,
    /// print the resulting policy as JSON
    #[arg(long)]
    json: bool,
    /// no colour
    #[arg(long)]
    plain: bool,
}

#[derive(Args, Debug)]
struct ProviderArgs {
    #[arg(long, value_parser = PROVIDERS)]
    provider: String,
}

#[derive(Args, Debug)]
struct StatusArgs {
    #[arg(long, value_parser = PROVIDERS)]
    provider: String,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    plain: bool,
}

#[derive(Args, Debug)]
struct HistoryArgs {
    #[arg(long, value_parser = PROVIDERS)]
    provider: String,
    #[arg(long, default_value_t = 20, allow_negative_numbers = true)]
    limit: i64,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    #[arg(long, value_parser = PROVIDERS)]
    provider: String,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum IngestSource {
    /// Claude Code status-line JSON on standard input
    ClaudeStatusline {
        #[arg(long, required = true)]
        stdin: bool,
        /// original capture time when replaying saved statusline JSON
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// one explicitly selected Codex JSONL session file
    Codex {
        /// one explicitly selected JSONL file
        #[arg(long)]
        file: String,
        #[arg(long)]
        json: bool,
    },
    /// import an exported account/rateLimits response
    #[command(group(ArgGroup::new("export").required(true).args(["stdin", "file"])))]
    CodexAppServer {
        #[arg(long)]
        stdin: bool,
        /// one explicitly selected JSON export
        #[arg(long)]
        file: Option<String>,
        /// original observation time, including UTC offset
        #[arg(long)]
        at: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct HookArgs {
    #[arg(long, value_parser = PROVIDERS)]
    client: String,
    /// opt-in refresh from this explicit Codex JSONL file before output
    #[arg(long)]
    codex_file: Option<String>,
}

#[derive(Args, Debug)]
struct HookConfigArgs {
    #[arg(long, value_parser = PROVIDERS)]
    client: String,
    /// include explicit Codex JSONL refresh in the command
    #[arg(long)]
    codex_file: Option<String>,
    #[arg(long, value_parser = ["template", "native"], default_value = "template")]
    format: String,
}

#[derive(Args, Debug)]
struct PreviewArgs {
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..=3))]
    seconds: u64,
    #[arg(long)]
    plain: bool,
}

#[derive(Args, Debug)]
struct GuiArgs {
    /// port to listen on (default 47823; 0 picks a free one)
    #[arg(long, default_value_t = gui::DEFAULT_PORT)]
    port: u16,
    /// print the address and do not open a browser
    #[arg(long)]
    no_browser: bool,
}

#[derive(Subcommand, Debug)]
enum TempoAction {
    /// print a Tempo tool-policy fragment for the read-only demon_* tools
    Policy,
    /// print a secret-free MCP client entry with absolute paths
    McpConfig,
}

/// Everything a command touches outside itself, so tests can run the CLI in-process.
pub struct Io<'a> {
    pub stdin: &'a mut dyn Read,
    pub stdin_is_terminal: bool,
    pub stdout: &'a mut dyn Write,
    pub stdout_is_terminal: bool,
    pub stderr: &'a mut dyn Write,
    pub no_color: bool,
    pub now: Ts,
    /// Sleep for the given time; `false` means Ctrl+C arrived.
    pub sleep: &'a mut dyn FnMut(Duration) -> bool,
    /// The binary path written into templates; computed on first use when `None`.
    pub exe: Option<String>,
    /// Print Windows (PowerShell) hook templates.
    pub windows_templates: bool,
    /// Running as the real process: install the Ctrl+C handler, open a browser.
    pub process: bool,
}

impl Io<'_> {
    fn exe(&mut self) -> String {
        self.exe.get_or_insert_with(paths::current_exe).clone()
    }
}

enum Fail {
    Demon(Error),
    Output(io::Error),
}

impl From<Error> for Fail {
    fn from(error: Error) -> Fail {
        Fail::Demon(error)
    }
}

type Outcome = std::result::Result<i32, Fail>;

fn emit(io: &mut Io, text: &str) -> std::result::Result<(), Fail> {
    io.stdout.write_all(text.as_bytes()).map_err(Fail::Output)
}

fn emit_json(io: &mut Io, value: &Value) -> std::result::Result<(), Fail> {
    emit(io, &(pyjson::pretty(value) + "\n"))
}

/// Run one command line; returns the process exit code.
pub fn run<I, T>(args: I, io: &mut Io) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let text = error.render().to_string();
            let target: &mut dyn Write = if error.use_stderr() { &mut *io.stderr } else { &mut *io.stdout };
            let _ = target.write_all(text.as_bytes());
            return error.exit_code();
        }
    };
    let outcome = match cli.command {
        Command::Hook(args) => hook_command(cli.db.as_deref(), args, io),
        command => dispatch(cli.db.as_deref(), command, io),
    };
    let outcome = outcome.and_then(|code| io.stdout.flush().map(|_| code).map_err(Fail::Output));
    match outcome {
        Ok(code) => code,
        Err(Fail::Output(error)) if error.kind() == ErrorKind::BrokenPipe => 0,
        Err(Fail::Output(_)) => {
            let _ = writeln!(io.stderr, "demon: local input or state is unavailable");
            2
        }
        Err(Fail::Demon(error)) => {
            let _ = writeln!(io.stderr, "demon: {error}");
            2
        }
    }
}

fn provider(name: &str) -> Provider {
    Provider::parse(name).expect("clap restricts provider names")
}

/// The ten-slot weekly gauge: one `#` per full ten percent used.
pub fn gauge(policy: &Policy) -> String {
    match policy.weekly_used {
        None => "----------".into(),
        Some(used) => {
            let filled = ((used / 10.0) as i64).clamp(0, 10) as usize;
            format!("{}{}", "#".repeat(filled), "-".repeat(10 - filled))
        }
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".into(), |v| format!("{v:.1}%"))
}

/// The one-line status, as `demon status` prints it.
pub fn status_line(policy: &Policy, color: bool) -> String {
    let estimate = percent(policy.estimated_weekly_used);
    let ratio = policy.ratio.map_or_else(|| "unknown".into(), |v| format!("{v:.2}x"));
    let reset = policy.weekly_reset_at.map_or_else(|| "unknown".into(), Ts::iso);
    let mut label = policy.mode.name().to_uppercase();
    if color {
        label = format!("\x1b[{}m{label}\x1b[0m", policy.mode.color());
    }
    let refresh = if policy.refresh_required { "required" } else { "not_required" };
    format!(
        "{label} [{}] weekly={} estimated={estimate} headroom={ratio} reset={reset} five_hour={} reason={} refresh={refresh}",
        gauge(policy),
        percent(policy.weekly_used),
        percent(policy.short_used),
        policy.reason,
    )
}

fn show_policy(io: &mut Io, policy: &Policy, as_json: bool, plain: bool) -> std::result::Result<(), Fail> {
    if as_json {
        return emit_json(io, &policy.as_json(io.now));
    }
    let color = io.stdout_is_terminal && !plain && !io.no_color;
    emit(io, &(status_line(policy, color) + "\n"))
}

/// Python truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// The fields of a manual reading as `record` flags or the GUI form give them.
#[derive(Default)]
pub struct ManualArgs {
    pub provider: Option<Val>,
    pub weekly: Option<Val>,
    pub resets: Option<Val>,
    pub five_hour: Option<Val>,
    pub five_resets: Option<Val>,
    /// `None` (or anything falsy, which callers map to `None`) stamps the reading now.
    pub observed_at: Option<Val>,
}

pub fn record_from_args(database: Option<&str>, args: ManualArgs, now: Ts) -> crate::Result<Policy> {
    if args.provider.is_none() || args.weekly.is_none() || args.resets.is_none() {
        return Err(Error::invalid("record requires --provider, --weekly, and --resets"));
    }
    let reading = crate::governor::Reading {
        provider: args.provider,
        observed_at: Some(args.observed_at.unwrap_or(Val::Time(now))),
        weekly_used: args.weekly,
        weekly_reset_at: args.resets,
        short_used: args.five_hour,
        short_reset_at: args.five_resets,
    };
    store::record_manual(database, &reading, now)
}

fn record(database: Option<&str>, args: RecordArgs, io: &mut Io) -> Outcome {
    let json_input = args.stdin_json
        || (args.provider.is_none() && args.weekly.is_none() && args.resets.is_none() && !io.stdin_is_terminal);
    let policy = if json_input {
        let value = sources::read_json(io.stdin, sources::MAX_MANUAL_BYTES)?;
        const FIELDS: [&str; 6] =
            ["provider", "observed_at", "weekly_used", "weekly_reset_at", "short_used", "short_reset_at"];
        let map = value
            .as_object()
            .filter(|map| FIELDS[..4].iter().all(|key| map.contains_key(*key)))
            .filter(|map| map.keys().all(|key| FIELDS.contains(&key.as_str())))
            .ok_or_else(|| Error::invalid("manual input has missing or unsupported fields"))?;
        let get = |key: &str| map.get(key).and_then(Val::from_json);
        let reading = crate::governor::Reading {
            provider: get("provider"),
            observed_at: get("observed_at"),
            weekly_used: get("weekly_used"),
            weekly_reset_at: get("weekly_reset_at"),
            short_used: get("short_used"),
            short_reset_at: get("short_reset_at"),
        };
        store::record_manual(database, &reading, io.now)?
    } else {
        let manual = ManualArgs {
            provider: args.provider.as_deref().map(Val::text),
            weekly: args.weekly.map(Val::Num),
            resets: args.resets.as_deref().map(Val::text),
            five_hour: args.five_hour.map(Val::Num),
            five_resets: args.five_resets.as_deref().map(Val::text),
            observed_at: args.observed_at.as_deref().filter(|at| !at.is_empty()).map(Val::text),
        };
        record_from_args(database, manual, io.now)?
    };
    show_policy(io, &policy, args.json, args.plain)?;
    Ok(0)
}

fn history(database: Option<&str>, args: HistoryArgs, io: &mut Io) -> Outcome {
    let observations = store::load_history(database, provider(&args.provider), args.limit, io.now)?;
    let rows: Vec<Value> = observations
        .iter()
        .map(|item| {
            let mut row = Map::new();
            row.insert("observed_at".into(), item.observed_at.iso().into());
            row.insert("weekly_used_percent".into(), pyjson::float(item.weekly_used));
            row.insert("weekly_reset_at".into(), item.weekly_reset_at.iso().into());
            row.insert("short_used_percent".into(), pyjson::opt_float(item.short_used));
            row.insert("short_reset_at".into(), item.short_reset_at.map_or(Value::Null, |at| at.iso().into()));
            Value::Object(row)
        })
        .collect();
    if args.json {
        emit_json(io, &Value::Array(rows))?;
        return Ok(0);
    }
    let lines: Vec<String> = observations
        .iter()
        .map(|item| {
            format!(
                "{} weekly={:.1}% weekly_reset={} five_hour={} five_hour_reset={}",
                item.observed_at.iso(),
                item.weekly_used,
                item.weekly_reset_at.iso(),
                percent(item.short_used),
                item.short_reset_at.map_or_else(|| "unknown".into(), Ts::iso),
            )
        })
        .collect();
    let text = if lines.is_empty() { "No local observations.".to_string() } else { lines.join("\n") };
    emit(io, &(text + "\n"))?;
    Ok(0)
}

fn ledger_path(database: Option<&str>) -> std::path::PathBuf {
    match database {
        Some(value) => paths::absolute(&paths::expanduser(value)),
        None => paths::absolute(&paths::default_database_path()),
    }
}

fn doctor(database: Option<&str>, args: DoctorArgs, io: &mut Io) -> Outcome {
    let provider = provider(&args.provider);
    let ledger = match database {
        Some(":memory:") => None,
        _ => Some(ledger_path(database)),
    };
    let present = ledger.as_ref().is_some_and(|path| path.exists());
    let policy = store::load_policy(database, provider, io.now);
    let first_step = if policy.reason == "state_unavailable" {
        "Check database permissions, locks, and schema compatibility; preserve the existing database."
    } else if policy.refresh_required {
        "Import a supported provider reading or record measured usage with its original observation time."
    } else {
        "Review hook-config output, merge it additively, and verify delivery in the native client."
    };
    let migration = format!(
        "If a client hook still runs `python -m demon_mode`, replace that one command with the output of \
         `demon hook-config --client {}` and keep every other hook.",
        provider.name()
    );
    let steps = [first_step.to_string(), migration];
    let exe = io.exe();
    let tempo = "MCP tools through `demon mcp` (read-only); `demon tempo mcp-config` prints the entry. \
                 No network check is made: Demon Mode never contacts Tempo or any other service.";
    if args.json {
        let report = json!({
            "version": VERSION,
            "runtime": "rust",
            "executable": exe,
            "provider": provider.name(),
            "mode": policy.mode.name(),
            "source": policy.source,
            "reason": policy.reason,
            "refresh_required": policy.refresh_required,
            "host_integration": "Untested",
            "ledger": ledger.as_ref().map_or_else(|| ":memory:".to_string(), |path| path.to_string_lossy().into_owned()),
            "ledger_present": present,
            "tempo": tempo,
            "next_steps": steps,
        });
        emit_json(io, &report)?;
        return Ok(0);
    }
    let ledger_line = match &ledger {
        None => "Ledger: in memory (nothing is kept)".to_string(),
        Some(path) if present => format!("Ledger: {}", path.display()),
        Some(path) => format!("Ledger: {} (not created yet)", path.display()),
    };
    let text = format!(
        "Demon Mode {VERSION}; {}\n{}\n{ledger_line}\nBinary: {exe}\nNative host delivery: Untested.\nTempo: {tempo}\n{}\n",
        provider.name(),
        status_line(&policy, false),
        steps.join("\n"),
    );
    emit(io, &text)?;
    Ok(0)
}

fn ingest(database: Option<&str>, source: IngestSource, io: &mut Io) -> Outcome {
    let now = io.now;
    let (observation, as_json) = match source {
        IngestSource::ClaudeStatusline { at, json, .. } => {
            let payload = sources::read_json(io.stdin, sources::MAX_STATUSLINE_BYTES)?;
            (sources::statusline_observation(&payload, now, at.as_deref())?, json)
        }
        IngestSource::Codex { file, json } => (sources::codex_observation(&file, now)?, json),
        IngestSource::CodexAppServer { stdin, file, at, json } => {
            let payload = match (stdin, file) {
                (true, _) => sources::read_json(io.stdin, sources::MAX_STATUSLINE_BYTES)?,
                (false, Some(file)) => {
                    let mut handle = sources::open_selected_file(&file, "Codex export")?;
                    sources::read_json(&mut handle, sources::MAX_STATUSLINE_BYTES)?
                }
                (false, None) => unreachable!("clap requires --stdin or --file"),
            };
            (sources::app_server_observation(&payload, Some(Val::Text(at)), now)?, json)
        }
    };
    let policy = store::record_observation(database, &observation, now, WRITE_TIMEOUT)?;
    show_policy(io, &policy, as_json, true)?;
    Ok(0)
}

fn preview(seconds: u64, plain: bool, io: &mut Io) -> Outcome {
    if !io.stdout_is_terminal || plain || io.no_color {
        emit(io, "DEMON preview [###-------] (three-frame preview unavailable in static output)\n")?;
        return Ok(0);
    }
    if io.process {
        term::catch_interrupt();
    }
    let frames = ["DEMON [#---------] 1/3", "DEMON [#####-----] 2/3", "DEMON [##########] 3/3"];
    let colors = ["34", "33", "31"];
    emit(io, "\x1b[?25l")?;
    let mut code = 0;
    for (index, frame) in frames.iter().enumerate() {
        emit(io, &format!("\r\x1b[{}m{frame}\x1b[0m", colors[index]))?;
        io.stdout.flush().map_err(Fail::Output)?;
        if index < frames.len() - 1 && !(io.sleep)(Duration::from_millis(seconds * 500)) {
            code = 130;
            break;
        }
    }
    emit(io, "\x1b[?25h\n")?;
    Ok(code)
}

fn dispatch(database: Option<&str>, command: Command, io: &mut Io) -> Outcome {
    match command {
        Command::Record(args) => record(database, args, io),
        Command::Status(args) => {
            let policy = store::load_policy(database, provider(&args.provider), io.now);
            show_policy(io, &policy, args.json, args.plain)?;
            Ok(0)
        }
        Command::History(args) => history(database, args, io),
        Command::Context(args) => {
            let policy = store::load_policy(database, provider(&args.provider), io.now);
            emit(io, &(hook_context(&policy, 2048) + "\n"))?;
            Ok(0)
        }
        Command::Doctor(args) => doctor(database, args, io),
        Command::Ingest { source } => ingest(database, source, io),
        Command::HookConfig(args) => {
            let exe = io.exe();
            let windows = io.windows_templates;
            let config =
                hook::hook_config(&exe, provider(&args.client), database, args.codex_file.as_deref(), windows)?;
            let shown = if args.format == "native" { &config["config"] } else { &config };
            emit_json(io, shown)?;
            Ok(0)
        }
        Command::Preview(args) => preview(args.seconds, args.plain, io),
        Command::Gui(args) => {
            let exe = io.exe();
            let options = gui::Options {
                database: database.map(str::to_string),
                port: args.port,
                exe,
                windows_templates: io.windows_templates,
                idle: gui::IDLE,
            };
            let process = io.process;
            let mut open = |url: &str| {
                if process {
                    gui::open_browser(url);
                }
            };
            let opener: Option<&mut dyn FnMut(&str)> = if args.no_browser { None } else { Some(&mut open) };
            if process {
                term::catch_interrupt();
            }
            gui::serve(options, io.stdout, opener)?;
            Ok(0)
        }
        Command::Mcp => {
            let mut input = BufReader::new(&mut *io.stdin);
            mcp::serve(database, &mut input, io.stdout, &Ts::now).map_err(Fail::Output)?;
            Ok(0)
        }
        Command::Tempo { action } => {
            let value = match action {
                TempoAction::Policy => mcp::tempo_policy(),
                TempoAction::McpConfig => {
                    let exe = io.exe();
                    mcp::mcp_config(&exe, database)?
                }
            };
            emit_json(io, &value)?;
            Ok(0)
        }
        Command::Hook(_) => unreachable!("handled by hook_command"),
    }
}

/// The per-prompt hook: never reads standard input, never fails open, always prints advice.
fn hook_command(database: Option<&str>, args: HookArgs, io: &mut Io) -> Outcome {
    let client = provider(&args.client);
    let codex_file = args.codex_file.filter(|file| !file.is_empty());
    if codex_file.is_some() && client != Provider::Codex {
        return Err(Error::invalid("--codex-file requires --client codex").into());
    }
    let now = io.now;
    let refresh_failed = codex_file.is_some_and(|file| {
        sources::codex_observation(&file, now)
            .and_then(|observation| store::record_observation(database, &observation, now, READ_TIMEOUT))
            .is_err()
    });
    let mut policy = store::load_policy(database, client, now);
    if refresh_failed {
        if !matches!(policy.mode, Mode::Austere | Mode::Lean) {
            policy.mode = Mode::Lean;
        }
        policy.reason = "input_refresh_unavailable";
        policy.freshness = "input_unavailable";
        policy.refresh_required = true;
    }
    emit(io, &(hook::hook_json(&policy) + "\n"))?;
    Ok(0)
}

/// Entry point for the `demon` binary.
pub fn main() -> std::process::ExitCode {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr();
    let mut sleep = term::sleep;
    let mut io = Io {
        stdin_is_terminal: term::stdin_is_terminal(),
        stdout_is_terminal: term::stdout_is_terminal(),
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        no_color: term::no_color(),
        now: Ts::now(),
        sleep: &mut sleep,
        exe: None,
        windows_templates: cfg!(windows),
        process: true,
    };
    let code = run(std::env::args_os(), &mut io);
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(2))
}
