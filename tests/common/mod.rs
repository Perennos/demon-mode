//! Synthetic fixtures and an in-process CLI runner. No credentials, prompts or private data.
#![allow(dead_code)]

use demon_mode::cli::{self, Io};
use demon_mode::governor::{Observation, Policy, Reading, Source, Val, normalize_observation};
use demon_mode::store;
use demon_mode::ts::Ts;
#[allow(unused_imports)]
pub use demon_mode::ts::{HOUR, MINUTE, SECOND};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub const NOW: Ts = Ts::utc(2026, 10, 23, 12, 0, 0);
/// The binary path in-process runs write into hook templates.
pub const EXE: &str = "/opt/perennos/bin/demon";

/// A private temporary folder under the canonical temp path (macOS links `/var` to `/private/var`,
/// and the store refuses linked parents), removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        // Canonical on Unix; on Windows canonicalize adds a `\\?\` prefix, which the Codex reader
        // refuses like any other `\\` path, as the Python release did.
        let base = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            std::fs::canonicalize(std::env::temp_dir()).expect("temp dir")
        };
        let unique = format!(
            "demon-test-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            Ts::now().micros()
        );
        let path = base.join(unique);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A ledger path inside this folder, as a string for `--db`.
    pub fn db(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn reading(provider: &str, at: Ts, used: f64, reset: Ts, short: Option<(f64, Ts)>) -> Reading {
    Reading {
        provider: Some(Val::text(provider)),
        observed_at: Some(Val::Time(at)),
        weekly_used: Some(Val::Num(used)),
        weekly_reset_at: Some(Val::Time(reset)),
        short_used: short.map(|(value, _)| Val::Num(value)),
        short_reset_at: short.map(|(_, reset)| Val::Time(reset)),
    }
}

/// The Python `support.observation` fixture: a manual observation, reset 100 hours out by default.
pub fn observation(at: Ts, used: f64, reset: Option<Ts>, short: Option<(f64, Ts)>, now: Option<Ts>) -> Observation {
    let reset = reset.unwrap_or(at + 100 * HOUR);
    let now = now.unwrap_or(NOW.max(at));
    normalize_observation(&reading("claude", at, used, reset, short), Source::Manual, now).expect("valid fixture")
}

/// `store.record_manual` with the Python test keyword shape.
pub fn record(db: &str, provider: &str, at: Ts, used: f64, reset: Ts, short: Option<(f64, Ts)>, now: Ts) -> Policy {
    store::record_manual(Some(db), &reading(provider, at, used, reset, short), now).expect("record")
}

pub fn try_record(
    db: &str,
    provider: &str,
    at: Ts,
    used: f64,
    reset: Ts,
    short: Option<(f64, Ts)>,
    now: Ts,
) -> demon_mode::Result<Policy> {
    store::record_manual(Some(db), &reading(provider, at, used, reset, short), now)
}

pub fn sql(db: &str) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open ledger for inspection")
}

/// A reader that counts how often it is read, to prove the hook never touches stdin.
pub struct CountingReader {
    pub data: io::Cursor<Vec<u8>>,
    pub reads: usize,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.data.read(buf)
    }
}

/// A stdout that always reports a closed pipe.
pub struct BrokenPipe;

impl Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }
}

pub struct Outcome {
    pub code: i32,
    pub out: String,
    pub err: String,
    pub stdin_reads: usize,
}

/// Options for [`invoke_with`]; the default is a non-terminal run with colour allowed.
pub struct Options<'a> {
    pub stdin: &'a [u8],
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub no_color: bool,
    pub now: Ts,
    pub sleep: Box<dyn FnMut(Duration) -> bool + 'a>,
    pub windows: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            stdin: b"",
            stdin_tty: false,
            stdout_tty: false,
            no_color: false,
            now: NOW,
            sleep: Box::new(|_| true),
            windows: false,
        }
    }
}

fn argv(args: &[&str]) -> Vec<String> {
    std::iter::once("demon").chain(args.iter().copied()).map(str::to_string).collect()
}

/// Run the CLI in-process, like the Python `support.invoke`.
pub fn invoke_with(args: &[&str], mut options: Options) -> Outcome {
    let mut stdin = CountingReader { data: io::Cursor::new(options.stdin.to_vec()), reads: 0 };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = {
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_terminal: options.stdin_tty,
            stdout: &mut stdout,
            stdout_is_terminal: options.stdout_tty,
            stderr: &mut stderr,
            no_color: options.no_color,
            now: options.now,
            sleep: &mut *options.sleep,
            exe: Some(EXE.to_string()),
            windows_templates: options.windows,
            process: false,
        };
        cli::run(argv(args), &mut io)
    };
    Outcome {
        code,
        out: String::from_utf8(stdout).expect("utf-8 stdout"),
        err: String::from_utf8(stderr).expect("utf-8 stderr"),
        stdin_reads: stdin.reads,
    }
}

pub fn invoke(args: &[&str]) -> Outcome {
    invoke_with(args, Options::default())
}

pub fn invoke_at(args: &[&str], now: Ts) -> Outcome {
    invoke_with(args, Options { now, ..Options::default() })
}

pub fn invoke_stdin(args: &[&str], stdin: &[u8]) -> Outcome {
    invoke_with(args, Options { stdin, ..Options::default() })
}

pub fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

/// The real `demon` binary built for this test run.
pub fn demon_binary() -> &'static str {
    env!("CARGO_BIN_EXE_demon")
}
