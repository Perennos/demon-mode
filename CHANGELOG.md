# Changelog

## 0.2.0 (unreleased): Rust

Demon Mode is now one Rust binary, `demon`. The commands, flags, aliases, outputs, exit codes and
the SQLite ledger are the same as 0.1.1, so readings you already recorded keep working.

### Upgrading from 0.1.x

1. Install the binary: `bash install.sh` (macOS/Linux) or `install.cmd` (Windows), or
   `cargo install --locked --path .`. No Python is needed any more.
2. Nothing to do for your readings. The default ledger path is unchanged
   (`%LOCALAPPDATA%\Perennos\demon-mode\state.sqlite3` on Windows,
   `${XDG_DATA_HOME:-~/.local/share}/Perennos/demon-mode/state.sqlite3` elsewhere), the schema is
   the same (`user_version` 2) and the file is read and written in place. A ledger from before 0.1.1
   (`user_version` 1) is still migrated in place on first open, as 0.1.1 did.
3. Replace the hook command, once per client. A hook written by the old `hook-config` runs
   `<venv>/python -m demon_mode ... hook --client ...`. Run `demon hook-config --client claude` (or
   `--client codex`) with the same `--db` and `--codex-file` options you used before, and in your
   client's hook settings replace only that one command. Keep every other hook. Until you do, the old
   hook keeps running 0.1.1 from its virtual environment; both versions read the same ledger, so
   nothing breaks in the meantime. `demon doctor` prints the binary path the new command uses.
4. After that, the old `.venv` folder can be deleted. Deleting it first breaks the old hook.

### Changed

- `hook-config` templates (and the GUI's copy) start with the absolute path of the `demon` binary
  instead of `<python> -m demon_mode`. On Windows the Codex template still uses the literal
  PowerShell `-EncodedCommand` form, now invoking `demon.exe`.
- The hook runs in about 3 ms per prompt on a Linux test machine; 0.1.1 took about 60 ms there.
- `doctor` no longer has a `python` field. It adds `runtime`, `executable`, `ledger`,
  `ledger_present` and `tempo`, and its plain output adds Ledger, Binary and Tempo lines plus a hook
  migration step. It makes no network check: Demon Mode never contacts Tempo or anything else.
- The `demon-mode` alias command is no longer installed; the command is `demon`.
- `--db` is also accepted after the subcommand (`demon status --provider codex --db ./x.sqlite3`).
- Usage errors (a missing or unknown flag) use different wording; they still exit with status 2.
  Demon Mode's own error messages are unchanged.
- JSON input with `NaN` or `Infinity` literals, or integers too large for a float, is now rejected
  as invalid JSON rather than as an invalid percentage or timestamp. Both exit 2 and record nothing.
- Timestamps accept the ISO 8601 forms Python 3.11 and later accept (the parser is a port of
  CPython 3.13's), whatever Python version ran 0.1.1 before.
- The installers put the binary in `~/.local/bin` or `%LOCALAPPDATA%\Perennos\demon-mode` (added to
  your user PATH on Windows) and check downloads against the release's `SHA256SUMS`. The Windows
  shortcuts start `demon.exe gui` in a minimized console window instead of `pythonw`.
- The GUI's JSON API answers without spaces after separators. The page itself is unchanged.

### Added

- `demon mcp`: a read-only MCP server over stdio with `demon_status` and `demon_context`, for
  Perennos Tempo or any MCP host. `demon tempo policy` prints a Tempo tool-policy fragment and
  `demon tempo mcp-config` a secret-free client entry. See [docs/TEMPO.md](docs/TEMPO.md).
- Prebuilt release binaries for Linux (static musl, x86_64 and aarch64), Windows (x86_64) and macOS
  (Apple silicon and Intel), with `SHA256SUMS`.

### Kept

The policy math, mode wording, freshness caps (one hour caps the mode at Normal; past six hours
the weekly reading counts as missing), the hard brake at 95% or more of the five-hour window, the
one-minute future-dating rejection, conflict handling, retention and the dense-history pace upper
bound, the GUI page and its protections (loopback only, Host and Origin checks, per-run token,
bounded bodies, `no-store`, `nosniff`, strict CSP, idle shutdown, single instance per ledger), and the
prompt-blind hook.

### Removed

The Python implementation: `demon_mode/`, `pyproject.toml`, `MANIFEST.in`,
`scripts/smoke_installed.py` and the Python tests. Their behaviours are pinned again by the Rust
tests in `tests/`.

### Verification

The Python suite's behaviours were ported to Rust tests. A differential run fed the same randomized
readings through 0.1.1 and 0.2.0 and compared every output and the resulting ledgers; see the
pull request for the numbers. Native host delivery (a client actually showing the hook text) is
still Untested, as before.

## 0.1.1 and earlier

Python releases. See the git history.
