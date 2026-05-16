# Host-harness contract

What a BYOM host harness must provide for Atelier's hooks to work. Atelier is host-agnostic by design (§1, §15) but the hook scripts assume the host honors a small contract.

This file is the canonical reference. Audit-finding **N41** flagged the contract as unverified across hosts; this document makes the assumptions explicit so a new host integrator knows what to honor.

## Required behavior

### 1. Working directory

When the host runs a hook command listed in `.atelier/settings.json`, it MUST set `cwd` to the Atelier project root (the directory containing `.atelier/`).

**Why:** hook commands use project-relative paths (e.g., `.atelier/hooks/bounded-reads.sh`) so the configuration carries no vendor-specific env var. The relative path resolves only if `cwd` is the project root.

**How a host satisfies this:** call `chdir(project_root)` before `exec()`-ing the hook command, or invoke the command via a shell launched with `cwd=project_root`.

### 2. Stdin payload

When the host runs a hook command, it MUST pipe the event payload to the command's stdin as a single JSON object, then close stdin.

The expected shape varies by event:

- **`PreToolUse`** — `{tool_name: string, tool_input: object}`. `tool_name` matches the registered tool's `name`; `tool_input` is the args object the model emitted.
- **`UserPromptSubmit`** — `{prompt: string}`. The raw user message.
- **`SessionStart`** — `{}` is acceptable; current hooks don't read the payload.

Hook scripts that need `python3` or `jq` no-op out when those are absent (`command -v jq >/dev/null || exit 0`), so a host with neither still gets a clean session start — just no hook side effects.

### 3. Stdout response

A hook MAY write a single JSON object to stdout. The host SHOULD honor `hookSpecificOutput.additionalContext: <string>` by injecting that string into the model's context for the next turn. Hooks that have nothing to say emit nothing.

### 4. Exit code semantics

A hook command's exit code is **advisory, never blocking** (spec §15). A non-zero exit MUST NOT cancel the action the hook responded to. The host MAY log the failure; it MUST proceed with the tool dispatch / prompt / session start as if the hook had succeeded.

### 5. No required env vars

Hooks do not depend on host-set env vars. Specifically, hooks DO NOT require `$CLAUDE_PROJECT_DIR`, `$ATELIER_PROJECT_DIR`, or any other host-specific variable to be set. Each hook script derives `ATELIER_PROJECT_DIR` from `${BASH_SOURCE[0]}` at startup. If a host wishes to set additional env vars (e.g., a model identifier for telemetry), the hooks ignore them.

### 6. Time budget

The host SHOULD enforce a per-hook timeout (spec §15 suggests "median + warning"). Atelier's hooks aim to complete in <50ms typical, <500ms worst-case on a cold cache. A host that kills the hook after 5s is well within bounds.

## What Atelier does NOT require

- A specific shell. Hooks use `#!/usr/bin/env bash`; any POSIX-compatible bash works.
- A specific Python or jq version. Hooks check `command -v` and no-op if absent.
- Privileged execution. Hooks read stdin, write stdout, and (for `session-start-memcheck.sh`) optionally call out to `~/.atelier/bin/memcheck.sh` if installed.
- Network access. Hooks are local-only by construction.

## Hook-by-hook quick reference

| Hook                          | Event             | Reads stdin       | Writes stdout                                | Side effects |
|-------------------------------|-------------------|-------------------|----------------------------------------------|--------------|
| `bounded-reads.sh`            | `PreToolUse`      | `{tool_name, tool_input}` | nudge object iff unbounded Read/Grep | none |
| `save-nudge.sh`               | `UserPromptSubmit`| `{prompt}`         | nudge object iff prompt looks like a directive | none |
| `session-start-memcheck.sh`   | `SessionStart`    | empty `{}` accepted | nothing (logs to stderr)                   | regenerates `.atelier/memory/MEMORY.md` if `~/.atelier/bin/memcheck.sh` is installed |

## Verifying a new host integration

A host integrator who wants to confirm Atelier's hooks fire correctly:

```sh
# 1. Hook resolves with cwd=project_root + correct stdin.
cd /path/to/atelier
echo '{"tool_name":"Read","tool_input":{"file_path":"coding-harness-spec.md"}}' \
  | .atelier/hooks/bounded-reads.sh
# Expected: JSON object with hookSpecificOutput.additionalContext mentioning "limit".

# 2. SessionStart works even without ~/.atelier/bin/memcheck.sh installed.
.atelier/hooks/session-start-memcheck.sh
# Expected: exit 0, possibly a stderr line about memcheck.sh missing.
```

If both of those succeed, your host satisfies the contract.
