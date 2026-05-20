# Plan - Deep-scan fixes from 2026-05-20

Date: 2026-05-20. Source: four parallel deep-scan agents covering Rust core, Rust GUI/TUI, Python/CI, and configs/schemas, plus local hotspot/static checks. No Critical findings. Local gates were clean at scan time: `cargo clippy --workspace --all-targets -- -D warnings` and `pytest -q` (`168 passed`).

Items are numbered **DS20-H01..H10**, **DS20-M01..M18**, and **DS20-L01..L06** by original severity for commit and PR traceability.

## Success criteria

- Every issue from the scan has either a code/doc fix, a failing-before/passing-after regression test, or a documented deferral with owner and reason.
- Security and filesystem issues are fixed before behavior/documentation drift.
- All changed Rust code passes `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and targeted `cargo test -p ...`.
- All changed Python/schema/CI code passes `pytest -q` and the relevant `make` target (`make check`, `make audit`, or schema/artifact validation).
- Docs and examples match runtime behavior after each bundle.

## Standing gates

Run after every bundle unless the bundle is documentation-only:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `pytest -q`
- `make check`

For security-sensitive bundles, add the targeted regression tests named below and confirm they fail on the old behavior.

---

## Bundle 1 - Process and egress hardening (DS20-H01, DS20-H02)

Touch points: `crates/atelier-core/src/dispatcher.rs`, `crates/atelier-core/src/subprocess.rs`, `crates/atelier-core/src/mcp/stdio_launcher.rs`, MCP config tests.

### DS20-H01 - Kill subprocesses on cancellation/deadline

- Severity: High.
- Files: `crates/atelier-core/src/dispatcher.rs:667-676`, `crates/atelier-core/src/subprocess.rs:212-245`.
- Problem: dispatcher cancellation/deadline can drop `tool.execute(...)`; `subprocess::run` spawns `tokio::process::Child` without `kill_on_drop(true)` or an abort-safe guard.
- Fix: set `kill_on_drop(true)` where available and add an abort-safe child/process-group guard that kills the whole process group on drop. Preserve existing timeout behavior and audit output.
- Verify: add a Rust async test that starts a shell payload with a long-lived child, cancels/deadlines the tool future, and asserts no descendant process remains.

### DS20-H02 - Prevent MCP stdio proxy-env bypass when network is denied

- Severity: High.
- File: `crates/atelier-core/src/mcp/stdio_launcher.rs:358-382`.
- Problem: proxy-deny env vars are set when `allow_net=false`, then manifest env overrides are applied afterward.
- Fix: reject or strip proxy-related keys from manifest env when network is denied, and/or apply deny proxy env after manifest env. Prefer explicit rejection with a clear `McpLaunchError`.
- Verify: unit test with `allow_net=false` and manifest `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` confirms launch is rejected or the deny values win; test `allow_net=true` still permits explicit proxy config.

### Bundle gate

`cargo test -p atelier-core subprocess mcp -- --nocapture` plus standing gates.

---

## Bundle 2 - Workspace containment and symlink safety (DS20-H03..H04, DS20-M01..M02)

Touch points: `write_file`, staging, persistence, CLI compaction blob storage, shared path-containment helpers.

### DS20-H03 - Validate containment before `write_file` creates directories

- Severity: High.
- File: `crates/atelier-core/src/tools/write_file.rs:92-110`.
- Problem: `create_dir_all(parent)` runs before workspace containment validation, so a symlinked path component can cause mutation outside the workspace before rejection.
- Fix: validate canonical existing ancestors before creation; create missing path components without following untrusted symlinks where possible. Reuse or extract a shared helper rather than adding local ad hoc checks.
- Verify: test workspace with a symlinked directory component pointing outside; `write_file` must fail and must not create outside directories/files.

### DS20-H04 - Close staged-commit symlink TOCTOU windows

- Severity: High.
- Files: `crates/atelier-core/src/staging.rs:649-688`, `crates/atelier-core/src/staging.rs:840-848`.
- Problem: target paths are checked, then later `create_dir_all`/`rename` act on paths that can be swapped by a concurrent workspace actor.
- Fix: use fd-relative/no-symlink operations where practical, or revalidate immediately before every filesystem mutation. Keep atomic-write and fsync guarantees intact.
- Verify: regression test simulates a path component changing to a symlink between validation and mutation; staged commit must reject without writing outside workspace.

### DS20-M01 - Refuse symlinked `.atelier` persistence paths

- Severity: Medium.
- Files: `crates/atelier-core/src/persistence.rs:245-270`, `crates/atelier-cli/src/compaction_blob.rs:103-118`.
- Problem: session and compaction paths create `.atelier/...` directories before proving canonical targets remain under the workspace.
- Fix: canonicalize and validate existing ancestors before directory creation; refuse symlinked `.atelier` unless a future explicit trust flow is added.
- Verify: tests for symlinked `.atelier` in session persistence and compaction blob creation confirm no outside write occurs.

### DS20-M02 - Make containment helper shared and audited

- Severity: Medium.
- Problem: multiple path mutation call sites need the same ancestor-before-mkdir discipline.
- Fix: centralize the helper in core path utilities with focused docs and tests; migrate `write_file`, staging, persistence, and compaction paths to it.
- Verify: helper unit tests cover normal nested creation, pre-existing symlink rejection, missing child creation, and platform-specific edge cases.

### Bundle gate

`cargo test -p atelier-core staging write_file persistence` and `cargo test -p atelier-cli compaction_blob` plus standing gates.

---

## Bundle 3 - Adapter, GUI, and TUI security/fragility (DS20-H05, DS20-M03..M06, DS20-L01..L02)

Touch points: `crates/atelier-gui`, `crates/atelier-tui`, GUI Svelte UI.

### DS20-H05 - Apply adapter base-URL allowlist to default/executor resolution

- Severity: High.
- Files: `crates/atelier-gui/src/lib.rs:1912-1960`, `crates/atelier-gui/src/lib.rs:2039-2050`.
- Problem: `resolve_default_adapter` and `resolve_executor_adapter` can build OpenAI-compatible adapters from workspace config/env without the allowlist/consent gate used by `swap_adapter`.
- Fix: route all OpenAI-compatible adapter construction through the same base URL allowlist and explicit consent flow. Ensure workspace `.atelier/providers.toml` cannot silently exfiltrate API keys.
- Verify: GUI backend tests for malicious workspace base URL in default and executor profiles assert rejection/pending consent; approved hosts still work.

### DS20-M03 - Activate compaction model-drift guard from frontend

- Severity: Medium.
- Files: `crates/atelier-gui/ui/src/lib/components/ContextPane.svelte:92-98`, `crates/atelier-gui/src/lib.rs:531-550`.
- Problem: backend supports `expected_model_id`, but frontend invokes `compact_context_items` with only `{ ids }`.
- Fix: pass current model ID into `ContextPane` and include it in the invoke payload.
- Verify: frontend/unit or integration test confirms payload includes current model ID; backend test confirms mismatch rejects.

### DS20-M04 - Use private RAII tempdirs in TUI demo workspaces

- Severity: Medium.
- File: `crates/atelier-tui/src/lib.rs:3264-3272`.
- Problem: demo workspace uses `std::env::temp_dir()` plus a predictable name and no RAII cleanup.
- Fix: use `tempfile::Builder::tempdir()` and hold the handle for the demo lifetime.
- Verify: test confirms directory is unique/private enough and removed after drop.

### DS20-M05 - Stop using global temp dir as TUI compaction workspace

- Severity: Medium.
- Files: `crates/atelier-tui/src/lib.rs:3481`, `crates/atelier-tui/src/lib.rs:3528`.
- Problem: compaction/expansion use the OS temp directory as workspace root, risking collisions and disclosure.
- Fix: use the active run workspace or a private per-process tempdir.
- Verify: test asserts compaction blob paths live under the selected workspace/private tempdir, not raw `std::env::temp_dir()`.

### DS20-M06 - Gate Tauri devtools to debug/dev builds

- Severity: Medium.
- File: `crates/atelier-gui/Cargo.toml:25-27`.
- Problem: `tauri` is compiled with `features = ["devtools"]` unconditionally.
- Fix: move devtools behind an explicit debug/dev feature or profile-specific configuration.
- Verify: release build metadata/dependency feature check confirms devtools is absent; dev build can still enable it.

### DS20-L01 - Use real TOML parsing for GUI settings

- Severity: Low.
- Files: `crates/atelier-gui/src/lib.rs:1724-1749`, `crates/atelier-gui/src/lib.rs:1758-1766`.
- Problem: `gui.toml` is hand-parsed and hand-written without escaping; quotes/newlines in paths corrupt persistence.
- Fix: use TOML serde parser/serializer and cap settings file size.
- Verify: test path values containing quotes, backslashes, and newlines round-trip or are rejected clearly.

### DS20-L02 - Add confirmation or undo for memory deletion

- Severity: Low.
- Files: `crates/atelier-gui/ui/src/lib/components/MemoryPane.svelte:66-69`, `crates/atelier-gui/ui/src/lib/components/MemoryPane.svelte:202-209`.
- Problem: durable memory cards can be deleted by one misclick.
- Fix: add confirmation or undo consistent with existing eviction/compaction flows.
- Verify: UI test or component-level assertion that delete is not invoked until confirmation.

### Bundle gate

`cargo test -p atelier-gui`, `cargo test -p atelier-tui`, frontend checks if available, plus standing gates.

---

## Bundle 4 - MCP protocol, catalog, secrets, and hook behavior (DS20-H06..H08, DS20-M07..M12)

Touch points: MCP launchers/config/catalog, schemas, SECURITY/CAPABILITIES docs, hook execution.

### DS20-H06 - Align HTTP/SSE MCP `allow_net` schema/docs/runtime

- Severity: High.
- Files: `schemas/config/mcp_servers.v1.json:57,98`, `crates/atelier-core/src/mcp/http_launcher.rs:111`.
- Problem: schema/docs say HTTP/SSE ignore `allow_net` and default false; launcher refuses HTTP/SSE unless `allow_net=true`.
- Fix: choose fail-closed semantics: require `allow_net: true` for HTTP/SSE in schema, examples, docs, and bundled catalog entries.
- Verify: schema validation rejects HTTP/SSE servers without `allow_net: true`; launcher and docs agree.

### DS20-H07 - Resolve or de-advertise `${keychain:...}` secrets

- Severity: High.
- Files: `SECURITY.md:58`, `crates/atelier-core/src/mcp_config.rs:230`, `crates/atelier-core/src/mcp_config.rs:268`.
- Problem: docs recommend `${keychain:...}`, but runtime always returns `KeychainNotYet`.
- Fix: either implement keychain resolution for supported platforms or remove the advertised secure path and explicitly reject unresolved keychain tokens with actionable messaging. Prefer implementation if scope is bounded.
- Verify: test for successful configured keychain resolution, or docs/test asserting unresolved keychain tokens fail closed and plaintext is documented as discouraged.

### DS20-H08 - Pin bundled `npx -y` MCP catalog packages

- Severity: High.
- File: `crates/atelier-core/catalog/mcp_servers.json:15,100`.
- Problem: curated catalog materializes third-party packages without version or integrity pins.
- Fix: pin exact package versions and add integrity/checksum metadata if the installation path can enforce it; otherwise emit an approval warning for upgrades.
- Verify: catalog schema/test rejects unversioned npm packages in curated entries.

### DS20-M07 - Replace fragile HTTP MCP protocol check

- Severity: Medium.
- File: `crates/atelier-core/src/mcp/http_launcher.rs:317-333`.
- Problem: protocol version is checked with `format!("{:?}", protocol_version).contains(...)`.
- Fix: use typed serde string extraction like `stdio_launcher`.
- Verify: unit tests for exact supported version, unsupported version containing the supported substring, and missing/malformed version.

### DS20-M08 - Add meaningful MCP integration coverage to normal CI

- Severity: Medium.
- Files: `crates/atelier-cli/tests/mcp_integration.rs:86-170`, `crates/atelier-cli/tests/mcp_integration.rs:465-509`.
- Problem: end-to-end stdio/HTTP tests are ignored or require live `npx`/network/manual setup.
- Fix: add local mock MCP server fixtures that run without network in CI and cover launch, registration, shutdown, and audit rows.
- Verify: ignored coverage is replaced or supplemented by non-ignored tests.

### DS20-M09 - Remove or implement HTTP hook manifests

- Severity: Medium.
- Files: `schemas/config/hook_manifest.v1.json:45`, `crates/atelier-core/src/dispatcher.rs:1990`.
- Problem: schema accepts `implementation.kind = "http"`, but production executor skips non-shell hooks as no-op.
- Fix: either implement HTTP hook executor or remove HTTP kind from v1 schema/docs. Prefer removal unless there is a near-term runtime design.
- Verify: schema rejects HTTP hooks if removed, or executor test proves HTTP hook dispatch if implemented.

### DS20-M10 - Align hook event docs with schema/code

- Severity: Medium.
- Files: `CAPABILITIES.md:123`, `schemas/config/hook_manifest.v1.json:17`, `crates/atelier-core/src/hooks.rs:53`.
- Problem: docs advertise `user-prompt-submit` and `session-start`; schema/code support only `pre-tool`, `post-tool`, `on-verify-pass`, `on-verify-fail`.
- Fix: separate host-harness settings hooks from Atelier hook manifests, or add runtime/schema support for advertised events.
- Verify: doc/schema/code event list is generated or tested for consistency.

### DS20-M11 - Make MCP catalog secret targets explicit

- Severity: Medium.
- Files: `schemas/config/mcp_catalog.v1.json:92`, `crates/atelier-core/catalog/mcp_servers.json:104`.
- Problem: `requires_secrets` records only keychain name/location; no env var/header target/template is defined.
- Fix: add explicit `env_name`, `header_name`, or template fields and update catalog entries.
- Verify: schema requires target fields for each secret injection mode; catalog validates.

### DS20-M12 - Align subagent default max turns

- Severity: Medium.
- Files: `schemas/config/subagent_type.v1.json:29`, `crates/atelier-core/src/subagents.rs:39`.
- Problem: schema says default max turns is 25; code constant is 10.
- Fix: update schema/docs to 10 or centralize default generation from code. Prefer schema/doc correction unless product wants 25.
- Verify: test comparing schema default/description fixture against runtime constant.

### Bundle gate

`cargo test -p atelier-core mcp`, `cargo test -p atelier-cli mcp`, schema validation, catalog validation, plus standing gates.

---

## Bundle 5 - Python, CI, schema validation, and telemetry correctness (DS20-H09..H10, DS20-M13..M18, DS20-L03)

Touch points: `scripts/npm_ioc_sweep.py`, `tests/`, workflows, schema validators.

### DS20-H09 - Support npm lockfile v1 dependency scanning

- Severity: High.
- File: `scripts/npm_ioc_sweep.py:96,124`.
- Problem: scanner only checks `doc["packages"]`; lockfile v1 stores packages under top-level recursive `dependencies`.
- Fix: add recursive lockfile v1 dependency traversal for lifecycle scripts and suspicious resolved URLs.
- Verify: tests with malicious v1 `postinstall` and `git+` URL fail before fix and pass after.

### DS20-H10 - Enforce JSON Schema `format` checks

- Severity: High.
- Files: `tests/_schema_helpers.py:41-42`, `tests/validate_artifacts.py:152`, schemas with `uuid`/`date-time`/`uri`.
- Problem: validators omit `jsonschema.FormatChecker`, so invalid formatted fields pass.
- Fix: instantiate validators with `format_checker=jsonschema.FormatChecker()`.
- Verify: fixtures with invalid UUID/date-time/URI are rejected by schema and artifact validation.

### DS20-M13 - Honor live test exit code in nightly phase B summary

- Severity: Medium.
- File: `.github/workflows/nightly_phase_b_gate.yml:120-150,176-218`.
- Problem: live test `exit_code` can be ignored if a summary file exists, setting `all_passed=true`.
- Fix: include nonzero exit in status and `all_passed` composition.
- Verify: workflow composition test feeds nonzero exit plus existing summary and expects failed/yellow status, not green.

### DS20-M14 - Fail baseline comparison on missing workload tasks

- Severity: Medium.
- File: `tests/workload/runner/compare_baselines.py:47-57,80-85`.
- Problem: missing tasks are printed but pass/fail uses only aggregate ratio.
- Fix: fail when task sets differ unless an explicit allowlist flag is passed.
- Verify: test missing an expensive/failing task returns nonzero.

### DS20-M15 - Check job-level reusable workflow `uses`

- Severity: Medium.
- File: `tests/test_ci.py:34-39`.
- Problem: `_flatten_uses()` scans step-level `uses` only, missing `jobs.<job>.uses`.
- Fix: include job-level reusable workflow references in pinning/policy checks.
- Verify: fixture with `jobs.foo.uses: owner/repo/.github/workflows/x.yml@tag` is caught.

### DS20-M16 - Validate non-object fenced JSON intentionally

- Severity: Medium.
- Files: `tests/validate_artifacts.py:124-126`, `schemas/model_protocol/envelope.v1.json:6`.
- Problem: non-object fenced JSON is marked OK/skipped even though envelope schema requires object.
- Fix: fail non-object fenced JSON by default; add a narrow documented exception if needed.
- Verify: fixture with fenced JSON array/string in an envelope context fails.

### DS20-M17 - Tie telemetry `channel` to matching body schema

- Severity: Medium.
- Files: `schemas/telemetry/payload.v1.json:11`, `schemas/telemetry/payload.v1.json:17`.
- Problem: `channel: "crash"` can validate with a usage body.
- Fix: add `if`/`then` constraints mapping each channel to the correct `body` variant.
- Verify: cross-channel/body fixtures are rejected; valid channel/body pairs pass.

### DS20-M18 - Enforce skill arg default semantics

- Severity: Medium.
- File: `schemas/config/skill_manifest.v1.json:24`.
- Problem: `args[].default` says only valid when `required=false`, but schema does not enforce it.
- Fix: add conditional validation forbidding `default` when `required=true`.
- Verify: invalid manifest fixture is rejected; optional arg default remains valid.

### DS20-L03 - Include `.yaml` workflow files in CI policy tests

- Severity: Low.
- File: `tests/test_ci.py:26-27`.
- Problem: tests glob only `*.yml`; GitHub also runs `*.yaml`.
- Fix: include both extensions.
- Verify: fixture or repository test catches policy violations in `.yaml`.

### Bundle gate

`pytest -q`, `make audit`, schema/artifact validation, plus standing gates where Rust changed.

---

## Bundle 6 - Release metadata, security docs, and product docs (DS20-L04..L06)

Touch points: workspace `Cargo.toml`, crate manifests, `SECURITY.md`, `README.md`, `CAPABILITIES.md`, docs/layout where needed.

### DS20-L04 - Fix placeholder or publishable crate metadata

- Severity: Low.
- Files: `Cargo.toml:10-16`, `crates/atelier-cli/Cargo.toml:1-7`.
- Problem: workspace version/repository metadata is placeholder and app crates are publishable by default.
- Fix: set real metadata or mark non-published crates `publish = false`.
- Verify: `cargo package --list -p atelier-cli` or metadata check confirms intended publish state.

### DS20-L05 - Replace placeholder vulnerability contact

- Severity: Low.
- File: `SECURITY.md:10`.
- Problem: email is `security@atelier.example` with a rotate-before-release note.
- Fix: replace with a real monitored address, GitHub private vulnerability reporting instructions, or remove email until ready.
- Verify: docs review only.

### DS20-L06 - Reconcile README/CAPABILITIES GUI/TUI behavior

- Severity: Low.
- Files: `README.md:182`, `README.md:196`, `CAPABILITIES.md:38`.
- Problem: README describes GUI diff/hunk accept-reject and `submit_approval`; CAPABILITIES says GUI is chat-REPL and TUI is file-level.
- Fix: update README/layout docs to current GUI/TUI behavior and remove stale command references.
- Verify: docs review plus link/reference check if available.

### Bundle gate

Docs review; `cargo metadata --no-deps` if Cargo manifests changed.

---

## Recommended execution order

1. Bundle 1: process/egress hardening.
2. Bundle 2: filesystem containment and symlink safety.
3. Bundle 4 high-severity MCP/catalog/secret items.
4. Bundle 3 GUI/TUI security and fragility.
5. Bundle 5 Python/CI/schema correctness.
6. Bundle 6 low-severity metadata/docs.

## Open decisions before implementation

- DS20-H07: implement keychain support now, or fail closed and update docs until support exists.
- DS20-M09: implement HTTP hooks now, or remove them from v1 schema/docs.
- DS20-M12: keep runtime default at 10, or intentionally raise it to 25.
- DS20-L05: choose the real security reporting channel.
