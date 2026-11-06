//! Perennos DEMON MODE: a local, advisory pace governor for Claude Code and Codex subscriptions.
//!
//! No model calls, no network service, no prompt reading. Readings come from the user: typed in,
//! piped in, or read from one local file the user names.

pub mod cli;
pub mod governor;
pub mod gui;
pub mod hook;
pub mod mcp;
pub mod paths;
pub mod pyjson;
pub mod sources;
pub mod store;
pub mod term;
pub mod ts;

use std::fmt;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The two failure families the Python release distinguished.
#[derive(Debug)]
pub enum Error {
    /// A rejected input (Python `ValueError`/`TypeError`); shown as `demon: <message>`.
    Invalid(String),
    /// Local storage or file access failed (Python `OSError`/`sqlite3.Error`); the detail is kept
    /// for debugging but never printed, so paths and SQL never leak into hook output.
    Unavailable(String),
}

impl Error {
    pub fn invalid(message: impl Into<String>) -> Error {
        Error::Invalid(message.into())
    }

    pub fn unavailable(detail: impl Into<String>) -> Error {
        Error::Unavailable(detail.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(message) => f.write_str(message),
            Error::Unavailable(_) => f.write_str("local input or state is unavailable"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Error {
        Error::Unavailable(error.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Error {
        Error::Unavailable(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
