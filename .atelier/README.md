# .atelier/

Project-owned, user-curated content for atelier. Atelier is a BYOM harness, so tracked source uses `.atelier/` exclusively — see `feedback_atelier_path_directive.md` for the directive and `tests/test_runner.py::test_no_claude_paths_in_tracked_source` for the enforcing lint.

## Layout

```
.atelier/
├── README.md                       you are here
├── settings.json                   shared harness settings (committed; relative-path hook commands)
├── hooks/                          harness hook scripts (referenced from settings.json)
│   ├── bounded-reads.sh            PreToolUse — nudges on unbounded Read/Grep
│   ├── session-start-memcheck.sh   SessionStart — regenerate + lint memory indices
│   └── save-nudge.sh               UserPromptSubmit — flag durable-directive prompts
├── memory/                         Retrievable project memory (symlinked from harness preload path)
│   ├── MEMORY.md                   Index, regenerated from frontmatter (do not hand-edit entries)
│   └── feedback_*.md, project_*.md, user_*.md, reference_*.md
└── docs/
    ├── host-harness-contract.md    What a BYOM host must provide for the hooks to work
    └── memory-system.md            How the memory system works end-to-end
```

`settings.local.json` is per-user (gitignored) — the host harness writes it; each contributor regenerates it locally. Global tools (`memcheck.sh`, `mempromote.py`, `memrecall.py`) live in `~/.atelier/bin/`, not in this tree.

## Companion locations

- **`~/.atelier/bin/`** — global tools (`memcheck.sh`, `mempromote.py`, `memrecall.py`).
- **`~/.atelier/memory/`** — global cross-project memory.

## Harness-mandated paths (the residual `.claude/` references)

The host harness hard-codes a couple of read paths. We satisfy them with **shim symlinks** that point back into `.atelier/`:

| Shim (harness reads)                       | Real file (you edit)                  |
|--------------------------------------------|---------------------------------------|
| `<atelier>/.claude/settings.json`          | `<atelier>/.atelier/settings.json`    |
| `<atelier>/CLAUDE.md`                      | `<atelier>/ATELIER.md`                |

`<atelier>/.claude/settings.local.json` is per-user state the host harness manages locally; it's gitignored on both sides.

Plus the memory preload path, which is a symlink in `$HOME`:
- `~/.claude/projects/-Users-chris-adkin-Projects-atelier/memory` → `<atelier>/.atelier/memory/`

The shims under `.claude/` are gitignored. Tracked source never references `.claude/` paths.

Per project policy: new content goes in `.atelier/`, never `.claude/`. The shims exist only because the harness hard-codes those names. Lint enforces this on every push.
