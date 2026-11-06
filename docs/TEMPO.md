# Perennos Tempo and other MCP hosts

An agent can read the pace before it decides how wide to work, instead of waiting for the
per-prompt hook. `demon mcp` is a small [Model Context Protocol](https://modelcontextprotocol.io)
server over standard input and output with two read-only tools. Any MCP host can use it: an agent
hosted through [Perennos Tempo](https://github.com/Perennos/perennos-tempo), Claude Code, Codex, or
another client.

Demon Mode is the tool here, never the client. It does not call Tempo, does not read Tempo's
gateway token, does not open a network listener, and makes no model calls. The tools read the local
ledger the way `demon status` does and change nothing.

## The tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `demon_status` | `provider` (optional): `claude` or `codex`; omit for both | Per provider: `mode`, the ten-slot `gauge`, `status_line`, weekly used and estimated percent, headroom ratio, target, weekly reset and seconds to it, `weekly_reading_age_seconds`, five-hour used and projected percent and reset, `freshness`, `reason`, `refresh_required`, `source`, and the mode's `instruction`. Plus `ledger_present` and `advisory_only: true`. |
| `demon_context` | `provider` (required): `claude` or `codex` | `context`: the advisory text `demon context` prints (the same text the hook injects), plus `mode` and `refresh_required`. |

Both are annotated `readOnlyHint: true`, `destructiveHint: false`, `idempotentHint: true`,
`openWorldHint: false`, and both input schemas set `additionalProperties: false`. A ledger that does
not exist yet is reported as empty (`ledger_present: false`, mode `Lean`) and is not created. The
fields are the ones `demon status --json` prints, so the numbers match the command line exactly.

## Connect an MCP host

Print the client entry once the binary is where it will stay:

~~~sh
demon tempo mcp-config
~~~

~~~json
{
  "mcpServers": {
    "demon-mode": {
      "command": "/home/you/.local/bin/demon",
      "args": ["mcp", "--db", "/home/you/.local/share/Perennos/demon-mode/state.sqlite3"]
    }
  }
}
~~~

The paths are absolute and the entry holds no secrets. Pass `--db <path>` before `tempo mcp-config`
to point it at another ledger. Paste the entry into the MCP client's user-level configuration, the
same way Tempo's own `tempo.py mcp-config` output is used, then restart or reload the client.
Regenerate it if you move or reinstall the binary. A host that keeps its MCP servers in TOML takes the
same `command` and `args`.

To try the server by hand: `demon mcp` reads one JSON-RPC message per line and answers on standard
output; diagnostics never go to standard output.

## Allow the tools in Tempo's tool policy

Tempo's gateway runs model-proposed tool calls only when its owner-written tool policy (the file given
to the gateway with `--tool-policy`, or `TEMPO_TOOL_POLICY`) allows them by exact name. Print a
fragment for Demon Mode's tools:

~~~sh
demon tempo policy
~~~

It defines a `demon-mode` profile with `max_calls_per_request: 2` and both tools as risk `observe`,
locality `local`, with parameter schemas that use only the keywords Tempo's policy loader accepts.
Merge the profile into your policy file and add the caller yourself, for example
`"callers": {"pace-agent": {"token_sha256": "<sha256 of that caller's token>", "profile": "demon-mode"}}`.
Demon Mode never generates, reads or prints tokens, and Tempo refuses a policy with no callers, so
the fragment does not load on its own.

What was checked: with a synthetic caller added, the fragment loads in the Tempo checkout's
`tool_policy.load_policy`; both tools are allowed on private routes without approval, and a
`demon_context` call without `provider` is denied as `malformed_arguments`. What was not: Tempo
runs allowed tools through its job desk, which is not part of Tempo's public core, so a full call
from a Tempo route through a desk to `demon mcp` has not been exercised. The tool-calling gateway is
marked as a candidate in the Tempo repository.

## How an agent should use the pace

Call `demon_status` (or `demon_context`) at the start of a task and again before fanning out to
helpers or a long review. Then:

| Mode | Working envelope (advice for work already authorized) |
| --- | --- |
| `Austere` | One bounded work unit at a time; no optional helpers; keep the required correctness, security and accessibility checks. |
| `Lean` | Work directly; at most one focused review and one helper that is already permitted. |
| `Normal` | The normal working policy for the task; nothing beyond its scope. |
| `Rich` | Up to two independent helpers if already supported and permitted; one independent review for consequential output. |
| `DEMON` | Fresh headroom and a reset within 24 hours: up to three independent helpers if already authorized, deeper exploration, a review pass on high-value work. Stop at target pressure or stale readings. |

If `refresh_required` is true, or the reason is `state_unavailable` or `state_history_incomplete`,
treat the reading as unconfirmed: stay at or below the mode given (it is already capped) and ask the
user for a fresh reading rather than guessing. A five-hour reading at 95% or more always reads
`Austere`. The mode never grants permissions, never authorizes new tasks or extra usage, and never
overrides the user's instructions.

## Example session

A synthetic Codex reading (41% weekly, reset in 30 hours; 22% of the five-hour window) and four
messages on standard input; the `text` content (the same object as `structuredContent`, as compact
JSON) is elided:

~~~text
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"example","version":"1"}}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"demon-mode","title":"Perennos DEMON MODE","version":"0.2.0"},"instructions":"Read-only pace advice from the local DEMON MODE ledger. ..."}}
→ {"jsonrpc":"2.0","method":"notifications/initialized"}
   (no reply: notifications are never answered)
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"demon_status","arguments":{"provider":"codex"}}}
← {"jsonrpc":"2.0","id":2,"result":{"isError":false,"content":[{"type":"text","text":"..."}],"structuredContent":{
     "advisory_only":true,"ledger_present":true,"providers":{"codex":{
     "mode":"Rich","gauge":"####------","weekly_used_percent":41.0,"weekly_estimated_percent":41.00003119911836,
     "headroom_ratio":5.721967748235635,"weekly_reset_at":"2026-11-08T00:14:34Z","weekly_seconds_to_reset":107999,
     "weekly_reading_age_seconds":0,"short_used_percent":22.0,"short_projected_percent":55.0,
     "freshness":"measured_with_estimate","reason":"weekly_headroom_high","refresh_required":false,
     "status_line":"RICH [####------] weekly=41.0% estimated=41.0% headroom=5.72x reset=2026-11-08T00:14:34Z five_hour=22.0% reason=weekly_headroom_high refresh=not_required",
     "instruction":"For this authorized task, use up to two independent helpers only if already supported and permitted, ...", ...}}}}}
→ {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"demon_context","arguments":{"provider":"codex"}}}
← {"jsonrpc":"2.0","id":3,"result":{"isError":false,"content":[{"type":"text","text":"..."}],"structuredContent":{
     "provider":"codex","mode":"Rich","refresh_required":false,
     "context":"Perennos DEMON MODE — advisory resource policy. Mode: Rich. Provider: codex; source: manual; ..."}}}
~~~

## Protocol details

- JSON-RPC 2.0, one UTF-8 JSON object per line. Messages over 1 MiB are answered with a parse error
  (-32700) and skipped.
- `initialize` answers with the client's `protocolVersion` when it is `2025-11-25`, `2025-06-18`,
  `2025-03-26` or `2024-11-05`, otherwise `2025-06-18`, and with `capabilities.tools.listChanged: false`.
- `ping`, `tools/list` and `tools/call` are supported. An unknown method is -32601, malformed JSON
  -32700, an invalid request -32600, and bad `tools/call` parameters or an unknown tool name -32602.
- Invalid tool arguments (for example a provider other than `claude` or `codex`) come back as a tool
  result with `isError: true`, so the model can correct the call.
