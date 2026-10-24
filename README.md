<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="brand/hero-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="brand/hero-light.svg">
    <img src="brand/hero.svg" width="100%"
         alt="Perennos DEMON MODE. Burn it before it resets. A tachometer needle sweeps from Austere through Lean, Normal and Rich to DEMON as the countdown to the plan reset runs down.">
  </picture>
</p>

<h1 align="center">Perennos DEMON MODE 😈</h1>
<p align="center"><strong>Burn it before it resets.</strong><br>
A use-it-or-lose-it governor for Claude Code and Codex subscriptions.</p>

**1.** Double-click <code>install.cmd</code> (macOS/Linux: <code>bash install.sh</code>).<br>
**2.** Click the Demon Mode shortcut.

The GUI is a local page on 127.0.0.1 only.

> The countdown and percentage shown inside the hero are illustrative gauge state, not a claim about the tool.

Your plan's allowance resets on a clock: a weekly window, plus a five-hour one if your plan has it. Demon Mode reads your own readings (percent used, reset time), works out your pace against that clock, and, through an optional hook, tells your agent on every prompt how wide to work: <code>Austere</code>, <code>Lean</code>, <code>Normal</code>, <code>Rich</code> or <code>DEMON</code>.

Why: allowance you don't use is gone at reset, and allowance you burn early leaves you at a wall mid-task. Tight means narrower, careful steps. Reset within a day and headroom left means <code>DEMON</code>: go wide on the work you already approved, with more helpers and deeper review. Yes, the name is dramatic. Under the hood it is mostly arithmetic and a SQLite file.

Hard lines: it never bypasses a limit, never scrapes an account, never buys extra usage and never invents work. [Full list below.](#what-it-will-not-do)

## Install

~~~sh
git clone https://github.com/Perennos/demon-mode.git
cd demon-mode
~~~

Use Python 3.10 or newer. From the root of a clean checkout, create a virtual environment and install into it:

~~~powershell
# Windows PowerShell
py -3 -m venv .venv
.\.venv\Scripts\python.exe -m pip install .
.\.venv\Scripts\demon.exe --help
~~~

~~~sh
# macOS / Linux
python3 -m venv .venv
.venv/bin/python -m pip install .
.venv/bin/demon --help
~~~

The install smoke command is <code>demon --help</code> when the virtual environment is active. Commands below use that environment's <code>demon</code> executable. Activate it with <code>. .venv/bin/activate</code> on macOS/Linux or <code>.\.venv\Scripts\Activate.ps1</code> in Windows PowerShell. At run time, the CLI has no third-party dependencies, model calls, or network service.

## First five minutes

Demon Mode cannot look up your usage. The numbers come from you: typed in, piped in, or read from a local file you point it at. Step 1 needs your own reading, and step 2 has nothing to show without it. Steps 3 and 4 do not read your numbers.

**1. Put a reading in.** A reading is the weekly percent used and the weekly reset time. Add the five-hour pair if you have it. Pick one route.

By hand, for Claude Code or Codex. Everything in angle brackets is a placeholder: replace it with your real value. Times are ISO 8601 with a UTC offset; a trailing <code>Z</code> works.

~~~sh
demon record --provider claude --weekly <percent-used> --resets '<weekly-reset-time>' --five-hour <five-hour-percent-used> --five-hour-resets '<five-hour-reset-time>'
~~~

Use <code>--provider codex</code> for Codex. The reading is stamped with the current time unless you pass <code>--at '&lt;time-you-read-it&gt;'</code>. The two five-hour flags go together; drop both if you have no five-hour reading.

From Claude status-line JSON piped to standard input. It needs <code>rate_limits.seven_day</code> with <code>used_percentage</code> and <code>resets_at</code>; <code>five_hour</code> is optional. Without the weekly block the command stops with an error and records nothing.

~~~sh
demon ingest claude-statusline --stdin
~~~

The reading is stamped with the moment you run it, so pipe in live JSON, not an old copy.

From Codex, pointing at one JSONL file that you choose. It looks near the end of the file for the newest rate-limit event and uses that event's own timestamp.

~~~sh
demon ingest codex --file <path-to-codex-jsonl>
~~~

Each of these prints a status line when it finishes.

**2. Check the mode.**

~~~sh
demon status --provider claude
~~~

One line: the mode, a ten-slot gauge of weekly use, weekly and estimated percent, the headroom ratio and the reset time. Add <code>--json</code> for the reason and freshness behind the mode. An empty ledger reads <code>LEAN</code> with <code>unknown</code> for every number.

A stale reading caps the mode. Past one hour the report asks for a refresh and the mode tops out at <code>Normal</code>; a missing five-hour reading does the same. Past six hours the weekly reading counts as missing and <code>Lean</code> is the best you get.

**3. Get the hook template.**

~~~sh
demon hook-config --client claude
~~~

It prints a small JSON template: the <code>UserPromptSubmit</code> event and the <code>command</code> to run. It does not edit your settings and registers nothing. You merge that command into your client's existing hook list yourself, keeping every hook you already have. Use <code>--client codex</code> for Codex. The command points at the Python that ran <code>hook-config</code>, so run it from the environment you installed into. It reads the default ledger; if you recorded with <code>--db</code>, put the same <code>--db &lt;path&gt;</code> before <code>hook</code> in the command. The hook section below has the rest.

**4. Preview the gauge.**

~~~sh
demon preview
~~~

A three-frame gauge animation that plays for three seconds by default (<code>--seconds 1</code> or <code>2</code> for shorter). It plays fixed frames and does not read your ledger, so it is for looks only. With <code>--plain</code>, with <code>NO_COLOR</code> set, or when output is not a terminal, you get one static line instead.

## GUI

<code>demon gui</code> opens a local page on the same ledger the command line uses: the mode word and the gauge needle, the time to the weekly reset, the percent left, the five-hour percent, and how old the last reading is. You can record a reading, paste Claude status-line JSON, and copy the hook template. It calls the same code as <code>record</code>, <code>ingest claude-statusline</code>, <code>status</code> and <code>hook-config</code>, so the numbers and the error messages match.

~~~sh
demon gui --no-browser
~~~

That prints the address instead of opening a browser; <code>--port &lt;number&gt;</code> picks the port. The page is served from 127.0.0.1 only, loads nothing from the internet, refuses requests with a foreign Host or Origin, and needs a per-run token to save a reading. Press Ctrl+C in the terminal to stop it.

## Start with a manual ledger

The manual ledger is the fallback when no supported local source is available. Replace each quoted timestamp placeholder with the actual UTC reset time or original observation time; omit the angle brackets from the value. The value <code>71</code> below is an invented example for syntax only; it is not a live reading or a suggested allowance.

~~~sh
demon record --provider codex --weekly 71 --resets '<actual-reset-UTC>' --at '<original-observed-UTC>' --json
demon status --provider codex --json
~~~

Never invent or future-date a reading, and do not present stale data as fresh. The ledger is stored locally in SQLite. <code>--five</code> (alias for <code>--five-hour</code>) records the measured five-hour used percentage; <code>--five-resets</code> (alias for <code>--five-hour-resets</code>) records that reading's UTC reset time. Without fresh short-window data, the advisory mode is capped at Normal until refreshed.

To keep a demonstration ledger separate, pass the same <code>--db</code> path to both commands:

~~~sh
demon --db ./demo.sqlite3 record --provider codex --weekly 71 --resets '<actual-reset-UTC>' --at '<original-observed-UTC>' --json
demon --db ./demo.sqlite3 status --provider codex --json
~~~

Optional ingestion is limited to supported provider output: Claude status-line JSON supplied on standard input, or an explicit Codex JSON file. Do not scrape accounts, read prompts, supply credentials, or treat token counts or cost totals as subscription allowance.

## Working modes and limits

| Mode | Advisory working envelope |
| --- | --- |
| <code>Austere</code> | One bounded work unit at a time; no optional helpers. Reuse evidence and minimize exploratory branches while retaining required correctness, security, and accessibility checks. |
| <code>Lean</code> | Work directly; use one focused review when needed and at most one helper that is already permitted. Skip speculative polishing. |
| <code>Normal</code> | Use the normal working policy for the authorized task. Do not add work beyond its scope. |
| <code>Rich</code> | For the authorized task, use up to two independent helpers only if already supported and permitted, with one independent review for consequential output. |
| <code>DEMON</code> | With fresh headroom and a reset within 24 hours, use up to three independent helpers only if already authorized, explore more deeply, and review high-value work already authorized. Stop at target pressure or stale readings; do not invent tasks or start background work. |

A short-window reading at 95% used or higher triggers the hard brake and <code>Austere</code>. Stale or missing input is not treated as current. Incomplete history is handled conservatively and caps the mode at <code>Lean</code> until coverage is refreshed. When dense history has to be reduced to fit the finite retention window, the report exposes a recent pace upper bound instead of implying that the retained samples are a complete timeline.

The mode is an instruction, not an enforcement boundary. Provider tools and client permissions remain under the user's control.

## What it will not do

Demon Mode is a gauge with opinions, not a bouncer. It does not:

- bypass a limit or grant permissions. The mode text is advice for work you already authorized.
- change client settings or override higher-priority instructions. <code>hook-config</code> prints a template and stops there.
- start background work, authorize new tasks, or invent work.
- buy or authorize extra usage.
- scrape accounts, read your prompts, take credentials, or make model or network calls. Input is status-line JSON you pipe in, one Codex JSONL file you name, or a reading you type.
- count token totals or cost totals as subscription allowance. Token and cost fields are ignored.
- treat old or missing readings as current. Stale input lowers the ceiling, and a reading dated more than a minute in the future is rejected.

## Optional hook output

<code>hook-config</code> prints a command template for an additive <code>UserPromptSubmit</code> hook. It does not edit Claude or Codex settings or register itself automatically. Review the template and merge it into the existing hook list, preserving every other hook and confirming the host's trust prompt where applicable.

~~~sh
demon hook-config --client codex
demon hook --client codex
~~~

<code>hook</code> emits compact JSON containing advisory <code>additionalContext</code>; it does not read prompt text from standard input. Confirm that the native client recognizes the output before relying on hook delivery. The commands above are local output examples, not evidence of a completed native-host integration.

For a terminal preview, run <code>demon preview</code>. <code>--plain</code> or the <code>NO_COLOR</code> environment variable suppresses color; redirected or non-interactive output uses a static fallback.

## Project and credits

The Python runtime uses the standard library and local files. No model or network service is needed to calculate policy from the local ledger. Keep allowance records and provider exports on a machine you control.

Source is licensed under the Apache License, Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Brand font materials have their own SIL Open Font License 1.1 terms in [brand/licenses](brand/licenses/) and at [the SIL OFL page](http://scripts.sil.org/OFL): Barlow Condensed by The Barlow Project, Martian Mono by The Martian Mono Project, and IBM Plex Mono © 2017 IBM Corp. IBM's reserved font name is “Plex”; the distributed derivative subset is renamed Perennos Mono Subset.

See [CONTRIBUTING](CONTRIBUTING.md), [CODE_OF_CONDUCT](CODE_OF_CONDUCT.md), [SECURITY](SECURITY.md), and [ROADMAP](ROADMAP.md).

<p align="center">
  <a href="https://github.com/Perennos">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="brand/promo-footer/footer-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="brand/promo-footer/footer-light.svg">
      <img src="brand/promo-footer/footer.svg" width="360" height="84" alt="Made in the Cayman Islands by Perennos, 19.29°N 81.37°W">
    </picture>
  </a>
</p>
<p align="center">
  Made in the Cayman Islands by <a href="https://github.com/Perennos">Perennos</a>.
  <a href="https://github.com/Perennos/perennos-tempo">Perennos Tempo</a> left no crumbs.<br>
  <a href="https://caymanrituals.com">Cayman Rituals</a>, candles, scent and apparel from the Cayman Islands
  · <a href="https://n4mu.com">N4MU</a>
  · <a href="https://marzoucamedical.com">Marzouca Medical</a>
</p>
