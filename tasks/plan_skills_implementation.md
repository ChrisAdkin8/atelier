# Plan — finish §15 Skills

Date: 2026-05-19. Source: the §15 Skills sub-section of `coding-harness-spec.md` (lines 765–810). Three skill manifests (`review`, `security-review`, `test`) are bundled under `crates/atelier-core/skills/`, the schema (`schemas/config/skill_manifest.v1.json`) validates them, and `tests/validate_artifacts.py` + `tests/test_schemas.py` round-trip them — but **no Rust loader, no slash-command interception, no `/help` output, no ledger annotation** exists. This plan closes that gap.

Items are numbered **S01–S15** for traceability in commit messages and PR descriptions.

## Standing gates (all bundles)

Same convention as the medium / low plans:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p atelier-core` (and `-p atelier-cli` / `-p atelier-gui` / `-p atelier-tui` where touched)
- `make check` if schemas / fixtures change

Each item below adds a **targeted** verification on top of these — the smallest test that would have caught the issue, except where noted.

## Out of scope (deferred to a later phase)

- **Proactive triggers** (`proactive_trigger` field) — the spec defines them but they require a §9 "Run /name?" uncertainty surface that isn't built yet. Listed as S15 with a clear DEFERRED label; planning artifact only, not landed by this plan.
- **`atelier login` / `logout` / `rotate` / `whoami`** — the spec mentions `/help` should print these as harness-intercepted CLI verbs; they're tracked elsewhere in the build plan.

## Where the new-skill catalogue came from

The bundled v1 set (`/review`, `/security-review`, `/test`) was the spec's minimum viable set. To pick the next batch, I scanned two signals:

1. **What other coding harnesses ship out of the box.** GitHub Copilot Chat has `/explain` `/fix` `/tests` `/doc` `/optimize` `/new`; Aider has `/ask` `/code` `/commit` `/diff` `/run` `/test` `/lint` `/web`; Cody has `/explain` `/fix` `/edit` `/doc` `/test` `/smell`; Claude Code has `/init` `/clear` `/compact` `/memory` `/model` `/config` `/help`. The intersection that appears in three or more is **`/explain`, `/fix`, `/document`, `/commit`** — those are table stakes for any coding harness in 2026.
2. **What this repo's own history begs for.** A quick keyword scan of `CHANGELOG.md` (using `grep -oE '\b(audit|fix|scan|...)\b' CHANGELOG.md | sort | uniq -c | sort -rn`) ranks atelier's most-repeated workflows:
   - `audit` × **97** — multiple deep-scan / audit cycles; clear `/audit` candidate
   - `spec` × **88** — `/spec` for write-spec / update-spec authoring help
   - `fix` × **50** — confirms `/fix` from the cross-harness signal
   - `commit` × **50** — every changelog cycle drafts one; confirms `/commit`
   - `sweep` × **25** — meta-pattern; `/sweep` for cleanup sweeps
   - `scan` × **16** — security-scan / IoC-scan motifs; `/scan`
   - `refactor` × **13** — confirms `/refactor`
   - `hygiene` × **18**, `cleanup` × **9** — folds into `/sweep`

Taking the union and de-duplicating, **eleven new manifests** get bundled in v60.50.5 (a small extra bundle after the foundation lands but before the driver bundles, so the new manifests round-trip through the same loader path the bundled-three already exercise).

---

## v60.50 — `atelier-core::skills` foundation (S01–S05)

Touches: `crates/atelier-core/src/skills.rs` (new), `crates/atelier-core/src/lib.rs` (one-line `pub mod skills`), `crates/atelier-core/Cargo.toml` (probably zero new deps — `serde`, `serde_json`, `jsonschema`, `thiserror` already in the workspace).

### S01 — `Skill` struct + manifest deserialisation

- New file: `crates/atelier-core/src/skills.rs`.
- Public struct mirroring `schemas/config/skill_manifest.v1.json`:
  ```rust
  pub struct Skill {
      pub version: u32,
      pub name: String,                       // `^/?[a-z][a-z0-9_-]*$`, validated
      pub description: String,
      pub prompt_template: String,
      pub args: Vec<SkillArg>,               // Option-of-Vec collapses to empty
      pub pinned_context: Vec<String>,
      pub tools_required: Vec<String>,
      pub proactive_trigger: Option<String>,
      pub side_effect_class: SideEffectClass, // reuse the existing enum
      pub source: SkillSource,                // Bundled / UserHome / RepoLocal
  }
  ```
- `SkillArg`: `name`, `required: bool`, `description: Option<String>`, `default: Option<String>`.
- Validate via `jsonschema::Validator` against the bundled `skill_manifest.v1.json` ahead of `serde(deny_unknown_fields)` parsing (mirrors `BuiltInToolWrapper`'s validation order).
- **Verify:** `cargo test -p atelier-core skills::deserialise_tests` — round-trip each of the three bundled manifests; assert one happy-path + one rejection-on-extra-field + one rejection-on-bad-slug.

### S02 — `SkillRegistry::load(repo_root, home_dir)` with layered override

- Walks three layers in spec-mandated order, **later wins** when names collide:
  1. Bundled (`include_str!` from `crates/atelier-core/skills/*.json`).
  2. Global (`~/.atelier/skills/*.json`).
  3. Per-repo (`<workspace>/.atelier/skills/*.json`).
- Returns a `SkillRegistry { skills: BTreeMap<String, Skill> }` so iteration is stable for `/help` output.
- Records `source` on each `Skill` so `/help` can render `[bundled]` / `[~/.atelier/skills/]` / `[<repo>/.atelier/skills/]` — and so override-shadowing is debuggable in tests.
- Tolerant of missing layers (no `~/.atelier/skills/` is the common case).
- **Verify:** new test fixtures under `crates/atelier-core/tests/skills/` exercising three cases:
  - Bundled-only (no global, no per-repo) — returns the three bundled skills.
  - Per-repo override (e.g. fixture defines `/review` differently) — bundled `review` shadowed; assert the per-repo body wins.
  - Bundle + user + per-repo, all three skills present — assert per-repo > global > bundled override order.

### S03 — `substitute(template, &SkillSubstitutionContext) -> Result<String, SubstitutionError>`

- Replaces `${<arg_name>}` from the `args` map; `${repo_root}` from the context; `${atelier_md}` from a reader that returns the repo's ATELIER.md content (or empty string if absent).
- Rejects unknown variable refs (no silent passthrough) — returns `SubstitutionError::UnknownVariable { name }`.
- Rejects missing required args — returns `SubstitutionError::MissingRequiredArg { name }`.
- **Verify:** new test in `skills::substitute_tests`:
  - happy path: `"Run ${cmd} in ${repo_root}"` with `cmd = "make check"` → `"Run make check in /path/to/repo"`.
  - unknown var: `"Hi ${nope}"` → `Err(UnknownVariable { name: "nope" })`.
  - missing required arg: a manifest declares `required = true` for `cmd`, no value passed → `Err(MissingRequiredArg { name: "cmd" })`.
  - empty `ATELIER.md`: `${atelier_md}` → `""` (not panicking on missing file).

### S04 — `SkillRegistry::format_help()` matches the spec contract

- Mirrors the spec's `/help` format (§15 lines 786–797):
  ```
  /<name>  <description>  [proactive]  <source>
  ```
- Left-justify `<name>` to the longest registered skill name.
- Suppress shadowed entries (only the winner shows up).
- Group ordering: bundled → global → per-repo, then alphabetical within group.
- Below the list, a one-line summary of harness verbs (`/init`, `/help`) — see spec §15 line 797.
- **Verify:** snapshot test in `skills::help_tests` against a fixture registry with one override + one proactive skill; assert the rendered output byte-for-byte.

### S05 — `pub mod skills` in `lib.rs`; surface `Skill`, `SkillRegistry`, `SubstitutionError`

- One-line addition to `crates/atelier-core/src/lib.rs`.
- Update `crates/atelier-core/README.md`'s module list with a one-paragraph entry.
- **Verify:** `cargo doc -p atelier-core --no-deps` builds without warnings on the new module.

### Bundle gate

`cargo test -p atelier-core skills::` (all S01–S04 tests) plus `cargo doc -p atelier-core --no-deps`.

---

## v60.50.5 — bundled-manifest catalogue expansion (S05a–S05k)

Touches: `crates/atelier-core/skills/*.json` only (new files; no Rust changes). Each item below is a single manifest. All eleven ride together because they share the same `cargo test -p atelier-core` validation path (the schema test in S01 walks `crates/atelier-core/skills/*.json` and round-trips them); landing them as one bundle means one PR pays the rig + clippy + fmt overhead instead of eleven.

Manifest format follows the v1 bundled three (`pinned_context: ["ATELIER.md"]`, `side_effect_class: "local-safe"` unless noted). Slugs prefixed with the existing `/review` / `/security-review` / `/test` style — no namespace collisions.

### Cross-harness staples (high signal from other tools)

| ID | Slug | Description |
|---|---|---|
| **S05a** | `/explain` | Explain selected code / a named file / the current diff. Args: `target` (file path or "diff"). Pinned context: `ATELIER.md`. Bread-and-butter request — currently users type a long paragraph asking "what does this do" — the skill normalises the ask. |
| **S05b** | `/fix` | Diagnose and propose a fix for a named error message, failing test, or compiler diagnostic. Args: `error` (required, free-text or pasted block). Pinned context: `ATELIER.md`. `side_effect_class: "local-risky"` — the model will usually propose a code change. |
| **S05c** | `/document` | Generate or update doc comments / module-level rustdoc / panel-level READMEs for a target. Args: `target` (required). Pinned context: `ATELIER.md` + the repo's existing doc style if a `docs/style.md` exists. |
| **S05d** | `/refactor` | Targeted refactor for a named function / module / pattern with explicit intent. Args: `target`, `intent` (both required). Pinned context: `ATELIER.md`. `side_effect_class: "local-risky"`. |
| **S05e** | `/optimize` | Performance pass on a named target — model proposes optimisations + estimates impact. Args: `target` (required), `metric` (optional: "latency", "tokens", "memory"; default "latency"). |

### Atelier-specific (high signal from this repo's own changelog)

| ID | Slug | Description |
|---|---|---|
| **S05f** | `/commit` | Draft a Conventional-Commits-shaped message from the staged diff. Args: none (reads `git diff --staged`). Pinned context: `ATELIER.md` + the last ten commit subjects so the model picks up the repo's prefix conventions (`v60.NN:`, `hotfix:`, `docs(README):`). Signal: 50 mentions in CHANGELOG. |
| **S05g** | `/changelog` | Draft a `CHANGELOG.md` entry for the current branch's commits. Args: none (reads `git log main..HEAD`). Pinned context: `CHANGELOG.md` (last ~80 lines so the model matches existing voice + format). The Friday-evening "rolled up release" cadence in the git history suggests this is the highest-frequency human-authored artifact. |
| **S05h** | `/audit` | Focused audit pass against a named subsystem. Args: `scope` (required: "security", "performance", "hygiene", "schemas", "CI"). Pinned context: `ATELIER.md` + `coding-harness-spec.md`'s relevant section. Signal: **97** mentions in CHANGELOG — by far the most-repeated workflow in this repo's history. |
| **S05i** | `/spec` | Author or update a `coding-harness-spec.md` section. Args: `section` (required, e.g. "§7", "§15.Skills"). Pinned context: the full spec file plus `ATELIER.md`. Signal: 88 mentions of "spec". `side_effect_class: "local-risky"`. |
| **S05j** | `/sweep` | Run a hygiene-pass: rename inconsistencies, dead-code, doc-comment drift, conventions in `ATELIER.md` not respected. Args: `target` (optional: defaults to whole workspace; can scope to a crate or path). Pinned context: `ATELIER.md`. Signal: 25 mentions of "sweep", 18 of "hygiene", 9 of "cleanup". |
| **S05k** | `/scan` | Security scan focused on a known threat class. Args: `vector` (required: "supply-chain", "secrets", "egress", "iac-misconfig"). Pinned context: `ATELIER.md` + `tasks/shai_hulud_sweep_2026-05-19.md` for the supply-chain case. Signal: 16 mentions of "scan", direct echo of the v60.40 Shai-Hulud IoC sweep gate work. |

### Acceptance gates (the whole sub-bundle)

- All eleven manifests round-trip through `schemas/config/skill_manifest.v1.json` validation (mechanical — the S01 loader will reject any malformed one at startup; the rig's `validate_artifacts.py` catches it earlier still).
- `/help` output (S04) lists 14 total bundled skills (3 original + 11 new), grouped under `[bundled]`, alphabetical within group.
- Add three regression tests in `skills::manifest_catalogue_tests`:
  1. `every_bundled_manifest_parses` — walks the directory, deserialises each file, asserts none error.
  2. `bundled_slugs_match_filenames` — for `frob.json`, the manifest's `name` is `"frob"`.
  3. `bundled_help_renders_all_fourteen` — snapshot of `format_help()` lists the right count.

### Sub-bundle gate

`cargo test -p atelier-core skills::manifest_catalogue_tests` plus `make check` (the rig walks the manifest dir).

### Things deliberately left out of this batch

- **`/clear`** (Aider, Claude Code) — already handled by the GUI's "new session" affordance (you close + relaunch); slash-command parity would be cosmetic.
- **`/compact`** (Claude Code) — partly handled by the §5 Context-panel compaction button; CLI / TUI surfacing is a separate feature.
- **`/diff`** — handled in CLI by `git diff`; the GUI's DiffPane was removed in v60.43; surfacing wouldn't earn its keep until staging comes back.
- **`/onboard`** — tempting from the cross-harness signal but Atelier already has the harness-intercepted `/init` for this workflow.
- **`/migrate`** — too domain-specific (which migration?); better as a per-repo skill that users author themselves.
- **`/web`** (Aider, fetches a URL) — depends on an MCP web-fetch server being registered; orthogonal feature.

---

## v60.51 — `atelier-cli` slash-command interception (S06–S08)

Touches: `crates/atelier-cli/src/runner.rs` (initial-prompt path), `crates/atelier-cli/src/main.rs` (the `run` subcommand's prompt-resolution), `crates/atelier-cli/Cargo.toml` (already depends on `atelier-core`).

### S06 — `Runner` consults the skill registry before sending the first turn

- New `Runner::with_skill_registry(SkillRegistry) -> Self` builder (mirrors the existing `with_*` pattern).
- In `Runner::run(prompt: String)`, before constructing the first user turn:
  - If `prompt` starts with `/`, isolate the first whitespace-bounded token as `name`, the rest as `arg_str`.
  - If `name == "/help"`, intercept: print the registry's `format_help()` output, return a clean exit code (0). Don't hit the model.
  - If the registry has a skill matching `name`, parse `arg_str` per the skill's `args` list (positional → named; `key=value` parsing), substitute, replace `prompt` with the expansion before continuing.
  - Annotate `note: Some(format!("skill: {name}"))` on the *next* `LedgerEntry::ModelCall` so the cost ledger records the invocation (spec §15 line 808).
  - If `/` prefix but no match, surface a typed `SkillError::Unknown { name, available: Vec<String> }` and bail with a clean exit code.
- **Verify:** new test in `runner::skill_dispatch_tests`:
  - `/review` invocation expands to the bundled review template; assert the first model-call payload's user message equals the expanded template.
  - `/help` short-circuits (no adapter call); assert exit code 0 + stdout contains "/review", "/security-review", "/test".
  - `/nonsense` returns `SkillError::Unknown` with a hint listing available names.
  - First ledger entry after a `/review` carries `note = Some("skill: review")`.

### S07 — `atelier <subcommand>` parsing — distinguish skill calls from `run`

- The CLI today treats `atelier run "/review"` as a normal prompt. With S06 wired, `Runner::run` handles the interception internally — no `main.rs` changes needed for the canonical path.
- Add `atelier skills` as a new subcommand that prints `format_help()` and exits, useful for shell completion / man-page-style queries without spinning up a runner.
- **Verify:** integration test under `crates/atelier-cli/tests/skills_subcommand.rs` — spawn the binary with `atelier skills`, capture stdout, assert it contains all three bundled names.

### S08 — Manual end-to-end smoke against MockAdapter

- New test in `crates/atelier-cli/tests/common/canonical.rs` (the canonical fixture loader): drive a `/review` invocation through the §2.5 loop against a Mock scripted to return `claimed_done: true`. Assert:
  - the user-message text in the session log is the expanded template (not the literal `/review`);
  - the model_call ledger entry's `note` is `"skill: review"`.
- **Verify:** the test is the verification.

### Bundle gate

`cargo test -p atelier-cli` plus the new `tests/skills_subcommand.rs` integration test.

---

## v60.52 — GUI slash-command surface (S09–S11)

Touches: `crates/atelier-gui/src/lib.rs` (new Tauri commands), `crates/atelier-gui/ui/src/lib/components/Composer.svelte`, `crates/atelier-gui/ui/src/App.svelte` (registry hydration on mount, same pattern as the swap dropdown).

### S09 — `list_skills` Tauri command

- Mirrors `list_provider_profiles` shape — `SessionState` doesn't own the registry; it's loaded on demand per call:
  ```rust
  #[tauri::command]
  fn list_skills(state: tauri::State<'_, SessionState>) -> Vec<SkillWire> { ... }
  ```
- `SkillWire { name, description, proactive: bool, source: "bundled"|"home"|"repo" }`. Stable wire shape, no `prompt_template` exposed (the renderer doesn't need it; expansion happens server-side).
- Walks bundled + `~/.atelier/skills/` + `<workspace>/.atelier/skills/` via `SkillRegistry::load(workspace_root, home_dir)`.
- **Verify:** new unit test in the GUI lib (`list_skills_returns_three_bundled_in_a_clean_workspace`).

### S10 — `invoke_skill(name: String, args: HashMap<String, String>) -> Result<(), String>` Tauri command

- Routes through the same expansion path as the CLI's `Runner::run` skill interception, then feeds the expanded text into the existing `start_chat_run` pipeline.
- Emits the same `MessageCommitted { role: User, .. }` as a normal chat (so the conversation pane shows the *user's slash call* — not the expanded template — for readability) followed by the model's reply.
- The expanded template *is* what the adapter sees, but it's not displayed in the conversation pane (this is the inverse of the CLI: a CLI user looking at logs wants the actual expansion; a GUI user looking at the conversation wants the human-readable slash they typed).
- Decision to revisit if user feedback says they want to see the full expansion in the pane — could add a "show expansion" disclosure triangle.
- **Verify:** GUI lib test that posts `invoke_skill("review", {})` and asserts the adapter received the bundled review template's expanded body.

### S11 — Composer slash-command UX

- Composer detects a leading `/` on the textarea — switch the placeholder hint to "type a skill name (Tab to autocomplete)" and surface the dropdown of available skills as a transient menu beneath the textarea.
- On `Tab` from a partial slash, autocomplete to the longest unambiguous prefix.
- On `Enter` with a complete `/<name>` (and optional args), invoke `invoke_skill`; otherwise fall through to normal `start_chat_run`.
- `Esc` dismisses the dropdown without committing.
- **Verify:** Playwright-style integration would be ideal but is out of scope for this bundle. Settle for an in-tree Svelte component test that mounts the Composer with a stubbed `invoke`, types `/rev`, hits Tab, asserts the textarea value becomes `/review `.

### Bundle gate

`cargo test -p atelier-gui` plus `cd crates/atelier-gui/ui && npm run check` (svelte-check).

---

## v60.53 — TUI slash-command surface (S12–S13)

Touches: `crates/atelier-tui/src/lib.rs` (input handler), `crates/atelier-tui/src/skills_completion.rs` (new — autocomplete logic).

### S12 — Detect slash commands in `handle_key` text-input path

- Mirror the GUI's approach: a leading `/` on the textarea kicks the input into `InputMode::SlashCompletion { matches: Vec<String>, cursor: usize }`.
- Reuse `SkillRegistry::load` server-side (the TUI links `atelier-core` directly, no IPC).
- `Tab` cycles through matches; `Enter` commits the slash + args to the runner via the existing `Runner::with_skill_registry` path.
- **Verify:** new tests in `tui::slash_completion_tests` exercising the state machine deterministically (no terminal IO, just `apply()`-style assertions).

### S13 — Render the completion menu above the prompt line

- Mirror ratatui's standard popup pattern (List widget with a Block border).
- Visibility tied to `InputMode::SlashCompletion`.
- Highlight the selected match; show `[proactive]` and `<source>` suffixes to match the spec's `/help` format.
- **Verify:** snapshot test in `tui::render_slash_menu_tests` against a fixture registry; assert the rendered Buffer matches the expected glyph grid.

### Bundle gate

`cargo test -p atelier-tui`.

---

## v60.51.5 — user-creation ergonomics (S16–S25)

What it takes to make end users **comfortable authoring their own skills**, not just consume the bundled set. Surveyed against Claude Code (`.claude/commands/` + `.claude/agents/` markdown files, hot-reloaded), Aider (limited — config-based), Continue (`~/.continue/config.json`), Cody (JSON config), and what atelier's spec already mandates (`~/.atelier/skills/<name>.json` global + `<workspace>/.atelier/skills/<name>.json` per-repo, both with layered override).

The directory convention is already specified and the loader (S02) walks it. **What's missing for usable end-user authoring** breaks into ten items:

### Authoring surface — CLI

| ID | Subcommand | Purpose |
|---|---|---|
| **S16** | `atelier skills new <name> [--scope user\|repo] [--from <existing>]` | Scaffold a starter manifest at the right path. Validates the slug (`^[a-z][a-z0-9_-]*$`), refuses to overwrite, optionally seeds from an existing skill (e.g. `--from review` to fork the bundled review skill into a per-repo override). Opens the new file in `$EDITOR` if set, else prints the path. |
| **S17** | `atelier skills validate [path]` | Lint a manifest without running it. JSON Schema check (the same `jsonschema::Validator` the loader uses) + substitution lint (walk `prompt_template`, every `${name}` must resolve to a declared `arg`, `repo_root`, or `atelier_md`). With no `path`, validates every manifest in the registry. Exits non-zero on any failure so it can ride in pre-commit hooks. |
| **S18** | `atelier skills edit <name>` | Resolve the skill via the registry's layered lookup, open the winning manifest in `$EDITOR`. Refuses to edit a bundled skill without `--scope user --from <name>` (which makes a copy at `~/.atelier/skills/<name>.json`). |
| **S19** | `atelier skills delete <name>` | Remove a user-scope or per-repo skill. If the deleted skill was shadowing a bundled one, prints a one-liner: "deleted; bundled `/review` will be active again next session." Refuses to delete bundled skills (those are `include_str!`'d into the binary anyway). |
| **S20** | `atelier skills show <name>` | Print the resolved manifest + source path + `[shadows: <other source>]` if applicable. Useful when "is this the one I edited?" is the question. |

### Friction-killing affordances

| ID | What | Detail |
|---|---|---|
| **S21** | **Hot reload via the existing file watcher** | The `file_watcher` module already watches the tool dispatcher's read-set for §14 concurrent-edit detection. Extend it to also watch `~/.atelier/skills/` and `<workspace>/.atelier/skills/`. On change, reload the registry and emit a new `Event::SkillsReloaded { added, removed, modified }` so the GUI/TUI dropdown can re-hydrate without a relaunch. **Big UX win** — without this, every manifest edit requires a restart, and end users will give up after the third reload. |
| **S22** | **Schema-aware error messages** | The `jsonschema` crate's default error output is verbose and points at JSON Pointer paths. Map the three most common authoring mistakes to friendly one-liners: (a) bad slug → "`name` must be lowercase letters / digits / `_-`, got `<value>`"; (b) missing required field → "`<field>` is required (see `examples/skills/explain.v1.json` for a complete manifest)"; (c) unknown variable in template → "`prompt_template` references `${foo}` but no arg `foo` is declared; available: `${repo_root}`, `${atelier_md}`, declared args: `${target}`, `${detail_level}`." |
| **S23** | **`pinned_context` existence check at load** | When a manifest declares `pinned_context: ["ATELIER.md", "docs/style.md"]`, the loader warns (via `tracing::warn!`) if any referenced file doesn't exist in the workspace. **Warn, don't refuse** — `pinned_context` paths can legitimately be repo-specific and a user's home-scope skill shouldn't fail to load just because the active repo lacks `docs/style.md`. |
| **S24** | **`tools_required` runtime check** | When invoking a skill that declares `tools_required: ["read_file", "shell"]`, refuse if any are missing from the registry. Surfaces as a clear "this skill needs `shell` but it's not registered in this run's tool set" message rather than the model silently failing to call a tool it expected. |
| **S25** | **`examples/skills/` expansion + author's guide** | The bundled `examples/skills/explain.v1.json` already demonstrates the args + substitution pattern. Add three more examples: a no-args skill, a skill with required + optional args, a skill with `proactive_trigger` and explicit `tools_required` — covering the full schema surface so a user can copy-and-modify. The author's guide planned in S14 grows a "Writing your first skill" section walking through `atelier skills new`, the substitution variables, the layered override semantics, and how to share a per-repo skill via git. |

### GUI surface (deferred to v60.52.5)

The "Add skill" affordance in the GUI is **explicitly out of scope for this bundle.** Most users will reach for `$EDITOR` and the CLI scaffolding; building a Svelte form for skill authoring duplicates work and introduces a second source of truth for validation (the front-end would need to know the schema). Revisit if user feedback says editing JSON manifests is friction.

### Sub-bundle gate

`cargo test -p atelier-cli skills::user_creation_tests` plus a manual exercise: `atelier skills new my-test --scope user`, edit it, `atelier skills validate`, `atelier skills show my-test`, `atelier skills delete my-test` — round-trip should complete without errors.

---

## v60.54 — Documentation + capability surfacing (S14)

Touches: `README.md`, `CAPABILITIES.md`, `ATELIER.md`, `CHANGELOG.md`, `crates/atelier-core/skills/README.md` (new), `coding-harness-spec.md` (none — the spec is authoritative; the implementation now matches).

### S14 — README + CAPABILITIES + ATELIER + CHANGELOG sweep

- **README.md** — add a "Skills" section under "Memory" (similar shape). Cover: what skills are, the three bundled ones, how to add a per-repo skill, how to call them from each driver (CLI / GUI / TUI), the `/help` shortcut.
- **CAPABILITIES.md** — promote the §15 Skills table from "Bundled: `review`, `security-review`, `test`" to a full row in the §15 capability matrix, mark the proactive-trigger path as deferred.
- **ATELIER.md** — one-line bump in the rolling project-context paragraph naming v60.50–v60.53 as "Skills closeout".
- **CHANGELOG.md** — bundled entry covering all four release versions in one block (the same shape v60.43–v60.49 used).
- **`crates/atelier-core/skills/README.md`** — author's-guide for writing a skill manifest: schema reference, substitution variables, side-effect class, how to test locally.
- **Verify:** trivially reviewable; no automated check.

### Bundle gate

`make check` (catches schema drift and rig integration) plus a manual `grep -n "Skills" README.md ATELIER.md CAPABILITIES.md` showing the section is consistent across the three docs.

---

## v60.55 — Proactive triggers (S15, DEFERRED)

**Out of scope for this plan but enumerated for traceability.** Listed so the cross-cutting question "is this done?" has a single answer.

### S15 — `proactive_trigger` evaluation + §9 "Run /<name>?" UI

- **Status:** DEFERRED to a Phase E bundle. The §9 uncertainty UI ("Run /<name>? — _reason_") doesn't exist yet — neither in GUI nor TUI. Implementing proactive triggers alone without the UI surface gives the model a way to suggest skills it can never deliver.
- **When to land it:** after the §9 surface lands. Use the system-prompt-aware trigger pattern the spec lays out — summarise registered `proactive_trigger` strings in the system prompt, listen for a model-emitted "suggested skill" envelope tag, route into the §9 UI for user accept/dismiss.
- **What's deferrable today, what isn't:** the bundled `security-review.json` carries a `proactive_trigger`. Until S15 lands, that field is loaded into the `Skill` struct (S01), surfaced in `/help` output (S04), but not acted upon. That's fine — the static `/security-review` call still works manually.

---

## Sequencing & risk

- The four bundles (v60.50–v60.53) are **file-disjoint** between core, CLI, GUI, TUI and can be developed concurrently on separate worktrees per L-D-2. v60.50 ships first because the other three depend on `SkillRegistry`; after that the three driver bundles can land in any order.
- **v60.50.5 (catalogue expansion) lands immediately after v60.50 and before the driver bundles.** Pure manifest files — no Rust code — so its only dependency is the S01 loader being able to walk the directory. Landing first means v60.51–v60.53 can exercise the larger 14-skill registry from day one (e.g. the GUI's autocomplete UX is more interesting against 14 entries than 3).
- **v60.51.5 (user-creation ergonomics) lands after v60.51 (CLI slash interception) and before v60.52 (GUI).** Order matters: S16's `atelier skills new` reuses the registry from v60.50; S21's hot reload uses the file watcher that the dispatcher already owns; and landing this BEFORE the GUI bundle means the GUI's autocomplete dropdown picks up `SkillsReloaded` events for free.
- v60.54 (docs) MUST land after v60.50–v60.53 are all merged — the README narrative depends on all three drivers' UX being consistent.
- **Risk surface is small** — skills are a prompt-expansion layer, not a new transport. The state machine doesn't change. The cost ledger gains one new optional `note` value. The schema doesn't change.
- **One subtle correctness item:** when `prompt_template` contains a `${...}` reference that *doesn't* match any declared arg, the substitution should fail loudly per S03 — silently leaving the literal text in the prompt would let a typo'd manifest produce confused model behaviour. Resist the urge to add a "passthrough on unknown var" affordance during review.
- Each bundle ends with: green CI, `CHANGELOG.md` entry (rolled into v60.54), one-line tag, digest in `tasks/todo.md`.

## Open questions

1. **Argument parsing syntax.** The spec says `/<name> [args]` but doesn't define the args grammar. Proposal: `key=value` whitespace-delimited, allow `"quoted strings"` for values with spaces, positional fallback only when a single required arg is declared (then the whole remainder is that arg's value). Trade-off: positional fallback is friendlier but ambiguous when a skill has multiple required args. Recommend the explicit `key=value` form for v1; revisit if user feedback says it's clunky.
2. **In-pane echo of expanded vs literal slash.** v60.52 S10 settles on showing the *literal slash* in the GUI conversation pane (not the expansion). Open to revision if the conversation log later becomes the durable record — at that point logging the expansion is more useful than logging the typed shortcut.
3. **Side-effect-class enforcement.** Each manifest declares `side_effect_class`. The spec is silent on whether it gates anything; the trust-budget integration is a §8 question. Recommend treating it as documentation-only for v1 (the cost is just one `local-safe` budget unit per skill invocation, which is fine).
4. **User-authored `irreversible` skills.** A user can write a manifest with `side_effect_class: "irreversible"` and a `prompt_template` that nudges the model to take destructive actions. Should `atelier skills validate` refuse this combination, or warn but allow? Recommend **warn but allow** — the user is the author; refusing would force them to lie about the class. The §8 trust-budget cost of 20 + double-confirm already gates invocation; that's the right place for the check, not authoring time.
5. **Naming conflicts in `atelier skills new`.** If the user runs `atelier skills new review --scope repo` they'll shadow the bundled `/review`. Recommend the command **prints a one-line confirmation** at create time ("creating per-repo `/review`; this will shadow the bundled skill") rather than refusing. The layered-override semantics are spec'd; making them visible at authoring time is the right ergonomic.
6. **Hot-reload race with in-flight invocations.** If a user edits `~/.atelier/skills/review.json` mid-turn, what happens? Recommend the registry uses a `Arc<SkillRegistry>` slot and atomic swap; in-flight invocations keep referencing the pre-swap copy; the next invocation reads the new one. Standard read-copy-update pattern; no locking on the read path.
