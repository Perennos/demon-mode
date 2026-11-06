//! Filesystem paths: the default ledger location, `~` expansion, absolute paths, and the guard that
//! keeps SQLite from following a linked or non-regular state path.

use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// The user's home directory, as Python's `Path.home()` finds it.
pub fn home() -> Option<PathBuf> {
    #[cfg(windows)]
    let found = env_path("USERPROFILE");
    #[cfg(not(windows))]
    let found = env_path("HOME");
    #[allow(deprecated)]
    found.or_else(std::env::home_dir)
}

/// The per-user ledger path. Unchanged from the Python release, so existing ledgers keep working:
/// `%LOCALAPPDATA%\Perennos\demon-mode\state.sqlite3` on Windows and
/// `${XDG_DATA_HOME:-~/.local/share}/Perennos/demon-mode/state.sqlite3` elsewhere (macOS included).
pub fn default_database_path() -> PathBuf {
    let home = || home().unwrap_or_default();
    #[cfg(windows)]
    let base = env_path("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData").join("Local"));
    #[cfg(not(windows))]
    let base = env_path("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local").join("share"));
    base.join("Perennos").join("demon-mode").join("state.sqlite3")
}

/// `Path.expanduser()` for `~` and `~/...` (and `~\...` on Windows).
pub fn expanduser(value: &str) -> PathBuf {
    let rest = if value == "~" {
        Some("")
    } else if let Some(rest) = value.strip_prefix("~/") {
        Some(rest)
    } else if cfg!(windows) {
        value.strip_prefix("~\\")
    } else {
        None
    };
    match (rest, home()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(value),
    }
}

/// `Path.absolute()`: join the working directory and drop `.` components, keeping `..`.
pub fn absolute(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        joined.components().filter(|part| *part != Component::CurDir).collect()
    }
}

/// `str(Path(value).expanduser().absolute())`, as hook and MCP templates print it.
pub fn absolute_text(value: &str) -> String {
    absolute(&expanduser(value)).to_string_lossy().into_owned()
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_type().is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Reject linked or non-regular state paths before SQLite can follow them.
pub fn validate_state_path(path: &Path) -> Result<()> {
    let path = absolute(path);
    let mut parent = PathBuf::new();
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => parent.push(component.as_os_str()),
            other => parts.push(other),
        }
    }
    if let Some((_, between)) = parts.split_last() {
        for part in between {
            parent.push(part.as_os_str());
            match fs::symlink_metadata(&parent) {
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
                Ok(info) if is_link(&info) => return Err(Error::unavailable("state parent must not be a link")),
                Ok(info) if !info.is_dir() => return Err(Error::unavailable("state parent must be a directory")),
                Ok(_) => {}
            }
        }
    }
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(info) if is_link(&info) => Err(Error::unavailable("state file must not be a link")),
        Ok(info) if !info.is_file() => Err(Error::unavailable("state file must be regular")),
        Ok(_) => Ok(()),
    }
}

/// `Path.resolve(strict=False)`: resolve links in the part that exists, keep the rest lexically.
fn resolve_lenient(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    let mut exists = true;
    for component in absolute(path).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if exists {
                    match fs::canonicalize(&resolved) {
                        Ok(real) => resolved = real,
                        Err(_) => exists = false,
                    }
                }
            }
        }
    }
    strip_verbatim(resolved)
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// A stable, non-reversible identifier for the ledger a GUI serves (`None` for `:memory:`).
///
/// Matches the Python release's `sha256(normcase(normpath(resolve(path))))`, so a second launch can
/// tell whether the page already running on the port serves the same ledger.
pub fn database_identity(database: Option<&str>) -> Option<String> {
    let path = match database {
        Some(":memory:") => return None,
        Some(value) => expanduser(value),
        None => default_database_path(),
    };
    let resolved = resolve_lenient(&path);
    let text = resolved.to_string_lossy().into_owned();
    #[cfg(windows)]
    let text = text.replace('/', "\\").to_lowercase();
    Some(hex(&Sha256::digest(text.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The absolute path of the running binary, for hook and MCP templates.
pub fn current_exe() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("demon"));
    #[cfg(not(windows))]
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    exe.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn absolute_drops_dot_components_and_keeps_parent_references() {
        assert_eq!(absolute(Path::new("/a/./b/../c/")), PathBuf::from("/a/b/../c"));
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute(Path::new("./x.sqlite3")), cwd.join("x.sqlite3"));
    }

    #[test]
    fn identity_is_hex_and_memory_has_none() {
        assert_eq!(database_identity(Some(":memory:")), None);
        let id = database_identity(Some("relative/ledger.sqlite3")).unwrap();
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
