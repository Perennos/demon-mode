# Integration and troubleshooting

Demon Mode advises on measured subscription allowance. It does not authenticate,
invoke models, manage API billing, buy credits, or enforce permissions. A low
percentage is not proof a provider currently permits inference.

| Source or host | Supported behavior | Verification boundary |
| --- | --- | --- |
| Manual ledger | Weekly and optional five-hour usage, with original timestamps | Synthetic and installed-package tests |
| Codex local JSONL | Explicit file; last 1 MiB; `event_msg` with `token_count` or legacy `token_info` | Synthetic tests; transcripts are not a stable API |
| Codex app-server export | Captured read result, updated-notification params, or unwrapped payload | Synthetic and installed-package tests; no server is launched |
| Claude statusline JSON | `rate_limits.seven_day` and optional `five_hour` | Synthetic tests; receipt time cannot prove provider refresh |
| Local Codex hooks | `UserPromptSubmit` and `hookSpecificOutput.additionalContext` | Generated command tests; native host delivery Untested |
| Current Claude hooks | Same advisory output; shell-free executable and `args` | Generated command tests; native host delivery Untested |
| ChatGPT web/cloud orchestration | Paste `demon context` output manually | Local command hooks do not integrate these hosts |
| OpenAI API billing/rate limits | Unsupported: separate from subscription allowance | No API keys or calls required |

Supported measured windows are **10080 minutes weekly** and **300 minutes short**.
Provider schemas permit other durations and nullable windows; those are not
relabeled. Missing weekly data, duplicate windows, unsupported app-server
durations, and explicit blocking states are rejected without replacing the
ledger. A nonzero ingest result is failure, never evidence of headroom. Saved
history cannot override a provider restriction. App-server exports require the
original capture time. Sparse updates are never completed using older data.

## Codex inputs

Select one local session file. The tool never discovers/scans its directory:

```sh
demon ingest codex --file '/absolute/path/to/selected-session.jsonl' --json
demon status --provider codex --json
demon doctor --provider codex
```

Only numeric quota values, source identifiers, and timestamps are retained.
Prompts and file paths are not stored. The reader preserves event timestamps,
skips token-only events, and validates the newest measurement. Conflicting
same-time measurements fail. A malformed latest measurement does not revive
older good data. An unterminated JSONL record waits for its newline.

If a trusted integration already captures official app-server output, import
its JSON file using the original capture time with UTC offset:

```sh
demon ingest codex-app-server --file './rate-limits.json' --at '<original-observed-UTC>' --json
```

`--stdin` is also available; both forms accept up to 64 KiB of UTF-8, with or
without a BOM. Multi-bucket exports select `codex` only. Explicit
`ordinaryUsageAllowed: false`, `spendControlReached: true`, or non-null
`rateLimitReachedType` fails even with low percentages. Account identifiers,
credit details, and unrelated export fields are not retained.

## Hooks and Windows

Install into the Python environment used to generate the configuration. Review
the template, then print the native object:

```sh
demon --db './allowance.sqlite3' hook-config --client codex
demon --db './allowance.sqlite3' hook-config --client codex --format native
```

Merge its entry into the existing `UserPromptSubmit` list, preserving other
hooks/settings. Neither command writes settings. On local Codex, review/trust
the hook through `/hooks`. Existing admin/host policy still applies. Regenerate
config after moving/deleting the virtual environment.

Custom database paths become absolute. For per-turn refresh, add
`--codex-file '/absolute/path/to/selected-session.jsonl'` to `hook-config`.
Select a new file when changing sessions and regenerate the template. Refresh
reads that file only, never prompt stdin. Failure emits Lean/Austere advice and
preserves the ledger. Hook database waits are 0.2 seconds per operation.

Windows Codex templates invoke Windows PowerShell with an encoded command whose
arguments are literal strings. The default template includes the decoded
`windows_script` and full `argv` for review. This avoids assuming the host uses
cmd, PowerShell, or Git Bash quoting. Windows PowerShell must be installed.
Claude uses current documented executable-plus-`args` form; older clients need
a native compatibility check. Use the native handler object, not the template's
human-readable command, for Claude.

Test the handler from another directory, then submit a harmless prompt in the
native client and confirm receipt. Command tests alone do not verify delivery.

## Claude freshness

Pipe supported live JSON to `demon ingest claude-statusline --stdin`. For saved
JSON add `--at '<original-captured-UTC>'`; otherwise timestamp means local receipt
time. Claude also reruns scripts for UI/timer updates. Receipt is not proof of a
new provider measurement. Do not replay cached data to make it appear fresh.
Absent short-window data caps policy conservatively.

## Diagnostics

```sh
demon --version
demon doctor --provider codex --json
demon status --provider codex --json
demon context --provider codex
```

Unknown/stale readings require measured input. State errors require checking
locks, permissions, schema compatibility, and local paths. Preserve the database.
`doctor` always labels native host delivery Untested: it cannot observe whether
the client accepted the hook.

## Source layout and contributor checks

| Path | Responsibility |
| --- | --- |
| `demon_mode/__main__.py` | CLI, diagnostics, hook output/config |
| `demon_mode/sources.py` | Bounded input readers and schema selection |
| `demon_mode/governor.py` | Pure policy, projections, hysteresis, context |
| `demon_mode/store.py` | Transactions, conflicts, retention |
| `tests/support.py` | Synthetic fixtures and CLI capture |
| `tests/test_governor.py`, `test_store.py`, `test_sources.py`, `test_cli.py` | Tests grouped by responsibility |
| `tests/test_reliability.py` | Integration regressions |
| `scripts/smoke_installed.py` | Installed-wheel workflow outside checkout |

Run `python -m unittest discover -s tests -v`. Build into a clean `dist/`, then
run `python scripts/smoke_installed.py`. CI targets Python 3.10/3.14 on Linux,
Windows, and macOS; results are evidence only after the specific run passes.
