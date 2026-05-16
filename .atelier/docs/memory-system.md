# Atelier memory system

How Claude retains and retrieves information across sessions for the atelier project. This document is descriptive: it explains the system as built. The tools are at `~/.atelier/bin/`; the data is at `.atelier/memory/` and `~/.atelier/memory/`.

## Why a memory system at all

Claude Code's context window is bounded and resets every session. Anything Claude learned in a prior session — your preferences, project decisions, references to external systems, lessons from corrections — has to be re-derived or it's lost. The memory system is a curated, file-based knowledge store that the harness preloads into context (via each `MEMORY.md` index) and that Claude reads on demand.

## Layout

```
Project-scoped (atelier-specific):
  <atelier>/.atelier/memory/
    ├── MEMORY.md                     index, regenerated from frontmatter
    ├── feedback_*.md                 guidance from corrections/confirmations
    ├── project_*.md                  project state, decisions, in-flight context
    ├── user_*.md                     user preferences for this project
    └── reference_*.md                pointers to external systems

Global (cross-project):
  ~/.atelier/memory/
    ├── MEMORY.md                     index
    └── feedback_*.md                 lessons that apply across projects

Harness-required symlink (do not delete):
  ~/.claude/projects/-Users-chris-adkin-Projects-atelier/memory
    → /Users/chris.adkin/Projects/atelier/.atelier/memory
```

The symlink exists because the host harness hard-codes the system path for memory preloading. The symlink lets us keep the real files under `.atelier/` while satisfying the harness — atelier is BYOM, so we never write *new* tracked paths under `.claude/`. The lint test `test_no_claude_paths_in_tracked_source` in `tests/test_runner.py` enforces this.

## Memory types

| Prefix          | Type      | Purpose                                                                    | Lifetime           |
|-----------------|-----------|----------------------------------------------------------------------------|--------------------|
| `user_*.md`     | user      | Role, expertise, working preferences for this project                       | Long, slow change  |
| `feedback_*.md` | feedback  | Rules from corrections ("don't") and confirmations ("yes, exactly")         | Long, stable       |
| `project_*.md`  | project   | Active state, decisions, in-flight context. Carries a `verified:` date.    | Decays — re-verify |
| `reference_*.md`| reference | External system pointers (Linear projects, dashboards, channels)            | Long, slow change  |

`feedback_*.md` is the strongest candidate for promotion to global. `project_*.md` is the most decay-prone — re-verify every 60 days or remove.

## Frontmatter schema

Every memory file starts with YAML frontmatter:

```yaml
---
name: kebab-case-slug              # required, unique within the dir
title: Display Title For Index     # optional, falls back to derived-from-filename
description: One-line hook used in MEMORY.md and as retrieval signal.  # required
verified: 2026-05-16               # required for project_*; YYYY-MM-DD
metadata:
  type: feedback                   # required: user | feedback | project | reference
---
```

The harness may post-process saved memories to add `metadata.node_type: memory` and `metadata.originSessionId`. Leave those alone.

Cross-link related memories with `[[name-slug]]` syntax — the linter flags broken ones.

## MEMORY.md — the index

Each memory dir has a `MEMORY.md`. It is:
- **Auto-generated**, not hand-edited (entries below the boundary preamble).
- **Always loaded** into Claude's context at session start.
- **Capped** at the first ~200 lines by the harness — keep entries terse.
- **Format**: `- [Title](file.md) — description`, one line per memory.

The leading `<!-- ... -->` boundary preamble is preserved across regenerations.

## Tools

All tools live in `~/.atelier/bin/`. Add it to `PATH` or call by full path.

### `memcheck.sh` — regenerate + lint

```bash
memcheck.sh regen <memdir>     # rebuild MEMORY.md from frontmatter descriptions
memcheck.sh lint  <memdir>     # validate frontmatter, links, freshness, index sync
memcheck.sh all   <memdir>     # both
memcheck.sh                    # auto-discover and run "all" on every known memory dir
```

Lint checks:
- Required frontmatter fields (`name`, `description`, `metadata.type`)
- Broken `[[name]]` links
- Orphan files (in dir, not in `MEMORY.md`) → suggests `memcheck regen`
- Stale index entries (in `MEMORY.md`, file missing) → error
- `project_*.md` with missing or stale (`>60d`) `verified:` date

The SessionStart hook (`.atelier/hooks/session-start-memcheck.sh`) runs `memcheck all` on the project memory and global memory at every session start. Its output goes into Claude's session context as a `[memcheck]`-prefixed line.

### `mempromote.py` — find cross-project promotion candidates

```bash
mempromote.py
```

Scans every project's memory dir under `~/.claude/projects/*/memory/`. Reports:
- **Strong signal**: same `name:` slug appears in >1 project → almost certainly should be global.
- **Weaker signal**: Jaccard description similarity >= 0.5 across projects → review manually.

The script does not move files. Promote manually:

```bash
cp <project>/.../memory/feedback_X.md ~/.atelier/memory/
~/.atelier/bin/memcheck.sh regen ~/.atelier/memory
# Optionally: replace project copies with a [[link]] pointer.
```

### `memrecall.py` — verify retrieval

```bash
memrecall.py
```

Prints a verification prompt to paste into a fresh Claude Code session in atelier, plus the ground truth from the actual memory files. Compare Claude's recall against ground truth to catch silent retrieval failures.

## Hooks

Three hooks live in `<atelier>/.atelier/hooks/` and are registered in `<atelier>/.atelier/settings.json` (which the harness reads through a `.claude/settings.json` shim symlink):

| Hook                              | Event              | Purpose                                                                                 |
|-----------------------------------|--------------------|-----------------------------------------------------------------------------------------|
| `bounded-reads.sh`                | PreToolUse (Read\|Grep) | Non-blocking nudge when `Read` is called on >500-line files without `limit`, or `Grep` with `output_mode=content` without `head_limit`. |
| `session-start-memcheck.sh`       | SessionStart       | Runs `memcheck all` on project + global memory.                                          |
| `save-nudge.sh`                   | UserPromptSubmit   | Pattern-matches durable-directive language ("remember", "from now on", "always/never") and injects a reminder to consider saving a memory. Conservative — limited keyword set to avoid noise. |

The hooks output non-blocking `additionalContext` via JSON — they nudge, they don't gate.

## Boundary: what goes where

| Location                | Loaded                  | Best for                                                  |
|-------------------------|-------------------------|-----------------------------------------------------------|
| User's global config    | Always, all projects    | Global preferences, methodology, response style (lives at `~/.claude/CLAUDE.md` for the Claude Code harness; rename to suit your harness) |
| `<atelier>/ATELIER.md`  | Always, atelier only    | Stable facts about atelier itself (stack, build, layout). Auto-loaded via a `CLAUDE.md → ATELIER.md` shim symlink. |
| `.atelier/memory/`      | Index always; bodies on retrieval | Retrievable facts — user prefs, decisions, references, feedback |
| `~/.atelier/memory/`    | Index always (when active); bodies on retrieval | Cross-project memories |
| `tasks/todo.md`         | Read on demand          | Active in-flight work — volatile                         |
| `tasks/lessons.md`      | Read on demand          | Per-project process lessons (per the user's global config rule) |

**Conflict resolution**: if a fact is in a file the model can always see (`ATELIER.md` for atelier, or the user's global config), don't duplicate it as a memory. Memories are for things the model needs to *retrieve*.

## Lifecycle

1. **Capture** — Claude saves a memory when:
   - The user corrects an approach (feedback).
   - The user confirms a non-obvious choice was right (feedback).
   - The user shares role/expertise/preferences (user).
   - The user names a decision, deadline, or driver (project, with `verified:` date).
   - The user references an external system (reference).
   - The `save-nudge.sh` hook fires on durable-directive language.

2. **Index** — `memcheck regen` rebuilds `MEMORY.md` from each file's frontmatter.

3. **Retrieve** — Claude scans `MEMORY.md` (preloaded), then reads relevant files based on `description:` matches and the current task.

4. **Verify** — Before recommending a remembered fact, Claude verifies it still holds (file exists, function still named, flag still present). Memories are claims about a point in time, not present truth.

5. **Decay** — `project_*.md` with `verified:` > 60 days old gets flagged at SessionStart. Re-verify or remove.

6. **Promote** — When `mempromote.py` flags a `feedback_*.md` as duplicated across projects, move it to `~/.atelier/memory/` and (optionally) replace project copies with `[[name]]` pointers.

7. **Retire** — Delete memories that are wrong, superseded, or no longer relevant. Don't leave stale entries: they crowd out useful retrieval.

## Troubleshooting

| Symptom                                          | Likely cause                                            | Fix                                              |
|--------------------------------------------------|---------------------------------------------------------|--------------------------------------------------|
| Claude doesn't recall a memory                   | Not indexed in `MEMORY.md`, or description too vague    | `memcheck regen`; rewrite description            |
| `MEMORY.md` references a file that doesn't exist | Orphan after a delete                                   | `memcheck regen`                                 |
| Linter reports broken `[[link]]`                 | Renamed or deleted target                               | Update the link or restore the target            |
| `project_*` keeps showing as stale               | Missed re-verification cycle                            | Re-check the fact; bump `verified:` or delete     |
| Memory dir not found                             | Symlink broken under `~/.claude/projects/.../memory`    | Recreate: `ln -s <atelier>/.atelier/memory <sys>` |
| Same lesson re-saved across projects             | Should be a global memory                               | `mempromote.py` to confirm; copy to `~/.atelier/memory/` |

## Design choices worth knowing

- **Files, not a database.** `grep`-able, diff-able, version-controllable. Memory is text.
- **Frontmatter, not filenames, carries semantics.** Filenames are stable identifiers; descriptions/titles can be edited without renaming.
- **Index regeneration over manual edits.** `MEMORY.md` is a build artifact. Hand-edit the source files; let `memcheck` rebuild the index.
- **Symlink, not duplication.** Project memory lives once, in `.atelier/`. The harness reads it through a symlink. No sync logic.
- **Advisory hooks, not gates.** All three hooks emit `additionalContext` — they nudge, they don't block. Cheap to ignore, hard to circumvent if you really need to.
- **No automatic promotion.** `mempromote.py` recommends; humans (or Claude with permission) decide. Lossy automation here would silently homogenize memories.
