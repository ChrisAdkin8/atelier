# Examples

Reference manifests for Atelier's pluggable extension points. Each example validates against its declared schema and exists primarily so the schemas have something concrete to validate (per the rig's "schemas without consumers will drift" pattern from earlier reappraisals).

## Layout

| Directory | Schema | Description |
|---|---|---|
| `tools/*.v1.json` | `schemas/config/tool_manifest.v1.json` | Custom tool manifests. Drop these in `~/.atelier/tools/` (global) or `<repo>/.atelier/tools/` (per-repo override). |
| `hooks/*.v1.json` | `schemas/config/hook_manifest.v1.json` | Hook manifests. Same locations: `~/.atelier/hooks/` or `<repo>/.atelier/hooks/`. |
| `config/routing.v1.json` | `schemas/config/routing.v1.json` | Per-task model routing. Drop at `~/.atelier/routing.json` (global) or `<repo>/.atelier/routing.json` (per-repo override). |
| `config/permissions.v1.json` | `schemas/config/permission_state.v1.json` | Persistent permission-learning state. Drop at `~/.atelier/permissions.json` (global) or `<repo>/.atelier/permissions.json` (per-repo). |
| `skills/*.v1.json` | `schemas/config/skill_manifest.v1.json` | Skill manifests. Drop at `~/.atelier/skills/` (global) or `<repo>/.atelier/skills/` (per-repo override). Bundled skills ship at `crates/atelier-core/skills/`. |
| `subagents/*.v1.json` | `schemas/config/subagent_type.v1.json` | Sub-agent type manifests. Drop at `~/.atelier/subagents/` (global) or `<repo>/.atelier/subagents/` (per-repo override). Bundled types ship at `crates/atelier-core/subagents/`. |

## Current examples

| File | Shape |
|---|---|
| `tools/web_fetch.v1.json` | `shared-state` http tool using `${keychain:…}` interpolation. Demonstrates how to register a user-supplied tool against a hosted endpoint. Built-in tools (`read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`, `spawn_subagent`) live at `crates/atelier-core/tools/` and use `implementation.kind: builtin`. |
| `hooks/log_pre_tool.v1.json` | `pre-tool` shell hook with 50 ms time budget |
| `config/routing.v1.json` | Anthropic executor + Opus planner; critic disabled |
| `config/permissions.v1.json` | Three always-allow entries (one of each shape kind), one always-deny |
| `skills/explain.v1.json` | Skill with `${target}` + `${detail_level}` args, pinned ATELIER.md |
| `subagents/code-reviewer.v1.json` | Read-only reviewer sub-agent with model_routing override to Opus + `local-safe` side-effect cap |

## Validating

```sh
make artifacts   # validates these alongside meta.json / checks.json / etc.
```
