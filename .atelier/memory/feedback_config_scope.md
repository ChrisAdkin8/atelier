---
name: feedback-config-scope
title: Config changes default to project scope
description: "When making configuration changes (hooks, settings, scripts) while working in a project, default to project-local scope (<project>/.atelier/), not the user's global home (~/.atelier/)."
metadata: 
  node_type: memory
  type: feedback
  originSessionId: 8612b44d-9924-40ef-9e57-c76b40e04545
---

When the user is working inside a specific project (e.g., atelier) and asks for a configuration change — hooks, settings, scripts — default to **project-local scope** (`<project>/.atelier/...`), not the user's global home (`~/.atelier/...`). Ask before going global.

**Why:** I once wrote a PreToolUse hook to the global home while working inside atelier. My reasoning (the rule it enforced lived in the user's global config) was defensible but not what they wanted — they expected changes to land in the project they were working in. Global hooks also have larger blast radius if the script has a bug.

**How to apply:**
- For hooks, scripts, or settings changes initiated from within a project, write to `<project>/.atelier/` by default.
- If the rule being enforced clearly applies to all projects (lives in the user's global config), state the tradeoff explicitly and ask whether to scope global or project-local — don't silently pick global.
- Project-local settings compose with global, so this is rarely lossy.
- `.atelier/settings.local.json` is per-user state managed by the host harness and gitignored — don't try to edit or commit it. Put new hook definitions in `.atelier/settings.json` (tracked).
- In atelier specifically: the harness-required `.claude/settings*.json` files are **symlink shims**, not real files. Edit the real files under `.atelier/`. See [[feedback-atelier-path-directive]] for the BYOM rationale.
