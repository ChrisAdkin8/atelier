# Code quality metrics

Collected on 2026-05-20/21 from the tracked repository state.

These metrics are a scorecard, not a single quality grade. "Good" means the number is healthy for this repo today; "Watch" means it is acceptable but worth tracking; "Risk" means it deserves follow-up or targeted review.

## Gate health

| Metric | Result | Judgement | Comment |
|---|---:|---|---|
| `cargo fmt --check` | pass | Good | Formatting is mechanically enforced and currently clean. |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass | Good | Rust lint debt is zero under the repo's strict warning-as-error policy. |
| `cargo test --workspace` | pass | Good | The full Rust workspace test suite is green. |
| `make check` | pass | Good | The project-specific rig, schema, artifact, and workload checks are green. This is the strongest Atelier-specific quality signal. |
| `make audit` | pass | Good | Known dependency/security checks and the repo's supply-chain sweep are clean. |
| Schemas valid | 26/26 | Good | Every schema meta-validates. |
| Artifacts validated | 81/81 | Good | All matched checked-in artifacts conform to their schemas. |
| Rig self-tests | 185 passed | Good | The Python rig has a healthy self-test baseline. |
| Workflow files | 4 | Neutral | Small enough to audit manually; useful context rather than a quality signal by itself. |
| GitHub Actions `uses:` SHA-pinned | 27 | Good | Third-party Actions are pinned by commit SHA, reducing CI supply-chain risk. |
| Unpinned workflow `uses:` | 0 | Good | No obvious unpinned Action references were found. |

## Size and maintainability

| Metric | Result | Judgement | Comment |
|---|---:|---|---|
| Nonblank source LOC proxy | 72,097 | Watch | This is a substantial codebase. Size is not bad on its own, but it makes modular boundaries and focused tests important. |
| Nonblank test LOC proxy | 9,627 | Watch | There is significant test code, but the proxy ratio leaves room for more targeted regression tests around complex core paths. |
| Test-to-code LOC proxy | 13.35% | Watch | Reasonable for a systems/Rust workspace with integration rig coverage, but not high enough to treat as comprehensive coverage. |
| Markdown/doc nonblank LOC | 8,552 | Good | Documentation volume is strong and appropriate for a spec-first project. Needs periodic sweeps to prevent drift. |
| Rust nonblank LOC | 62,853 | Neutral | Rust is the dominant implementation language as expected. Track crate-level growth rather than the absolute number. |
| Python nonblank LOC | 3,679 | Good | Rig and validation code are material but not disproportionately large. |
| Svelte nonblank LOC | 4,163 | Watch | GUI frontend size is moderate; keep component boundaries tight as chat/workspace features grow. |
| JSON nonblank LOC | 6,951 | Neutral | Expected from schemas, fixtures, manifests, and lockfiles. Schema validation keeps this manageable. |

## Hotspots

| Metric | Count | Judgement | Comment |
|---|---:|---|---|
| `unsafe` tokens in Rust crates | 23 | Watch | Low enough to audit manually. Each unsafe site should have a documented invariant and ideally a focused test around the safe wrapper. |
| `.unwrap(` calls in crates/tests | 1,454 | Watch | High as a raw count, but this includes tests. Production-only counts would be more actionable; still worth watching for new unwraps in runtime paths. |
| `.expect(` calls in crates/tests | 331 | Watch | Acceptable in tests and setup code; risky in long-running runtime paths unless the message explains an invariant. |
| `panic!` / `todo!` / `unimplemented!` | 169 | Watch | Likely test-heavy, but production occurrences should be reviewed. `todo!`/`unimplemented!` in shipped paths would be bad. |
| Ignored Rust tests | 8 | Watch | Some ignored live/integration tests are expected, but each should have a clear reason and manual/nightly path. |
| Pytest skip/xfail markers | 4 | Good | Low skip/xfail count; unlikely to be hiding large unverified areas. |

## Largest tracked text files

| File | Nonblank LOC | Judgement | Comment |
|---|---:|---|---|
| `crates/atelier-tui/src/lib.rs` | 5,518 | Risk | Very large single module. Consider splitting rendering, input handling, state projection, and tests into submodules. |
| `crates/atelier-cli/tests/run_integration.rs` | 4,995 | Watch | Large test file is less risky than large production code, but helper extraction could improve maintainability. |
| `crates/atelier-core/src/dispatcher.rs` | 4,248 | Risk | Core orchestration file is large and security-sensitive. It should stay under active review for complexity and error-path coverage. |
| `crates/atelier-gui/src/lib.rs` | 3,780 | Risk | Tauri backend has many responsibilities. Splitting commands/settings/provider logic would reduce review burden. |
| `crates/atelier-cli/src/runner.rs` | 3,429 | Risk | Agent-loop driver is central and complex. Keep adding regression tests for every behavior change and consider extracting phases into smaller modules. |

## Overall reading

The repo has strong mechanical quality gates: formatting, Clippy, Rust tests, schema validation, rig checks, audit checks, and CI pinning are all green. The main quality risk is not current breakage; it is maintainability concentration in a few very large Rust modules and broad raw counts of panic/unwrap-style constructs that need production-path filtering.

Recommended follow-up metrics:

1. Add a production-only `unwrap` / `expect` / `panic!` count that excludes tests.
2. Track largest-file LOC over time and set soft thresholds for refactor candidates.
3. Add coverage instrumentation locally or in CI with `cargo llvm-cov` if coverage trend becomes important.
4. Maintain an explicit inventory of `unsafe` sites with invariants and owning tests.

## Missing / additional critical quality checks

Collected on 2026-05-21 after the initial scorecard.

| Measure | Result | Judgement | Commentary |
|---|---:|---|---|
| Coverage tooling (`cargo llvm-cov`) | missing locally | Risk | The repo has strong pass/fail tests, but no collected line/branch coverage trend. This is the biggest missing quantitative quality measure. Add `cargo llvm-cov` in CI or as a periodic local report before treating test depth as known. |
| Mutation testing (`cargo mutants`) | missing locally | Watch | Mutation testing is not required on every PR, but it would be valuable for core modules where ordinary tests can pass while assertions are weak. Recommended as a nightly/manual gate for `dispatcher`, `runner`, `staging`, `path_safety`, and `persistence`. |
| License/advisory policy (`cargo deny`) | missing locally | Risk | `make audit` covers known advisories, but a formal license/source/duplicate policy is not currently measured here. Add `cargo deny` if dependency policy needs to be enforceable. |
| Dependency freshness (`cargo outdated`) | missing locally | Watch | Staleness is not the same as vulnerability, but without this metric dependency drift is invisible until updates become painful. |
| Unused dependency scan (`cargo machete`) | failed with findings | Watch | `cargo machete` found unused dependencies only in spike crates: `experiments/lsp_spike` (`async-lsp`, `futures`, `lsp-types`, `serde`, `serde_json`, `tower`) and `experiments/rmcp_spike` (`serde`). This is low risk because they are experiments, but the findings should be cleaned up or ignored explicitly. |
| Unused dependency alternatives (`cargo udeps`) | missing locally | Watch | `cargo udeps` can catch cases `cargo machete` misses, but requires nightly and is best suited to periodic local/CI runs. |
| Complexity tools (`scc`, `tokei`) | missing locally | Neutral | External complexity/LOC tools are absent. The scorecard uses internal LOC/function-length proxies instead, which is enough for hotspot discovery but not a formal complexity gate. |
| Markdown link checker (`lychee`) | missing locally | Watch | No full external link check was available. A local-link proxy was run instead. |

## Production-path panic / unwrap proxy

The earlier hotspot counts included tests. This pass applies a crude production-only filter: tracked Rust files under `crates/`, excluding obvious test files and `#[cfg(test)] mod tests` blocks. It is not a parser, but it is a better risk signal than the all-code count.

| Metric | Production-proxy count | Judgement | Commentary |
|---|---:|---|---|
| `unsafe` | 7 | Watch | Small enough to audit manually. Six are in `crates/atelier-core/src/lsp/install.rs`; one is in `crates/atelier-core/src/subprocess.rs`. Each site should carry a clear invariant and safe wrapper test. |
| `.unwrap(` | 38 | Watch | Much healthier than the all-code count, but still worth tracking. Top production files are `dispatcher.rs` (10), `audit.rs` (8), `main.rs` (7), and `tools/mod.rs` (6). |
| `.expect(` | 44 | Watch | Acceptable if expectations encode invariants, but GUI/backend and core orchestration sites should be reviewed for user-triggerable failure paths. |
| `panic!` / `todo!` / `unimplemented!` | 11 | Risk | Any production-path `todo!` or `unimplemented!` is release-blocking; production `panic!` should be reserved for impossible invariants. Top file is `dispatcher.rs` with 6. |

## Complexity proxies

| Metric | Result | Judgement | Commentary |
|---|---:|---|---|
| Largest Rust file | `crates/atelier-tui/src/lib.rs` — 5,518 nonblank LOC | Risk | This remains the clearest maintainability hotspot. Split state, rendering, key handling, and tests into modules. |
| Largest production function proxy | `crates/atelier-cli/src/runner.rs:1137 run` — 1,559 nonblank LOC | Risk | A function this large is hard to review and reason about. It should be decomposed into phase helpers with regression tests around each transition. |
| `dispatcher.rs::dispatch` proxy length | 241 nonblank LOC | Watch | Large but not extreme. Because it is central/security-sensitive, keep targeted tests around error paths, hooks, staging, and ledger behavior. |
| `atelier-gui/src/lib.rs::bridge_event` proxy length | 340 nonblank LOC | Watch | Projection functions grow naturally with event count, but snapshot-style tests or table-driven mapping would reduce drift risk. |
| `atelier-tui/src/lib.rs::run_async` proxy length | 364 nonblank LOC | Watch | UI event loops are naturally broad; still a candidate for extracting input/event/render orchestration. |

## Frontend quality gates

| Measure | Result | Judgement | Commentary |
|---|---:|---|---|
| `npm --prefix crates/atelier-gui/ui run check` | pass, 0 errors / 1 warning | Watch | Type/Svelte diagnostics are clean except one accessibility warning: `Header.svelte` uses `autofocus`. The warning is not critical but should be justified or removed. |
| `npm --prefix crates/atelier-gui/ui run build` | pass, Vite chunk warning | Watch | Build succeeds. Large chunks over 500 KiB are reported, especially Mermaid/parser-related chunks; this is acceptable for now but suggests future code-splitting if startup size matters. |

## Feature / toolchain matrix proxies

| Measure | Result | Judgement | Commentary |
|---|---:|---|---|
| Rust toolchain | `rustc 1.85.0`, `cargo 1.85.0` | Good | Matches the pinned project toolchain. |
| `cargo check --workspace --all-targets` | pass | Good | Workspace-wide check target is clean. |
| `cargo check -p atelier-core --no-default-features --all-targets` | pass | Good | Core compiles under the no-default-features profile. |
| `cargo check -p atelier-cli --no-default-features --all-targets` | pass | Good | CLI compiles under the no-default-features profile. |
| `cargo check -p atelier-tui --no-default-features --all-targets` | pass | Good | TUI compiles under the no-default-features profile. |
| `cargo check -p atelier-gui --no-default-features --all-targets` | pass | Good | GUI crate compiles under the no-default-features profile. |

## Dependency and docs proxies

| Measure | Result | Judgement | Commentary |
|---|---:|---|---|
| Duplicate dependency tree (`cargo tree -d`) | 632 output lines | Watch | There are duplicate transitive versions, including common ecosystem duplicates such as `base64` and `bitflags`. This is not automatically bad, but it affects build size and should be reviewed periodically. |
| Local Markdown links checked | 76 | Neutral | Internal link proxy only; external links were not checked because `lychee` is unavailable. |
| Missing local Markdown links | 3 | Good | The three misses are placeholder/example paths (`file.md`, `rel/path.ext`, `path`), not real documentation breakage. |

## Updated priority recommendations

1. **Add coverage trend first**: install/use `cargo llvm-cov` and publish workspace line/branch coverage in CI or nightly artifacts.
2. **Gate production-path panics**: create a small script that excludes tests and fails on new production `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!` unless allowlisted.
3. **Split the largest runtime modules**: start with `Runner::run`, `atelier-tui/src/lib.rs`, `dispatcher.rs`, and `atelier-gui/src/lib.rs`.
4. **Introduce dependency policy**: add `cargo deny` for license/source/advisory policy and either clean or explicitly ignore the `cargo machete` spike-crate findings.
5. **Add periodic mutation testing**: run `cargo mutants` manually/nightly on selected core modules once the tool is available.
