# Research and next improvements

Reviewed for 0.1.1 on October 23, 2026; refreshed October 31, 2026. Public
primary sources only; no private accounts, provider credentials, or hosted
model calls were used.

| Evidence | Consequence implemented |
| --- | --- |
| [OpenAI hooks](https://learn.chatgpt.com/docs/hooks): wrapper, trust, working directory, local/cloud boundary | Additive native config, absolute paths, pasteable fallback, honest host limits |
| [OpenAI app-server](https://learn.chatgpt.com/docs/app-server): camelCase, durations, multiple buckets, sparse updates | Export reader, original timestamp, shared `codex` selection, unsupported-window rejection |
| [Account protocol](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/account.rs): permission is nullable; null means unavailable | Reject present null or malformed permission values as well as explicit denial; tolerate omitted permission fields for backwards compatibility, including sparse notifications |
| [Codex hook runner](https://github.com/openai/codex/blob/main/codex-rs/hooks/src/engine/command_runner.rs) and [shell detection](https://github.com/openai/codex/blob/main/codex-rs/shell-command/src/shell_detect.rs) | Literal PowerShell wrapper on Windows; no assumed Codex exec form |
| [Claude command fields](https://code.claude.com/docs/en/hooks#command-hook-fields) | Shell-free native Claude executable/args configuration |
| [Claude statusline](https://code.claude.com/docs/en/statusline): UI/timer updates, optional fields | Original capture timestamp option; freshness caveat |
| Independent synthetic policy review | Correct aged short projection, contradictory backfills, and cross-week short conflicts |

Transcripts are unstable compatibility inputs. App-server exports offer a
documented structured boundary; automatically collecting them would introduce
process/auth/network behavior the current local product does not have.

## Prioritized next work

1. **Native Windows acceptance.** On the user's machine, read its workflow
   contract and installed `/ponytail` instructions; have locally configured
   Argon review this plan and the immutable change diff. Neither was callable in
   the implementation environment. Run installed-wheel checks, merge hooks
   additively, trust them, verify harmless prompt delivery, and record exact
   client versions/results.
2. **Provider-status persistence.** Future state can represent denial separately
   from numeric allowance. Current blocked exports fail and preserve history;
   downstream automation must honor failure rather than reuse old headroom.
   Define explicit recovery before expanding automatic refresh beyond JSONL.
3. **Additional durations.** Decide how different quota lengths coexist before
   supporting them. Do not relabel 15 minutes as five hours or infer entitlement
   from token counts, credits, or billing data.
4. **Signed releases/installers.** Add platform artifacts after native flows
   pass. Preserve reproducible wheel/sdist checks; source tests alone do not
   demonstrate a successful desktop install.

Use existing local tools. This plan requires no new paid service, private-data
upload, or replacement architecture.
