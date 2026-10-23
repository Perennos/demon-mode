# Contributing

Contributions should keep Demon Mode small, local, reviewable, and advisory.

## Changes

- Keep runtime dependencies in the Python standard library.
- Include synthetic tests for behavior changes; never use private user data as
  a fixture.
- Explain the problem, the smallest change that addresses it, and any behavior
  or compatibility impact.
- Keep changes focused so a reviewer can understand and reproduce them.
- Do not include credentials, raw private transcripts, account details, or
  machine-specific private paths in issues, patches, or test output.

## Safe examples

When reporting a behavior or proposing a change, use invented readings and
synthetic inputs. A reproducible example should state the expected result and
the observed result without exposing private prompts or logs.

Contributions must not add limit bypasses, scraping, paid extra usage, model
automation, or permission-changing automation. The project is
advisory; proposed changes must preserve the local operator's control over
work and permissions.

Please open a focused issue or pull request with enough synthetic detail for
review. Do not assume that a report or contribution has been accepted until it
has been reviewed.
