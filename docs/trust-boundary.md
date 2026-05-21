# Trust boundary contract

Atelier has several user-visible surfaces (CLI, GUI, TUI) and several tool
origins (built-in tools, MCP tools, hooks, LSP helpers, provider adapters).
The harness is safe only when all of those paths enforce the same trust
boundary. This note records the invariants that are expected to be enforced by
shared helpers and tests.

## Invariants

| Boundary | Invariant | Primary implementation |
|---|---|---|
| Workspace file reads/writes | Model-supplied paths are repo-relative, contain no `..`, and canonicalize inside the workspace before I/O. Directory creation walks one component at a time and rejects symlink escapes before mutation. | `atelier_core::path_safety` |
| Persistence writes | Session, registry, compaction, and split-sidecar writes use atomic temp-file persistence, parent-directory sync where supported, and workspace-contained directory creation for `.atelier/` paths. | `persistence`, `compaction_blob`, `path_safety` |
| Tool dispatch | Built-in and MCP tools are registered into the same `ToolRegistry` and dispatched through the same dispatcher lifecycle: validation, hooks, concurrent-edit policy, execution, ledger, audit/event projection, and verification input. | `dispatcher`, `tools::register_builtins`, `mcp::register_mcp_servers` |
| Network egress | Subprocess network access is denied by default. MCP HTTP/SSE egress is explicit, host-checked, and audited. Provider endpoints that can receive credentials must be allowlisted or explicitly supplied by the user at the invoking surface. | `sandbox`, `subprocess`, `mcp`, `trust_boundary` |
| Approval state | MCP, hook, LSP, and adapter-swap approvals are scoped to the workspace or current surface and must not silently carry across unrelated repositories. | `mcp_config`, `hooks`, `lsp`, GUI provider swap |
| UI surfaces | CLI, GUI, and TUI may differ in presentation, but policy-sensitive actions must delegate to shared core/runner helpers rather than reimplementing local security decisions. | `atelier-core`, `atelier-cli::Runner`, GUI/TUI command wrappers |

## Provider credential egress

OpenAI-compatible endpoints are intentionally flexible because Atelier is
BYOM. The dangerous case is a repo-controlled profile silently combining an
arbitrary remote `base_url` with a credential from `OPENAI_API_KEY`.

The shared rule is:

1. Allow known public provider hosts and loopback endpoints.
2. Allow arbitrary endpoints when no credential is present.
3. Allow arbitrary endpoints when the user supplied `--base-url` explicitly for
   the current run.
4. Reject repo-profile endpoints that are both unallowlisted and credentialed.

The shared predicate lives in `atelier_core::trust_boundary`.

## Required regression tests

- Path helpers reject symlink escapes before creating outside files or
  directories.
- Persistence and compaction writes reject symlinked `.atelier` escapes.
- GUI provider default/swap/executor resolution and CLI profile resolution use
  the shared provider credential-egress predicate.
- Built-in and MCP tools register through the same dispatcher abstraction and
  produce compatible audit/ledger/event behavior.
- Non-interactive concurrent-edit resolution still uses the dispatcher's
  read-set and file-watcher policy.
