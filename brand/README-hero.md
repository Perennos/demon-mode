# README hero snippet: Perennos DEMON MODE

Paste this at the very top of `README.md`. Paths assume the kit lives at `brand/` in the repo root
(`hero.svg`, `hero-dark.svg`, `hero-light.svg` are there). GitHub renders the SVG inside an `<img>`,
so the CSS animation runs on github.com; the loop is 12 s and collapses to the DEMON frame under
`prefers-reduced-motion`.

```html
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
```

Other assets in this folder:

| File | Use |
|---|---|
| `social-preview-1280x640.png` | Repo settings → Social preview (GitHub wants 1280×640). |
| `avatar-512.png` | Org/repo avatar, Discussions, badges. |
| `favicon.svg` | Docs site or status page favicon; simplified for 16–32 px. |
| `mark.svg`, `mark-light.svg` | The logo mark on dark and on the black-hot light palette. |
| `alt-isotherm.svg` | The riskier alternative mark; not used unless chosen. |

The copy inside the hero is illustrative gauge state (a countdown and a percentage left), not a claim about the tool.
