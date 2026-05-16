---
name: feedback-atelier-path-directive
title: Atelier uses .atelier/ paths, never .claude/
description: "Always use .atelier/ for project-scoped config/data in this repo. Never introduce new .claude/ paths. Atelier is a BYOM harness; hardcoding another vendor's path is a contract violation."
metadata:
  node_type: memory
  type: feedback
---

In this project (atelier), all project-scoped config and data MUST live under `.atelier/` — never `.claude/`. Tracked source files must not reference `.claude/` paths except in three narrow places:

1. **`CHANGELOG.md`** — historical record of *why* the directive exists.
2. **`.gitignore`** — the `.claude/` exclusion that catches any harness-required files contributors may have locally.
3. **`.atelier/README.md` / `ATELIER.md`** — documenting that the harness itself hardcodes some paths and how the symlink/exception works.

Anywhere else, replace `.claude/` → `.atelier/`, `.claudeignore` → `.atelierignore`, and `claude_code_version`-style field names with vendor-neutral equivalents (`baseline_harness_name` + `baseline_harness_version`).

**Why:** Atelier is a bring-your-own-model harness. The repo's whole stance is that it should run with any provider and any host harness. Hardcoding `.claude/` paths into tracked code, schemas, or example data quietly couples it to one vendor. The user surfaced this in audit round 3 ("when this should be a stand alone harness that supports bring-your-own-model why does the code contain references to claude") and the directive came in round 4 ("ensure that .atelier is always used instead of .claude"). Several previous audits caught structural violations: a `claude_code_version` schema field, a `.claudeignore` reference in a built-in tool's description, two `.claude/settings*.json` files leaked into the repo.

**How to apply:**
- New tracked source: never write `.claude/` paths. Use `.atelier/`.
- Example data (`anthropic:claude-sonnet-4-6` model strings in `examples/`, `tests/sessions/examples/`, etc.) is *not* a violation — these are illustrative provider:model strings in a multi-vendor list. The structural commitment is the schema/field/path layer.
- If a future user-supplied tool or schema needs an "ignore file" reference, name it `.atelierignore` (gitignore fallback). Do not invent vendor-prefixed names.
- The lint test `test_no_claude_paths_in_tracked_source` in `tests/test_runner.py` enforces the directive; if you legitimately need to add a `.claude/` reference, extend that test's allowlist with a rationale.
- This directive supersedes the older `feedback-config-scope` memory's mention of `<project>/.claude/`: in atelier, that path is `<project>/.atelier/`. Other projects without an explicit `.atelier/` convention may still default to `.claude/`.
