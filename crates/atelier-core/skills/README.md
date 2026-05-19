# Bundled skill manifests

This directory holds the v1 bundled skill catalogue. Each `*.json` file
is `include_str!`'d into the `atelier-core` binary at compile time, so
the catalogue ships with the harness and works offline.

To override any of these locally, drop a manifest of the same name
into one of:

- `~/.atelier/skills/<name>.json` — your scope (applies in every repo).
- `<repo>/.atelier/skills/<name>.json` — per-repo (checked into git, applies only here).

Per-repo wins over your scope, which wins over bundled.

## Writing your first skill

The fastest path:

```sh
# Per-repo (default).
atelier skills new my-helper

# Your scope (cross-project).
atelier skills new my-helper --scope user

# Fork an existing skill and customise it.
atelier skills new my-review --from review --scope user
```

The command scaffolds a starter manifest at the right path, refuses to
overwrite, and opens it in `$EDITOR` if set.

## Schema reference

Every manifest validates against
`schemas/config/skill_manifest.v1.json`. Required fields:

| Field | Type | Notes |
|---|---|---|
| `version` | const `1` | Pin the manifest schema version. |
| `name` | string | `^[a-z][a-z0-9_-]*$` — the slug invoked via `/<name>`. |
| `description` | string | One-liner shown in `/help` and the GUI menu. |
| `prompt_template` | string | The body expanded as the next user turn. |

Optional fields:

| Field | Type | Notes |
|---|---|---|
| `args` | array of `{name, description?, required?, default?}` | Declared `${arg}` parameters. |
| `pinned_context` | array of strings | Files to pin into §5 context before the prompt fires (e.g. `["ATELIER.md"]`). |
| `tools_required` | array of strings | Tools the skill expects. UI surfaces a warning if missing. |
| `proactive_trigger` | string | Natural-language description of when the model should auto-suggest the skill. (Surface is deferred.) |
| `side_effect_class` | `local-safe` / `local-risky` / `shared-state` / `irreversible` | §8 trust-budget hint. Defaults to `local-safe`. |

## Substitution variables

The `prompt_template` body supports `${...}` substitution:

- `${<arg_name>}` — values from the declared `args` list. Required args
  must be supplied at invocation; optional args fall back to their
  declared `default`, then to the empty string.
- `${repo_root}` — absolute path of the repo root.
- `${atelier_md}` — contents of `<repo>/ATELIER.md`, or empty string if absent.

Unknown variable refs are rejected at expansion time — silent
passthrough would hide manifest typos behind confused model behaviour.

## Linting locally

```sh
# Validate every manifest in the registry.
atelier skills validate

# Validate one path (handy in pre-commit hooks).
atelier skills validate ~/.atelier/skills/my-helper.json
```

Validation catches: bad slugs, missing required fields, extra fields,
and `${var}` references in `prompt_template` that don't resolve to a
declared arg / `repo_root` / `atelier_md`. Missing `pinned_context`
paths emit a `warn:` line but don't fail validation — `pinned_context`
is per-skill, and a home-scope skill shouldn't refuse to load just
because the active repo lacks the pinned file.

## Cost-ledger attribution

Skill invocations are recorded as a `note` on the next turn's
`model_call` ledger entry: `"skill: <name>"`. No additional ledger
event; the skill is a prompt expansion, not a separate turn.

## See also

- `coding-harness-spec.md` §15 Skills — the authoritative contract.
- `crates/atelier-core/src/skills.rs` — the loader + substitution
  implementation.
- `examples/skills/` — hand-rolled examples covering the full schema
  surface (no-args, required + optional args, `tools_required` +
  `proactive_trigger`).
