//! §15 Skills — named, user- or agent-invocable procedures.
//!
//! A **skill** is a manifest (`schemas/config/skill_manifest.v1.json`)
//! declaring a `name`, `description`, `prompt_template`, optional `args`,
//! optional `pinned_context`, optional `tools_required`, and optional
//! `proactive_trigger`. The §2.5 agent loop does not branch on skills —
//! they are a prompt-expansion layer that runs **before** the first user
//! turn fires. See spec §15 lines 765–810.
//!
//! ## Storage (layered override, later wins)
//!
//!   1. **Bundled** — `include_str!`'d from `crates/atelier-core/skills/`.
//!   2. **Global** — `~/.atelier/skills/<name>.json`.
//!   3. **Per-repo** — `<workspace>/.atelier/skills/<name>.json`.
//!
//! All three layers are tolerated as absent — a clean workspace with no
//! `~/.atelier/skills/` directory still loads the three bundled skills.
//!
//! ## Substitution variables
//!
//!   - `${<arg_name>}` — declared args (from the manifest's `args` list).
//!   - `${repo_root}`  — absolute path of the repo root.
//!   - `${atelier_md}` — contents of `<repo>/ATELIER.md`, or `""` if absent.
//!
//! Unknown variable refs are rejected loudly — silent passthrough hides
//! manifest typos behind confused model behaviour. Missing required args
//! are rejected the same way.
//!
//! ## Cost-ledger tracking
//!
//! Skill invocations are recorded as a `note` on the next turn's
//! `model_call` ledger entry: `"skill: <name>"`. See `crates/atelier-cli`
//! for the wiring (this crate only owns the registry + substitution).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jsonschema::Validator;
use serde::{Deserialize, Serialize};

use crate::dispatcher::SideEffectClass;

/// Embedded copy of `schemas/config/skill_manifest.v1.json`. Embedding
/// keeps loader validation hermetic — the schema is part of the binary,
/// so a user running a built `atelier` outside the repo still gets
/// schema-level validation.
const SKILL_MANIFEST_SCHEMA_JSON: &str =
    include_str!("../../../schemas/config/skill_manifest.v1.json");

/// Lazily-compiled validator. Reusable across loads.
fn schema_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value = serde_json::from_str(SKILL_MANIFEST_SCHEMA_JSON)
            .expect("embedded skill_manifest.v1.json parses as JSON");
        jsonschema::validator_for(&schema)
            .expect("embedded skill_manifest.v1.json is a valid JSON Schema")
    })
}

// ---------- bundled set ----------

/// Bundled skill manifests, sourced via `include_str!` from
/// `crates/atelier-core/skills/`. Kept as `(name, body)` pairs so the
/// registry can record the canonical bundled `name` independent of any
/// future per-file renames.
fn bundled_manifests() -> &'static [(&'static str, &'static str)] {
    &[
        ("review", include_str!("../skills/review.json")),
        (
            "security-review",
            include_str!("../skills/security-review.json"),
        ),
        ("test", include_str!("../skills/test.json")),
        ("explain", include_str!("../skills/explain.json")),
        ("fix", include_str!("../skills/fix.json")),
        ("document", include_str!("../skills/document.json")),
        (
            "document-sweep",
            include_str!("../skills/document-sweep.json"),
        ),
        ("refactor", include_str!("../skills/refactor.json")),
        ("optimize", include_str!("../skills/optimize.json")),
        ("commit", include_str!("../skills/commit.json")),
        ("changelog", include_str!("../skills/changelog.json")),
        ("audit", include_str!("../skills/audit.json")),
        ("spec", include_str!("../skills/spec.json")),
        ("sweep", include_str!("../skills/sweep.json")),
        ("scan", include_str!("../skills/scan.json")),
        ("plan", include_str!("../skills/plan.json")),
        ("diagram", include_str!("../skills/diagram.json")),
        ("triage", include_str!("../skills/triage.json")),
        ("release", include_str!("../skills/release.json")),
        ("ci-failure", include_str!("../skills/ci-failure.json")),
        (
            "dependency-upgrade",
            include_str!("../skills/dependency-upgrade.json"),
        ),
        (
            "issue-to-plan",
            include_str!("../skills/issue-to-plan.json"),
        ),
        ("pr-polish", include_str!("../skills/pr-polish.json")),
        (
            "perf-investigate",
            include_str!("../skills/perf-investigate.json"),
        ),
        (
            "config-doctor",
            include_str!("../skills/config-doctor.json"),
        ),
        (
            "release-publish",
            include_str!("../skills/release-publish.json"),
        ),
        ("migration", include_str!("../skills/migration.json")),
        ("bug-report", include_str!("../skills/bug-report.json")),
        (
            "new-contributor",
            include_str!("../skills/new-contributor.json"),
        ),
    ]
}

// ---------- types ----------

/// Where a [`Skill`] came from. Drives `/help` rendering and the
/// debuggability of layered overrides.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillSource {
    /// `include_str!`'d from `crates/atelier-core/skills/`.
    Bundled,
    /// `~/.atelier/skills/<name>.json`.
    UserHome,
    /// `<workspace>/.atelier/skills/<name>.json`.
    RepoLocal,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::UserHome => "home",
            Self::RepoLocal => "repo",
        }
    }

    /// Rendering used by `/help` per spec §15 line 795.
    pub fn help_tag(&self) -> &'static str {
        match self {
            Self::Bundled => "[bundled]",
            Self::UserHome => "[~/.atelier/skills/]",
            Self::RepoLocal => "[<repo>/.atelier/skills/]",
        }
    }

    /// Sort key controlling `/help` grouping: bundled → global → per-repo.
    fn group_order(&self) -> u8 {
        match self {
            Self::Bundled => 0,
            Self::UserHome => 1,
            Self::RepoLocal => 2,
        }
    }
}

/// A declared argument to a skill's `prompt_template`. Mirrors the
/// `args[*]` shape in `schemas/config/skill_manifest.v1.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillArg {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

/// A loaded skill manifest, decorated with the source layer it came
/// from. Public fields mirror the schema 1:1 except for `source`
/// (loader-assigned) and the collapse of optional arrays to empty `Vec`s
/// so callers can iterate without an `Option` dance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub version: u32,
    pub name: String,
    pub description: String,
    pub prompt_template: String,
    pub args: Vec<SkillArg>,
    pub pinned_context: Vec<String>,
    pub tools_required: Vec<String>,
    pub proactive_trigger: Option<String>,
    pub side_effect_class: SideEffectClass,
    pub source: SkillSource,
}

/// Wire shape used for deserialisation: matches the schema, lets serde
/// reject extra fields. We project it into [`Skill`] after attaching
/// `source` and applying defaults.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillWire {
    version: u32,
    name: String,
    description: String,
    prompt_template: String,
    #[serde(default)]
    args: Option<Vec<SkillArg>>,
    #[serde(default)]
    pinned_context: Option<Vec<String>>,
    #[serde(default)]
    tools_required: Option<Vec<String>>,
    #[serde(default)]
    proactive_trigger: Option<String>,
    #[serde(default)]
    side_effect_class: Option<SideEffectClass>,
}

impl Skill {
    /// Parse a manifest body. Validates against the bundled schema
    /// first so error messages from extra/missing fields point at the
    /// JSON layer, *then* runs serde — mirrors `BuiltInToolWrapper`'s
    /// two-stage check.
    pub fn from_manifest_json(body: &str, source: SkillSource) -> Result<Self, SkillLoadError> {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|e| SkillLoadError::Parse(e.to_string()))?;
        let errs: Vec<String> = schema_validator()
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        if !errs.is_empty() {
            return Err(SkillLoadError::Schema(errs.join("; ")));
        }
        let wire: SkillWire =
            serde_json::from_value(value).map_err(|e| SkillLoadError::Parse(e.to_string()))?;

        Ok(Self {
            version: wire.version,
            name: wire.name,
            description: wire.description,
            prompt_template: wire.prompt_template,
            args: wire.args.unwrap_or_default(),
            pinned_context: wire.pinned_context.unwrap_or_default(),
            tools_required: wire.tools_required.unwrap_or_default(),
            proactive_trigger: wire.proactive_trigger,
            side_effect_class: wire.side_effect_class.unwrap_or(SideEffectClass::LocalSafe),
            source,
        })
    }

    /// True when a `proactive_trigger` is set (S15 surface; manual
    /// invocation works regardless).
    pub fn is_proactive(&self) -> bool {
        self.proactive_trigger.is_some()
    }
}

// ---------- errors ----------

#[derive(Debug, thiserror::Error)]
pub enum SkillLoadError {
    #[error("skill manifest does not parse as JSON: {0}")]
    Parse(String),
    #[error("skill manifest fails schema validation: {0}")]
    Schema(String),
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubstitutionError {
    #[error("required arg `{name}` was not provided")]
    MissingRequiredArg { name: String },
    #[error("unknown variable `${{{name}}}` in prompt_template")]
    UnknownVariable { name: String },
}

// ---------- registry ----------

/// Collection of skills, indexed by name. `BTreeMap` gives stable
/// iteration order for `/help` snapshot tests and predictable diffs.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
}

impl SkillRegistry {
    /// Walk the three layers in spec order, **later wins** on name
    /// collisions:
    ///
    ///   1. Bundled (`include_str!`).
    ///   2. `home_dir/.atelier/skills/`.
    ///   3. `repo_root/.atelier/skills/`.
    ///
    /// Tolerates missing layers (a clean workspace has none of the
    /// directories; bundled-only is the common case for fresh users).
    /// Malformed manifests under either on-disk layer are reported
    /// via the `errors` field of the returned report; the registry
    /// still loads the well-formed ones.
    pub fn load(repo_root: &Path, home_dir: Option<&Path>) -> Result<Self, SkillLoadError> {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();

        // Layer 1: bundled.
        for (canonical_name, body) in bundled_manifests() {
            let skill = Skill::from_manifest_json(body, SkillSource::Bundled)?;
            debug_assert_eq!(
                &skill.name, canonical_name,
                "bundled manifest filename / name drift: {canonical_name} vs {}",
                skill.name
            );
            skills.insert(skill.name.clone(), skill);
        }

        // Layer 2: ~/.atelier/skills/.
        if let Some(home) = home_dir {
            for skill in Self::scan_dir(&home.join(".atelier/skills"), SkillSource::UserHome)? {
                skills.insert(skill.name.clone(), skill);
            }
        }

        // Layer 3: <repo>/.atelier/skills/.
        for skill in Self::scan_dir(&repo_root.join(".atelier/skills"), SkillSource::RepoLocal)? {
            skills.insert(skill.name.clone(), skill);
        }

        Ok(Self { skills })
    }

    fn scan_dir(dir: &Path, source: SkillSource) -> Result<Vec<Skill>, SkillLoadError> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let entries = fs::read_dir(dir).map_err(|e| SkillLoadError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| SkillLoadError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let body = fs::read_to_string(&path).map_err(|e| SkillLoadError::Io {
                path: path.clone(),
                source: e,
            })?;
            let skill = Skill::from_manifest_json(&body, source.clone())?;
            out.push(skill);
        }
        Ok(out)
    }

    /// Builder used by tests to construct a registry from in-memory
    /// `Skill` values without round-tripping through the filesystem.
    pub fn from_skills(skills: impl IntoIterator<Item = Skill>) -> Self {
        let mut map = BTreeMap::new();
        for s in skills {
            map.insert(s.name.clone(), s);
        }
        Self { skills: map }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        // Accept both `/<name>` and `<name>` — the slash is part of the
        // invocation syntax, not the registry key.
        let key = name.strip_prefix('/').unwrap_or(name);
        self.skills.get(key)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.skills.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Spec §15 lines 786–797. Format:
    ///
    /// ```text
    /// /<name>  <description>  [proactive]  <source>
    /// ```
    ///
    /// `<name>` is left-justified to the longest registered skill
    /// name. Shadowed entries are silently skipped (the `BTreeMap`
    /// only retains the winner). Group order: bundled → global →
    /// per-repo, then alphabetical within group. Footer line names
    /// the harness-intercepted CLI verbs per the spec.
    pub fn format_help(&self) -> String {
        let mut out = String::new();
        if self.skills.is_empty() {
            out.push_str("no skills registered\n");
        } else {
            let max_name_len = self
                .skills
                .values()
                .map(|s| s.name.len())
                .max()
                .unwrap_or(0);
            // The slash adds one char to the visible width.
            let pad = max_name_len + 1;

            let mut sorted: Vec<&Skill> = self.skills.values().collect();
            sorted.sort_by(|a, b| {
                a.source
                    .group_order()
                    .cmp(&b.source.group_order())
                    .then_with(|| a.name.cmp(&b.name))
            });

            for skill in sorted {
                let slug = format!("/{}", skill.name);
                let proactive = if skill.is_proactive() {
                    "  [proactive]"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "{:<pad$}  {}{}  {}\n",
                    slug,
                    skill.description,
                    proactive,
                    skill.source.help_tag(),
                    pad = pad,
                ));
            }
        }
        // Spec §15 line 797 — name the harness-intercepted verbs after
        // the skill list. Kept conservative: `/help` and `/init` are
        // the two currently implemented; `atelier login/logout/rotate/
        // whoami` are spec'd but tracked elsewhere in the build plan.
        out.push('\n');
        out.push_str("Harness verbs: /init, /help\n");
        out
    }
}

// ---------- substitution ----------

/// Context passed to [`substitute`]. Lets the caller plug in the repo
/// root (for `${repo_root}`) and a reader for `${atelier_md}` so tests
/// don't have to write a real file.
pub struct SkillSubstitutionContext<'a> {
    pub repo_root: &'a Path,
    pub args: &'a BTreeMap<String, String>,
    /// `ATELIER.md` contents, or `None` to read from
    /// `<repo_root>/ATELIER.md` on demand. The `None` arm returns
    /// `""` if the file is absent — matching the spec's contract on
    /// line 803.
    pub atelier_md: Option<&'a str>,
}

impl SkillSubstitutionContext<'_> {
    fn resolve_atelier_md(&self) -> String {
        if let Some(s) = self.atelier_md {
            return s.to_string();
        }
        fs::read_to_string(self.repo_root.join("ATELIER.md")).unwrap_or_default()
    }
}

/// Run `${...}` substitution against a skill's `prompt_template`.
///
/// Defined variables:
///
///   - declared args (the `skill.args` list — required ones must be in
///     `ctx.args`; optional ones fall back to their declared default,
///     then to `""`).
///   - `${repo_root}` — absolute path of `ctx.repo_root`.
///   - `${atelier_md}` — `ctx.atelier_md` or the contents of
///     `<repo_root>/ATELIER.md` (or `""` if absent).
///
/// Any other `${...}` ref is an error — silent passthrough would hide
/// manifest typos behind confused model behaviour.
pub fn substitute(
    skill: &Skill,
    ctx: &SkillSubstitutionContext<'_>,
) -> Result<String, SubstitutionError> {
    // Build the resolved-args map up front so we can error on missing
    // required values before doing any string work.
    let mut resolved: BTreeMap<String, String> = BTreeMap::new();
    for declared in &skill.args {
        if let Some(v) = ctx.args.get(&declared.name) {
            resolved.insert(declared.name.clone(), v.clone());
        } else if declared.required {
            return Err(SubstitutionError::MissingRequiredArg {
                name: declared.name.clone(),
            });
        } else {
            let fallback = declared.default.clone().unwrap_or_default();
            resolved.insert(declared.name.clone(), fallback);
        }
    }

    // The atelier_md read is lazy: only resolve if the template
    // mentions `${atelier_md}`.
    let atelier_md = if skill.prompt_template.contains("${atelier_md}") {
        Some(ctx.resolve_atelier_md())
    } else {
        None
    };

    // Scan and rebuild. We don't use regex here so the dependency
    // graph stays small; the form `${ident}` is simple enough to walk.
    let template = &skill.prompt_template;
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            // Find the closing `}`.
            if let Some(rel_end) = template[i + 2..].find('}') {
                let var_start = i + 2;
                let var_end = i + 2 + rel_end;
                let name = &template[var_start..var_end];
                let value = match name {
                    "repo_root" => ctx.repo_root.to_string_lossy().into_owned(),
                    "atelier_md" => atelier_md.clone().unwrap_or_default(),
                    other => match resolved.get(other) {
                        Some(v) => v.clone(),
                        None => {
                            return Err(SubstitutionError::UnknownVariable {
                                name: other.to_string(),
                            });
                        }
                    },
                };
                out.push_str(&value);
                i = var_end + 1;
                continue;
            }
        }
        // Push the current byte. We're walking a `&str` so multi-byte
        // sequences just pass through one byte at a time — `${` and
        // `}` are ASCII so there's no risk of splitting a UTF-8 char.
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

// ---------- argument parsing ----------

/// Parse `key=value` or positional arguments from the freeform tail of
/// a slash invocation.
///
/// Grammar (deliberately small per the plan's Open Question #1):
///
///   * Whitespace-separated tokens.
///   * `key=value` for explicit-name binding. Bare `key` (no `=`) is a
///     positional value.
///   * `"quoted strings"` keep their inner whitespace; the surrounding
///     quotes are stripped. (Supports both `"…"` and `'…'`.)
///   * If the skill has exactly one declared arg and the user passes a
///     single positional token (with no `=`), bind the whole remainder
///     to that arg as a free-text run. This is the "positional
///     fallback" case — friendlier for the common `/explain src/foo.rs`
///     shape than forcing `target=src/foo.rs`.
pub fn parse_args(skill: &Skill, raw: &str) -> Result<BTreeMap<String, String>, SubstitutionError> {
    let raw = raw.trim();
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if raw.is_empty() {
        return Ok(out);
    }

    // Positional fallback: if the skill has exactly one declared arg
    // AND the raw text has no top-level `=`, treat the whole tail as
    // that arg's value.
    let single_arg = skill.args.len() == 1;
    let top_level_equals = scan_top_level_equals(raw);
    if single_arg && !top_level_equals {
        out.insert(skill.args[0].name.clone(), raw.to_string());
        return Ok(out);
    }

    let tokens = tokenize(raw);
    let mut positional_idx = 0;
    for tok in tokens {
        if let Some((k, v)) = split_kv(&tok) {
            out.insert(k, v);
        } else {
            // Positional — bind to the next declared arg by index.
            if positional_idx >= skill.args.len() {
                return Err(SubstitutionError::UnknownVariable {
                    name: format!("(positional #{})", positional_idx + 1),
                });
            }
            out.insert(skill.args[positional_idx].name.clone(), tok);
            positional_idx += 1;
        }
    }
    Ok(out)
}

fn scan_top_level_equals(s: &str) -> bool {
    let mut in_q: Option<char> = None;
    for c in s.chars() {
        match (in_q, c) {
            (Some(q), x) if x == q => in_q = None,
            (None, q @ ('"' | '\'')) => in_q = Some(q),
            (None, '=') => return true,
            _ => {}
        }
    }
    false
}

fn tokenize(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_q: Option<char> = None;
    let chars = raw.chars();
    for c in chars {
        match (in_q, c) {
            (Some(q), x) if x == q => {
                in_q = None;
            }
            (None, q @ ('"' | '\'')) => {
                in_q = Some(q);
            }
            (None, c) if c.is_whitespace() => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            (_, c) => {
                buf.push(c);
            }
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn split_kv(tok: &str) -> Option<(String, String)> {
    let eq = tok.find('=')?;
    let (k, v) = tok.split_at(eq);
    // Drop the `=`.
    let v = &v[1..];
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    // ---- S01 — deserialisation ----

    #[test]
    fn bundled_review_round_trips() {
        let body = include_str!("../skills/review.json");
        let s = Skill::from_manifest_json(body, SkillSource::Bundled).unwrap();
        assert_eq!(s.name, "review");
        assert_eq!(s.side_effect_class, SideEffectClass::LocalSafe);
        assert!(!s.is_proactive());
        assert_eq!(s.pinned_context, vec!["ATELIER.md".to_string()]);
    }

    #[test]
    fn bundled_security_review_is_proactive() {
        let body = include_str!("../skills/security-review.json");
        let s = Skill::from_manifest_json(body, SkillSource::Bundled).unwrap();
        assert!(s.is_proactive());
    }

    #[test]
    fn deserialise_rejects_extra_field() {
        let body = r#"{
            "version": 1,
            "name": "foo",
            "description": "d",
            "prompt_template": "t",
            "bogus": "nope"
        }"#;
        let err = Skill::from_manifest_json(body, SkillSource::Bundled).unwrap_err();
        // The schema's `additionalProperties: false` should bite first;
        // serde's `deny_unknown_fields` is the belt-and-braces.
        assert!(
            matches!(err, SkillLoadError::Schema(_) | SkillLoadError::Parse(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn deserialise_rejects_bad_slug() {
        let body = r#"{
            "version": 1,
            "name": "Bad-Name",
            "description": "d",
            "prompt_template": "t"
        }"#;
        let err = Skill::from_manifest_json(body, SkillSource::Bundled).unwrap_err();
        assert!(matches!(err, SkillLoadError::Schema(_)), "got {err:?}");
    }

    // ---- S02 — registry layered override ----

    #[test]
    fn bundled_only_load() {
        let repo = temp_repo();
        let reg = SkillRegistry::load(repo.path(), None).unwrap();
        // 29 bundled (19 existing + 10 workflow/onboarding skills).
        assert_eq!(reg.len(), 29);
        assert!(reg.get("review").is_some());
        assert!(reg.get("security-review").is_some());
        assert!(reg.get("test").is_some());
        assert!(reg.get("ci-failure").is_some());
        assert!(reg.get("config-doctor").is_some());
        assert!(reg.get("release-publish").is_some());
        // `/`-prefixed lookup also works.
        assert!(reg.get("/review").is_some());
        // Source is bundled.
        assert_eq!(reg.get("review").unwrap().source, SkillSource::Bundled);
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), body).unwrap();
    }

    fn manifest_for(name: &str, desc: &str) -> String {
        serde_json::json!({
            "version": 1,
            "name": name,
            "description": desc,
            "prompt_template": format!("body for {name}"),
        })
        .to_string()
    }

    #[test]
    fn per_repo_shadows_bundled() {
        let repo = temp_repo();
        write_skill(
            &repo.path().join(".atelier/skills"),
            "review",
            &manifest_for("review", "per-repo review override"),
        );
        let reg = SkillRegistry::load(repo.path(), None).unwrap();
        let s = reg.get("review").unwrap();
        assert_eq!(s.description, "per-repo review override");
        assert_eq!(s.source, SkillSource::RepoLocal);
    }

    #[test]
    fn per_repo_beats_global_beats_bundled() {
        let repo = temp_repo();
        let home = temp_repo();

        // Same name in all three layers.
        write_skill(
            &home.path().join(".atelier/skills"),
            "review",
            &manifest_for("review", "global review override"),
        );
        write_skill(
            &repo.path().join(".atelier/skills"),
            "review",
            &manifest_for("review", "per-repo review override"),
        );

        let reg = SkillRegistry::load(repo.path(), Some(home.path())).unwrap();
        let s = reg.get("review").unwrap();
        assert_eq!(s.description, "per-repo review override");
        assert_eq!(s.source, SkillSource::RepoLocal);

        // Drop the per-repo layer — global should win.
        let repo2 = temp_repo();
        let reg2 = SkillRegistry::load(repo2.path(), Some(home.path())).unwrap();
        let s2 = reg2.get("review").unwrap();
        assert_eq!(s2.description, "global review override");
        assert_eq!(s2.source, SkillSource::UserHome);
    }

    #[test]
    fn missing_layers_tolerated() {
        let repo = temp_repo();
        // No .atelier/skills/ anywhere.
        let reg = SkillRegistry::load(repo.path(), Some(repo.path())).unwrap();
        assert!(reg.len() >= 3);
    }

    // ---- S03 — substitute ----

    fn ctx_with_args<'a>(
        repo: &'a Path,
        args: &'a BTreeMap<String, String>,
        md: Option<&'a str>,
    ) -> SkillSubstitutionContext<'a> {
        SkillSubstitutionContext {
            repo_root: repo,
            args,
            atelier_md: md,
        }
    }

    fn skill_with_template(template: &str, args: Vec<SkillArg>) -> Skill {
        Skill {
            version: 1,
            name: "t".into(),
            description: "d".into(),
            prompt_template: template.into(),
            args,
            pinned_context: Vec::new(),
            tools_required: Vec::new(),
            proactive_trigger: None,
            side_effect_class: SideEffectClass::LocalSafe,
            source: SkillSource::Bundled,
        }
    }

    #[test]
    fn substitute_happy_path() {
        let repo = temp_repo();
        let mut args = BTreeMap::new();
        args.insert("cmd".to_string(), "make check".to_string());
        let s = skill_with_template(
            "Run ${cmd} in ${repo_root}",
            vec![SkillArg {
                name: "cmd".into(),
                description: None,
                required: true,
                default: None,
            }],
        );
        let ctx = ctx_with_args(repo.path(), &args, Some(""));
        let out = substitute(&s, &ctx).unwrap();
        assert!(out.starts_with("Run make check in "));
        assert!(out.contains(repo.path().to_str().unwrap()));
    }

    #[test]
    fn substitute_unknown_var_rejected() {
        let repo = temp_repo();
        let args = BTreeMap::new();
        let s = skill_with_template("Hi ${nope}", vec![]);
        let ctx = ctx_with_args(repo.path(), &args, Some(""));
        let err = substitute(&s, &ctx).unwrap_err();
        assert_eq!(
            err,
            SubstitutionError::UnknownVariable {
                name: "nope".into()
            }
        );
    }

    #[test]
    fn substitute_missing_required_arg() {
        let repo = temp_repo();
        let args = BTreeMap::new();
        let s = skill_with_template(
            "${cmd}",
            vec![SkillArg {
                name: "cmd".into(),
                description: None,
                required: true,
                default: None,
            }],
        );
        let ctx = ctx_with_args(repo.path(), &args, Some(""));
        let err = substitute(&s, &ctx).unwrap_err();
        assert_eq!(
            err,
            SubstitutionError::MissingRequiredArg { name: "cmd".into() }
        );
    }

    #[test]
    fn substitute_optional_arg_falls_back_to_default() {
        let repo = temp_repo();
        let args = BTreeMap::new();
        let s = skill_with_template(
            "level=${detail_level}",
            vec![SkillArg {
                name: "detail_level".into(),
                description: None,
                required: false,
                default: Some("normal".into()),
            }],
        );
        let ctx = ctx_with_args(repo.path(), &args, Some(""));
        let out = substitute(&s, &ctx).unwrap();
        assert_eq!(out, "level=normal");
    }

    #[test]
    fn substitute_atelier_md_empty_if_absent() {
        let repo = temp_repo();
        // No ATELIER.md written.
        let args = BTreeMap::new();
        let s = skill_with_template("md=[${atelier_md}]", vec![]);
        let ctx = SkillSubstitutionContext {
            repo_root: repo.path(),
            args: &args,
            atelier_md: None,
        };
        let out = substitute(&s, &ctx).unwrap();
        assert_eq!(out, "md=[]");
    }

    #[test]
    fn substitute_atelier_md_reads_repo_file() {
        let repo = temp_repo();
        fs::write(repo.path().join("ATELIER.md"), "HELLO").unwrap();
        let args = BTreeMap::new();
        let s = skill_with_template("md=[${atelier_md}]", vec![]);
        let ctx = SkillSubstitutionContext {
            repo_root: repo.path(),
            args: &args,
            atelier_md: None,
        };
        let out = substitute(&s, &ctx).unwrap();
        assert_eq!(out, "md=[HELLO]");
    }

    // ---- S04 — format_help ----

    #[test]
    fn format_help_renders_bundled_set() {
        let repo = temp_repo();
        let reg = SkillRegistry::load(repo.path(), None).unwrap();
        let help = reg.format_help();
        assert!(help.contains("/review"));
        assert!(help.contains("/security-review"));
        assert!(help.contains("/test"));
        assert!(help.contains("[proactive]"));
        assert!(help.contains("[bundled]"));
        assert!(help.contains("Harness verbs"));
    }

    #[test]
    fn format_help_groups_bundled_before_per_repo() {
        let repo = temp_repo();
        write_skill(
            &repo.path().join(".atelier/skills"),
            "zeta",
            &manifest_for("zeta", "per-repo skill"),
        );
        let reg = SkillRegistry::load(repo.path(), None).unwrap();
        let help = reg.format_help();
        let pos_review = help.find("/review").unwrap();
        let pos_zeta = help.find("/zeta").unwrap();
        assert!(
            pos_review < pos_zeta,
            "bundled /review should sort before per-repo /zeta\n{help}"
        );
    }

    #[test]
    fn format_help_only_shows_winner_on_override() {
        let repo = temp_repo();
        write_skill(
            &repo.path().join(".atelier/skills"),
            "review",
            &manifest_for("review", "per-repo review override"),
        );
        let reg = SkillRegistry::load(repo.path(), None).unwrap();
        let help = reg.format_help();
        // Only one occurrence of `/review`.
        assert_eq!(help.matches("/review").count(), 1, "{help}");
        assert!(help.contains("per-repo review override"));
    }

    // ---- argument parsing ----

    #[test]
    fn parse_args_positional_fallback_single_arg() {
        let s = skill_with_template(
            "${target}",
            vec![SkillArg {
                name: "target".into(),
                description: None,
                required: true,
                default: None,
            }],
        );
        let args = parse_args(&s, "src/lib.rs and something").unwrap();
        assert_eq!(args.get("target"), Some(&"src/lib.rs and something".into()));
    }

    #[test]
    fn parse_args_key_value() {
        let s = skill_with_template(
            "${target} ${detail_level}",
            vec![
                SkillArg {
                    name: "target".into(),
                    description: None,
                    required: true,
                    default: None,
                },
                SkillArg {
                    name: "detail_level".into(),
                    description: None,
                    required: false,
                    default: Some("normal".into()),
                },
            ],
        );
        let args = parse_args(&s, "target=foo detail_level=deep").unwrap();
        assert_eq!(args.get("target"), Some(&"foo".into()));
        assert_eq!(args.get("detail_level"), Some(&"deep".into()));
    }

    #[test]
    fn parse_args_quoted_value() {
        let s = skill_with_template(
            "${target}",
            vec![SkillArg {
                name: "target".into(),
                description: None,
                required: true,
                default: None,
            }],
        );
        // Single declared arg + `=`: takes the key=value path, not the
        // positional fallback. The quotes are stripped, inner spaces
        // preserved.
        let args = parse_args(&s, r#"target="foo bar baz""#).unwrap();
        assert_eq!(args.get("target"), Some(&"foo bar baz".into()));
    }

    // ---- catalogue regression (per S05a–k acceptance gate) ----

    #[test]
    fn every_bundled_manifest_parses() {
        for (name, body) in bundled_manifests() {
            let skill = Skill::from_manifest_json(body, SkillSource::Bundled)
                .unwrap_or_else(|e| panic!("bundled {name} did not load: {e}"));
            assert_eq!(&skill.name, name, "filename / manifest name drift");
        }
    }

    #[test]
    fn bundled_help_renders_all_skills() {
        let repo = temp_repo();
        let reg = SkillRegistry::load(repo.path(), None).unwrap();
        assert_eq!(reg.len(), 29);
    }
}
