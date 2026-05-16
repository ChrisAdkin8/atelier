# Repository layout

Full tree with one-line annotations. The top-level [README.md](../README.md) lists only first-level entries; this file is the exhaustive reference.

```
.
├── README.md                          you are here
├── CHANGELOG.md                       spec + rig revisions
├── coding-harness-spec.md             the spec
├── Cargo.toml                         Rust workspace root (pins `rmcp = "0.1"`)
├── rust-toolchain.toml                pinned Rust 1.85.0
├── crates/
│   ├── atelier-core/                  agent loop, BYOM adapters, MCP client, session state
│   │   ├── Cargo.toml                 declares `rmcp = { workspace = true }` — the MCP client lives here
│   │   ├── catalog/                   bundled MCP server catalog
│   │   ├── skills/                    bundled skills (/review, /security-review, /test)
│   │   ├── subagents/                 bundled sub-agent types (researcher, test-runner, general-purpose)
│   │   ├── tools/                     bundled built-in tool manifests (read_file, write_file, edit_file, list_dir, grep, ast_grep, shell, spawn_subagent) — matches spec §15
│   │   └── templates/                 ATELIER.md seed template
│   ├── atelier-cli/                   headless `atelier` binary (entry point for `atelier init`; spec §11)
│   ├── atelier-gui/                   Tauri 2.x shell (scaffold)
│   └── atelier-tui/                   ratatui + crossterm frontend (scaffold)
├── pyproject.toml                     rig manifest (jsonschema, pytest)
├── Makefile                           one-command rig orchestration
├── schemas/                           20 JSON Schemas (see schemas/README.md)
├── tasks/
│   └── todo.md                        phased build plan + open questions
├── tests/
│   ├── _schema_helpers.py             shared registry for cross-schema $ref resolution
│   ├── validate_schemas.py            meta-validate every schema
│   ├── validate_artifacts.py          validate artifacts + envelope JSON in fewshot
│   ├── test_schemas.py                schema regression suite (valid+invalid corpora; cross-schema $ref)
│   ├── test_validators.py             end-to-end validator tests
│   ├── test_runner.py                 runner internals + subprocess tests
│   ├── perf/reference.md              reference machine spec (populated v13: M1 Pro / 32 GB / macOS 26.4.1)
│   ├── sessions/examples/             example session artifacts validated against schemas/session/v1.json
│   └── workload/
│       ├── canonical/                 11 task fixtures (10 Python + 1 TypeScript) + README + baseline procedure
│       │                              each task: prompt.md, expected.md, fixture/, meta.json, checks.json
│       └── runner/                    workload runner + baseline comparison tool
├── examples/                          reference manifests for pluggable extension points
│   ├── tools/                         custom tool manifests
│   ├── hooks/                         hook manifests
│   ├── skills/                        skill manifests (invocable as /<name>)
│   ├── subagents/                     sub-agent type manifests (spawned via spawn_subagent)
│   └── config/                        routing.json + persistent permission state examples
├── prompts/
│   └── protocol_fewshot/              Model Protocol few-shot examples (validated by validate_artifacts.py)
├── experiments/
│   └── rmcp_spike/                    Phase A prerequisite: rmcp maturity assessment procedure
├── LICENSE                            Apache 2.0
├── SECURITY.md                        vulnerability disclosure policy
├── CODE_OF_CONDUCT.md                 Contributor Covenant 2.1
├── CONTRIBUTING.md                    how to contribute
├── .github/
│   ├── workflows/check.yml            runs `make check` on every push/PR
│   ├── PULL_REQUEST_TEMPLATE.md       PR template
│   └── ISSUE_TEMPLATE/                bug-report + feature-request forms
└── ci/
    └── nightly/                       nightly CI job stubs (e.g., protocol overhead)
```
