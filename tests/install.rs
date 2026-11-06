//! Ported from tests/test_install.py.
mod common;

use common::TempDir;
use std::path::Path;
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn install_cmd_is_crlf() {
    let data = std::fs::read(root().join("install.cmd")).unwrap();
    let lf = data.iter().filter(|b| **b == b'\n').count();
    let crlf = data.windows(2).filter(|w| w == b"\r\n").count();
    assert_eq!(lf, crlf);
}

#[test]
fn install_scripts_pin_the_crate_version() {
    let pinned = format!("VERSION=\"{}\"", demon_mode::VERSION);
    let sh = std::fs::read_to_string(root().join("install.sh")).unwrap();
    assert!(sh.contains(&pinned), "install.sh must pin {pinned}");
    let cmd = std::fs::read_to_string(root().join("install.cmd")).unwrap();
    assert!(cmd.contains(&format!("set \"VERSION={}\"", demon_mode::VERSION)));
}

fn usable_bash() -> Option<&'static str> {
    let probe = Command::new("bash").args(["-c", "exit 0"]).output().ok()?;
    let diagnostic = String::from_utf8_lossy(&probe.stderr).to_lowercase();
    // A WSL launcher with no Linux distribution is not a usable shell.
    if !probe.status.success() && (diagnostic.contains("distribution") || diagnostic.contains("wsl")) {
        return None;
    }
    probe.status.success().then_some("bash")
}

#[test]
fn install_sh_is_lf_and_parses() {
    assert!(!std::fs::read(root().join("install.sh")).unwrap().contains(&b'\r'));
    let Some(bash) = usable_bash() else {
        eprintln!("skipped: no usable bash on PATH");
        return;
    };
    let result = Command::new(bash).args(["-n", "install.sh"]).current_dir(root()).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
}

#[cfg(unix)]
#[test]
fn install_sh_dry_run_changes_nothing() {
    let temp = TempDir::new();
    let target = temp.path().join("target");
    let result = Command::new("bash")
        .args(["install.sh", "--dry-run"])
        .current_dir(root())
        .env("INSTALL_BIN_DIR", target.join("bin"))
        .env("INSTALL_SHORTCUT_DIR", target.join("shortcuts"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(result.status.success(), "{stdout}{}", String::from_utf8_lossy(&result.stderr));
    assert!(stdout.contains("Dry run: nothing was changed."));
    assert!(stdout.contains("SHA256SUMS"));
    assert!(stdout.contains("hook-config"));
    assert!(!target.exists());
}

#[cfg(windows)]
#[test]
fn install_cmd_dry_run_changes_nothing() {
    let temp = TempDir::new();
    let target = temp.path().join("target");
    let result = Command::new("cmd")
        .args(["/c", root().join("install.cmd").to_str().unwrap(), "/dry-run"])
        .current_dir(root())
        .env("INSTALL_BIN_DIR", target.join("bin"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(result.status.success(), "{stdout}");
    assert!(stdout.contains("demon.exe"));
    assert!(stdout.to_lowercase().contains("shortcut"));
    assert!(stdout.contains("SHA256SUMS"));
    assert!(!target.exists());
}

/// A real install: downloads or builds, writes shortcuts. Opt in with DEMON_TEST_REAL_INSTALL=1.
#[cfg(windows)]
#[test]
fn install_cmd_real_run() {
    if std::env::var_os("DEMON_TEST_REAL_INSTALL").is_none() {
        eprintln!("skipped: set DEMON_TEST_REAL_INSTALL=1 to run a real install");
        return;
    }
    let temp = TempDir::new();
    let shortcuts = temp.path().join("shortcuts");
    let bin = temp.path().join("bin");
    let result = Command::new("cmd")
        .args(["/c", root().join("install.cmd").to_str().unwrap()])
        .current_dir(root())
        .env("INSTALL_SHORTCUT_DIR", &shortcuts)
        .env("INSTALL_BIN_DIR", &bin)
        .env("INSTALL_NO_PAUSE", "1")
        .env("INSTALL_NO_PATH", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(result.status.success(), "{stdout}");
    assert!(bin.join("demon.exe").is_file());
    let mut links: Vec<String> = std::fs::read_dir(&shortcuts)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    links.sort();
    assert_eq!(links, ["Demon Mode (Start menu).lnk", "Demon Mode.lnk"]);
    assert!(stdout.contains("click the Demon Mode shortcut"));
}
