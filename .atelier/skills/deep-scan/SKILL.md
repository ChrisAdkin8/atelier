---
name: deep-scan
description: Hybrid audit workflow for the atelier Rust workspace — runs cargo clippy + cargo audit + cargo fmt + gitleaks first, then proposes targeted Claude review via Explore subagents partitioned per crate. Use for security, brittleness, or style passes across the codebase, before a major refactor, or before cutting a release.
---

# Deep scan

Audit-style pass that combines deterministic tooling (linters, vulnerability scans, secret scans) with focused Claude review, structured to stay fast on the 34k+ line workspace.

## When to invoke

User explicitly asks for: a code-quality audit, security review, "look for bugs", "find brittleness", "scan for vulnerabilities", or "review the codebase". Also fits before a major refactor or release.

## Phase 1 — deterministic tooling

Run these first, before reading any source code. The output drives Phase 2.

```sh
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee /tmp/atelier-clippy.log
cargo audit --json > /tmp/atelier-audit.json 2>/dev/null || echo "cargo-audit not installed; skipping"
cargo fmt --all -- --check 2>&1 | head -50 | tee /tmp/atelier-fmt.log
# Optional, if installed:
command -v gitleaks >/dev/null && gitleaks detect --report-path /tmp/atelier-gitleaks.json --no-banner --redact
command -v semgrep  >/dev/null && semgrep --config auto --error 2>&1 | tee /tmp/atelier-semgrep.log
```

Summarise the outputs into four buckets:

- **Clippy:** count by lint code; list unique findings with `file:line`.
- **Vulnerable crates:** CVE IDs + affected versions + advised pin.
- **Format drift:** files needing `cargo fmt`.
- **Secrets:** any gitleaks hits — treat as **P0** unless explicitly redacted in a test fixture.

Do not move to Phase 2 if a P0 secret was found. Surface it to the user first.

## Phase 2 — targeted Claude review

Use the Phase 1 output to scope where Claude reads. **Never** ask Claude to "read every .rs file."

For broad reviews, spawn **Explore subagents in parallel**, one per scope. Partition by crate so nothing overlaps:

```text
Spawn 3 Explore subagents:
  1. atelier-core — async correctness, error handling, sandbox boundaries
  2. atelier-cli  — argument parsing, exit codes, user-facing error messages
  3. atelier-gui  — Tauri command surface, concurrent-run guard, IPC safety
```

For focused **security** review of a single suspect module, route through the `rust-reviewer` subagent (`.atelier/agents/rust-reviewer.md`) with that module as scope.

For **spec-conformance** questions ("does this change still match §7?"), route through `atelier-spec-conformance`.

## Phase 3 — synthesise

Once subagents return, produce one report. Severity ladder:

- **P0 (release blocker):** secrets in committed history, CVEs in production deps, panics on common inputs, unsoundness.
- **P1 (fix soon):** brittle error handling, missing recovery paths, race conditions, locks held across `.await`.
- **P2 (nice-to-have):** style violations, naming inconsistencies, needless clones, simplification opportunities.

Format each finding as one line:

```
<severity> <crate>/path/file.rs:LINE — <category> — <one-line issue> — <one-line suggested fix>
```

End with a one-paragraph summary of the workspace's overall health and any architectural concerns that span multiple crates.

Drop the final report at `/tmp/atelier-audit-$(date +%Y%m%d-%H%M).md` so the user has a checkpoint between sessions.

## What NOT to do

- Don't ask Claude to read the whole workspace before running linters. Linters find ~80% of low-level issues in seconds.
- Don't spawn parallel Explore subagents with overlapping scope — partition by crate or module.
- Don't run parallel subagents that all do `WebSearch` — that exhausts quota.
- Don't paste the full `CHANGELOG.md` into the prompt. Use `git log -10 --oneline` for recent context.
- Don't mark something P0 unless it's a real release blocker. Avoid severity inflation.
- Don't propose fixes for findings — that's the parent session's job. The skill scopes the audit; the user decides what to do.
