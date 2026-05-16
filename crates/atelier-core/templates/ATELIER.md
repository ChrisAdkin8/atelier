# `ATELIER.md` — project instructions for Atelier

<!--
This file lives at the root of your repo. Atelier reads it at the start of every
session and injects its contents into the system prompt as persistent context.
Treat it as a message that applies to every turn, not just the first.

Comments in HTML form (like this one) are stripped before injection — use them
freely for notes to humans that the model never sees.

You can delete, rewrite, or reorganise any section below. The harness imposes no
schema; the sections are a suggested skeleton, not a contract.
-->

## What this project is

<!-- One-paragraph description. The agent uses this to orient itself before
making any change. Include: what the codebase does, who runs it, the primary
language and runtime, the deployment target if relevant. -->

(replace this paragraph)

## Conventions

<!-- Bullet list of project-specific conventions. Examples (delete what doesn't
apply, add your own):

- Format with `<tool>`; lint with `<tool>`; max line length `<n>`.
- Tests live under `tests/`; framework is `<pytest|jest|cargo-test|…>`.
- Run tests with `<command>`.
- Run the build with `<command>`.
- Source files go under `src/`; generated files under `gen/` (don't edit by hand).
- Commit messages follow `<convention>`.
-->

- (replace this list)

## Don't touch

<!-- Files or directories the agent should treat as off-limits without explicit
permission. Examples:

- `src/generated/` — produced by codegen; edit the templates, not the output.
- `migrations/` — DB migrations; needs review even for typo fixes.
- `vendor/`, `third_party/` — vendored dependencies.
- `.env`, `secrets/` — credentials (also covered by §12 redaction).
-->

- (replace this list)

## Useful commands

<!-- Shortcuts the agent should know about. Examples:

- `make test` — run the test suite.
- `make lint` — run the linter.
- `cargo nextest run` — fast Rust test runner.
- `npm run dev` — start the dev server on :3000.
-->

- (replace this list)

## Anything else

<!-- Free-form. Project quirks, links to design docs, current refactors,
preferred error-handling style, performance targets, anything that would help
the agent contribute without breaking unspoken rules. -->

(replace this paragraph)
