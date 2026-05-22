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
| Resume pointers | UI/driver resume pointers must refer to the durable session UUID that was written on disk, and UI surfaces must validate `session.json` before chaining a follow-up run. Stale in-memory pointers are cleared rather than retried indefinitely. | `atelier-cli::Runner`, `atelier-gui::take_valid_resume_session_id` |
| Tool dispatch | Built-in and MCP tools are registered into the same `ToolRegistry` and dispatched through the same dispatcher lifecycle: validation, hooks, concurrent-edit policy, execution, ledger, audit/event projection, and verification input. | `dispatcher`, `tools::register_builtins`, `mcp::register_mcp_servers` |
| Network egress | Subprocess network access is denied by default. MCP HTTP/SSE egress is explicit, host-checked, and audited. Provider endpoints that can receive credentials must be allowlisted or explicitly supplied by the user at the invoking surface. | `sandbox`, `subprocess`, `mcp`, `trust_boundary` |
| Approval state | MCP, hook, LSP, and adapter-swap approvals are scoped to the workspace or current surface and must not silently carry across unrelated repositories. | `mcp_config`, `hooks`, `lsp`, GUI provider swap |
| UI surfaces | CLI, GUI, and TUI may differ in presentation, but policy-sensitive actions must delegate to shared core/runner helpers rather than reimplementing local security decisions. | `atelier-core`, `atelier-cli::Runner`, GUI/TUI command wrappers |

## Provider credential egress

OpenAI-compatible endpoints are intentionally flexible because Atelier is
BYOM. The dangerous case is a repo-controlled profile silently combining an
arbitrary remote `base_url` with a credential from `OPENAI_API_KEY` or a
profile `api_key = "keyring:..."` reference.

The shared rule is:

1. Allow known public provider hosts, loopback endpoints, and explicitly
   reviewed project-owned OpenAI-compatible infrastructure.
2. Allow arbitrary endpoints when no credential is present.
3. Allow arbitrary endpoints when the user supplied `--base-url` explicitly for
   the current run.
4. Reject repo-profile endpoints that are both unallowlisted and credentialed.

The shared predicate lives in `atelier_core::trust_boundary`.

The current built-in provider credential allowlist is:

| Host | Intended use |
|---|---|
| `api.anthropic.com` | Anthropic cloud provider endpoint. |
| `api.openai.com` | OpenAI cloud provider endpoint. |
| `atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com` | Project-owned Atelier dev vLLM ALB for OpenAI-compatible GPU inference. |
| `localhost`, `127.0.0.1`, `::1` | Local OpenAI-compatible servers and local proxies/tunnels. |

The GUI adapter swap path uses the same host list. This means a profile such as
`base_url = "http://atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com/v1"`
can receive OpenAI-compatible credentials without being rejected by
`swap_adapter`, while other repo-controlled remote hosts still require explicit
user action at the invoking surface.

## Required regression tests

- Path helpers reject symlink escapes before creating outside files or
  directories.
- Persistence and compaction writes reject symlinked `.atelier` escapes.
- GUI provider default/swap/executor resolution and CLI profile resolution use
  the shared provider credential-egress predicate.
- Resumed Runner reports return the durable persisted UUID, and GUI resume
  validation drops missing/deleted session manifests before `with_resume`.
- Built-in and MCP tools register through the same dispatcher abstraction and
  produce compatible audit/ledger/event behavior.
- Non-interactive concurrent-edit resolution still uses the dispatcher's
  read-set and file-watcher policy.
