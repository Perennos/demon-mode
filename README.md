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

> The countdown and percentage shown inside the hero are illustrative gauge state, not a claim about the tool.

<code>demon-mode</code> is a stdlib-first Python CLI and per-turn hook governor for Claude Code and Codex subscription allowances. Subscription windows reset on a clock: move too fast early and you hit a wall mid-task; move too cautiously and included capacity expires unused. <code>demon-mode</code> estimates your remaining pace before reset—using your own local session logs, official CLI outputs, or a manual ledger fallback—and injects advisory working-mode instructions (<code>Austere</code>, <code>Lean</code>, <code>Normal</code>, <code>Rich</code>, or <code>DEMON</code>) at each turn. When allowance is tight, it advises narrower, conservative steps; when reset is near and capacity remains, it directs spare allowance toward pre-authorised backlog and review tasks.

The mode text is advice for work already authorized. It does not grant permissions, change client settings, override higher-priority instructions, start background work, or authorize new tasks.

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
