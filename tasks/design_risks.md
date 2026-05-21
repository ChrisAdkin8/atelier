# Design risks

Date: 2026-05-21.

This file ranks architecture/design risks for Atelier as a coding harness. The ranking is about architectural blast radius and long-term maintainability, not known current defects.

## Severity-ranked risks

| Severity | Design risk | Why it ranks there | Suggested direction |
|---|---|---|---|
| **Critical** | Security/trust boundaries are distributed across multiple policy islands | Path safety, sandboxing, MCP approvals, hook approvals, GUI provider config, persistence, audit, and dispatcher behavior each enforce part of the trust model. Drift here can become sandbox escape, credential egress, or unsafe file mutation. | Define one trust-boundary contract and back it with invariant tests that prove every CLI/TUI/GUI + built-in/MCP path uses the same approval, containment, audit, and sandbox rules. Contract: [`docs/trust-boundary.md`](../docs/trust-boundary.md). |
| **High** | `Runner` is becoming the integration kernel | `crates/atelier-cli/src/runner.rs` owns adapter calls, tool dispatch, verification, resume, compaction, sub-agents, routing, and persistence. Changes here have high blast radius and are hard to reason about. | Split the run loop into explicit phase modules: model-call preparation, tool dispatch, verification, persistence/recovery, sub-agent coordination, and routing. |
| **High** | GUI backend is becoming a second orchestration layer | `crates/atelier-gui/src/lib.rs` duplicates or reinterprets provider, workspace, chat, memory, settings, and event policy. Surface-specific drift is a real risk. | Split backend commands into modules (`workspace`, `provider`, `chat`, `memory`, `skills`, `events`, `settings`) and route policy-sensitive work through shared core/runner APIs. |
| **High** | Large monolithic runtime/UI files concentrate too much behavior | `crates/atelier-tui/src/lib.rs`, `crates/atelier-core/src/dispatcher.rs`, `crates/atelier-cli/src/runner.rs`, and `crates/atelier-gui/src/lib.rs` are large enough that review quality drops and hidden coupling increases. | Establish soft LOC/function-size thresholds and refactor large files into state, render/input, orchestration phase, and policy modules. |
| **Medium** | `atelier-core` public API is too wide | `crates/atelier-core/src/lib.rs` re-exports nearly every subsystem, weakening encapsulation and making incidental internals look stable. | Replace blanket-style re-export growth with intentional API/prelude modules and keep unstable internals module-scoped. |
| **Medium** | Persistence still contains flexible or partially untyped state | Some session/persistence data remains schema-driven or `serde_json::Value`-like for evolution, which moves correctness from compile time to runtime validation and migration discipline. | Gradually type high-value persistence fields, document schema migration rules, and add load/save round-trip tests for every migration-sensitive shape. |
| **Medium** | MCP/BYOM extensibility depends on orchestration discipline | Adapter/tool abstractions are strong, but provider routing, model strategy, MCP tools, hooks, and sub-agents all meet in runner/dispatcher paths. Extension points can tangle without stricter boundaries. | Keep adapter, dispatcher, routing, and sub-agent responsibilities separate; add conformance tests for each new provider/tool transport. |
| **Medium** | Verification policy can drift by surface or path | CLI/TUI/GUI, Mock/live providers, MCP/built-in tools, and sub-agents all need equivalent verification and audit semantics. The current architecture supports this but does not make drift impossible. | Add cross-surface tests that assert equivalent verification/audit behavior for built-in vs MCP tools and parent vs sub-agent runs. |
| **Low** | Documentation/spec drift risk | Docs are extensive and useful, but large. Stale docs can mislead contributors even when code gates are green. | Keep periodic documentation sweeps and add lightweight link/example checks where practical. |
| **Low** | Experiment/spike crates carry dependency noise | Unused dependencies in spike crates are low operational risk, but they blur dependency hygiene signals. | Clean the spike manifests or add explicit `cargo machete` ignores with comments explaining why the dependencies remain. |

## Priority order

1. Consolidate the trust-boundary contract and invariant tests.
2. Decompose `Runner` into smaller, named run-loop phases.
3. Split GUI backend commands and state by responsibility.
4. Split the TUI and other large monoliths into reviewable modules.
5. Narrow public re-exports from `atelier-core`.

## Notes

The architecture is sound in its core direction: `atelier-core` is UI-free, the session actor + broadcast event model fits multiple surfaces, and BYOM/MCP/tool abstractions are appropriate for the harness. The risk is boundary erosion as features accumulate, not a flawed underlying model.
