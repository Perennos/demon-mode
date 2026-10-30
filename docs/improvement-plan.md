# Reliability and OpenAI compatibility

Requested outcome: improve Demon Mode as a working product while retaining its
local, standard-library-only, advisory subscription governor design.

The implementation keeps the existing ledger and math rather than replacing the
product with an API service. No API keys, model calls, account scraping, private
fixtures, background work, or paid fallback are needed or added.

## Work and verification

- [x] Reproduce and fix hook configuration losing a custom database. Print an
  additive native hooks object, retain absolute paths, and allow explicit Codex
  JSONL refresh without reading hook prompt input. Refresh failures must cap any
  existing high-headroom policy conservatively.
- [x] Accept exported official `account/rateLimits/read` responses and
  `account/rateLimits/updated` notifications. Require the original observation
  time, select only the `codex` bucket, require supported durations, and reject
  ambiguous/missing values without overwriting the ledger.
- [x] Fix log selection so old unavailable/malformed readings do not hide a newer
  valid reading; reject conflicting newest readings and other metered buckets.
- [x] Bound text fallback without recursion, handle oversized/deep/invalid numeric
  input quietly, and fix additional reproduced policy/storage defects.
- [x] Improve terminal diagnostics, document supported host boundaries and exact
  setup, and test the installed wheel outside the source directory.
- [x] Run the full synthetic suite, package checks, an independent read-only
  review, and configure CI on the proposed GitHub branch. Keep untested host integration
  distinct from tested output/schema compatibility.

## Definition of done

Every behavior fix has a synthetic reproduction that failed before the change.
Source tests and clean installed-package workflow tests pass. Documentation
matches the implementation. Proposed changes are preserved in a GitHub pull
request; no assertion that the user's Windows client has been exercised.

## Review focus

Stale or missing observations; same-time conflicting events; per-model quotas
mistaken for the shared Codex bucket; shell quoting and a changed working
directory; locked/corrupt SQLite and hook refresh failures.

Baseline: commit `3cd7cb0bf1c8396356967837d33937b248bf7fd4`, 38 tests pass on
Python 3.12.14/Linux. Windows workflow contract unavailable in this environment.

Verification: 65 synthetic tests pass on Python 3.12.14/Linux. Clean wheel
installation and workflow smoke pass outside the checkout. Independent final
review found no remaining critical/important defects after its reproduced
reader finding was fixed with failing-then-passing regression tests. Native
client delivery remains Untested. Remote publication and CI are blocked by
GitHub integration write permissions (403); no remote files were changed.
